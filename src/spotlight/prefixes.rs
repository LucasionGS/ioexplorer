//! Prefix shortcuts: the built-in set, user-defined additions, and the command
//! templating that turns `g cats` into a real shell line.

use std::time::Duration;

use crate::config::{SpotlightConfig, SpotlightPrefixConfig, SpotlightWindowsConfig};
use crate::spotlight::ai::{self, AiProvider};

/// Default quiet-typing window before a `get_results` command runs.
const DEFAULT_RESULTS_DELAY: f64 = 0.5;
/// Bounds on that window, so a typo can neither hammer the command on every
/// keystroke nor leave the prefix looking permanently broken.
const MIN_RESULTS_DELAY: f64 = 0.0;
const MAX_RESULTS_DELAY: f64 = 10.0;

/// The placeholder a paginated `get_results` template puts the page number in.
pub const PAGE_PLACEHOLDER: &str = "{page}";
/// The page a paginated prefix starts on, and the lowest it can go back to.
pub const FIRST_PAGE: i64 = 1;

/// Default artwork size for a `get_results` row: an icon beside the text.
pub const DEFAULT_RESULTS_ICON_SIZE: i32 = 22;
/// Bounds on that size. The ceiling keeps a row from outgrowing the list.
const MIN_RESULTS_ICON_SIZE: i32 = 16;
const MAX_RESULTS_ICON_SIZE: i32 = 256;

/// What activating a prefix actually does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrefixKind {
    /// Run the argument as a shell command.
    Shell,
    /// Open the argument as a filesystem path.
    OpenPath,
    /// Evaluate the argument as an arithmetic expression.
    Calculator,
    /// Search the filesystem for the argument.
    FileSearch,
    /// List every available prefix.
    Help,
    /// Run a user-configured command template.
    Command { command: String, terminal: bool },
    /// List the windows that are currently open and switch to one.
    Windows,
    /// List the hosts in the user's SSH config and connect to one.
    Ssh,
    /// List running processes with their resource use, and act on one.
    Processes,
    /// Ask a user-configured command for the rows to show, then run `action`
    /// on whichever one is picked.
    CustomResults {
        command: String,
        action: Option<String>,
        delay: Duration,
        terminal: bool,
        /// Pixel size of each row's artwork.
        icon_size: i32,
        /// Whether the user can page through the command's output.
        paginated: bool,
    },
    /// Open a chat with the AI provider at this index.
    ///
    /// An index rather than the provider by value: `PrefixKind` derives `Eq`
    /// and is cloned on every render, so holding a model string here would copy
    /// it on each keystroke.
    Ai(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Prefix {
    pub key: String,
    pub label: String,
    pub description: String,
    pub icon: String,
    pub kind: PrefixKind,
}

impl Prefix {
    /// Symbolic prefixes bind directly to their argument (`=1+2`); alphanumeric
    /// ones require a space (`g cats`) so binding `g` does not hijack every
    /// query that happens to start with the letter g.
    pub fn is_symbolic(&self) -> bool {
        is_symbolic_key(&self.key)
    }
}

pub fn is_symbolic_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|ch| !ch.is_alphanumeric())
}

/// The resolved prefix set, ordered longest key first so `gh` wins over `g`.
#[derive(Clone, Debug, Default)]
pub struct PrefixTable {
    prefixes: Vec<Prefix>,
}

impl PrefixTable {
    pub fn get(&self, key: &str) -> Option<&Prefix> {
        self.prefixes.iter().find(|prefix| prefix.key == key)
    }

    pub fn all(&self) -> &[Prefix] {
        &self.prefixes
    }
}

