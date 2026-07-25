//! The result model and the providers that populate it.

use std::path::{Path, PathBuf};

use directories::UserDirs;

use crate::{
    bookmarks,
    launcher::{
        app_index::{AppEntry, AppIndex, IconRef},
        frecency::{FRECENCY_MAX_BONUS, Frecency},
        fuzzy::{self, Field},
    },
    spotlight::{
        calc,
        paths::{self, PathCandidate},
        prefixes::{Prefix, PrefixKind, PrefixTable, build_command_line},
    },
};

/// What activating a result does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Activation {
    LaunchApp(String),
    OpenPath(PathBuf),
    RunShell(String),
    RunInTerminal(String),
    CopyText(String),
    /// Rewrite the entry text, e.g. accepting a prefix hint or a path completion.
    Replace(String),
    /// Open the chat view and send `prompt` to the provider at this index.
    AskAi {
        provider: usize,
        prompt: String,
    },
    /// Informational rows such as the help listing.
    Inert,
}

#[derive(Clone, Debug)]
pub struct SpotlightResult {
    pub title: String,
    pub subtitle: String,
    pub icon: IconRef,
    pub primary: Activation,
    pub secondary: Option<Activation>,
    /// Text Tab rewrites the entry to, when this row is selected.
    pub completion: Option<String>,
    pub frecency_key: Option<String>,
    pub score: i32,
}

impl SpotlightResult {
    fn new(title: impl Into<String>, subtitle: impl Into<String>, icon: IconRef) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            icon,
            primary: Activation::Inert,
            secondary: None,
            completion: None,
            frecency_key: None,
            score: 0,
        }
    }
}

/// Sorts by score, then by the tie-breakers that keep ordering stable across runs.
pub fn sort_results(results: &mut [SpotlightResult]) {
    results.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.title.chars().count().cmp(&right.title.chars().count()))
            .then_with(|| left.title.cmp(&right.title))
    });
}

/// Builds the default, no-prefix results: applications, places, and bookmarks.
pub fn default_results(
    query: &str,
    index: &AppIndex,
    frecency: &Frecency,
    now_secs: u64,
    limit: usize,
) -> Vec<SpotlightResult> {
    let mut results = Vec::new();

    for entry in index.entries() {
        let Some(found) = fuzzy::match_fields(query, &app_fields(entry)) else {
            continue;
        };

        let key = format!("app:{}", entry.desktop_id);
        let mut result = SpotlightResult::new(
            entry.name.clone(),
            entry
                .generic_name
                .clone()
                .or_else(|| entry.comment.clone())
                .unwrap_or_else(|| "Application".to_string()),
            entry.icon.clone(),
        );
        result.primary = Activation::LaunchApp(entry.desktop_id.clone());
        result.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
        result.frecency_key = Some(key);
        results.push(result);
    }

    for (name, path, icon) in places() {
        push_path_result(
            &mut results,
            query,
            &name,
            &path,
            &icon,
            "Folder",
            frecency,
            now_secs,
        );
    }

    for path in bookmarks::load() {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let icon = IconRef::from_icon_name("user-bookmarks-symbolic");
        push_path_result(
            &mut results,
            query,
            &name,
            &path,
            &icon,
            "Bookmark",
            frecency,
            now_secs,
        );
    }

    if query.is_empty() {
        // Every entry ties at zero, so the usual length tie-break would show an
        // arbitrary set of short names. Lead with what the user actually opens.
        sort_by_usage_then_name(&mut results);
    } else {
        sort_results(&mut results);
    }
    results.truncate(limit);
    results
}

/// Ordering for the opening state: most-used first, then alphabetical.
fn sort_by_usage_then_name(results: &mut [SpotlightResult]) {
    results.sort_by(|left, right| {
        right.score.cmp(&left.score).then_with(|| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.title.cmp(&right.title))
        })
    });
}

fn app_fields(entry: &AppEntry) -> Vec<Field<'_>> {
    let mut fields = vec![Field::new(entry.name.as_str(), 100)];
    if let Some(generic_name) = &entry.generic_name {
        fields.push(Field::new(generic_name.as_str(), 70));
    }
    if let Some(exec_name) = &entry.exec_name {
        fields.push(Field::new(exec_name.as_str(), 60));
    }
    fields.extend(
        entry
            .keywords
            .iter()
            .map(|keyword| Field::new(keyword.as_str(), 50)),
    );
    if let Some(comment) = &entry.comment {
        fields.push(Field::new(comment.as_str(), 30));
    }
    fields.extend(
        entry
            .categories
            .iter()
            .map(|category| Field::new(category.as_str(), 20)),
    );
    fields
}

