//! AI chat providers for spotlight.
//!
//! The provider set is a plain enum rather than a trait object: adding a third
//! backend means one variant, one arm in [`resolve_providers`], one arm in
//! [`run_stream`], and one new file — nothing in the window, keyboard, results
//! or config plumbing changes.
//!
//! Everything that crosses the worker-thread boundary ([`Provider`],
//! [`ChatMessage`], [`AiEvent`]) is plain data. No GObject may travel here; the
//! `assert_send` guard at the bottom of this file makes that a compile error
//! rather than a runtime surprise.

mod claude;
pub mod markdown;
mod mock;
mod ndjson;
mod ollama;
mod sse;
pub mod tools;

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use crate::config::{AiEffort, SpotlightAiConfig, SpotlightConfig};

/// Model used when a `claude` provider names none.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5";
/// Environment variable consulted when a `claude` provider names none.
pub const DEFAULT_CLAUDE_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const DEFAULT_OLLAMA_ENDPOINT: &str = "http://localhost:11434";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

/// One block inside a turn.
///
/// A turn is a list of blocks rather than a string because tool use cannot be
/// expressed any other way: the model's request to call a tool rides in the
/// *assistant* turn as a `tool_use` block, and its outcome rides in the
/// *following user* turn as a `tool_result` carrying the same id. Flattening
/// either to text loses the id, and the API rejects a `tool_use` whose
/// `tool_result` never arrives.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Text(String),
    /// The model asking for a tool to be run.
    ToolUse {
        id: String,
        name: String,
        /// Always a JSON object in practice, but held as a `Value` because it is
        /// model output — a malformed input is data to reject, not to panic on.
        input: serde_json::Value,
    },
    /// The outcome handed back. `is_error` is how a failure — or a user
    /// declining — is reported without breaking the id pairing.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// One conversation turn.
///
/// Not `Eq`: a `tool_use` input is a `serde_json::Value`, which is only
/// `PartialEq` because it can hold a float.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            blocks: vec![Block::Text(text.into())],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            blocks: vec![Block::Text(text.into())],
        }
    }

    /// The turn's visible text, with every non-text block dropped.
    ///
    /// For display and for providers with no tool-block wire format of their
    /// own. Blocks are joined with a newline so two separate text blocks do not
    /// run together into one word.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn api_role(&self) -> &'static str {
        match self.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    /// The turn's `content` in Claude's wire format.
    ///
    /// A turn that is nothing but one text block serializes to a bare string
    /// rather than a one-element array. Both are accepted, but the string is
    /// what Phase 1 sent, and the prompt cache keys on the exact bytes — so
    /// preserving it keeps every existing conversation's cached prefix valid.
    fn api_content(&self) -> serde_json::Value {
        if let [Block::Text(text)] = self.blocks.as_slice() {
            return serde_json::Value::String(text.clone());
        }

        serde_json::Value::Array(
            self.blocks
                .iter()
                .map(|block| match block {
                    Block::Text(text) => serde_json::json!({ "type": "text", "text": text }),
                    Block::ToolUse { id, name, input } => serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    }),
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                        "is_error": is_error,
                    }),
                })
                .collect(),
        )
    }
}

/// An API key that cannot be printed by accident.
///
/// No `Display`, and `Debug` redacts — so a `tracing::warn!(?provider)` further
/// up the stack is structurally incapable of leaking it.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(***)")
    }
}

/// What the worker reports back. Plain data — safe across the channel.
#[derive(Clone, Debug)]
pub enum AiEvent {
    /// The request is in flight; nothing has arrived yet.
    Started {
        generation: u64,
    },
    /// The model opened a reasoning block. On Claude this is accurate and can
    /// legitimately last seconds before any visible text.
    Thinking {
        generation: u64,
    },
    /// Visible assistant text. The UI coalesces these per tick.
    Delta {
        generation: u64,
        text: String,
    },
    /// The model wants tools run. The worker stops here: execution, approval
    /// and the follow-up request all belong to the main thread.
    ToolRequested {
        generation: u64,
        /// The assistant text that preceded the call, which has to be replayed
        /// in the assistant turn alongside the `tool_use` blocks.
        text: String,
        calls: Vec<tools::ToolCall>,
    },
    Done {
        generation: u64,
        stop_reason: Option<String>,
    },
    Failed {
        generation: u64,
        error: AiError,
    },
}

