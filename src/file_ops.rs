//! Filesystem operations shared by every surface that manages files.
//!
//! These used to live inside `ui::window`, welded to `AppWindow` for its status
//! label and refresh. The desktop surface needs the same operations but reports
//! through a toast rather than a status bar, so the ones that touched the window
//! now return an outcome and let the caller render it. The pure helpers moved
//! across unchanged.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use gio::prelude::FileExt;
use gtk::prelude::*;

use crate::{
    config::CustomActionConfig,
    custom_actions::{self, ActionTarget},
    providers::{FileItem, FileKind},
    ui::dnd,
};

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum FileClipboardOperation {
    Copy,
    Cut,
}

impl FileClipboardOperation {
    pub fn gnome_action(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Cut => "cut",
        }
    }

    pub fn past_tense(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Cut => "Cut",
        }
    }

    pub fn drop_operation(self) -> dnd::DropOperation {
        match self {
            Self::Copy => dnd::DropOperation::Copy,
            Self::Cut => dnd::DropOperation::Move,
        }
    }
}

pub fn file_clipboard_provider(
    paths: &[PathBuf],
    operation: FileClipboardOperation,
) -> gtk::gdk::ContentProvider {
    let files = paths.iter().map(gio::File::for_path).collect::<Vec<_>>();
    let file_list = gtk::gdk::FileList::from_array(&files);
    let file_list_provider = gtk::gdk::ContentProvider::for_value(&file_list.to_value());

    let gnome_payload = file_clipboard_payload(paths, operation);
    let gnome_bytes = glib::Bytes::from_owned(gnome_payload.into_bytes());
    let gnome_provider =
        gtk::gdk::ContentProvider::for_bytes("x-special/gnome-copied-files", &gnome_bytes);

    let uri_payload = file_uri_list_payload(paths);
    let uri_bytes = glib::Bytes::from_owned(uri_payload.into_bytes());
    let uri_provider = gtk::gdk::ContentProvider::for_bytes("text/uri-list", &uri_bytes);

    gtk::gdk::ContentProvider::new_union(&[file_list_provider, gnome_provider, uri_provider])
}

pub fn file_clipboard_payload(paths: &[PathBuf], operation: FileClipboardOperation) -> String {
    let mut payload = operation.gnome_action().to_string();
    for path in paths {
        payload.push('\n');
        payload.push_str(&file_uri_for_path(path));
    }
    payload.push('\n');
    payload
}

pub fn file_uri_list_payload(paths: &[PathBuf]) -> String {
    let mut payload = String::new();
    for path in paths {
        payload.push_str(&file_uri_for_path(path));
        payload.push_str("\r\n");
    }
    payload
}

pub fn file_uri_for_path(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

pub fn same_paths(left: &[PathBuf], right: &[PathBuf]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort();
    right.sort();
    left == right
}

/// Puts `paths` on `clipboard`. `Ok` carries the message to show the user.
pub fn copy_paths_to_clipboard(
    clipboard: &gtk::gdk::Clipboard,
    paths: &[PathBuf],
    operation: FileClipboardOperation,
) -> Result<String, String> {
    if paths.is_empty() {
        return Err("No item selected".to_string());
    }

    let provider = file_clipboard_provider(paths, operation);
    match clipboard.set_content(Some(&provider)) {
        Ok(()) => Ok(format!(
            "{} {} item(s) to clipboard",
            operation.past_tense(),
            paths.len()
        )),
        Err(error) => Err(format!("Failed to copy to clipboard: {error}")),
    }
}

// ---------------------------------------------------------------------------
// Item classification
// ---------------------------------------------------------------------------

pub fn is_desktop_entry_file(item: &FileItem) -> bool {
    item.kind == FileKind::File && item.name.to_ascii_lowercase().ends_with(".desktop")
}

/// An archive picked out of a selection, with the folder it should unpack into
/// already worked out from its name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchivePath {
    pub path: PathBuf,
    pub destination: PathBuf,
    pub format: crate::archive::ArchiveFormat,
}

