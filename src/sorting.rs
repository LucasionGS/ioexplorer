//! How a folder listing is ordered.
//!
//! The order is session state rather than a provider concern: picking a new key
//! re-sorts the listing already in memory instead of re-reading the directory.
//! [`crate::config::AppConfig`] supplies the starting order and
//! [`crate::state::AppState`] remembers whatever the user last chose.

use std::{cmp::Ordering, path::Path};

use serde::{Deserialize, Serialize};

use crate::providers::{FileItem, FileKind};

/// The field a listing is ordered by.
#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Created,
    Size,
    Extension,
}

impl SortKey {
    /// Every key, in the order the sort menu lists them.
    pub const ALL: [Self; 5] = [
        Self::Name,
        Self::Modified,
        Self::Created,
        Self::Size,
        Self::Extension,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Modified Date",
            Self::Created => "Created Date",
            Self::Size => "Size",
            Self::Extension => "Extension",
        }
    }
}

/// A complete ordering: which field, which direction, and whether folders are
/// pinned above files regardless of both.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SortOrder {
    pub key: SortKey,
    pub descending: bool,
    pub folders_first: bool,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            descending: false,
            folders_first: true,
        }
    }
}

impl SortOrder {
    pub fn direction_label(self) -> &'static str {
        if self.descending {
            "Descending"
        } else {
            "Ascending"
        }
    }

    pub fn icon_name(self) -> &'static str {
        if self.descending {
            "view-sort-descending-symbolic"
        } else {
            "view-sort-ascending-symbolic"
        }
    }

    pub fn summary(self) -> String {
        format!(
            "Sorted by {} - {}",
            self.key.label(),
            self.direction_label()
        )
    }
}

/// Orders `items` in place.
///
/// Every key falls back to the name, so ties — same size, same second, no
/// extension at all — land in a readable order rather than whichever one the
/// directory happened to yield first. That fallback stays ascending under
/// `descending`: flipping it would only shuffle rows that compare equal.
pub fn sort_items(items: &mut [FileItem], order: SortOrder) {
    items.sort_by(|left, right| {
        if order.folders_first {
            let folders = is_directory(right).cmp(&is_directory(left));
            if folders != Ordering::Equal {
                return folders;
            }
        }

        let compared = compare_key(left, right, order.key);
        let compared = if order.descending {
            compared.reverse()
        } else {
            compared
        };

        compared.then_with(|| name_key(left).cmp(&name_key(right)))
    });
}

fn compare_key(left: &FileItem, right: &FileItem, key: SortKey) -> Ordering {
    match key {
        SortKey::Name => name_key(left).cmp(&name_key(right)),
        SortKey::Modified => left.modified.cmp(&right.modified),
        SortKey::Created => left.created.cmp(&right.created),
        SortKey::Size => left.size.unwrap_or(0).cmp(&right.size.unwrap_or(0)),
        SortKey::Extension => extension_key(left).cmp(&extension_key(right)),
    }
}

fn is_directory(item: &FileItem) -> bool {
    item.kind == FileKind::Directory
}

fn name_key(item: &FileItem) -> String {
    item.display_name().to_lowercase()
}

