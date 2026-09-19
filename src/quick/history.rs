//! The clipboard history behind the Clipboard tab.
//!
//! Wayland hands the clipboard only to the focused window, so the menu cannot
//! follow it from one opening to the next. `ioexplorer-quick
//! --watch-clipboard` runs `wl-paste --watch`, which reads it through the
//! compositor's data-control protocol and starts `ioexplorer-quick
//! --record-clipboard` for every copy; that records the copy here. The menu
//! only reads what was recorded.
//!
//! Texts live in the index; images are files beside it. Pinned entries stay
//! until unpinned; the rest are dropped oldest first beyond the limit.

use std::{
    fs, io,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use directories::UserDirs;
use serde::{Deserialize, Serialize};

use super::gifs::ImageKind;
use crate::config::{write_atomic, write_bytes_atomic};

const INDEX_NAME: &str = "history.toml";
/// Written just before the menu puts a pick on the clipboard, so the copy is
/// not recorded: it is in the menu already.
const OWN_COPY_NAME: &str = ".own-copy";
/// How long an own-copy mark waits for the copy it announces.
const OWN_COPY_WINDOW: Duration = Duration::from_secs(5);
/// Longest text recorded, in bytes.
const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Largest image recorded.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Text types, best first. The X11 names are what XWayland clients offer.
const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
/// Image types, best first: an animation before a still of it.
const IMAGE_TYPES: [ImageKind; 4] = [
    ImageKind::Gif,
    ImageKind::Webp,
    ImageKind::Png,
    ImageKind::Jpeg,
];
/// Password managers mark their copies with this; they are never recorded.
const SECRET_HINT: &str = "x-kde-passwordManagerHint";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Entry {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The image's file name, in the history's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// When it was last copied, in seconds since the epoch.
    #[serde(default)]
    pub copied: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Index {
    #[serde(default, rename = "entry")]
    entries: Vec<Entry>,
}

pub struct History {
    directory: PathBuf,
    index: Index,
}

impl History {
    pub fn open(directory: PathBuf) -> Self {
        let index = match fs::read_to_string(directory.join(INDEX_NAME)) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot parse the clipboard history, starting empty");
                Index::default()
            }),
            Err(_) => Index::default(),
        };
        Self { directory, index }
    }

    pub fn open_default() -> Self {
        Self::open(default_directory())
    }

    pub fn index_path(&self) -> PathBuf {
        self.directory.join(INDEX_NAME)
    }

    pub fn image_path(&self, entry: &Entry) -> Option<PathBuf> {
        entry.image.as_ref().map(|name| self.directory.join(name))
    }

    /// Pinned first, then the most recently copied.
    pub fn list(&self) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self
            .index
            .entries
            .iter()
            .filter(|entry| {
                entry.text.is_some() || self.image_path(entry).is_some_and(|path| path.is_file())
            })
            .cloned()
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse((entry.pinned, entry.copied, entry.id)));
        entries
    }

    /// Texts holding every word of `query`, ignoring case.
    pub fn search(&self, query: &str) -> Vec<Entry> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.list()
            .into_iter()
            .filter(|entry| {
                entry.text.as_deref().is_some_and(|text| {
                    let text = text.to_lowercase();
                    words.iter().all(|word| text.contains(word.as_str()))
                })
            })
            .collect()
    }

    /// Records a copied text, or moves the same text back to the top.
    pub fn record_text(&mut self, text: &str, limit: usize) -> io::Result<bool> {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Ok(false);
        }
        let now = now_secs();
        if let Some(entry) = self
            .index
            .entries
            .iter_mut()
            .find(|entry| entry.text.as_deref() == Some(text))
        {
            entry.copied = now;
        } else {
            let id = self.next_id();
            self.index.entries.push(Entry {
                id,
                text: Some(text.to_string()),
                copied: now,
                ..Entry::default()
            });
        }
        self.trim(limit);
        self.write()?;
        Ok(true)
    }

    /// Records a copied image, or moves the same image back to the top.
    pub fn record_image(
        &mut self,
        bytes: &[u8],
        kind: ImageKind,
        limit: usize,
    ) -> io::Result<bool> {
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Ok(false);
        }
        let now = now_secs();
        let same = self.index.entries.iter().position(|entry| {
            self.image_path(entry).is_some_and(|path| {
                fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64)
                    && fs::read(&path).is_ok_and(|contents| contents == bytes)
            })
        });
        if let Some(position) = same {
            self.index.entries[position].copied = now;
        } else {
            fs::create_dir_all(&self.directory)?;
            let id = self.next_id();
            let name = format!("{id}.{}", kind.extension());
            write_bytes_atomic(&self.directory.join(&name), bytes)?;
            self.index.entries.push(Entry {
                id,
                image: Some(name),
                copied: now,
                ..Entry::default()
            });
        }
        self.trim(limit);
        self.write()?;
        Ok(true)
    }

    /// Marks an entry as just used, which brings it to the top.
    pub fn touch(&mut self, id: u64) -> io::Result<()> {
        if let Some(entry) = self.find(id) {
            entry.copied = now_secs();
            self.write()?;
        }
        Ok(())
    }

    /// Pins an entry, or unpins it. Returns whether it is pinned now.
    pub fn toggle_pin(&mut self, id: u64) -> io::Result<bool> {
        let Some(entry) = self.find(id) else {
            return Ok(false);
        };
        entry.pinned = !entry.pinned;
        let pinned = entry.pinned;
        self.write()?;
        Ok(pinned)
    }

    pub fn remove(&mut self, id: u64) -> io::Result<()> {
        if let Some(position) = self.index.entries.iter().position(|entry| entry.id == id) {
            let entry = self.index.entries.remove(position);
            self.delete_image(&entry);
            self.write()?;
        }
        Ok(())
    }

    fn find(&mut self, id: u64) -> Option<&mut Entry> {
        self.index.entries.iter_mut().find(|entry| entry.id == id)
    }

    fn next_id(&self) -> u64 {
        self.index
            .entries
            .iter()
            .map(|entry| entry.id)
            .max()
            .map_or(1, |id| id + 1)
    }

    /// Keeps at most `limit` unpinned entries, the most recently copied.
    fn trim(&mut self, limit: usize) {
        let mut unpinned: Vec<(u64, u64)> = self
            .index
            .entries
            .iter()
            .filter(|entry| !entry.pinned)
            .map(|entry| (entry.copied, entry.id))
            .collect();
        if unpinned.len() <= limit {
            return;
        }
        unpinned.sort_by(|a, b| b.cmp(a));
        let dropped: Vec<u64> = unpinned[limit..].iter().map(|(_, id)| *id).collect();
        let (gone, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.index.entries)
            .into_iter()
            .partition(|entry| dropped.contains(&entry.id));
        self.index.entries = kept;
        for entry in &gone {
            self.delete_image(entry);
        }
    }

    fn delete_image(&self, entry: &Entry) {
        if let Some(path) = self.image_path(entry) {
            let _ = fs::remove_file(path);
        }
    }

    fn write(&self) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let contents = toml::to_string_pretty(&self.index).map_err(io::Error::other)?;
        write_atomic(&self.directory.join(INDEX_NAME), &contents)
    }
}