#[allow(clippy::too_many_arguments)]
fn push_path_result(
    results: &mut Vec<SpotlightResult>,
    query: &str,
    name: &str,
    path: &Path,
    icon: &IconRef,
    kind: &str,
    frecency: &Frecency,
    now_secs: u64,
) {
    let Some(found) = fuzzy::match_fields(
        query,
        &[
            Field::new(name, 100),
            Field::new(&path.to_string_lossy(), 40),
        ],
    ) else {
        return;
    };

    let key = format!("path:{}", path.display());
    let mut result = SpotlightResult::new(
        name.to_string(),
        format!("{kind} · {}", paths::display_path(path)),
        icon.clone(),
    );
    result.primary = Activation::OpenPath(path.to_path_buf());
    result.secondary = path
        .parent()
        .map(|parent| Activation::OpenPath(parent.to_path_buf()));
    result.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
    result.frecency_key = Some(key);
    results.push(result);
}

/// The XDG user directories offered alongside applications.
pub fn places() -> Vec<(String, PathBuf, IconRef)> {
    let Some(dirs) = UserDirs::new() else {
        return Vec::new();
    };

    let mut places = vec![(
        "Home".to_string(),
        dirs.home_dir().to_path_buf(),
        IconRef::from_icon_name("user-home-symbolic"),
    )];

    let optional: [(&str, Option<&Path>, &str); 5] = [
        (
            "Documents",
            dirs.document_dir(),
            "folder-documents-symbolic",
        ),
        ("Downloads", dirs.download_dir(), "folder-download-symbolic"),
        ("Pictures", dirs.picture_dir(), "folder-pictures-symbolic"),
        ("Music", dirs.audio_dir(), "folder-music-symbolic"),
        ("Videos", dirs.video_dir(), "folder-videos-symbolic"),
    ];
    for (name, path, icon) in optional {
        if let Some(path) = path {
            places.push((
                name.to_string(),
                path.to_path_buf(),
                IconRef::from_icon_name(icon),
            ));
        }
    }

    places
}

/// The row offered when the text matches an alphanumeric prefix but has not yet
/// committed to it, e.g. `g` before the space.
pub fn hint_result(prefix: &Prefix) -> SpotlightResult {
    let mut result = SpotlightResult::new(
        prefix.label.clone(),
        format!("{} · press Tab", prefix.description),
        IconRef::from_icon_name(prefix.icon.clone()),
    );
    result.primary = Activation::Replace(format!("{} ", prefix.key));
    result.completion = Some(format!("{} ", prefix.key));
    // Above any fuzzy score so the hint stays pinned to the top.
    result.score = i32::MAX;
    result
}

/// Builds the results for an active prefix.
pub fn prefixed_results(
    prefix: &Prefix,
    arg: &str,
    table: &PrefixTable,
    limit: usize,
) -> Vec<SpotlightResult> {
    match &prefix.kind {
        PrefixKind::Shell => shell_results(arg),
        PrefixKind::OpenPath => path_results(arg, limit),
        PrefixKind::Calculator => calculator_results(arg),
        PrefixKind::Help => help_results(table),
        PrefixKind::FileSearch => Vec::new(), // filled asynchronously by the walker
        PrefixKind::Command { command, terminal } => {
            command_results(prefix, command, *terminal, arg)
        }
        PrefixKind::Ai(index) => ai_results(prefix, *index, arg),
    }
}

/// The single row that opens a chat. Enter on it hands the prompt to the model.
fn ai_results(prefix: &Prefix, index: usize, arg: &str) -> Vec<SpotlightResult> {
    let icon = IconRef::from_icon_name(prefix.icon.clone());

    if arg.trim().is_empty() {
        return vec![SpotlightResult::new(
            prefix.label.clone(),
            format!("{} · type a question", prefix.description),
            icon,
        )];
    }

    let mut result = SpotlightResult::new(arg.to_string(), format!("Ask {}", prefix.label), icon);
    result.primary = Activation::AskAi {
        provider: index,
        prompt: arg.to_string(),
    };
    result.frecency_key = Some(format!("ai:{}", prefix.key));
    vec![result]
}

/// The trailing "Ask …" row offered on a plain query when a provider is marked
/// `default = true`. Sorted last so it never displaces a real match.
pub fn default_ai_result(
    provider_index: usize,
    label: &str,
    icon: &str,
    prompt: &str,
) -> SpotlightResult {
    let mut result = SpotlightResult::new(
        format!("Ask {label}"),
        prompt.to_string(),
        IconRef::from_icon_name(icon),
    );
    result.primary = Activation::AskAi {
        provider: provider_index,
        prompt: prompt.to_string(),
    };
    result.score = i32::MIN + 1;
    result
}

