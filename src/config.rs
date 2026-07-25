use std::{fs, io, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const MIN_ICON_SIZE: i32 = 48;
pub const MAX_ICON_SIZE: i32 = 256;

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewMode {
    List,
    #[default]
    Icon,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListColumns {
    pub size: bool,
    pub kind: bool,
    pub modified: bool,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct CustomActionConfig {
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub run_on_each: bool,
    #[serde(default)]
    pub filters: Vec<String>,
}

/// A user-defined spotlight prefix, e.g. `g cats` running a web search.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightPrefixConfig {
    pub prefix: String,
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub terminal: bool,
}

/// How hard the model should work before answering. Maps to the Claude API's
/// `output_config.effort`; ignored by providers that have no equivalent.
#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AiEffort {
    /// The default: a launcher answer should come back fast.
    #[default]
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl AiEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// One AI provider bound to a spotlight prefix.
///
/// There is deliberately **no** API key field here. `AppConfig` is re-serialized
/// to `config.toml` in full every time the settings UI saves, so a key stored
/// here would be written to disk behind the user's back. Only the *name* of the
/// environment variable holding the key is recorded.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightAiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Prefix that opens a chat with this provider, e.g. `ai`.
    pub prefix: String,
    /// `claude`, `ollama`, or `mock`.
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    /// Ollama only; defaults to `http://localhost:11434`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Name of the environment variable holding the API key — never the key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Path to a file whose entire contents are the API key.
    ///
    /// A path is not a secret, so this is safe to keep in `config.toml`; the
    /// key itself stays in a file you can lock down to `0600`. Preferred over
    /// `api_key_env` for a daemon started by the compositor, which does not
    /// inherit a shell's exported variables.
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default = "default_ai_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub effort: AiEffort,
    /// Offer this provider on plain, unprefixed queries.
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct SpotlightConfig {
    #[serde(default)]
    pub prefixes: Vec<SpotlightPrefixConfig>,
    /// Built-in prefix keys to remove, e.g. `["="]` to drop the calculator.
    #[serde(default)]
    pub disabled_builtins: Vec<String>,
    #[serde(default = "default_result_limit")]
    pub result_limit: usize,
    /// Distance from the top of the screen, as a fraction of its height.
    #[serde(default = "default_top_ratio")]
    pub top_ratio: f64,
    #[serde(default = "default_spotlight_width")]
    pub width: i32,
    #[serde(default)]
    pub ai: Vec<SpotlightAiConfig>,
}

fn default_true() -> bool {
    true
}

/// Generous on purpose: on Claude Opus 5 thinking is on by default and shares
/// this budget with the visible answer, so a small cap truncates the reply.
fn default_ai_max_tokens() -> u32 {
    8192
}

fn default_result_limit() -> usize {
    12
}

fn default_top_ratio() -> f64 {
    0.22
}

fn default_spotlight_width() -> i32 {
    640
}

// Deliberately hand-written: `#[serde(default)]` on `AppConfig::spotlight` calls
// this for every user without a `[spotlight]` block, and a derived `Default`
// would hand them a zero-width window showing zero results.
impl Default for SpotlightConfig {
    fn default() -> Self {
        Self {
            prefixes: Vec::new(),
            disabled_builtins: Vec::new(),
            result_limit: default_result_limit(),
            top_ratio: default_top_ratio(),
            width: default_spotlight_width(),
            ai: Vec::new(),
        }
    }
}

impl SpotlightConfig {
    pub const MIN_WIDTH: i32 = 320;
    pub const MAX_WIDTH: i32 = 1200;

    pub fn clamped_width(&self) -> i32 {
        self.width.clamp(Self::MIN_WIDTH, Self::MAX_WIDTH)
    }

    pub fn clamped_top_ratio(&self) -> f64 {
        self.top_ratio.clamp(0.0, 0.8)
    }

    pub fn clamped_result_limit(&self) -> usize {
        self.result_limit.clamp(1, 50)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub default_view: ViewMode,
    pub show_hidden: bool,
    pub icon_size: i32,
    pub sidebar_width: i32,
    pub custom_css: Option<PathBuf>,
    pub list_columns: ListColumns,
    #[serde(default)]
    pub actions: Vec<CustomActionConfig>,
    #[serde(default)]
    pub spotlight: SpotlightConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_view: ViewMode::Icon,
            show_hidden: false,
            icon_size: 128,
            sidebar_width: 220,
            custom_css: None,
            list_columns: ListColumns {
                size: true,
                kind: true,
                modified: true,
            },
            actions: Vec::new(),
            spotlight: SpotlightConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };

        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to parse config, using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        ProjectDirs::from("io.github", "ionix", "ioexplorer")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = Self::config_path() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, contents)
    }
}