/// The lowercased extension, empty for anything without one.
///
/// Read off the real file name rather than the display name: a `.desktop` entry
/// shows as its application title, and filing every launcher under "no
/// extension" would defeat the point of grouping by type.
fn extension_key(item: &FileItem) -> String {
    if item.kind == FileKind::Directory {
        return String::new();
    }

    Path::new(&item.name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::providers::ProviderUri;

    use super::*;

    fn item(name: &str, kind: FileKind, size: u64, seconds: u64) -> FileItem {
        let time = UNIX_EPOCH + Duration::from_secs(seconds);
        FileItem {
            uri: ProviderUri::local(format!("/tmp/{name}")),
            name: name.to_string(),
            display_name: None,
            icon: None,
            kind,
            size: (kind == FileKind::File).then_some(size),
            modified: Some(time),
            created: Some(time),
            hidden: name.starts_with('.'),
        }
    }

    fn names(items: &[FileItem]) -> Vec<&str> {
        items.iter().map(|item| item.name.as_str()).collect()
    }

    fn listing() -> Vec<FileItem> {
        vec![
            item("notes.txt", FileKind::File, 300, 30),
            item("photo.JPG", FileKind::File, 100, 10),
            item("Archive", FileKind::Directory, 0, 20),
            item("readme", FileKind::File, 200, 20),
        ]
    }

    #[test]
    fn sorts_by_name_with_folders_first() {
        let mut items = listing();

        sort_items(&mut items, SortOrder::default());

        assert_eq!(
            names(&items),
            ["Archive", "notes.txt", "photo.JPG", "readme"]
        );
    }

    #[test]
    fn descending_reverses_the_chosen_key_only() {
        let mut items = listing();

        sort_items(
            &mut items,
            SortOrder {
                descending: true,
                ..SortOrder::default()
            },
        );

        assert_eq!(
            names(&items),
            ["Archive", "readme", "photo.JPG", "notes.txt"]
        );
    }

    #[test]
    fn folders_join_the_ordering_when_not_pinned() {
        let mut items = listing();

        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Size,
                folders_first: false,
                ..SortOrder::default()
            },
        );

        // The folder carries no size, so it sorts alongside the smallest files.
        assert_eq!(
            names(&items),
            ["Archive", "photo.JPG", "readme", "notes.txt"]
        );
    }

    #[test]
    fn sorts_by_modified_and_created_times() {
        let mut items = listing();

        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Modified,
                descending: true,
                folders_first: false,
            },
        );

        assert_eq!(
            names(&items),
            ["notes.txt", "Archive", "readme", "photo.JPG"]
        );

        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Created,
                descending: false,
                folders_first: false,
            },
        );

        assert_eq!(
            names(&items),
            ["photo.JPG", "Archive", "readme", "notes.txt"]
        );
    }

    #[test]
    fn sorts_by_extension_case_insensitively_then_by_name() {
        let mut items = vec![
            item("b.txt", FileKind::File, 1, 1),
            item("a.TXT", FileKind::File, 1, 1),
            item("song.flac", FileKind::File, 1, 1),
            item("LICENSE", FileKind::File, 1, 1),
        ];

        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Extension,
                ..SortOrder::default()
            },
        );

        assert_eq!(names(&items), ["LICENSE", "song.flac", "a.TXT", "b.txt"]);
    }

    #[test]
    fn a_missing_timestamp_does_not_drop_an_item() {
        let mut items = listing();
        items[0].modified = None;

        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Modified,
                ..SortOrder::default()
            },
        );

        assert_eq!(items.len(), 4);
        assert!(items.iter().any(|item| item.name == "notes.txt"));
    }

    #[test]
    fn dotfiles_have_no_extension() {
        let dotfile = item(".bashrc", FileKind::File, 1, 1);

        assert_eq!(extension_key(&dotfile), "");
    }

    #[test]
    fn sorting_is_stable_for_equal_keys() {
        let mut items = vec![
            item("b", FileKind::File, 5, 1),
            item("a", FileKind::File, 5, 1),
        ];

        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Size,
                ..SortOrder::default()
            },
        );

        assert_eq!(names(&items), ["a", "b"]);
    }

    #[test]
    fn every_key_is_listed_in_the_menu_order() {
        assert_eq!(
            SortKey::ALL.map(SortKey::label),
            ["Name", "Modified Date", "Created Date", "Size", "Extension"]
        );
    }

    #[test]
    fn an_order_round_trips_through_toml() {
        let order = SortOrder {
            key: SortKey::Extension,
            descending: true,
            folders_first: false,
        };

        let text = toml::to_string(&order).expect("serialize");
        let parsed: SortOrder = toml::from_str(&text).expect("deserialize");

        assert_eq!(parsed, order);
    }

    #[test]
    fn a_partial_order_falls_back_to_the_defaults() {
        let parsed: SortOrder = toml::from_str("key = \"size\"\n").expect("deserialize");

        assert_eq!(
            parsed,
            SortOrder {
                key: SortKey::Size,
                ..SortOrder::default()
            }
        );
    }

    #[test]
    fn unknown_timestamps_are_ordered_consistently() {
        let mut with_time = item("dated.txt", FileKind::File, 1, 50);
        let mut without = item("undated.txt", FileKind::File, 1, 0);
        without.created = None;
        with_time.created = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(50));

        let mut items = vec![with_time, without];
        sort_items(
            &mut items,
            SortOrder {
                key: SortKey::Created,
                ..SortOrder::default()
            },
        );

        assert_eq!(names(&items), ["undated.txt", "dated.txt"]);
    }
}
