use std::{fs, io, path::PathBuf};

use directories::UserDirs;

pub fn load() -> Vec<PathBuf> {
    let Some(path) = storage_path() else {
        return Vec::new();
    };

    match fs::read_to_string(path) {
        Ok(contents) => parse_bookmarks(&contents),
        Err(_) => Vec::new(),
    }
}

pub fn save(bookmarks: &[PathBuf]) -> io::Result<()> {
    let Some(path) = storage_path() else {
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut contents = String::new();
    for bookmark in bookmarks {
        contents.push_str(&bookmark.to_string_lossy());
        contents.push('\n');
    }

    fs::write(path, contents)
}

pub fn add(bookmarks: &mut Vec<PathBuf>, path: PathBuf) -> bool {
    let path = normalized_path(path);
    if contains(bookmarks, &path) {
        return false;
    }

    bookmarks.push(path);
    true
}

pub fn remove(bookmarks: &mut Vec<PathBuf>, path: impl AsRef<std::path::Path>) -> bool {
    let path = normalized_path(path);
    let original_len = bookmarks.len();
    bookmarks.retain(|bookmark| normalized_path(bookmark) != path);
    bookmarks.len() != original_len
}

pub fn contains(bookmarks: &[PathBuf], path: impl AsRef<std::path::Path>) -> bool {
    let path = normalized_path(path);
    bookmarks
        .iter()
        .any(|bookmark| normalized_path(bookmark) == path)
}

pub fn normalized_path(path: impl AsRef<std::path::Path>) -> PathBuf {
    let path = path.as_ref();
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn storage_path() -> Option<PathBuf> {
    UserDirs::new().map(|dirs| dirs.home_dir().join(".local/ioexplorer/bookmarks"))
}

fn parse_bookmarks(contents: &str) -> Vec<PathBuf> {
    let mut bookmarks = Vec::new();

    for line in contents.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        if path.is_absolute() {
            add(&mut bookmarks, path);
        }
    }

    bookmarks
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{contains, parse_bookmarks, remove};

    #[test]
    fn parses_absolute_bookmark_paths() {
        assert_eq!(
            parse_bookmarks("\n/tmp/projects\nrelative/path\n/tmp/downloads\n"),
            vec![
                PathBuf::from("/tmp/projects"),
                PathBuf::from("/tmp/downloads")
            ]
        );
    }

    #[test]
    fn deduplicates_bookmark_paths() {
        assert_eq!(
            parse_bookmarks("/tmp/projects\n/tmp/projects\n"),
            vec![PathBuf::from("/tmp/projects")]
        );
    }

    #[test]
    fn removes_matching_bookmark_path() {
        let mut bookmarks = vec![
            PathBuf::from("/tmp/projects"),
            PathBuf::from("/tmp/downloads"),
        ];

        assert!(remove(&mut bookmarks, "/tmp/projects"));
        assert_eq!(bookmarks, vec![PathBuf::from("/tmp/downloads")]);
        assert!(!remove(&mut bookmarks, "/tmp/missing"));
    }

    #[test]
    fn detects_existing_bookmark_path() {
        let bookmarks = vec![PathBuf::from("/tmp/projects")];

        assert!(contains(&bookmarks, "/tmp/projects"));
        assert!(!contains(&bookmarks, "/tmp/missing"));
    }
}
