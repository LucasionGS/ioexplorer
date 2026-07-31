use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::sorting::SortOrder;

pub const MIN_ICON_SIZE: i32 = 48;
pub const MAX_ICON_SIZE: i32 = 256;

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewMode {
    List,
    #[default]
    Icon,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListColumns {
    pub size: bool,
    pub kind: bool,
    pub modified: bool,
    /// Off by default: a fourth metadata column crowds the row, and most
    /// filesystems' birth times are less useful than the modified time beside
    /// it. Defaulted rather than required so configs predating it still load.
    #[serde(default)]
    pub created: bool,
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

/// A user-defined spotlight prefix.
///
/// Two shapes. A `command` prefix runs one fixed command line, e.g. `g cats`
/// running a web search. A `get_results` prefix instead asks a command for a
/// list of rows to pick from, and runs `action` on whichever the user chooses.
///
/// Not `Eq`: `delay` is a float, so only `PartialEq` is available.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct SpotlightPrefixConfig {
    pub prefix: String,
    /// Falls back to the prefix key when empty.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub terminal: bool,
    /// Command printing `{"results": [{"title", "value", "icon"}]}` on stdout.
    /// Takes precedence over `command`.
    #[serde(default)]
    pub get_results: Option<String>,
    /// Command run when a `get_results` row is activated. `{value}` is
    /// substituted shell-quoted, `{value_escaped}` backslash-escaped.
    #[serde(default)]
    pub action: Option<String>,
    /// Seconds of quiet typing before `get_results` runs, so the command is not
    /// spawned on every keystroke. Defaults to 0.5.
    #[serde(default)]
    pub delay: Option<f64>,
    /// Pixel size of the artwork a `get_results` row carries. Defaults to 22,
    /// which suits a glyph; a provider returning photographs wants far more.
    #[serde(default)]
    pub icon_size: Option<i32>,
    /// Lets the user page through a `get_results` command's output. Requires a
    /// `{page}` in `get_results` — there is nowhere else to put the number.
    #[serde(default)]
    pub pagination: bool,
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
    /// Let the model use the built-in tools. Off by default: a provider that can
    /// act on your machine is a different thing from one that can only answer.
    #[serde(default)]
    pub builtin_tools: bool,
    /// Let the model run arbitrary shell commands. Gated separately from
    /// `builtin_tools` because no other built-in can do unbounded damage, and it
    /// is the tool a prompt-injection payload would aim for.
    #[serde(default)]
    pub run_command: bool,
    /// Declare Anthropic's server-side web search and fetch. Claude only —
    /// Ollama has no equivalent.
    #[serde(default)]
    pub web_search: bool,
    /// Replaces the built-in system prompt outright.
    ///
    /// The built-in one is what makes the model work a problem through with its
    /// tools instead of stopping to ask; an override is the whole prompt, so it
    /// has to say that itself if that behaviour is wanted.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// How many rounds of tool calls one prompt may take before the model has to
    /// answer with what it has. Generous, because the point of the tools is that
    /// it can keep digging rather than hand the work back.
    #[serde(default = "default_ai_max_tool_rounds")]
    pub max_tool_rounds: usize,
    /// Seconds a single command may run before it is stopped. A long build or
    /// download needs a bigger number; the command runs on a worker, so raising
    /// it never blocks the overlay.
    #[serde(default = "default_ai_command_timeout")]
    pub command_timeout: u64,
    /// User-defined tools, as `[[spotlight.ai.tools]]`.
    #[serde(default)]
    pub tools: Vec<SpotlightAiToolConfig>,
}

/// Whether a custom tool asks before running.
#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AiToolConfirm {
    /// The default. Every run shows the expanded command first.
    #[default]
    Always,
    /// Runs unattended. The user's explicit, per-tool choice.
    Never,
}

/// The JSON-Schema types a custom tool parameter may take.
///
/// Deliberately small: the schema is generated from these, so nobody has to
/// write JSON Schema in TOML, and every value maps to something that can be
/// shell-quoted into a command.
#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AiParamType {
    #[default]
    String,
    Integer,
    Number,
    Boolean,
}

impl AiParamType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightAiToolParam {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: AiParamType,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// A user-defined tool: a command template plus typed parameters.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightAiToolConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Command template. Each parameter is substituted as `{name}`, always
    /// shell-quoted — the values are model output.
    pub command: String,
    #[serde(default)]
    pub confirm: AiToolConfirm,
    #[serde(default)]
    pub params: Vec<SpotlightAiToolParam>,
}