impl AiEvent {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Started { generation }
            | Self::Thinking { generation }
            | Self::Delta { generation, .. }
            | Self::ToolRequested { generation, .. }
            | Self::Done { generation, .. }
            | Self::Failed { generation, .. } => *generation,
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum AiError {
    #[error(
        "No API key. Point `api_key_file` at a file containing the key, or set \
the {var} environment variable and restart the daemon. The key itself cannot go \
in config.toml."
    )]
    MissingKey { var: String },
    #[error("Cannot read the key file {path}: {detail}")]
    KeyFileUnreadable { path: String, detail: String },
    #[error("Cannot reach {endpoint} — {detail}")]
    Network { endpoint: String, detail: String },
    #[error("{provider} rejected the API key in {var}.")]
    Unauthorized { provider: String, var: String },
    #[error("Rate limited{}.", .retry_after.map(|s| format!(" — retry in {s}s")).unwrap_or_default())]
    RateLimited { retry_after: Option<u64> },
    #[error("{provider} returned HTTP {status}: {message}")]
    Http {
        provider: String,
        status: u16,
        message: String,
    },
    #[error("The model declined this request{}.", .category.as_ref().map(|c| format!(" ({c})")).unwrap_or_default())]
    Refused { category: Option<String> },
    #[error("Model '{model}' is not available{}", .hint.as_ref().map(|h| format!(" — {h}")).unwrap_or_else(|| ".".to_string()))]
    ModelUnavailable { model: String, hint: Option<String> },
    #[error("Malformed response from {provider}: {detail}")]
    Protocol { provider: String, detail: String },
}

/// A validated, ready-to-run backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    Claude {
        model: String,
        max_tokens: u32,
        effort: AiEffort,
        api_key_env: String,
        api_key_file: Option<PathBuf>,
    },
    Ollama {
        model: String,
        endpoint: String,
    },
    /// A scripted local echo. No network — the way to exercise the chat UI
    /// before any credentials exist.
    Mock {
        model: String,
    },
}

impl Provider {
    pub fn model(&self) -> &str {
        match self {
            Self::Claude { model, .. } | Self::Ollama { model, .. } | Self::Mock { model } => model,
        }
    }
}

/// A configured provider bound to a spotlight prefix.
///
/// Not `Eq`: the resolved tool set carries JSON schemas, which are only
/// `PartialEq`.
#[derive(Clone, Debug, PartialEq)]
pub struct AiProvider {
    pub prefix: String,
    pub label: String,
    pub icon: String,
    pub default: bool,
    /// Resolved once, at config load. The tool list renders at the very front
    /// of the prompt, so it must not be rebuilt per turn.
    pub tools: Vec<tools::ToolDef>,
    /// Declare Anthropic's server-side web search and fetch. Claude only, so a
    /// non-Claude provider that asks for it is told rather than silently ignored.
    pub web_search: bool,
    pub provider: Provider,
}

/// Turns config entries into runnable providers.
///
/// Pure: reads no environment and performs no I/O, so it is fully unit-testable.
/// Invalid and disabled entries are dropped with a warning rather than aborting,
/// matching how `prefixes::resolve` treats a malformed prefix.
pub fn resolve_providers(config: &SpotlightConfig) -> Vec<AiProvider> {
    let mut providers: Vec<AiProvider> = Vec::new();
    let mut seen_default = false;

    for entry in &config.ai {
        if !entry.enabled {
            continue;
        }

        let prefix = entry.prefix.trim();
        if prefix.is_empty() || prefix.chars().any(char::is_whitespace) {
            tracing::warn!(
                prefix = entry.prefix,
                "ignoring ai provider with an invalid prefix"
            );
            continue;
        }
        if providers.iter().any(|existing| existing.prefix == prefix) {
            tracing::warn!(prefix, "ignoring ai provider with a duplicate prefix");
            continue;
        }

        let Some(provider) = backend_from_config(entry) else {
            continue;
        };

        // Only the first `default = true` wins, so a plain query has one target.
        let is_default = entry.default && !seen_default;
        if entry.default && seen_default {
            tracing::warn!(prefix, "ignoring a second default ai provider");
        }
        seen_default |= is_default;

        providers.push(AiProvider {
            prefix: prefix.to_string(),
            label: entry
                .label
                .clone()
                .unwrap_or_else(|| default_label(&entry.provider, provider.model())),
            icon: entry
                .icon
                .clone()
                .unwrap_or_else(|| "dialog-information-symbolic".to_string()),
            default: is_default,
            tools: tools::definitions(entry),
            web_search: web_search_for(entry, &provider),
            provider,
        });
    }

    providers
}

