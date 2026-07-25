//! Prefix shortcuts: the built-in set, user-defined additions, and the command
//! templating that turns `g cats` into a real shell line.

use crate::config::{SpotlightConfig, SpotlightPrefixConfig};

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

/// Merges the built-in prefixes with the user's, letting the user override any key.
pub fn resolve(config: &SpotlightConfig) -> PrefixTable {
    let mut prefixes = builtins()
        .into_iter()
        .filter(|prefix| !config.disabled_builtins.contains(&prefix.key))
        .collect::<Vec<_>>();

    for entry in &config.prefixes {
        let Some(prefix) = prefix_from_config(entry) else {
            continue;
        };

        match prefixes
            .iter()
            .position(|existing| existing.key == prefix.key)
        {
            Some(index) => prefixes[index] = prefix,
            None => prefixes.push(prefix),
        }
    }

    prefixes.sort_by(|left, right| {
        right
            .key
            .chars()
            .count()
            .cmp(&left.key.chars().count())
            .then_with(|| left.key.cmp(&right.key))
    });

    PrefixTable { prefixes }
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
        let table = resolve(&SpotlightConfig::default());

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

        let table = resolve(&config);

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

        let table = resolve(&config);

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

        let table = resolve(&config);

        assert_eq!(table.all().len(), 5);
    }

    #[test]
    fn table_is_sorted_longest_key_first() {
        let config = SpotlightConfig {
            prefixes: vec![user_prefix("g", "echo"), user_prefix("gh", "echo")],
            ..Default::default()
        };

        let table = resolve(&config);
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
