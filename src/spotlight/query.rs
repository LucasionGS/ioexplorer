//! Splits the raw search text into a prefix and its argument.
//!
//! Pure over `(&str, &PrefixTable)` so the whole prefix-dispatch model is
//! testable without a GTK main loop.

use crate::spotlight::prefixes::{PrefixTable, is_symbolic_key};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Query {
    Empty,
    Plain(String),
    Prefixed { key: String, arg: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parsed {
    pub query: Query,
    /// An alphanumeric prefix the text matches exactly but has not yet
    /// committed to, e.g. typing `g` before the space. The UI offers it as a
    /// Tab-completable row alongside the normal results.
    pub hint: Option<String>,
}

/// Parses `raw` against the prefix table.
///
/// Only leading whitespace is trimmed — a trailing space is load-bearing, since
/// it is what distinguishes `g` (still typing a word) from `g ` (committed to
/// the Google prefix), and what tells path completion a segment is finished.
pub fn parse(raw: &str, table: &PrefixTable) -> Parsed {
    let text = raw.trim_start();

    if text.is_empty() {
        return Parsed {
            query: Query::Empty,
            hint: None,
        };
    }

    for prefix in table.all() {
        let key = prefix.key.as_str();
        let Some(rest) = text.strip_prefix(key) else {
            continue;
        };

        if is_symbolic_key(key) {
            return Parsed {
                query: Query::Prefixed {
                    key: key.to_string(),
                    arg: rest.trim_start().to_string(),
                },
                hint: None,
            };
        }

        // Alphanumeric keys need a separator, so `go` stays a plain search.
        if rest.is_empty() {
            return Parsed {
                query: Query::Plain(text.to_string()),
                hint: Some(key.to_string()),
            };
        }
        if rest.starts_with(char::is_whitespace) {
            return Parsed {
                query: Query::Prefixed {
                    key: key.to_string(),
                    arg: rest.trim_start().to_string(),
                },
                hint: None,
            };
        }
    }

    Parsed {
        query: Query::Plain(text.to_string()),
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SpotlightConfig, SpotlightPrefixConfig};
    use crate::spotlight::prefixes;

    fn table_with(keys: &[&str]) -> PrefixTable {
        let config = SpotlightConfig {
            prefixes: keys
                .iter()
                .map(|key| SpotlightPrefixConfig {
                    prefix: (*key).to_string(),
                    label: "Custom".to_string(),
                    command: "echo {query}".to_string(),
                    description: None,
                    icon: None,
                    terminal: false,
                })
                .collect(),
            ..Default::default()
        };
        prefixes::resolve_with_ai(&config).0
    }

    fn prefixed(key: &str, arg: &str) -> Query {
        Query::Prefixed {
            key: key.to_string(),
            arg: arg.to_string(),
        }
    }

    #[test]
    fn empty_text_is_empty() {
        assert_eq!(parse("", &table_with(&[])).query, Query::Empty);
        assert_eq!(parse("   ", &table_with(&[])).query, Query::Empty);
    }

    #[test]
    fn symbolic_prefixes_need_no_separator() {
        let table = table_with(&[]);

        assert_eq!(parse("=1+2", &table).query, prefixed("=", "1+2"));
        assert_eq!(parse("= 1+2", &table).query, prefixed("=", "1+2"));
        assert_eq!(parse("  =1+2", &table).query, prefixed("=", "1+2"));
    }

    #[test]
    fn alphanumeric_prefixes_require_a_space() {
        let table = table_with(&["g"]);

        assert_eq!(parse("g cats", &table).query, prefixed("g", "cats"));
        assert_eq!(parse("go", &table).query, Query::Plain("go".to_string()));
    }

    #[test]
    fn a_bare_alphanumeric_prefix_becomes_a_hint() {
        let table = table_with(&["g"]);

        let parsed = parse("g", &table);

        assert_eq!(parsed.query, Query::Plain("g".to_string()));
        assert_eq!(parsed.hint.as_deref(), Some("g"));
    }

    #[test]
    fn longest_matching_key_wins() {
        let table = table_with(&["g", "gh"]);

        assert_eq!(parse("gh issues", &table).query, prefixed("gh", "issues"));
        assert_eq!(parse("g issues", &table).query, prefixed("g", "issues"));
    }

    #[test]
    fn unknown_prefixes_fall_through_to_a_plain_query() {
        let table = table_with(&[]);

        assert_eq!(
            parse("zz thing", &table).query,
            Query::Plain("zz thing".to_string())
        );
    }

    #[test]
    fn trailing_whitespace_is_preserved_in_the_argument_boundary() {
        let table = table_with(&[]);

        // The trailing slash and space matter to path completion.
        assert_eq!(parse("> ~/Doc", &table).query, prefixed(">", "~/Doc"));
        assert_eq!(
            parse(">~/Documents/ ", &table).query,
            prefixed(">", "~/Documents/ ")
        );
    }

    #[test]
    fn an_empty_argument_is_still_a_prefixed_query() {
        let table = table_with(&[]);

        assert_eq!(parse("=", &table).query, prefixed("=", ""));
        assert_eq!(parse("? ", &table).query, prefixed("?", ""));
    }
}