/// Whether to declare Anthropic's server-side web tools.
///
/// Ollama has no server-side equivalent, and declaring them would be silently
/// ignored — saying so once beats leaving the user to wonder why the model
/// never searches.
fn web_search_for(entry: &SpotlightAiConfig, provider: &Provider) -> bool {
    if entry.web_search && !matches!(provider, Provider::Claude { .. }) {
        tracing::warn!(
            prefix = entry.prefix,
            "web_search needs Claude; this provider has no server-side web tools"
        );
        return false;
    }
    entry.web_search
}

fn backend_from_config(entry: &SpotlightAiConfig) -> Option<Provider> {
    match entry.provider.trim().to_lowercase().as_str() {
        "claude" | "anthropic" => Some(Provider::Claude {
            model: entry
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.to_string()),
            max_tokens: entry.max_tokens.max(1024),
            effort: entry.effort,
            api_key_env: entry
                .api_key_env
                .clone()
                .unwrap_or_else(|| DEFAULT_CLAUDE_KEY_ENV.to_string()),
            api_key_file: entry.api_key_file.clone(),
        }),
        "ollama" => Some(Provider::Ollama {
            model: entry
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string()),
            endpoint: entry
                .endpoint
                .clone()
                .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_string())
                .trim_end_matches('/')
                .to_string(),
        }),
        "mock" => Some(Provider::Mock {
            model: entry.model.clone().unwrap_or_else(|| "mock".to_string()),
        }),
        other => {
            tracing::warn!(
                provider = other,
                "ignoring ai provider with an unknown backend"
            );
            None
        }
    }
}

fn default_label(provider: &str, model: &str) -> String {
    match provider.trim().to_lowercase().as_str() {
        "claude" | "anthropic" => "Claude".to_string(),
        "ollama" => format!("Ollama · {model}"),
        "mock" => "Mock".to_string(),
        other => other.to_string(),
    }
}

/// A conversation's in-flight request.
///
/// Same shape as [`crate::spotlight::file_search::FileSearch`]: a monotonic
/// generation invalidates superseded work both inside the worker and on the
/// main thread, so cancellation never needs a join or a lock.
/// Everything one request needs. Taken by value — nothing borrowed from the
/// window may reach the worker thread.
pub struct Turn {
    pub provider: Provider,
    pub history: Vec<ChatMessage>,
    pub tools: Vec<tools::ToolDef>,
    /// Declare Anthropic's server-side web search and fetch.
    pub web_search: bool,
}

pub struct AiSession {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<AiEvent>,
    receiver: mpsc::Receiver<AiEvent>,
}

impl AiSession {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Invalidates any in-flight request without starting a new one.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Starts a request, superseding any in-flight one. Returns its generation.
    ///
    /// `provider` and `history` are taken by value: nothing borrowed from the
    /// window may reach the worker thread.
    pub fn start(&self, turn: Turn) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);
        thread::spawn(move || {
            let is_stale = || counter.load(Ordering::Relaxed) != generation;
            let mut emit = |event: AiEvent| sender.send(event).is_ok() && !is_stale();

            emit(AiEvent::Started { generation });
            if is_stale() {
                return;
            }
            run_stream(&turn, generation, &is_stale, &mut emit);
        });

        generation
    }

    /// Collects events that are still current, discarding superseded ones.
    pub fn drain(&self) -> Vec<AiEvent> {
        let current = self.current_generation();
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            if event.generation() == current {
                events.push(event);
            }
        }
        events
    }
}