pub fn clamp_icon_size(icon_size: i32) -> i32 {
    icon_size.clamp(MIN_ICON_SIZE, MAX_ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_view_mode_names() {
        let parsed: AppConfig = toml::from_str(
            r#"
default_view = "list"
show_hidden = true
icon_size = 64
sidebar_width = 210

[list_columns]
size = true
kind = false
modified = true

[[actions]]
label = "Open in Editor"
command = "code --reuse-window"
run_on_each = true
filters = ["*.txt", "*.md"]
"#,
        )
        .expect("valid config");

        assert_eq!(parsed.default_view, ViewMode::List);
        assert!(parsed.show_hidden);
        assert!(!parsed.list_columns.kind);
        assert_eq!(parsed.actions.len(), 1);
        assert_eq!(parsed.actions[0].label, "Open in Editor");
        assert!(parsed.actions[0].run_on_each);
    }

    #[test]
    fn missing_spotlight_section_uses_working_defaults() {
        let parsed: AppConfig = toml::from_str(
            r#"
default_view = "icon"
show_hidden = false
icon_size = 128
sidebar_width = 220

[list_columns]
size = true
kind = true
modified = true
"#,
        )
        .expect("valid config");

        assert_eq!(parsed.spotlight.width, 640);
        assert_eq!(parsed.spotlight.result_limit, 12);
        assert_eq!(parsed.spotlight.top_ratio, 0.22);
        assert!(parsed.spotlight.prefixes.is_empty());
    }

    #[test]
    fn parses_spotlight_prefixes() {
        let parsed: AppConfig = toml::from_str(
            r#"
default_view = "icon"
show_hidden = false
icon_size = 128
sidebar_width = 220

[list_columns]
size = true
kind = true
modified = true

[spotlight]
width = 720
disabled_builtins = ["/"]

[[spotlight.prefixes]]
prefix = "g"
label = "Google search"
command = "xdg-open 'https://google.com/search?q={query_url}'"
"#,
        )
        .expect("valid config");

        assert_eq!(parsed.spotlight.width, 720);
        assert_eq!(parsed.spotlight.result_limit, 12);
        assert_eq!(parsed.spotlight.disabled_builtins, vec!["/".to_string()]);
        assert_eq!(parsed.spotlight.prefixes.len(), 1);
        assert_eq!(parsed.spotlight.prefixes[0].prefix, "g");
        assert!(!parsed.spotlight.prefixes[0].terminal);
    }

    #[test]
    fn spotlight_config_round_trips_through_toml() {
        let config = AppConfig {
            spotlight: SpotlightConfig {
                prefixes: vec![SpotlightPrefixConfig {
                    prefix: "g".to_string(),
                    label: "Google search".to_string(),
                    command: "xdg-open 'https://google.com/search?q={query_url}'".to_string(),
                    description: None,
                    icon: None,
                    terminal: false,
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        let contents = toml::to_string_pretty(&config).expect("serializable config");
        let parsed: AppConfig = toml::from_str(&contents).expect("valid config");

        assert_eq!(parsed.spotlight, config.spotlight);
    }

    #[test]
    fn parses_spotlight_ai_providers() {
        let parsed: AppConfig = toml::from_str(
            r#"
default_view = "icon"
show_hidden = false
icon_size = 128
sidebar_width = 220

[list_columns]
size = true
kind = true
modified = true

[[spotlight.ai]]
prefix = "ai"
provider = "claude"
model = "claude-opus-5"
default = true
effort = "medium"

[[spotlight.ai]]
prefix = "ol"
provider = "ollama"
model = "llama3.2"
endpoint = "http://localhost:11434"
"#,
        )
        .expect("valid config");

        assert_eq!(parsed.spotlight.ai.len(), 2);
        assert!(parsed.spotlight.ai[0].enabled, "enabled defaults to true");
        assert!(parsed.spotlight.ai[0].default);
        assert_eq!(parsed.spotlight.ai[0].effort, AiEffort::Medium);
        assert_eq!(parsed.spotlight.ai[0].max_tokens, 8192);
        assert!(!parsed.spotlight.ai[1].default);
        assert_eq!(parsed.spotlight.ai[1].effort, AiEffort::Low);
    }

    #[test]
    fn missing_ai_section_yields_no_providers() {
        let parsed: AppConfig = toml::from_str(
            r#"
default_view = "icon"
show_hidden = false
icon_size = 128
sidebar_width = 220

[list_columns]
size = true
kind = true
modified = true
"#,
        )
        .expect("valid config");

        assert!(parsed.spotlight.ai.is_empty());
        assert_eq!(parsed.spotlight.width, 640, "other defaults stay intact");
    }

    /// Guards the rule that an API key must never reach `config.toml`: the
    /// settings UI re-serializes the whole `AppConfig` on every save, so a key
    /// field added here later would silently land on disk.
    #[test]
    fn serialized_config_never_contains_an_api_key() {
        let config = AppConfig {
            spotlight: SpotlightConfig {
                ai: vec![SpotlightAiConfig {
                    enabled: true,
                    prefix: "ai".to_string(),
                    provider: "claude".to_string(),
                    model: Some("claude-opus-5".to_string()),
                    label: None,
                    icon: None,
                    endpoint: None,
                    api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                    api_key_file: None,
                    max_tokens: 8192,
                    effort: AiEffort::Low,
                    default: true,
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        let contents = toml::to_string_pretty(&config).expect("serializable config");

        assert!(
            contents.contains("api_key_env"),
            "the var name is not secret"
        );
        assert!(
            !contents.contains("api_key ="),
            "an API key must never be written to config.toml"
        );
        assert!(!contents.contains("sk-ant-"));
    }

    #[test]
    fn spotlight_ai_round_trips_through_toml() {
        let config = AppConfig {
            spotlight: SpotlightConfig {
                ai: vec![SpotlightAiConfig {
                    enabled: true,
                    prefix: "ol".to_string(),
                    provider: "ollama".to_string(),
                    model: Some("llama3.2".to_string()),
                    label: Some("Local".to_string()),
                    icon: None,
                    endpoint: Some("http://localhost:11434".to_string()),
                    api_key_env: None,
                    api_key_file: None,
                    max_tokens: 4096,
                    effort: AiEffort::High,
                    default: false,
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        let contents = toml::to_string_pretty(&config).expect("serializable config");
        let parsed: AppConfig = toml::from_str(&contents).expect("valid config");

        assert_eq!(parsed.spotlight, config.spotlight);
    }

    #[test]
    fn clamps_spotlight_bounds() {
        let config = SpotlightConfig {
            width: 10_000,
            top_ratio: 5.0,
            result_limit: 0,
            ..Default::default()
        };

        assert_eq!(config.clamped_width(), SpotlightConfig::MAX_WIDTH);
        assert_eq!(config.clamped_top_ratio(), 0.8);
        assert_eq!(config.clamped_result_limit(), 1);
    }

    #[test]
    fn missing_run_on_each_defaults_to_false() {
        let parsed: CustomActionConfig = toml::from_str(
            r#"
label = "Open in Editor"
command = "code --reuse-window"
filters = ["*.txt"]
"#,
        )
        .expect("valid action config");

        assert!(!parsed.run_on_each);
    }

    #[test]
    fn serializes_actions_as_toml_array() {
        let config = AppConfig {
            actions: vec![CustomActionConfig {
                label: "Open in Editor".to_string(),
                command: "code --reuse-window".to_string(),
                run_on_each: true,
                filters: vec!["*.txt".to_string(), "*.md".to_string()],
            }],
            ..Default::default()
        };

        let contents = toml::to_string_pretty(&config).expect("serializable config");

        assert!(contents.contains("[[actions]]"));
        assert!(contents.contains("label = \"Open in Editor\""));
        assert!(contents.contains("run_on_each = true"));
        assert!(contents.contains("filters = ["));
        assert!(contents.contains("\"*.txt\""));
        assert!(contents.contains("\"*.md\""));
    }
}
