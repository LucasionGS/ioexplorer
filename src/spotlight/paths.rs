//! Path expansion and directory completion for the `>` prefix.

use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::UserDirs;

use crate::launcher::fuzzy;

/// Cap on how many directory entries a single completion will look at, so a
/// pathological directory cannot wedge the UI.
const MAX_SCANNED_ENTRIES: usize = 5_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathCandidate {
    pub path: PathBuf,
    pub is_dir: bool,
    pub score: i32,
}

/// Expands a leading `~`, resolving anything relative against the home directory
/// (spotlight has no meaningful working directory).
pub fn expand_tilde(input: &str) -> PathBuf {
    let home = home_dir();

    if input == "~" {
        return home;
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }

    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        home.join(path)
    }
}

/// Splits completion input into the directory to list and the partial name to match.
pub fn split_completion(input: &str) -> (PathBuf, String) {
    if input.is_empty() {
        return (home_dir(), String::new());
    }
    if input.ends_with('/') || input == "~" {
        return (expand_tilde(input), String::new());
    }

    let (head, tail) = match input.rsplit_once('/') {
        Some((head, tail)) => (head, tail),
        None => ("", input),
    };

    let dir = if head.is_empty() {
        if input.starts_with('/') {
            PathBuf::from("/")
        } else {
            home_dir()
        }
    } else {
        expand_tilde(head)
    };

    (dir, tail.to_string())
}

/// Lists completions for `input`, directories first.
///
/// Hidden entries only surface once the partial name starts with a dot, matching
/// shell completion.
pub fn complete(input: &str, limit: usize) -> Vec<PathCandidate> {
    let (dir, partial) = split_completion(input.trim_end_matches(' '));
    let include_hidden = partial.starts_with('.');

    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut candidates = entries
        .take(MAX_SCANNED_ENTRIES)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !include_hidden {
                return None;
            }

            let score = fuzzy::match_query(&partial, &name)?.score;
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);

            Some(PathCandidate {
                path: entry.path(),
                is_dir,
                score,
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates.truncate(limit);
    candidates
}

/// Renders a candidate back into entry text, adding a trailing slash for
/// directories so the next Tab descends into them.
pub fn completion_text(candidate: &PathCandidate) -> String {
    let text = display_path(&candidate.path);
    if candidate.is_dir && !text.ends_with('/') {
        format!("{text}/")
    } else {
        text
    }
}

/// Renders a path for display, abbreviating the home directory as `~`.
pub fn display_path(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn home_dir() -> PathBuf {
    UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_absolute_paths() {
        let (dir, partial) = split_completion("/etc/host");

        assert_eq!(dir, PathBuf::from("/etc"));
        assert_eq!(partial, "host");
    }

    #[test]
    fn a_trailing_slash_lists_the_directory_itself() {
        let (dir, partial) = split_completion("/etc/");

        assert_eq!(dir, PathBuf::from("/etc"));
        assert_eq!(partial, "");
    }

    #[test]
    fn a_bare_name_lists_the_root() {
        let (dir, partial) = split_completion("/etc");

        assert_eq!(dir, PathBuf::from("/"));
        assert_eq!(partial, "etc");
    }

    #[test]
    fn empty_input_lists_the_home_directory() {
        let (dir, partial) = split_completion("");

        assert_eq!(dir, expand_tilde("~"));
        assert_eq!(partial, "");
    }

    #[test]
    fn expands_the_tilde() {
        let home = expand_tilde("~");

        assert_eq!(expand_tilde("~/Documents"), home.join("Documents"));
        assert_eq!(expand_tilde("/tmp"), PathBuf::from("/tmp"));
        assert_eq!(expand_tilde("Documents"), home.join("Documents"));
    }

    #[test]
    fn completes_directories_before_files_and_hides_dotfiles() {
        let temp = tempfile::tempdir().expect("temp dir");
        fs::create_dir(temp.path().join("alpha-dir")).expect("dir");
        fs::write(temp.path().join("alpha-file"), b"x").expect("file");
        fs::write(temp.path().join(".alpha-hidden"), b"x").expect("hidden file");

        let input = format!("{}/alpha", temp.path().display());
        let candidates = complete(&input, 10);

        assert_eq!(candidates.len(), 2, "hidden entries must stay hidden");
        assert!(candidates[0].is_dir, "directories sort first");
        assert!(candidates[0].path.ends_with("alpha-dir"));
    }

    #[test]
    fn a_leading_dot_reveals_hidden_entries() {
        let temp = tempfile::tempdir().expect("temp dir");
        fs::write(temp.path().join(".config"), b"x").expect("hidden file");

        let input = format!("{}/.con", temp.path().display());
        let candidates = complete(&input, 10);

        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].path.ends_with(".config"));
    }

    #[test]
    fn completion_text_adds_a_trailing_slash_for_directories() {
        let candidate = PathCandidate {
            path: PathBuf::from("/tmp/example"),
            is_dir: true,
            score: 0,
        };

        assert_eq!(completion_text(&candidate), "/tmp/example/");
    }

    #[test]
    fn missing_directories_complete_to_nothing() {
        assert!(complete("/definitely/not/here/x", 10).is_empty());
    }
}
