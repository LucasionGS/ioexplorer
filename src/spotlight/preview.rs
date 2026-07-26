//! The large preview shown beside the result list.
//!
//! A `get_results` row may carry a preview: either text to display, or the URI
//! of an image. Only the row the user is pointing at — selected or hovered — is
//! ever shown, so images load lazily. Fetching all of them up front would pull
//! megabytes per keystroke for a search the user is still typing.
//!
//! Same concurrency idiom as the rest of the launcher: `std::thread` + `mpsc`,
//! drained from the GTK tick, with a monotonic generation counter standing in
//! for cancellation.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use crate::spotlight::image_cache;

/// Cache subdirectory. Separate from the icon cache: previews are far larger,
/// so clearing one should not throw away the other.
const CACHE_SUBDIR: &str = "spotlight-previews";
/// Cap on a downloaded preview. Generous next to an icon's 2 MiB — this is a
/// photograph — but still bounded.
const MAX_PREVIEW_BYTES: u64 = 16 * 1024 * 1024;

/// What a row's preview holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewKind {
    Text,
    Image,
    /// An icon-theme name or a serialized `gio::Icon`, drawn at preview size.
    ///
    /// Distinct from [`PreviewKind::Image`] because it resolves through the icon
    /// theme rather than the filesystem, and so needs neither a download nor the
    /// debounce that protects one.
    Icon,
}

impl PreviewKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            "icon" => Some(Self::Icon),
            _ => None,
        }
    }
}

/// A row's preview: text to display, or artwork to draw.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preview {
    pub kind: PreviewKind,
    pub content: String,
    /// Shown under the artwork. Ignored by [`PreviewKind::Text`], whose content
    /// is already the text.
    pub caption: Option<String>,
}

impl Preview {
    /// Artwork from the icon theme, with details written underneath.
    pub fn icon(content: impl Into<String>, caption: impl Into<String>) -> Self {
        Self {
            kind: PreviewKind::Icon,
            content: content.into(),
            caption: Some(caption.into()),
        }
    }
}

#[derive(Debug)]
pub enum PreviewEvent {
    Ready { generation: u64, path: PathBuf },
    Failed { generation: u64 },
}

impl PreviewEvent {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Ready { generation, .. } | Self::Failed { generation } => *generation,
        }
    }
}

/// The already-downloaded file for an image URL, if there is one.
///
/// Lets the main loop skip the placeholder entirely when the user comes back to
/// a row they have already looked at.
pub fn cached_image(url: &str) -> Option<PathBuf> {
    image_cache::cached(CACHE_SUBDIR, url)
}

pub struct PreviewLoader {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<PreviewEvent>,
    receiver: mpsc::Receiver<PreviewEvent>,
}

impl PreviewLoader {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Invalidates any in-flight load without starting a new one.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Starts a download, superseding any in-flight one.
    pub fn start(&self, url: String) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);
        thread::spawn(move || {
            let event = match image_cache::fetch(CACHE_SUBDIR, &url, MAX_PREVIEW_BYTES) {
                Some(path) => PreviewEvent::Ready { generation, path },
                None => PreviewEvent::Failed { generation },
            };
            // Checked after the download rather than before: the user has very
            // likely moved on, but the bytes are cached either way, so the only
            // thing worth suppressing is the stale UI update.
            if counter.load(Ordering::Relaxed) == generation {
                let _ = sender.send(event);
            }
        });

        generation
    }

    /// Collects events that are still current, discarding superseded ones.
    pub fn drain(&self) -> Vec<PreviewEvent> {
        let current = self.current_generation();
        let mut events = Vec::new();

        while let Ok(event) = self.receiver.try_recv() {
            if event.generation() == current {
                events.push(event);
            }
        }

        events
    }
}

impl Default for PreviewLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_is_parsed_case_insensitively() {
        assert_eq!(PreviewKind::parse("text"), Some(PreviewKind::Text));
        assert_eq!(PreviewKind::parse("Image"), Some(PreviewKind::Image));
        assert_eq!(PreviewKind::parse("  TEXT  "), Some(PreviewKind::Text));
        assert_eq!(PreviewKind::parse("icon"), Some(PreviewKind::Icon));
    }

    #[test]
    fn an_unknown_kind_is_rejected_rather_than_guessed() {
        assert_eq!(PreviewKind::parse("video"), None);
        assert_eq!(PreviewKind::parse(""), None);
    }

    #[test]
    fn a_superseded_load_never_reports() {
        let loader = PreviewLoader::new();
        loader.start("https://invalid.invalid/nope.png".to_string());
        loader.cancel();

        assert!(loader.drain().is_empty());
    }
}
