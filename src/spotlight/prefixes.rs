//! Prefix shortcuts: the built-in set, user-defined additions, and the command
//! templating that turns `g cats` into a real shell line.

use crate::config::{SpotlightConfig, SpotlightPrefixConfig};
use crate::spotlight::ai::{self, AiProvider};

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
/// Stages apply in order — builtins, user command prefixes, then AI entries —
/// and each stage replaces or appends, so a later stage wins a key collision.
pub fn resolve_with_ai(config: &SpotlightConfig) -> (PrefixTable, Vec<AiProvider>) {
    let mut prefixes = builtins()
        .into_iter()
        .filter(|prefix| !config.disabled_builtins.contains(&prefix.key))
        .collect::<Vec<_>>();

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
    if entry.label.trim().is_empty() || entry.command.trim().is_empty() {
        tracing::warn!(
            prefix = key,
            "ignoring spotlight prefix with an empty label or command"
        );
        return None;
    }

    Some(Prefix {
        key: key.to_string(),
        label: entry.label.trim().to_string(),
        description: entry
            .description
            .clone()
            .unwrap_or_else(|| entry.command.clone()),
        icon: entry
            .icon
            .clone()
            .unwrap_or_else(|| "application-x-executable-symbolic".to_string()),
        kind: PrefixKind::Command {
            command: entry.command.clone(),
            terminal: entry.terminal,
        },
    })
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
            description: None,
            icon: None,
            terminal: false,
        }
    }

    #[test]
    fn empty_config_yields_every_builtin() {
        let table = resolve_with_ai(&SpotlightConfig::default()).0;

        assert_eq!(table.all().len(), 5);
        for key in ["!", ">", "=", "/", "?"] {
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

        assert_eq!(table.all().len(), 5);
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
        assert_eq!(table.all().len(), 4);
    }

    #[test]
    fn invalid_user_prefixes_are_skipped() {
        let config = SpotlightConfig {
            prefixes: vec![
                user_prefix("", "echo"),
                user_prefix("a b", "echo"),
                SpotlightPrefixConfig {
                    label: String::new(),
                    ..user_prefix("x", "echo")
                },
                SpotlightPrefixConfig {
                    command: String::new(),
                    ..user_prefix("y", "echo")
                },
            ],
            ..Default::default()
        };

        let table = resolve_with_ai(&config).0;

        assert_eq!(table.all().len(), 5);
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
        assert_eq!(table.all().len(), 5, "it replaces rather than adds");
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

        assert_eq!(table.all().len(), 5);
        assert!(
            !table
                .all()
                .iter()
                .any(|p| matches!(p.kind, PrefixKind::Ai(_)))
        );
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
