//! Listing the desktop folder, and working out what changed since last time.

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use crate::{
    config::DesktopConfig,
    providers::{FileItem, Provider, ProviderError, ProviderUri, local::LocalProvider},
    sorting,
};

/// Lists `folder`, filtered and sorted the way `config` asks.
pub fn list(
    provider: &LocalProvider,
    folder: &Path,
    config: &DesktopConfig,
) -> Result<Vec<FileItem>, ProviderError> {
    let mut items = provider.list(&ProviderUri::local(folder))?;
    if !config.show_hidden {
        items.retain(|item| !item.hidden);
    }
    sorting::sort_items(&mut items, config.sort);
    Ok(items)
}

/// What a reload did to the set of icons on screen.
///
/// Names, not indices: the desktop keys everything — positions, selection,
/// widgets — by file name, so that a reload cannot silently re-point a
/// selection at a different file the way an index-based view would.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Reconciliation {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
}

impl Reconciliation {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Diffs the names on screen against the names now in the folder.
///
/// The desktop updates in place rather than rebuilding: a rebuild would scramble
/// every position, drop the selection, and re-queue every thumbnail, all because
/// one file appeared.
pub fn reconcile(current: &[String], next: &[String]) -> Reconciliation {
    let current_set: BTreeSet<&String> = current.iter().collect();
    let next_set: BTreeSet<&String> = next.iter().collect();

    Reconciliation {
        // `next` order, so new icons are placed in listing order rather than
        // whatever order a set iteration happens to produce.
        added: next
            .iter()
            .filter(|name| !current_set.contains(name))
            .cloned()
            .collect(),
        removed: current
            .iter()
            .filter(|name| !next_set.contains(name))
            .cloned()
            .collect(),
        kept: next
            .iter()
            .filter(|name| current_set.contains(name))
            .cloned()
            .collect(),
    }
}

/// Indexes a listing by file name, for the in-place update of surviving tiles.
pub fn by_name(items: Vec<FileItem>) -> HashMap<String, FileItem> {
    items
        .into_iter()
        .map(|item| (item.name.clone(), item))
        .collect()
}

pub fn names(items: &[FileItem]) -> Vec<String> {
    items.iter().map(|item| item.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn an_unchanged_folder_reconciles_to_nothing() {
        let current = strings(&["a.txt", "b.txt"]);

        let diff = reconcile(&current, &current);

        assert!(diff.is_empty());
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.kept, current);
    }

    #[test]
    fn a_new_file_is_the_only_addition() {
        let diff = reconcile(&strings(&["a.txt"]), &strings(&["a.txt", "b.txt"]));

        assert_eq!(diff.added, strings(&["b.txt"]));
        assert!(diff.removed.is_empty());
        assert_eq!(diff.kept, strings(&["a.txt"]));
    }

    #[test]
    fn a_deleted_file_is_the_only_removal() {
        let diff = reconcile(&strings(&["a.txt", "b.txt"]), &strings(&["a.txt"]));

        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, strings(&["b.txt"]));
        assert_eq!(diff.kept, strings(&["a.txt"]));
    }

    /// A rename arrives as a delete plus a create. The position survives it
    /// because the store is told about the rename first, not because the diff
    /// recognises one.
    #[test]
    fn a_rename_reads_as_one_addition_and_one_removal() {
        let diff = reconcile(&strings(&["old.txt"]), &strings(&["new.txt"]));

        assert_eq!(diff.added, strings(&["new.txt"]));
        assert_eq!(diff.removed, strings(&["old.txt"]));
        assert!(diff.kept.is_empty());
    }

    #[test]
    fn an_empty_desktop_gaining_files_is_all_additions() {
        let diff = reconcile(&[], &strings(&["a.txt", "b.txt"]));

        assert_eq!(diff.added, strings(&["a.txt", "b.txt"]));
        assert!(diff.removed.is_empty());
        assert!(diff.kept.is_empty());
    }

    #[test]
    fn emptying_a_desktop_is_all_removals() {
        let diff = reconcile(&strings(&["a.txt", "b.txt"]), &[]);

        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, strings(&["a.txt", "b.txt"]));
        assert!(diff.kept.is_empty());
    }

    #[test]
    fn two_empty_listings_reconcile_to_nothing() {
        assert!(reconcile(&[], &[]).is_empty());
    }

    /// New icons are placed in listing order, so a batch of files lands in a
    /// predictable run of cells rather than scattered.
    #[test]
    fn additions_keep_the_listing_order() {
        let diff = reconcile(&strings(&["m.txt"]), &strings(&["a.txt", "m.txt", "z.txt"]));

        assert_eq!(diff.added, strings(&["a.txt", "z.txt"]));
    }

    /// Reordering the sort must not read as churn — nothing was added or
    /// removed, so no tile should be rebuilt.
    #[test]
    fn a_reordered_listing_is_not_a_change() {
        let diff = reconcile(&strings(&["a.txt", "b.txt"]), &strings(&["b.txt", "a.txt"]));

        assert!(diff.is_empty());
        assert_eq!(diff.kept.len(), 2);
    }
}