/// The open-window switcher: listing running apps and focusing one.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightWindowsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Prefix that lists only open windows. Defaults to `w`.
    #[serde(default = "default_windows_prefix")]
    pub prefix: String,
    /// Also offer open windows on plain, unprefixed queries, so typing an app's
    /// name reaches the copy already running.
    #[serde(default = "default_true")]
    pub in_search: bool,
}

/// The VPN prefix: connecting, disconnecting, and picking a location.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightVpnConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Prefix that drives the VPN. Defaults to `vpn`.
    #[serde(default = "default_vpn_prefix")]
    pub prefix: String,
    /// Which VPN to drive, e.g. `windscribe`. Left unset, the installed
    /// providers are detected instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// The password-manager prefix: searching a vault and copying a secret out of it.
///
/// Off unless the user asks for it, unlike the VPN. The VPN prefix appears
/// wherever a client is installed because the worst it can do is report a
/// disconnected tunnel; this one sends a long-lived API token to a remote vault
/// on the user's behalf, and turning that on because a binary happens to be on
/// `PATH` is not a decision this config gets to make for them.
///
/// The credentials themselves are deliberately not fields here. `config.toml` is
/// world-readable in a directory the hot-reload watcher polls, so the token is
/// named by the command that prints it — `secret-tool`, `pass`, `gpg` — or left
/// out entirely, in which case the client reads it from the environment
/// IoExplorer was started with.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightPasswordsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Prefix that searches the vault. Defaults to `pw`.
    #[serde(default = "default_passwords_prefix")]
    pub prefix: String,
    /// Which password manager to drive, e.g. `passwork`. Left unset, the
    /// installed clients are detected instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The vault server the client talks to. Not a secret, so it is spelled out
    /// rather than fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Shell command whose output is the API token, e.g.
    /// `secret-tool lookup passwork token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_command: Option<String>,
    /// Shell command whose output is the refresh token, where the deployment
    /// issues short-lived access tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_command: Option<String>,
    /// Shell command whose output is the master key, for a vault using
    /// client-side encryption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_key_command: Option<String>,
}

/// The Software section: a two-level catalog of installable applications,
/// browsed as `install` → category → app.
///
/// The catalog itself ships built in, so the section works with no `[spotlight.software]`
/// block at all. What lands here is what the user changed.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightSoftwareConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Prefix that opens the catalog. Defaults to `install`.
    #[serde(default = "default_software_prefix")]
    pub prefix: String,
    /// Also offer software on plain, unprefixed queries, so typing an app's name
    /// reaches the row that installs it.
    #[serde(default = "default_true")]
    pub in_search: bool,
    /// Hold the terminal open once the install command has finished, so its
    /// output can still be read. Turn off for a package manager that pauses on
    /// its own.
    #[serde(default = "default_true")]
    pub keep_open: bool,
    /// Categories merged into the built-in catalog, matched by `id`.
    #[serde(default)]
    pub categories: Vec<SpotlightSoftwareCategoryConfig>,
    /// Built-in category ids to drop entirely, e.g. `["gaming"]`.
    #[serde(default)]
    pub disabled_categories: Vec<String>,
}

/// One category of the software catalog.
///
/// Merged into a built-in category of the same `id` rather than replacing it:
/// adding one app to Creativity should not cost the user the ones already there.
#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightSoftwareCategoryConfig {
    /// The merge key, and what the user types to enter the category.
    pub id: String,
    /// Falls back to the built-in label, or to `id` for a new category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub items: Vec<SpotlightSoftwareItemConfig>,
}

/// One installable application.
#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpotlightSoftwareItemConfig {
    /// Shown on the row, and the key items are merged on.
    pub name: String,
    /// The command line that installs it, run visibly in a terminal.
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Defaults to the category's icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Extra search terms, e.g. `["photoshop"]` on an image editor.
    #[serde(default)]
    pub keywords: Vec<String>,
}

fn default_software_prefix() -> String {
    "install".to_string()
}

impl Default for SpotlightSoftwareConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: default_software_prefix(),
            in_search: true,
            keep_open: true,
            categories: Vec::new(),
            disabled_categories: Vec::new(),
        }
    }
}

fn default_vpn_prefix() -> String {
    "vpn".to_string()
}

impl Default for SpotlightVpnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: default_vpn_prefix(),
            provider: None,
        }
    }
}

fn default_passwords_prefix() -> String {
    "pw".to_string()
}