impl ArchivePath {
    pub fn for_item(item: &FileItem) -> Option<Self> {
        if item.kind != FileKind::File {
            return None;
        }

        let recognized = crate::archive::recognize(&item.name)?;
        let path = item.uri.local_path().ok()?;
        let destination = path.parent()?.join(recognized.stem);

        Some(Self {
            path,
            destination,
            format: recognized.format,
        })
    }

    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

/// The selection as archives, or `None` if any of it is something else.
///
/// All or nothing: offering "Extract 3 Archives" over a selection of five
/// would quietly skip the two that are not, which is worse than not offering
/// it at all.
pub fn archive_paths(items: &[FileItem]) -> Option<Vec<ArchivePath>> {
    if items.is_empty() {
        return None;
    }

    items.iter().map(ArchivePath::for_item).collect()
}

// ---------------------------------------------------------------------------
// Folder monitoring
// ---------------------------------------------------------------------------

pub fn folder_monitor_event_affects_listing(event: gio::FileMonitorEvent) -> bool {
    matches!(
        event,
        gio::FileMonitorEvent::Changed
            | gio::FileMonitorEvent::ChangesDoneHint
            | gio::FileMonitorEvent::Deleted
            | gio::FileMonitorEvent::Created
            | gio::FileMonitorEvent::AttributeChanged
            | gio::FileMonitorEvent::Unmounted
            | gio::FileMonitorEvent::Moved
            | gio::FileMonitorEvent::Renamed
            | gio::FileMonitorEvent::MovedIn
            | gio::FileMonitorEvent::MovedOut
    )
}

impl dnd::DropOperation {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }

    pub fn past_tense(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Move => "Moved",
        }
    }
}

// ---------------------------------------------------------------------------
// Path primitives
// ---------------------------------------------------------------------------

pub fn copy_path_into(source: &Path, target_dir: &Path) -> std::io::Result<bool> {
    let Some(name) = source.file_name() else {
        return Ok(false);
    };

    if source.is_dir() && target_dir.starts_with(source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot copy a folder into itself",
        ));
    }

    let target = next_available_path(&target_dir.join(name));
    copy_path_to(source, &target).map(|()| true)
}

pub fn drop_target_is_selected(target_dir: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path == target_dir)
}

pub fn move_path_into(source: &Path, target_dir: &Path) -> std::io::Result<bool> {
    let Some(name) = source.file_name() else {
        return Ok(false);
    };

    if source.parent() == Some(target_dir) {
        return Ok(false);
    }

    if source.is_dir() && target_dir.starts_with(source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot move a folder into itself",
        ));
    }

    let target = next_available_path(&target_dir.join(name));
    match fs::rename(source, &target) {
        Ok(()) => Ok(true),
        Err(error) if is_cross_device_move(&error) => {
            copy_path_to(source, &target)?;
            remove_path(source)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

pub fn copy_path_to(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        copy_dir_recursive(source, target)
    } else {
        fs::copy(source, target).map(|_| ())
    }
}

pub fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

pub fn is_cross_device_move(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(18)
}

pub fn copy_dir_recursive(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let child_target = target.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &child_target)?;
        } else {
            fs::copy(&path, &child_target)?;
        }
    }
    Ok(())
}

pub fn new_folder_target(target_dir: &Path, name: &str) -> Result<PathBuf, &'static str> {
    if name.is_empty() {
        return Err("Name cannot be empty");
    }
    if name == "." || name == ".." || name.contains('/') {
        return Err("Name cannot contain path separators");
    }

    let target = target_dir.join(name);
    if target.exists() {
        return Err("A folder or file with that name already exists");
    }

    Ok(target)
}