fn builtins() -> Vec<Prefix> {
    vec![
        Prefix {
            key: "!".to_string(),
            label: "Run command".to_string(),
            description: "Run a shell command".to_string(),
            icon: "utilities-terminal-symbolic".to_string(),
            kind: PrefixKind::Shell,
        },
        Prefix {
            key: ">".to_string(),
            label: "Open path".to_string(),
            description: "Browse to a folder or file".to_string(),
            icon: "folder-open-symbolic".to_string(),
            kind: PrefixKind::OpenPath,
        },
        Prefix {
            key: "=".to_string(),
            label: "Calculate".to_string(),
            description: "Evaluate an expression".to_string(),
            icon: "accessories-calculator-symbolic".to_string(),
            kind: PrefixKind::Calculator,
        },
        Prefix {
            key: "/".to_string(),
            label: "Find files".to_string(),
            description: "Search your folders by name".to_string(),
            icon: "edit-find-symbolic".to_string(),
            kind: PrefixKind::FileSearch,
        },
        Prefix {
            key: "ssh".to_string(),
            label: "SSH".to_string(),
            description: "Connect to a host from your SSH config".to_string(),
            icon: "network-server-symbolic".to_string(),
            kind: PrefixKind::Ssh,
        },
        Prefix {
            key: "ps".to_string(),
            label: "Processes".to_string(),
            description: "Watch and manage running processes".to_string(),
            icon: "utilities-system-monitor-symbolic".to_string(),
            kind: PrefixKind::Processes,
        },
        Prefix {
            key: "?".to_string(),
            label: "Help".to_string(),
            description: "List every available prefix".to_string(),
            icon: "help-about-symbolic".to_string(),
            kind: PrefixKind::Help,
        },
    ]
}

/// Merges built-in prefixes, the user's command prefixes and the AI providers
/// into one table, plus the provider list the `Ai(usize)` prefixes index into.
///
/// Stages apply in order — builtins, the window switcher, user command prefixes,
/// then AI entries — and each stage replaces or appends, so a later stage wins a
/// key collision.
pub fn resolve_with_ai(config: &SpotlightConfig) -> (PrefixTable, Vec<AiProvider>) {
    let mut prefixes = builtins()
        .into_iter()
        .filter(|prefix| !config.disabled_builtins.contains(&prefix.key))
        .collect::<Vec<_>>();

    // Registered even where the compositor is not supported. Whether it can work
    // is an environment question, and this function is pure over the config so
    // the whole prefix table stays testable; the provider answers with one row
    // saying so, which beats a prefix that silently does not exist.
    if let Some(prefix) = windows_prefix(&config.windows)
        && !config.disabled_builtins.contains(&prefix.key)
    {
        replace_or_append(&mut prefixes, prefix);
    }

    for entry in &config.prefixes {
        let Some(prefix) = prefix_from_config(entry) else {
            continue;
        };
        replace_or_append(&mut prefixes, prefix);
    }

    let providers = ai::resolve_providers(config);
    for (index, provider) in providers.iter().enumerate() {
        if prefixes
            .iter()
            .any(|existing| existing.key == provider.prefix)
        {
            tracing::warn!(
                prefix = provider.prefix,
                "ai provider shadows an existing spotlight prefix"
            );
        }
        replace_or_append(
            &mut prefixes,
            Prefix {
                key: provider.prefix.clone(),
                label: provider.label.clone(),
                description: format!("Chat with {}", provider.provider.model()),
                icon: provider.icon.clone(),
                kind: PrefixKind::Ai(index),
            },
        );
    }

    // Sorting the table never invalidates an `Ai(usize)`: the index points into
    // `providers`, which is not sorted.
    prefixes.sort_by(|left, right| {
        right
            .key
            .chars()
            .count()
            .cmp(&left.key.chars().count())
            .then_with(|| left.key.cmp(&right.key))
    });

    (PrefixTable { prefixes }, providers)
}

/// The window-switcher prefix, or `None` when it is turned off or misconfigured.
fn windows_prefix(config: &SpotlightWindowsConfig) -> Option<Prefix> {
    if !config.enabled {
        return None;
    }

    let key = config.prefix.trim();
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        tracing::warn!(
            prefix = config.prefix,
            "ignoring the window switcher: its prefix is not a usable key"
        );
        return None;
    }

    Some(Prefix {
        key: key.to_string(),
        label: "Switch window".to_string(),
        description: "Switch to an open window".to_string(),
        icon: "focus-windows-symbolic".to_string(),
        kind: PrefixKind::Windows,
    })
}