pub fn default_directory() -> PathBuf {
    UserDirs::new()
        .map(|dirs| dirs.home_dir().join(".local/state/ioexplorer/clipboard"))
        .unwrap_or_else(|| PathBuf::from(".ioexplorer-clipboard"))
}

/// Announces that the menu is about to put a pick on the clipboard, so the
/// watcher leaves that copy out of the history.
pub fn mark_own_copy() {
    let directory = default_directory();
    let result = fs::create_dir_all(&directory)
        .and_then(|()| fs::write(directory.join(OWN_COPY_NAME), now_millis().to_string()));
    if let Err(error) = result {
        tracing::debug!(%error, "cannot mark the copy as the menu's own");
    }
}

/// Whether the copy being recorded was announced by [`mark_own_copy`]. Takes
/// the mark, so it covers one copy only.
fn take_own_copy(directory: &Path) -> bool {
    let path = directory.join(OWN_COPY_NAME);
    let Ok(contents) = fs::read_to_string(&path) else {
        return false;
    };
    let _ = fs::remove_file(&path);
    contents
        .trim()
        .parse::<u128>()
        .is_ok_and(|marked| now_millis().saturating_sub(marked) < OWN_COPY_WINDOW.as_millis())
}

/// Whether a `--watch-clipboard` is recording, judged by its `wl-paste`.
pub fn watcher_running() -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return true;
    };
    processes.flatten().any(|process| {
        fs::read(process.path().join("cmdline")).is_ok_and(|cmdline| {
            let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
            args.iter().any(|arg| *arg == b"--record-clipboard")
                && args.iter().any(|arg| arg.ends_with(b"ioexplorer-quick"))
        })
    })
}

/// `--watch-clipboard`: becomes `wl-paste --watch`, recording every copy
/// until the session ends. Only returns when that cannot start, with why.
pub fn watch() -> String {
    use std::os::unix::process::CommandExt;

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return format!("cannot find this program: {error}"),
    };
    // Its output is never used, and must not be a file: wl-paste picks the
    // type to ask for from a file's name, and would then call every copy
    // that is not text empty.
    let error = Command::new("wl-paste")
        .arg("--watch")
        .arg(exe)
        .arg("--record-clipboard")
        .stdout(Stdio::null())
        .exec();
    format!("cannot run wl-paste --watch: {error}")
}