pub fn next_available_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("copy");
    let extension = path.extension().and_then(|extension| extension.to_str());

    for index in 2.. {
        let name = match extension {
            Some(extension) => format!("{stem} {index}.{extension}"),
            None => format!("{stem} {index}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!()
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// What an operation did, so the caller can decide whether to refresh and what
/// to show. `message` is always the text to display; `changed` is whether the
/// filesystem moved under the caller's feet.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Outcome {
    pub message: String,
    pub changed: bool,
}

impl Outcome {
    fn quiet() -> Self {
        Self::default()
    }

    fn message(message: impl Into<String>, changed: bool) -> Self {
        Self {
            message: message.into(),
            changed,
        }
    }

    /// Nothing to say and nothing to refresh — callers skip both.
    pub fn is_silent(&self) -> bool {
        self.message.is_empty() && !self.changed
    }
}

pub fn delete_paths(paths: &[PathBuf]) -> Outcome {
    if paths.is_empty() {
        return Outcome::quiet();
    }

    let total = paths.len();
    let mut deleted = 0;
    let mut last_error = None;
    for path in paths {
        match remove_path(path) {
            Ok(()) => deleted += 1,
            Err(error) => {
                last_error = Some(format!("Failed to delete {}: {error}", path.display()))
            }
        }
    }

    // The error wins the label when both happened, exactly as before: a partial
    // failure is what the user needs to see, not the count that succeeded.
    match last_error {
        Some(error) => Outcome::message(error, deleted > 0),
        None => Outcome::message(format!("Deleted {deleted} of {total} item(s)"), deleted > 0),
    }
}

pub fn transfer_paths_into_target(
    operation: dnd::DropOperation,
    paths: &[PathBuf],
    target_dir: &Path,
) -> Outcome {
    if drop_target_is_selected(target_dir, paths) {
        return Outcome::message("Cannot drop onto a selected item", false);
    }

    let mut transferred = 0;
    let mut skipped = 0;
    let mut last_error = None;
    for path in paths {
        let result = match operation {
            dnd::DropOperation::Copy => copy_path_into(path, target_dir),
            dnd::DropOperation::Move => move_path_into(path, target_dir),
        };

        match result {
            Ok(true) => transferred += 1,
            Ok(false) => skipped += 1,
            Err(error) => {
                last_error = Some(format!(
                    "Failed to {} {}: {error}",
                    operation.verb(),
                    path.display()
                ))
            }
        }
    }

    if transferred > 0 {
        return Outcome::message(
            format!("{} {transferred} item(s)", operation.past_tense()),
            true,
        );
    }
    if let Some(error) = last_error {
        return Outcome::message(error, false);
    }
    if skipped > 0 {
        return Outcome::message("Already in that folder", false);
    }
    Outcome::quiet()
}

pub fn rename_path(source: &Path, new_name: &str) -> Outcome {
    if new_name.is_empty() {
        return Outcome::message("Name cannot be empty", false);
    }
    if new_name == "." || new_name == ".." || new_name.contains('/') {
        return Outcome::message("Name cannot contain path separators", false);
    }

    let Some(parent) = source.parent() else {
        return Outcome::message("Cannot rename this item", false);
    };
    let target = parent.join(new_name);
    if target == source {
        return Outcome::message("Name unchanged", false);
    }
    if target.exists() {
        return Outcome::message(format!("{} already exists", target.display()), false);
    }

    match fs::rename(source, &target) {
        Ok(()) => Outcome::message(format!("Renamed to {new_name}"), true),
        Err(error) => Outcome::message(
            format!("Failed to rename {}: {error}", source.display()),
            false,
        ),
    }
}

/// Creates `name` under `target_dir`. `Ok` carries the new folder so a caller
/// that wants to follow up on it — the desktop drops straight into an inline
/// rename — does not have to rebuild the path itself.
pub fn create_folder_named(target_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let target = new_folder_target(target_dir, name).map_err(str::to_string)?;

    match fs::create_dir(&target) {
        Ok(()) => Ok(target),
        Err(error) => Err(format!("Failed to create folder: {error}")),
    }
}

pub fn run_custom_action(
    action: &CustomActionConfig,
    targets: &[ActionTarget],
    current_dir: Option<&Path>,
) -> Outcome {
    if targets.is_empty() {
        return Outcome::message("No item selected", false);
    }

    let command_text = action.command.trim();
    let invocations = if action.run_on_each {
        targets
            .iter()
            .cloned()
            .map(|target| vec![target])
            .collect::<Vec<_>>()
    } else {
        vec![targets.to_vec()]
    };
    let target_count = invocations.iter().map(Vec::len).sum::<usize>();
    let mut launched = 0;
    let mut last_error = None;

    for invocation_targets in &invocations {
        let command_line = custom_actions::action_command_line(command_text, invocation_targets);
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&command_line)
            .arg("ioexplorer-action");
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }

        match command.spawn() {
            Ok(mut child) => {
                launched += 1;
                let label = action.label.clone();
                std::thread::spawn(move || {
                    if let Err(error) = child.wait() {
                        tracing::warn!(%error, action = %label, "custom action process failed");
                    }
                });
            }
            Err(error) => last_error = Some(error),
        }
    }

    let message = if launched == invocations.len() {
        format!("Running {} for {} item(s)", action.label, target_count)
    } else if launched > 0 {
        format!(
            "Running {} for {} command(s); {} failed",
            action.label,
            launched,
            invocations.len() - launched
        )
    } else if let Some(error) = last_error {
        format!("Failed to run {}: {error}", action.label)
    } else {
        format!("Failed to run {}", action.label)
    };

    Outcome::message(message, false)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::ErrorKind,
        path::{Path, PathBuf},
    };

    use crate::providers::{FileItem, FileKind, ProviderUri};
    use tempfile::tempdir;

    use super::{
        FileClipboardOperation, archive_paths, copy_path_into, drop_target_is_selected,
        file_clipboard_payload, file_uri_list_payload, folder_monitor_event_affects_listing,
        is_desktop_entry_file, move_path_into, new_folder_target,
    };

    fn entry(name: &str, kind: FileKind) -> FileItem {
        FileItem {
            uri: ProviderUri::local(format!("/tmp/{name}")),
            name: name.to_string(),
            display_name: None,
            icon: None,
            kind,
            size: Some(1),
            modified: None,
            created: None,
            hidden: false,
        }
    }

    #[test]
    fn an_archive_selection_carries_the_folder_it_unpacks_into() {
        let archives =
            archive_paths(&[entry("photos.tar.gz", FileKind::File)]).expect("an archive");

        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].path, PathBuf::from("/tmp/photos.tar.gz"));
        assert_eq!(archives[0].destination, PathBuf::from("/tmp/photos"));
    }

    #[test]
    fn a_mixed_selection_is_not_offered_extraction() {
        let mixed = archive_paths(&[
            entry("photos.zip", FileKind::File),
            entry("notes.txt", FileKind::File),
        ]);

        assert!(mixed.is_none());
    }

    #[test]
    fn a_folder_named_like_an_archive_is_not_extractable() {
        assert!(archive_paths(&[entry("photos.zip", FileKind::Directory)]).is_none());
    }

    #[test]
    fn an_empty_selection_is_not_offered_extraction() {
        assert!(archive_paths(&[]).is_none());
    }

    #[test]
    fn detects_desktop_entry_files_for_launching() {
        assert!(is_desktop_entry_file(&entry("app.desktop", FileKind::File)));
        assert!(is_desktop_entry_file(&entry("App.DESKTOP", FileKind::File)));
        assert!(!is_desktop_entry_file(&entry(
            "app.desktop",
            FileKind::Directory
        )));
        assert!(!is_desktop_entry_file(&entry("notes.txt", FileKind::File)));
    }

    #[test]
    fn file_clipboard_payload_marks_cut_operation() {
        let payload = file_clipboard_payload(
            &[PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.txt")],
            FileClipboardOperation::Cut,
        );

        assert_eq!(payload, "cut\nfile:///tmp/a.txt\nfile:///tmp/b.txt\n");
    }

    #[test]
    fn uri_list_payload_uses_crlf_separators() {
        let payload = file_uri_list_payload(&[PathBuf::from("/tmp/a.txt")]);

        assert_eq!(payload, "file:///tmp/a.txt\r\n");
    }

    #[test]
    fn builds_new_folder_target_from_requested_name() {
        let dir = tempdir().expect("temp dir");

        let target = new_folder_target(dir.path(), "Projects").expect("valid name");

        assert_eq!(target, dir.path().join("Projects"));
    }

    #[test]
    fn rejects_invalid_new_folder_names() {
        let dir = tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("Existing")).expect("create folder");

        assert!(new_folder_target(dir.path(), "").is_err());
        assert!(new_folder_target(dir.path(), ".").is_err());
        assert!(new_folder_target(dir.path(), "..").is_err());
        assert!(new_folder_target(dir.path(), "a/b").is_err());
        assert!(new_folder_target(dir.path(), "Existing").is_err());
    }

    #[test]
    fn folder_monitor_events_trigger_listing_updates() {
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::Created
        ));
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::Deleted
        ));
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::MovedIn
        ));
        assert!(!folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::PreUnmount
        ));
    }

    #[test]
    fn rejects_copying_folder_into_itself() {
        let dir = tempdir().expect("temp dir");
        let source = dir.path().join("outer");
        let nested = source.join("inner");
        fs::create_dir_all(&nested).expect("create nested folders");

        let error = copy_path_into(&source, &nested).expect_err("copying into itself must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn moves_file_into_target_directory() {
        let dir = tempdir().expect("temp dir");
        let source = dir.path().join("notes.txt");
        fs::write(&source, b"hello").expect("write source");
        let target_dir = dir.path().join("target");
        fs::create_dir(&target_dir).expect("create target");

        let moved = move_path_into(&source, &target_dir).expect("move succeeds");

        assert!(moved);
        assert!(!source.exists());
        assert!(target_dir.join("notes.txt").exists());
    }

    #[test]
    fn moving_to_same_parent_is_noop() {
        let dir = tempdir().expect("temp dir");
        let source = dir.path().join("notes.txt");
        fs::write(&source, b"hello").expect("write source");

        let moved = move_path_into(&source, dir.path()).expect("move succeeds");

        assert!(!moved);
        assert!(source.exists());
    }

    #[test]
    fn detects_selected_drop_target() {
        let target = Path::new("/tmp/folder");

        assert!(drop_target_is_selected(
            target,
            &[PathBuf::from("/tmp/folder")]
        ));
        assert!(!drop_target_is_selected(
            target,
            &[PathBuf::from("/tmp/other")]
        ));
    }
}