fn replace_or_append(prefixes: &mut Vec<Prefix>, prefix: Prefix) {
    match prefixes
        .iter()
        .position(|existing| existing.key == prefix.key)
    {
        Some(index) => prefixes[index] = prefix,
        None => prefixes.push(prefix),
    }
}

fn prefix_from_config(entry: &SpotlightPrefixConfig) -> Option<Prefix> {
    let key = entry.prefix.trim();
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        tracing::warn!(
            prefix = entry.prefix,
            "ignoring spotlight prefix with an invalid key"
        );
        return None;
    }

    let get_results = trimmed(entry.get_results.as_deref());
    let command = entry.command.trim();
    if get_results.is_none() && command.is_empty() {
        tracing::warn!(
            prefix = key,
            "ignoring spotlight prefix with neither a command nor get_results"
        );
        return None;
    }

    // `get_results` wins: a prefix that produces its own rows has no use for a
    // single fixed command line.
    let kind = match get_results {
        Some(get_results) => PrefixKind::CustomResults {
            command: get_results.to_string(),
            action: trimmed(entry.action.as_deref()).map(str::to_string),
            delay: results_delay(entry.delay),
            terminal: entry.terminal,
            icon_size: results_icon_size(entry.icon_size),
            paginated: pagination_enabled(key, entry.pagination, get_results),
        },
        None => PrefixKind::Command {
            command: command.to_string(),
            terminal: entry.terminal,
        },
    };

    Some(Prefix {
        key: key.to_string(),
        label: match entry.label.trim() {
            "" => key.to_string(),
            label => label.to_string(),
        },
        description: entry
            .description
            .clone()
            .unwrap_or_else(|| get_results.unwrap_or(command).to_string()),
        icon: entry
            .icon
            .clone()
            .unwrap_or_else(|| "application-x-executable-symbolic".to_string()),
        kind,
    })
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// The artwork size for `get_results` rows, clamped.
///
/// The ceiling is not arbitrary: every row grows to fit its icon, so an
/// unbounded value would push the whole list past the card's height budget and
/// leave one row filling the screen.
fn results_icon_size(size: Option<i32>) -> i32 {
    size.unwrap_or(DEFAULT_RESULTS_ICON_SIZE)
        .clamp(MIN_RESULTS_ICON_SIZE, MAX_RESULTS_ICON_SIZE)
}

/// Whether paging is actually usable, rather than merely asked for.
///
/// Without a `{page}` in the template every page would run the identical
/// command line, so paging would look like it worked and change nothing. Saying
/// so once at load time beats leaving the user to work that out from a list
/// that refuses to advance.
fn pagination_enabled(key: &str, requested: bool, template: &str) -> bool {
    if requested && !template.contains(PAGE_PLACEHOLDER) {
        tracing::warn!(
            prefix = key,
            "spotlight prefix sets pagination but its get_results has no {PAGE_PLACEHOLDER}"
        );
        return false;
    }
    requested
}

/// The debounce for a `get_results` command, clamped and NaN-proofed.
fn results_delay(delay: Option<f64>) -> Duration {
    let seconds = delay
        .filter(|delay| delay.is_finite())
        .unwrap_or(DEFAULT_RESULTS_DELAY)
        .clamp(MIN_RESULTS_DELAY, MAX_RESULTS_DELAY);
    Duration::from_secs_f64(seconds)
}

/// Expands a command template against a query.
///
/// `{query}` is shell-quoted; `{query_url}` is percent-encoded for use inside a
/// URL. A template with neither placeholder gets the quoted query appended, so
/// `command = "firefox"` still behaves sensibly.
pub fn build_command_line(template: &str, query: &str) -> String {
    let has_placeholder = template.contains("{query}") || template.contains("{query_url}");
    let mut line = template
        .replace("{query_url}", &url_encode(query))
        .replace("{query}", &crate::custom_actions::shell_quote(query));

    if !has_placeholder && !query.is_empty() {
        line.push(' ');
        line.push_str(&crate::custom_actions::shell_quote(query));
    }

    line
}