fn shell_results(arg: &str) -> Vec<SpotlightResult> {
    if arg.trim().is_empty() {
        return vec![SpotlightResult::new(
            "Run a command",
            "Type a shell command to run",
            IconRef::from_icon_name("utilities-terminal-symbolic"),
        )];
    }

    let mut result = SpotlightResult::new(
        arg.to_string(),
        "Run · Ctrl+Enter to run in a terminal",
        IconRef::from_icon_name("utilities-terminal-symbolic"),
    );
    result.primary = Activation::RunShell(arg.to_string());
    result.secondary = Some(Activation::RunInTerminal(arg.to_string()));
    vec![result]
}

fn command_results(
    prefix: &Prefix,
    command: &str,
    terminal: bool,
    arg: &str,
) -> Vec<SpotlightResult> {
    let line = build_command_line(command, arg);
    let mut result = SpotlightResult::new(
        if arg.trim().is_empty() {
            prefix.label.clone()
        } else {
            format!("{} — {arg}", prefix.label)
        },
        prefix.description.clone(),
        IconRef::from_icon_name(prefix.icon.clone()),
    );
    result.primary = if terminal {
        Activation::RunInTerminal(line.clone())
    } else {
        Activation::RunShell(line.clone())
    };
    result.secondary = Some(Activation::RunInTerminal(line));
    result.frecency_key = Some(format!("prefix:{}", prefix.key));
    vec![result]
}

fn path_results(arg: &str, limit: usize) -> Vec<SpotlightResult> {
    let candidates = paths::complete(arg, limit);
    if candidates.is_empty() {
        let expanded = paths::expand_tilde(arg.trim());
        if expanded.exists() {
            return vec![open_path_result(&PathCandidate {
                path: expanded.clone(),
                is_dir: expanded.is_dir(),
                score: 0,
            })];
        }
        return vec![SpotlightResult::new(
            "No matching path",
            "Type a folder or file path",
            IconRef::from_icon_name("folder-open-symbolic"),
        )];
    }

    candidates.iter().map(open_path_result).collect()
}

fn open_path_result(candidate: &PathCandidate) -> SpotlightResult {
    let name = candidate
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| candidate.path.display().to_string());

    let mut result = SpotlightResult::new(
        name,
        paths::display_path(&candidate.path),
        IconRef::from_icon_name(if candidate.is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        }),
    );
    result.primary = Activation::OpenPath(candidate.path.clone());
    result.secondary = candidate
        .path
        .parent()
        .map(|parent| Activation::OpenPath(parent.to_path_buf()));
    result.completion = Some(format!("> {}", paths::completion_text(candidate)));
    result.frecency_key = Some(format!("path:{}", candidate.path.display()));
    result.score = candidate.score;
    result
}

/// Wraps filesystem-search hits, which arrive asynchronously.
pub fn file_hit_results(
    hits: &[crate::spotlight::file_search::FileHit],
    limit: usize,
) -> Vec<SpotlightResult> {
    let mut results = hits
        .iter()
        .map(|hit| {
            let name = hit
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| hit.path.display().to_string());

            let mut result = SpotlightResult::new(
                name,
                paths::display_path(&hit.path),
                IconRef::from_icon_name(if hit.is_dir {
                    "folder-symbolic"
                } else {
                    "text-x-generic-symbolic"
                }),
            );
            result.primary = Activation::OpenPath(hit.path.clone());
            result.secondary = hit
                .path
                .parent()
                .map(|parent| Activation::OpenPath(parent.to_path_buf()));
            result.frecency_key = Some(format!("path:{}", hit.path.display()));
            result.score = hit.score;
            result
        })
        .collect::<Vec<_>>();

    sort_results(&mut results);
    results.truncate(limit);
    results
}

/// The trailing row shown when the filesystem walk hit one of its bounds.
pub fn truncation_notice(shown: usize) -> SpotlightResult {
    let mut result = SpotlightResult::new(
        format!("Showing the first {shown} matches"),
        "Narrow your search to see more",
        IconRef::from_icon_name("view-more-symbolic"),
    );
    // Below every real hit, so it always lands at the bottom.
    result.score = i32::MIN;
    result
}