impl Default for AiSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs one request to completion on the worker thread.
///
/// `emit` returns false once the caller has moved on (channel closed or this
/// generation superseded), which unwinds the provider loop promptly.
fn run_stream(
    turn: &Turn,
    generation: u64,
    is_stale: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(AiEvent) -> bool,
) {
    let history = turn.history.as_slice();
    match &turn.provider {
        Provider::Claude {
            model,
            max_tokens,
            effort,
            api_key_env,
            api_key_file,
        } => claude::run(
            claude::Request {
                model,
                max_tokens: *max_tokens,
                effort: *effort,
                api_key_env,
                api_key_file: api_key_file.as_deref(),
                tools: turn.tools.as_slice(),
                web_search: turn.web_search,
            },
            history,
            generation,
            is_stale,
            emit,
        ),
        Provider::Ollama { model, endpoint } => ollama::run(
            model,
            endpoint,
            history,
            turn.tools.as_slice(),
            generation,
            is_stale,
            emit,
        ),
        Provider::Mock { .. } => mock::run(history, generation, is_stale, emit),
    }
}

// A future field that is not `Send` must fail to compile, not fail to spawn.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Provider>();
    assert_send::<ChatMessage>();
    assert_send::<AiEvent>();
    assert_send::<Turn>();
    assert_send::<tools::ToolDef>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 1 sent a bare string for a plain turn, and the prompt cache keys on
    /// the exact bytes — emitting a one-element array instead would invalidate
    /// every cached conversation prefix for no benefit.
    #[test]
    fn a_plain_text_turn_still_serializes_to_a_bare_string() {
        assert_eq!(
            ChatMessage::user("hello").api_content(),
            serde_json::json!("hello")
        );
        assert_eq!(
            ChatMessage::assistant("hi there").api_content(),
            serde_json::json!("hi there")
        );
    }

    #[test]
    fn an_assistant_turn_carries_its_tool_use_blocks() {
        let message = ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Text("Let me look.".to_string()),
                Block::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "search_files".to_string(),
                    input: serde_json::json!({ "query": "invoice" }),
                },
            ],
        };

        assert_eq!(message.api_role(), "assistant");
        assert_eq!(
            message.api_content(),
            serde_json::json!([
                { "type": "text", "text": "Let me look." },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "search_files",
                    "input": { "query": "invoice" },
                },
            ])
        );
    }

    /// The result rides in the *following user* turn and must carry the same id,
    /// or the API rejects the request.
    #[test]
    fn a_tool_result_pairs_with_its_id_in_a_user_turn() {
        let message = ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "no matches".to_string(),
                is_error: false,
            }],
        };

        assert_eq!(message.api_role(), "user");
        assert_eq!(
            message.api_content(),
            serde_json::json!([{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "no matches",
                "is_error": false,
            }])
        );
    }

    /// A failure — including the user declining — has to come back as a result
    /// rather than a dropped block, so the model can adapt instead of hanging.
    #[test]
    fn a_failed_tool_is_still_a_result() {
        let message = ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "toolu_2".to_string(),
                content: "The user declined to run this.".to_string(),
                is_error: true,
            }],
        };

        assert_eq!(
            message.api_content()[0]["is_error"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn text_reads_through_non_text_blocks() {
        let message = ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Text("first".to_string()),
                Block::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "calculate".to_string(),
                    input: serde_json::json!({}),
                },
                Block::Text("second".to_string()),
            ],
        };

        assert_eq!(message.text(), "first\nsecond");
        // A turn that is only a tool call has nothing to display.
        assert_eq!(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "calculate".to_string(),
                    input: serde_json::json!({}),
                }],
            }
            .text(),
            ""
        );
    }

    fn entry(prefix: &str, provider: &str) -> SpotlightAiConfig {
        SpotlightAiConfig {
            enabled: true,
            prefix: prefix.to_string(),
            provider: provider.to_string(),
            model: None,
            label: None,
            icon: None,
            endpoint: None,
            api_key_env: None,
            api_key_file: None,
            max_tokens: 8192,
            effort: AiEffort::Low,
            default: false,
            builtin_tools: false,
            run_command: false,
            web_search: false,
            tools: Vec::new(),
        }
    }

    fn config(entries: Vec<SpotlightAiConfig>) -> SpotlightConfig {
        SpotlightConfig {
            ai: entries,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_claude_and_ollama_defaults() {
        let providers =
            resolve_providers(&config(vec![entry("ai", "claude"), entry("ol", "ollama")]));

        assert_eq!(providers.len(), 2);
        assert_eq!(
            providers[0].provider,
            Provider::Claude {
                model: DEFAULT_CLAUDE_MODEL.to_string(),
                max_tokens: 8192,
                effort: AiEffort::Low,
                api_key_env: DEFAULT_CLAUDE_KEY_ENV.to_string(),
                api_key_file: None,
            }
        );
        assert_eq!(providers[0].label, "Claude");
        assert_eq!(
            providers[1].provider,
            Provider::Ollama {
                model: DEFAULT_OLLAMA_MODEL.to_string(),
                endpoint: DEFAULT_OLLAMA_ENDPOINT.to_string(),
            }
        );
    }

    #[test]
    fn trims_a_trailing_slash_from_the_ollama_endpoint() {
        let providers = resolve_providers(&config(vec![SpotlightAiConfig {
            endpoint: Some("http://localhost:11434/".to_string()),
            ..entry("ol", "ollama")
        }]));

        assert_eq!(
            providers[0].provider,
            Provider::Ollama {
                model: DEFAULT_OLLAMA_MODEL.to_string(),
                endpoint: "http://localhost:11434".to_string(),
            }
        );
    }

    #[test]
    fn disabled_entries_are_dropped_without_leaving_index_gaps() {
        let providers = resolve_providers(&config(vec![
            SpotlightAiConfig {
                enabled: false,
                ..entry("off", "claude")
            },
            entry("ai", "claude"),
        ]));

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].prefix, "ai");
    }

    #[test]
    fn unknown_backends_are_skipped() {
        let providers =
            resolve_providers(&config(vec![entry("x", "gpt-9"), entry("ai", "claude")]));

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].prefix, "ai");
    }

    #[test]
    fn invalid_prefixes_are_skipped() {
        let providers = resolve_providers(&config(vec![
            entry("", "claude"),
            entry("two words", "claude"),
            entry("ai", "claude"),
        ]));

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].prefix, "ai");
    }

    #[test]
    fn a_duplicate_prefix_keeps_the_first_entry() {
        let providers = resolve_providers(&config(vec![
            SpotlightAiConfig {
                label: Some("first".to_string()),
                ..entry("ai", "claude")
            },
            SpotlightAiConfig {
                label: Some("second".to_string()),
                ..entry("ai", "ollama")
            },
        ]));

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].label, "first");
    }

    #[test]
    fn only_the_first_default_provider_wins() {
        let providers = resolve_providers(&config(vec![
            SpotlightAiConfig {
                default: true,
                builtin_tools: false,
                run_command: false,
                web_search: false,
                tools: Vec::new(),
                ..entry("ai", "claude")
            },
            SpotlightAiConfig {
                default: true,
                builtin_tools: false,
                run_command: false,
                web_search: false,
                tools: Vec::new(),
                ..entry("ol", "ollama")
            },
        ]));

        assert!(providers[0].default);
        assert!(!providers[1].default, "a second default is ignored");
    }

    #[test]
    fn max_tokens_has_a_floor() {
        // Thinking shares this budget on Opus 5; a tiny cap truncates the answer.
        let providers = resolve_providers(&config(vec![SpotlightAiConfig {
            max_tokens: 16,
            ..entry("ai", "claude")
        }]));

        assert_eq!(
            providers[0].provider,
            Provider::Claude {
                model: DEFAULT_CLAUDE_MODEL.to_string(),
                max_tokens: 1024,
                effort: AiEffort::Low,
                api_key_env: DEFAULT_CLAUDE_KEY_ENV.to_string(),
                api_key_file: None,
            }
        );
    }

    #[test]
    fn api_keys_are_redacted_in_debug_output() {
        let key = ApiKey("sk-ant-secret-value".to_string());

        assert_eq!(format!("{key:?}"), "ApiKey(***)");
        assert!(!format!("{key:?}").contains("secret"));
    }

    #[test]
    fn drain_discards_superseded_generations() {
        let session = AiSession::new();
        session
            .sender
            .send(AiEvent::Delta {
                generation: 0,
                text: "stale".to_string(),
            })
            .expect("send");
        session.cancel();

        assert!(session.drain().is_empty());
    }
}