/// Expands a `get_results` template, filling `{page}` before the query.
///
/// The order matters both ways round. Substituting the page first means the
/// number lands in the template, not in whatever the user typed — a query
/// containing the literal text `{page}` is left alone rather than re-expanded.
/// And because a page is an `i64` it needs no quoting: there is no string a
/// caller could supply that survives into the shell as anything but digits.
pub fn build_results_line(template: &str, query: &str, page: i64) -> String {
    build_command_line(
        &template.replace(PAGE_PLACEHOLDER, &page.to_string()),
        query,
    )
}

/// Expands an action template against the value of the chosen result.
///
/// `{value}` is shell-quoted, which is safe both bare and inside the quotes a
/// template may already wrap it in — `''x''` is just `x` to the shell.
/// `{value_escaped}` backslash-escapes instead, for templates that need the
/// value unquoted. A template with neither gets the quoted value appended.
pub fn build_action_line(template: &str, value: &str) -> String {
    let has_placeholder = template.contains("{value}") || template.contains("{value_escaped}");
    let mut line = template
        .replace("{value_escaped}", &backslash_escape(value))
        .replace("{value}", &crate::custom_actions::shell_quote(value));

    if !has_placeholder && !value.is_empty() {
        line.push(' ');
        line.push_str(&crate::custom_actions::shell_quote(value));
    }

    line
}