impl Default for SpotlightPasswordsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prefix: default_passwords_prefix(),
            provider: None,
            host: None,
            token_command: None,
            refresh_token_command: None,
            master_key_command: None,
        }
    }
}

fn default_windows_prefix() -> String {
    "w".to_string()
}

impl Default for SpotlightWindowsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: default_windows_prefix(),
            in_search: true,
        }
    }
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
    #[serde(default)]
    pub windows: SpotlightWindowsConfig,
    #[serde(default)]
    pub vpn: SpotlightVpnConfig,
    #[serde(default)]
    pub passwords: SpotlightPasswordsConfig,
    #[serde(default)]
    pub software: SpotlightSoftwareConfig,
}

fn default_true() -> bool {
    true
}

/// Generous on purpose: on Claude Opus 5 thinking is on by default and shares
/// this budget with the visible answer, so a small cap truncates the reply.
fn default_ai_max_tokens() -> u32 {
    8192
}

/// High enough that a real investigation — look, read, try, check — finishes on
/// its own. It is a runaway guard, not a budget.
fn default_ai_max_tool_rounds() -> usize {
    25
}

/// Long enough for a package query or a short build, short enough that a
/// genuinely stuck command does not hold a tool round open all afternoon.
fn default_ai_command_timeout() -> u64 {
    60
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
            windows: SpotlightWindowsConfig::default(),
            vpn: SpotlightVpnConfig::default(),
            passwords: SpotlightPasswordsConfig::default(),
            software: SpotlightSoftwareConfig::default(),
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

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct AppConfig {
    pub default_view: ViewMode,
    pub show_hidden: bool,
    pub icon_size: i32,
    pub sidebar_width: i32,
    pub custom_css: Option<PathBuf>,
    pub list_columns: ListColumns,
    /// The order a freshly opened window starts in. The window then owns its
    /// sort the way it owns its view mode, persisted in [`crate::state`].
    #[serde(default)]
    pub sort: SortOrder,
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
                created: false,
            },
            sort: SortOrder::default(),
            actions: Vec::new(),
            spotlight: SpotlightConfig::default(),
        }
    }
}

/// Why a config file could not be turned into an [`AppConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl AppConfig {
    pub fn load() -> Self {
        match Self::load_result() {
            Ok(Some(config)) => config,
            Ok(None) => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "failed to load config, using defaults");
                Self::default()
            }
        }
    }

    /// Reads and parses a config file.
    ///
    /// `Ok(None)` means the file is absent, which is a first run rather than a
    /// failure. Everything else is returned instead of swallowed, so a *reload*
    /// can keep the last good config rather than silently resetting someone who
    /// is halfway through editing it.
    pub fn try_load_from(path: &Path) -> Result<Option<Self>, ConfigError> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        toml::from_str(&contents)
            .map(Some)
            .map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })
    }

    /// [`try_load_from`](Self::try_load_from) against [`config_path`](Self::config_path).
    pub fn load_result() -> Result<Option<Self>, ConfigError> {
        let Some(path) = Self::config_path() else {
            return Ok(None);
        };

        Self::try_load_from(&path)
    }

    pub fn config_path() -> Option<PathBuf> {
        ProjectDirs::from("io.github", "ionix", "ioexplorer")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = Self::config_path() else {
            return Ok(());
        };

        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        let contents = toml::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomic(path, &contents)
    }
}

/// Writes `contents` to `path` by renaming a sibling temporary over it.
///
/// A plain `fs::write` truncates first, so anything watching the file can read
/// it in that window and see an empty or half-written config. A rename within
/// the same directory is atomic, and produces one event rather than a burst.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no file name", path.display())))?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);

    fs::write(&temp, contents)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    Ok(())
}