fn calculator_results(arg: &str) -> Vec<SpotlightResult> {
    if arg.trim().is_empty() {
        return vec![SpotlightResult::new(
            "Calculate",
            "Type an expression, e.g. 2^10 or sqrt(2)",
            IconRef::from_icon_name("accessories-calculator-symbolic"),
        )];
    }

    match calc::eval(arg) {
        Ok(value) => {
            let text = calc::format_result(value);
            let mut result = SpotlightResult::new(
                text.clone(),
                format!("{arg} · Enter to copy"),
                IconRef::from_icon_name("accessories-calculator-symbolic"),
            );
            result.primary = Activation::CopyText(text);
            vec![result]
        }
        Err(error) => vec![SpotlightResult::new(
            "Cannot calculate",
            error.to_string(),
            IconRef::from_icon_name("dialog-warning-symbolic"),
        )],
    }
}

fn help_results(table: &PrefixTable) -> Vec<SpotlightResult> {
    table
        .all()
        .iter()
        .map(|prefix| {
            let mut result = SpotlightResult::new(
                format!("{}  {}", prefix.key, prefix.label),
                prefix.description.clone(),
                IconRef::from_icon_name(prefix.icon.clone()),
            );
            let text = if prefix.is_symbolic() {
                prefix.key.clone()
            } else {
                format!("{} ", prefix.key)
            };
            result.primary = Activation::Replace(text.clone());
            result.completion = Some(text);
            result
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SpotlightConfig;
    use crate::spotlight::prefixes;

    fn table() -> PrefixTable {
        prefixes::resolve_with_ai(&SpotlightConfig::default()).0
    }

    #[test]
    fn sorting_prefers_higher_scores_then_shorter_titles() {
        let mut results = vec![
            SpotlightResult {
                score: 10,
                ..SpotlightResult::new("Bravo", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 50,
                ..SpotlightResult::new("Alphabetical", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 50,
                ..SpotlightResult::new("Alpha", "", IconRef::fallback())
            },
        ];

        sort_results(&mut results);

        let titles = results.iter().map(|r| r.title.as_str()).collect::<Vec<_>>();
        assert_eq!(titles, vec!["Alpha", "Alphabetical", "Bravo"]);
    }

    #[test]
    fn the_opening_state_leads_with_usage_then_alphabetical() {
        let mut results = vec![
            SpotlightResult {
                score: 0,
                ..SpotlightResult::new("Zebra", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 0,
                ..SpotlightResult::new("Ant", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 40,
                ..SpotlightResult::new("Frequently Used Thing", "", IconRef::fallback())
            },
        ];

        sort_by_usage_then_name(&mut results);

        let titles = results.iter().map(|r| r.title.as_str()).collect::<Vec<_>>();
        assert_eq!(titles, vec!["Frequently Used Thing", "Ant", "Zebra"]);
    }

    #[test]
    fn calculator_reports_the_value_and_offers_a_copy() {
        let results = calculator_results("2^10");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "1024");
        assert_eq!(results[0].primary, Activation::CopyText("1024".to_string()));
    }

    #[test]
    fn calculator_surfaces_parse_errors_without_an_action() {
        let results = calculator_results("1 +");

        assert_eq!(results[0].title, "Cannot calculate");
        assert_eq!(results[0].primary, Activation::Inert);
    }

    #[test]
    fn shell_prefix_offers_a_terminal_as_the_secondary_action() {
        let results = shell_results("notify-send hi");

        assert_eq!(
            results[0].primary,
            Activation::RunShell("notify-send hi".to_string())
        );
        assert_eq!(
            results[0].secondary,
            Some(Activation::RunInTerminal("notify-send hi".to_string()))
        );
    }

    #[test]
    fn help_lists_every_prefix_and_completes_to_it() {
        let table = table();
        let results = help_results(&table);

        assert_eq!(results.len(), table.all().len());
        assert!(results.iter().any(|result| result.title.starts_with('=')));
        assert!(results.iter().all(|result| result.completion.is_some()));
    }

    #[test]
    fn a_hint_row_sorts_above_everything_else() {
        let table = table();
        let prefix = table.get("=").expect("calculator prefix");
        let hint = hint_result(prefix);

        let mut results = vec![
            SpotlightResult {
                score: 10_000,
                ..SpotlightResult::new("Loud", "", IconRef::fallback())
            },
            hint,
        ];
        sort_results(&mut results);

        assert_eq!(results[0].title, "Calculate");
    }

    #[test]
    fn user_command_prefixes_expand_their_template() {
        let prefix = Prefix {
            key: "g".to_string(),
            label: "Google".to_string(),
            description: "Search".to_string(),
            icon: "web-browser-symbolic".to_string(),
            kind: PrefixKind::Command {
                command: "xdg-open 'https://example.com/?q={query_url}'".to_string(),
                terminal: false,
            },
        };

        let results = prefixed_results(&prefix, "a b", &table(), 10);

        assert_eq!(
            results[0].primary,
            Activation::RunShell("xdg-open 'https://example.com/?q=a+b'".to_string())
        );
    }
}