/// `--record-clipboard`: run by `wl-paste --watch` for one copy.
pub fn record(limit: usize) -> Result<(), String> {
    // What wl-paste chose to hand over is not necessarily what is kept; it
    // is drained so wl-paste is not left writing into a closed pipe.
    let _ = io::stdin().lock().read_to_end(&mut Vec::new());

    // `sensitive` is a password manager's copy, `clear` an emptied clipboard.
    // `nil` only means nothing of the type wl-paste chose, which is not what
    // decides here.
    if std::env::var("CLIPBOARD_STATE").is_ok_and(|state| state == "sensitive" || state == "clear")
    {
        return Ok(());
    }
    let directory = default_directory();
    if take_own_copy(&directory) {
        return Ok(());
    }

    let types = paste(&["--list-types"])?;
    let types: Vec<&str> = std::str::from_utf8(&types)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .collect();
    if types.contains(&SECRET_HINT) {
        return Ok(());
    }

    let mut history = History::open(directory);
    if let Some(text_type) = TEXT_TYPES.iter().find(|mime| types.contains(mime)) {
        let text = paste(&["--no-newline", "--type", text_type])?;
        let text = String::from_utf8_lossy(&text);
        history
            .record_text(&text, limit)
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    for kind in IMAGE_TYPES {
        if !types.contains(&kind.mime_type()) {
            continue;
        }
        let bytes = paste(&["--type", kind.mime_type()])?;
        if let Some(kind) = ImageKind::sniff(&bytes) {
            history
                .record_image(&bytes, kind, limit)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
    }
    Ok(())
}

fn paste(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("wl-paste")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run wl-paste: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!("wl-paste {} failed", args.join(" ")))
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        86_400..172_800 => "yesterday".to_string(),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage";

    fn texts(history: &History) -> Vec<String> {
        history
            .list()
            .into_iter()
            .map(|entry| entry.text.or(entry.image).unwrap_or_default())
            .collect()
    }

    #[test]
    fn copies_are_recorded_newest_first_without_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path().to_path_buf());
        assert!(history.record_text("one", 10).unwrap());
        assert!(history.record_text("two", 10).unwrap());
        assert!(!history.record_text("  \n", 10).unwrap());
        // Same second: the id breaks the tie, so bump by hand.
        history.index.entries[0].copied -= 10;
        history.index.entries[1].copied -= 5;
        history.record_text("one", 10).unwrap();
        assert_eq!(
            texts(&History::open(dir.path().to_path_buf())),
            ["one", "two"]
        );
    }

    #[test]
    fn images_are_files_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path().to_path_buf());
        history.record_image(GIF, ImageKind::Gif, 10).unwrap();
        history.record_image(GIF, ImageKind::Gif, 10).unwrap();
        history.record_image(PNG, ImageKind::Png, 10).unwrap();
        let listed = History::open(dir.path().to_path_buf()).list();
        assert_eq!(listed.len(), 2);
        let gif = listed
            .iter()
            .find(|entry| entry.image.as_deref() == Some("1.gif"));
        assert_eq!(fs::read(dir.path().join("1.gif")).unwrap(), GIF);
        assert!(gif.is_some());
    }

    #[test]
    fn the_limit_spares_pinned_entries_and_removes_their_images() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path().to_path_buf());
        history.record_image(GIF, ImageKind::Gif, 10).unwrap();
        history.record_text("pinned", 10).unwrap();
        history.record_text("a", 10).unwrap();
        history.record_text("b", 10).unwrap();
        // Oldest to newest: the GIF, "pinned", "a", "b".
        for (entry, copied) in history.index.entries.iter_mut().zip([1, 2, 3, 4]) {
            entry.copied = copied;
        }
        assert!(history.toggle_pin(2).unwrap());

        history.trim(1);
        history.write().unwrap();
        assert_eq!(
            texts(&History::open(dir.path().to_path_buf())),
            ["pinned", "b"]
        );
        assert!(
            !dir.path().join("1.gif").exists(),
            "a dropped image is deleted"
        );
    }

    #[test]
    fn search_touch_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path().to_path_buf());
        history.record_text("Hello World", 10).unwrap();
        history.record_text("goodbye", 10).unwrap();
        history.record_image(PNG, ImageKind::Png, 10).unwrap();
        assert_eq!(history.search("world hel").len(), 1);
        assert!(history.search("xyz").is_empty());

        let hello = history.search("hello")[0].id;
        for entry in &mut history.index.entries {
            entry.copied = 100;
        }
        history.touch(hello).unwrap();
        assert_eq!(history.list()[0].id, hello);

        history.remove(hello).unwrap();
        assert!(history.search("hello").is_empty());
    }

    #[test]
    fn an_own_copy_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!take_own_copy(dir.path()));
        fs::write(dir.path().join(OWN_COPY_NAME), now_millis().to_string()).unwrap();
        assert!(take_own_copy(dir.path()));
        assert!(!take_own_copy(dir.path()));
        fs::write(dir.path().join(OWN_COPY_NAME), "1").unwrap();
        assert!(!take_own_copy(dir.path()), "a stale mark is ignored");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(1000, 990), "just now");
        assert_eq!(ago(1000, 1000 - 5 * 60), "5 min ago");
        assert_eq!(ago(100_000, 100_000 - 3 * 3600), "3 h ago");
        assert_eq!(ago(200_000, 200_000 - 90_000), "yesterday");
        assert_eq!(ago(1_000_000, 1_000_000 - 3 * 86_400), "3 days ago");
    }
}