/// Backslash-escapes every character the shell would otherwise act on, so a
/// value carrying `;` or `$(…)` cannot become a second command.
fn backslash_escape(value: &str) -> String {
    const SPECIAL: &str = " \t\n\r\"'\\$`!*?[]{}()<>|&;#~^";

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if SPECIAL.contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn url_encode(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_prefix(prefix: &str, command: &str) -> SpotlightPrefixConfig {
        SpotlightPrefixConfig {
            prefix: prefix.to_string(),
            label: "Custom".to_string(),
            command: command.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_config_yields_every_builtin() {
        let table = resolve_with_ai(&SpotlightConfig::default()).0;

        assert_eq!(table.all().len(), 7);
        for key in ["!", ">", "=", "/", "?", "w", "ssh"] {
            assert!(table.get(key).is_some(), "missing builtin {key}");
        }
    }

    #[test]
    fn user_prefix_overrides_a_builtin() {
        let config = SpotlightConfig {
            prefixes: vec![user_prefix("=", "qalc {query}")],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert_eq!(table.all().len(), 7);
        assert!(matches!(
            table.get("=").expect("overridden prefix").kind,
            PrefixKind::Command { .. }
        ));
    }

    #[test]
    fn disabled_builtins_are_removed() {
        let config = SpotlightConfig {
            disabled_builtins: vec!["=".to_string()],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(table.get("=").is_none());
        assert_eq!(table.all().len(), 6);
    }

    #[test]
    fn invalid_user_prefixes_are_skipped() {
        let config = SpotlightConfig {
            prefixes: vec![
                user_prefix("", "echo"),
                user_prefix("a b", "echo"),
                SpotlightPrefixConfig {
                    command: String::new(),
                    ..user_prefix("y", "echo")
                },
            ],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert_eq!(table.all().len(), 7);
    }

    #[test]
    fn an_omitted_label_falls_back_to_the_key() {
        let config = SpotlightConfig {
            prefixes: vec![SpotlightPrefixConfig {
                label: String::new(),
                ..user_prefix("x", "echo")
            }],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert_eq!(table.get("x").expect("x prefix").label, "x");
    }

    #[test]
    fn get_results_makes_a_custom_results_prefix() {
        let config = SpotlightConfig {
            prefixes: vec![SpotlightPrefixConfig {
                prefix: "search".to_string(),
                get_results: Some("search_command '{query}'".to_string()),
                action: Some("xdg-open '{value}'".to_string()),
                delay: Some(0.25),
                ..Default::default()
            }],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert_eq!(
            table.get("search").expect("search prefix").kind,
            PrefixKind::CustomResults {
                command: "search_command '{query}'".to_string(),
                action: Some("xdg-open '{value}'".to_string()),
                delay: Duration::from_millis(250),
                terminal: false,
                icon_size: DEFAULT_RESULTS_ICON_SIZE,
                paginated: false,
            }
        );
    }

    #[test]
    fn pagination_is_enabled_when_the_template_can_carry_a_page() {
        let config = SpotlightConfig {
            prefixes: vec![SpotlightPrefixConfig {
                prefix: "img".to_string(),
                get_results: Some("images {query} {page}".to_string()),
                pagination: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(matches!(
            table.get("img").expect("img prefix").kind,
            PrefixKind::CustomResults {
                paginated: true,
                ..
            }
        ));
    }

    /// Paging a template with no `{page}` would rerun the identical command
    /// line, so it is refused rather than left to look broken at runtime.
    #[test]
    fn pagination_without_a_page_placeholder_is_refused() {
        assert!(!pagination_enabled("img", true, "images {query}"));
        assert!(pagination_enabled("img", true, "images {query} {page}"));
        assert!(!pagination_enabled("img", false, "images {query} {page}"));
    }

    #[test]
    fn the_page_is_substituted_before_the_query() {
        assert_eq!(
            build_results_line("images {query} {page}", "cats", 3),
            "images 'cats' 3"
        );
        // A query that happens to contain the placeholder is data, not template.
        assert_eq!(
            build_results_line("images {query} {page}", "{page}", 2),
            "images '{page}' 2"
        );
    }

    #[test]
    fn a_page_never_reaches_the_shell_as_anything_but_digits() {
        assert_eq!(
            build_results_line("images {page}", "", i64::MIN),
            format!("images {}", i64::MIN)
        );
        // An empty query still expands to an explicit empty argument, so the
        // page stays in the position the template put it in.
        assert_eq!(
            build_results_line("images {query} {page}", "", 1),
            "images '' 1"
        );
    }

    #[test]
    fn a_custom_results_prefix_can_set_its_own_icon_size() {
        let config = SpotlightConfig {
            prefixes: vec![SpotlightPrefixConfig {
                prefix: "img".to_string(),
                get_results: Some("images {query}".to_string()),
                icon_size: Some(96),
                ..Default::default()
            }],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(matches!(
            table.get("img").expect("img prefix").kind,
            PrefixKind::CustomResults { icon_size: 96, .. }
        ));
    }

    #[test]
    fn the_results_icon_size_is_defaulted_and_clamped() {
        assert_eq!(results_icon_size(None), DEFAULT_RESULTS_ICON_SIZE);
        assert_eq!(results_icon_size(Some(96)), 96);
        assert_eq!(results_icon_size(Some(0)), MIN_RESULTS_ICON_SIZE);
        assert_eq!(results_icon_size(Some(-40)), MIN_RESULTS_ICON_SIZE);
        assert_eq!(results_icon_size(Some(4000)), MAX_RESULTS_ICON_SIZE);
    }

    #[test]
    fn get_results_takes_precedence_over_command() {
        let config = SpotlightConfig {
            prefixes: vec![SpotlightPrefixConfig {
                get_results: Some("list {query}".to_string()),
                ..user_prefix("s", "echo")
            }],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(matches!(
            table.get("s").expect("s prefix").kind,
            PrefixKind::CustomResults { .. }
        ));
    }

    #[test]
    fn the_results_delay_is_defaulted_and_clamped() {
        assert_eq!(results_delay(None), Duration::from_millis(500));
        assert_eq!(results_delay(Some(f64::NAN)), Duration::from_millis(500));
        assert_eq!(results_delay(Some(-3.0)), Duration::ZERO);
        assert_eq!(results_delay(Some(600.0)), Duration::from_secs(10));
    }

    #[test]
    fn an_action_value_is_quoted_even_inside_the_templates_own_quotes() {
        assert_eq!(
            build_action_line("xdg-open '{value}'", "https://example.com/a b"),
            "xdg-open ''https://example.com/a b''"
        );
        assert_eq!(
            build_action_line("xdg-open {value}", "a'; rm -rf ~"),
            r#"xdg-open 'a'"'"'; rm -rf ~'"#
        );
    }

    #[test]
    fn the_escaped_value_neutralises_every_shell_metacharacter() {
        assert_eq!(
            build_action_line("echo {value_escaped}", "a; rm -rf $HOME"),
            r"echo a\;\ rm\ -rf\ \$HOME"
        );
    }

    #[test]
    fn an_action_without_a_placeholder_gets_the_value_appended() {
        assert_eq!(build_action_line("open", "file.txt"), "open 'file.txt'");
        assert_eq!(build_action_line("open", ""), "open");
    }

    #[test]
    fn table_is_sorted_longest_key_first() {
        let config = SpotlightConfig {
            prefixes: vec![user_prefix("g", "echo"), user_prefix("gh", "echo")],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;
        let keys = table
            .all()
            .iter()
            .map(|prefix| prefix.key.as_str())
            .collect::<Vec<_>>();

        let gh = keys
            .iter()
            .position(|key| *key == "gh")
            .expect("gh present");
        let g = keys.iter().position(|key| *key == "g").expect("g present");
        assert!(gh < g, "longer key must sort first: {keys:?}");
    }

    fn ai_entry(prefix: &str, provider: &str) -> crate::config::SpotlightAiConfig {
        crate::config::SpotlightAiConfig {
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
            effort: crate::config::AiEffort::Low,
            default: false,
            builtin_tools: false,
            run_command: false,
            web_search: false,
            system_prompt: None,
            max_tool_rounds: 25,
            command_timeout: 60,
            tools: Vec::new(),
        }
    }

    #[test]
    fn ai_providers_become_prefixes_indexed_in_order() {
        let config = SpotlightConfig {
            ai: vec![ai_entry("ai", "claude"), ai_entry("ol", "ollama")],
            ..Default::default()
        };

        let (table, providers) = resolve_with_ai(&config);

        assert_eq!(providers.len(), 2);
        assert_eq!(table.get("ai").expect("ai prefix").kind, PrefixKind::Ai(0));
        assert_eq!(table.get("ol").expect("ol prefix").kind, PrefixKind::Ai(1));
    }

    #[test]
    fn ai_indices_survive_the_longest_key_first_sort() {
        let config = SpotlightConfig {
            // "z" sorts after "long" by length, so the table order differs from
            // the provider order — the indices must still line up.
            ai: vec![ai_entry("z", "claude"), ai_entry("long", "ollama")],
            ..Default::default()
        };

        let (table, providers) = resolve_with_ai(&config);

        let PrefixKind::Ai(index) = table.get("long").expect("long prefix").kind else {
            panic!("expected an ai prefix");
        };
        assert_eq!(providers[index].prefix, "long");
    }

    #[test]
    fn an_ai_prefix_shadows_a_builtin() {
        let config = SpotlightConfig {
            ai: vec![ai_entry("=", "claude")],
            ..Default::default()
        };

        let (table, _) = resolve_with_ai(&config);

        assert_eq!(table.get("=").expect("= prefix").kind, PrefixKind::Ai(0));
        assert_eq!(table.all().len(), 7, "it replaces rather than adds");
    }

    #[test]
    fn an_ai_prefix_shadows_a_user_command_prefix() {
        let config = SpotlightConfig {
            prefixes: vec![user_prefix("g", "echo")],
            ai: vec![ai_entry("g", "claude")],
            ..Default::default()
        };

        let (table, _) = resolve_with_ai(&config);

        assert_eq!(table.get("g").expect("g prefix").kind, PrefixKind::Ai(0));
    }

    #[test]
    fn resolve_still_returns_a_table_without_ai_configured() {
        let table = resolve_with_ai(&SpotlightConfig::default()).0;

        assert_eq!(table.all().len(), 7);
        assert!(
            !table
                .all()
                .iter()
                .any(|p| matches!(p.kind, PrefixKind::Ai(_)))
        );
    }

    #[test]
    fn the_window_switcher_can_be_moved_to_another_key() {
        let config = SpotlightConfig {
            windows: SpotlightWindowsConfig {
                prefix: "win".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert_eq!(
            table.get("win").expect("win prefix").kind,
            PrefixKind::Windows
        );
        assert!(table.get("w").is_none(), "the default key is not also kept");
    }

    #[test]
    fn the_window_switcher_can_be_turned_off() {
        let config = SpotlightConfig {
            windows: SpotlightWindowsConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(table.get("w").is_none());
        assert_eq!(table.all().len(), 6);
    }

    /// `disabled_builtins` is the one place a user already looks to remove a
    /// built-in prefix, so it has to work here too.
    #[test]
    fn the_window_switcher_honours_disabled_builtins() {
        let config = SpotlightConfig {
            disabled_builtins: vec!["w".to_string()],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(table.get("w").is_none());
        assert_eq!(table.all().len(), 6);
    }

    #[test]
    fn an_unusable_window_prefix_key_is_refused() {
        for key in ["", "   ", "a b"] {
            assert!(
                windows_prefix(&SpotlightWindowsConfig {
                    prefix: key.to_string(),
                    ..Default::default()
                })
                .is_none(),
                "{key:?} must not become a prefix"
            );
        }
    }

    /// A user prefix is applied after the switcher, so it wins the key — the
    /// same precedence every other collision follows.
    #[test]
    fn a_user_prefix_overrides_the_window_switcher() {
        let config = SpotlightConfig {
            prefixes: vec![user_prefix("w", "echo")],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert!(matches!(
            table.get("w").expect("w prefix").kind,
            PrefixKind::Command { .. }
        ));
        assert_eq!(table.all().len(), 7);
    }

    /// `ssh` is three characters, so it only reaches its prefix if the table is
    /// searched longest key first — a shorter user prefix such as `s` would
    /// otherwise swallow it.
    #[test]
    fn the_ssh_prefix_is_a_builtin_that_can_be_disabled() {
        let table = resolve_with_ai(&SpotlightConfig::default()).0;
        assert_eq!(table.get("ssh").expect("ssh prefix").kind, PrefixKind::Ssh);

        let table = resolve_with_ai(&SpotlightConfig {
            disabled_builtins: vec!["ssh".to_string()],
            ..Default::default()
        })
        .0;
        assert!(table.get("ssh").is_none());
    }

    #[test]
    fn classifies_symbolic_and_alphanumeric_keys() {
        assert!(is_symbolic_key("="));
        assert!(is_symbolic_key(">>"));
        assert!(!is_symbolic_key("g"));
        assert!(!is_symbolic_key("g2"));
        assert!(!is_symbolic_key(""));
    }

    #[test]
    fn shell_quotes_the_query_placeholder() {
        let line = build_command_line("echo {query}", "hello world");

        assert_eq!(line, "echo 'hello world'");
    }

    #[test]
    fn percent_encodes_the_url_placeholder() {
        let line = build_command_line("xdg-open 'https://example.com/?q={query_url}'", "a b&c");

        assert_eq!(line, "xdg-open 'https://example.com/?q=a+b%26c'");
    }

    #[test]
    fn appends_the_query_when_no_placeholder_is_present() {
        assert_eq!(
            build_command_line("firefox", "example.com"),
            "firefox 'example.com'"
        );
        assert_eq!(build_command_line("firefox", ""), "firefox");
    }

    #[test]
    fn survives_a_quote_in_the_query() {
        let line = build_command_line("echo {query}", "don't");

        assert_eq!(line, r#"echo 'don'"'"'t'"#);
    }
}