pub fn clamp_icon_size(icon_size: i32) -> i32 {
    icon_size.clamp(MIN_ICON_SIZE, MAX_ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("temp dir");

        let loaded = AppConfig::try_load_from(&dir.path().join("config.toml"));

        assert!(matches!(loaded, Ok(None)));
    }

    /// The block the README tells users to write, parsed as written.
    #[test]
    fn the_documented_passwords_section_parses() {
        let config = toml::from_str::<SpotlightConfig>(
            r#"
[passwords]
enabled = true
prefix = "pw"
provider = "passwork"
host = "https://vault.example.com"
token_command = "secret-tool lookup passwork token"
master_key_command = "pass passwork/master-key"
refresh_token_command = "secret-tool lookup passwork refresh-token"
"#,
        )
        .expect("valid spotlight config");

        let passwords = &config.passwords;
        assert!(passwords.enabled);
        assert_eq!(passwords.prefix, "pw");
        assert_eq!(passwords.provider.as_deref(), Some("passwork"));
        assert_eq!(passwords.host.as_deref(), Some("https://vault.example.com"));
        assert_eq!(
            passwords.token_command.as_deref(),
            Some("secret-tool lookup passwork token")
        );
        assert_eq!(
            passwords.master_key_command.as_deref(),
            Some("pass passwork/master-key")
        );
    }

    /// Turning the section on is the whole of the setup for someone who exports
    /// the client's own variables elsewhere, so it has to parse on its own.
    #[test]
    fn the_passwords_section_needs_nothing_but_enabled() {
        let config = toml::from_str::<SpotlightConfig>("[passwords]\nenabled = true\n")
            .expect("valid spotlight config");

        assert!(config.passwords.enabled);
        assert_eq!(config.passwords.prefix, "pw");
        assert_eq!(config.passwords.token_command, None);
    }

    /// A config with no `[spotlight.passwords]` block at all must not produce a
    /// prefix that talks to a vault.
    #[test]
    fn passwords_are_off_without_a_section() {
        let config = toml::from_str::<SpotlightConfig>("").expect("valid spotlight config");

        assert!(!config.passwords.enabled);
    }

    #[test]
    fn reads_a_valid_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let written = AppConfig {
            sidebar_width: 321,
            ..AppConfig::default()
        };
        written.save_to(&path).expect("write the config under test");

        let loaded = AppConfig::try_load_from(&path)
            .expect("readable")
            .expect("present");

        assert_eq!(loaded, written);
    }

    #[test]
    fn a_malformed_config_reports_the_parse_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "default_view = ").expect("write the config under test");

        let error = AppConfig::try_load_from(&path).expect_err("malformed TOML");

        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    #[test]
    fn atomic_writes_replace_the_file_and_leave_no_temporary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("theme.css");

        write_atomic(&path, "first").expect("first write");
        write_atomic(&path, "second").expect("overwrite");

        assert_eq!(fs::read_to_string(&path).expect("readable"), "second");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("listable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "theme.css")
            .collect();
        assert!(leftovers.is_empty(), "stray files: {leftovers:?}");
    }

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

    /// A config written before sorting existed still loads, on the default order.
    #[test]
    fn a_missing_sort_section_uses_the_default_order() {
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

        assert_eq!(parsed.sort, SortOrder::default());
    }

    #[test]
    fn parses_a_configured_sort_order() {
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

[sort]
key = "created"
descending = true
folders_first = false
"#,
        )
        .expect("valid config");

        assert_eq!(
            parsed.sort,
            SortOrder {
                key: crate::sorting::SortKey::Created,
                descending: true,
                folders_first: false,
            }
        );
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
        assert_eq!(parsed.spotlight.software.prefix, "install");
        assert!(parsed.spotlight.software.enabled);
        assert!(parsed.spotlight.software.keep_open);
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

    /// The shape `docs/software.md` documents has to be the shape serde reads.
    #[test]
    fn parses_a_software_catalog() {
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

[spotlight.software]
prefix = "soft"
keep_open = false
disabled_categories = ["gaming"]

[[spotlight.software.categories]]
id = "creativity"

[[spotlight.software.categories.items]]
name = "Inkscape"
command = "yay -S --needed inkscape"
description = "Vector graphics"
keywords = ["svg"]
"#,
        )
        .expect("valid config");

        let software = &parsed.spotlight.software;
        assert_eq!(software.prefix, "soft");
        assert!(software.enabled, "an omitted flag keeps its default");
        assert!(!software.keep_open);
        assert_eq!(software.disabled_categories, vec!["gaming".to_string()]);
        assert_eq!(software.categories.len(), 1);
        assert_eq!(software.categories[0].items.len(), 1);
        assert_eq!(software.categories[0].items[0].name, "Inkscape");
        assert_eq!(software.categories[0].items[0].keywords, vec!["svg"]);
    }

    #[test]
    fn spotlight_config_round_trips_through_toml() {
        let config = AppConfig {
            spotlight: SpotlightConfig {
                prefixes: vec![SpotlightPrefixConfig {
                    prefix: "g".to_string(),
                    label: "Google search".to_string(),
                    command: "xdg-open 'https://google.com/search?q={query_url}'".to_string(),
                    ..Default::default()
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
    fn parses_a_get_results_prefix() {
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

[[spotlight.prefixes]]
prefix = "search"
get_results = "search_command '{query}'"
delay = 0.25
action = "xdg-open '{value}'"
"#,
        )
        .expect("valid config");

        let prefix = &parsed.spotlight.prefixes[0];
        assert_eq!(
            prefix.get_results.as_deref(),
            Some("search_command '{query}'")
        );
        assert_eq!(prefix.action.as_deref(), Some("xdg-open '{value}'"));
        assert_eq!(prefix.delay, Some(0.25));
        assert!(
            prefix.label.is_empty() && prefix.command.is_empty(),
            "neither is required alongside get_results"
        );
    }

    /// The window switcher has to work for a user who has never heard of it, so
    /// an absent section must leave it on rather than off.
    #[test]
    fn the_window_switcher_defaults_to_on_without_a_section() {
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
"#,
        )
        .expect("valid config");

        assert!(parsed.spotlight.windows.enabled);
        assert_eq!(parsed.spotlight.windows.prefix, "w");
        assert!(parsed.spotlight.windows.in_search);
    }

    #[test]
    fn parses_the_window_switcher_section() {
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

[spotlight.windows]
enabled = true
prefix = "win"
in_search = false
"#,
        )
        .expect("valid config");

        assert!(parsed.spotlight.windows.enabled);
        assert_eq!(parsed.spotlight.windows.prefix, "win");
        assert!(!parsed.spotlight.windows.in_search);
    }

    /// Tools are opt-in, and the one that can run anything is opt-in separately
    /// — a config that says nothing about tools must grant nothing.
    #[test]
    fn ai_tools_are_off_unless_asked_for() {
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
prefix = "claude"
provider = "claude"
"#,
        )
        .expect("valid config");

        let provider = &parsed.spotlight.ai[0];
        assert!(!provider.builtin_tools);
        assert!(!provider.run_command);
        assert!(!provider.web_search);
        assert!(provider.tools.is_empty());
    }

    #[test]
    fn parses_a_custom_ai_tool_with_typed_params() {
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
prefix = "claude"
provider = "claude"
builtin_tools = true
web_search = true

[[spotlight.ai.tools]]
name = "play_music"
description = "Play a song or artist"
command = "playerctl-search {query}"
confirm = "never"

  [[spotlight.ai.tools.params]]
  name = "query"
  type = "string"
  description = "Song, album or artist"
  required = true
"#,
        )
        .expect("valid config");

        let provider = &parsed.spotlight.ai[0];
        assert!(provider.builtin_tools);
        assert!(provider.web_search);
        // Still off: enabling the ordinary built-ins must not enable this one.
        assert!(!provider.run_command);

        let tool = &provider.tools[0];
        assert_eq!(tool.name, "play_music");
        assert_eq!(tool.command, "playerctl-search {query}");
        assert_eq!(tool.confirm, AiToolConfirm::Never);
        assert_eq!(tool.params[0].name, "query");
        assert_eq!(tool.params[0].kind, AiParamType::String);
        assert!(tool.params[0].required);
    }

    /// The whole point of the no-key rule: a tool config must not become a
    /// place a secret can hide either.
    #[test]
    fn a_serialized_config_with_tools_still_contains_no_api_key() {
        let config = AppConfig {
            spotlight: SpotlightConfig {
                ai: vec![SpotlightAiConfig {
                    enabled: true,
                    prefix: "claude".to_string(),
                    provider: "claude".to_string(),
                    model: None,
                    label: None,
                    icon: None,
                    endpoint: None,
                    api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                    api_key_file: None,
                    max_tokens: 8192,
                    effort: AiEffort::Low,
                    default: false,
                    builtin_tools: true,
                    run_command: true,
                    web_search: true,
                    system_prompt: None,
                    max_tool_rounds: 25,
                    command_timeout: 60,
                    tools: vec![SpotlightAiToolConfig {
                        name: "play_music".to_string(),
                        description: String::new(),
                        command: "playerctl {query}".to_string(),
                        confirm: AiToolConfirm::Always,
                        params: Vec::new(),
                    }],
                }],
                ..SpotlightConfig::default()
            },
            ..AppConfig::default()
        };

        let serialized = toml::to_string(&config).expect("serializes");
        assert!(!serialized.contains("api_key ="), "{serialized}");
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
                    builtin_tools: false,
                    run_command: false,
                    web_search: false,
                    system_prompt: None,
                    max_tool_rounds: 25,
                    command_timeout: 60,
                    tools: Vec::new(),
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
                    builtin_tools: false,
                    run_command: false,
                    web_search: false,
                    system_prompt: None,
                    max_tool_rounds: 25,
                    command_timeout: 60,
                    tools: Vec::new(),
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
