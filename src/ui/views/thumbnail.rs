//! Shared image and video previews.
//!
//! Three places want the same picture of a file — the icon grid's tiles, the
//! list view's row icons, and the details panel — at three different sizes.
//! They all queue through the one cache and the one worker here, so a folder of
//! photos is decoded once per size rather than once per view.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    ffi::{OsStr, OsString},
    mem,
    path::{Path, PathBuf},
    rc::Rc,
    time::SystemTime,
};

use gio::prelude::*;
use gtk::prelude::*;

use crate::providers::{FileItem, FileKind};

pub type ThumbnailCache = Rc<RefCell<ThumbnailCacheStore>>;

/// How large a preview a view wants: `icon_size` bounds the height, and
/// `thumbnail_width` bounds the width, so a panorama stays a panorama instead
/// of being squared off.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ThumbnailSpec {
    pub icon_size: i32,
    pub thumbnail_width: i32,
}

impl ThumbnailSpec {
    /// A preview no wider than it is tall, for a row that must not grow.
    pub fn square(icon_size: i32) -> Self {
        Self {
            icon_size,
            thumbnail_width: icon_size,
        }
    }
}

/// What a cached render belongs to. The size is part of the identity: the same
/// photo is rendered small for a list row and large for the details panel, and
/// one must not evict the other.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ThumbnailKey {
    path: PathBuf,
    spec: ThumbnailSpec,
}

pub struct ThumbnailCacheStore {
    entries: HashMap<ThumbnailKey, ThumbnailCacheEntry>,
    pending: HashSet<ThumbnailKey>,
    queue: VecDeque<ThumbnailRequest>,
    /// Whether a worker is draining `queue`. Only ever one.
    running: bool,
    /// Bumped whenever a view is repopulated, so work queued for a listing the
    /// user has already navigated away from can be dropped on sight.
    generation: u64,
}

/// A thumbnail waiting its turn.
struct ThumbnailRequest {
    key: ThumbnailKey,
    validation: ThumbnailValidation,
    source: ThumbnailSource,
    target: ThumbnailTarget,
    generation: u64,
}

#[derive(Clone)]
struct ThumbnailCacheEntry {
    validation: ThumbnailValidation,
    render: ThumbnailRender,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ThumbnailValidation {
    size: Option<u64>,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy)]
enum ThumbnailSource {
    Image,
    Video,
}

#[derive(Clone)]
struct ThumbnailRender {
    texture: gtk::gdk::Texture,
    pixel_size: i32,
}

/// The widget a finished render belongs in.
///
/// A [`gtk::Image`] is sized by `pixel_size`, which suits an icon standing in a
/// fixed slot. The details panel instead wants the preview to take the shape of
/// the picture itself, which is what [`gtk::Picture`] does.
#[derive(Clone)]
pub enum ThumbnailTarget {
    Icon(gtk::Image),
    Picture(gtk::Picture),
}

impl ThumbnailTarget {
    pub fn icon(image: &gtk::Image) -> Self {
        Self::Icon(image.clone())
    }

    pub fn picture(picture: &gtk::Picture) -> Self {
        Self::Picture(picture.clone())
    }

    /// Whether the widget is still in a window. A tile scrolled out of a
    /// listing that has since been replaced is not worth decoding for.
    fn is_attached(&self) -> bool {
        match self {
            Self::Icon(image) => image.root().is_some(),
            Self::Picture(picture) => picture.root().is_some(),
        }
    }

    fn apply(&self, render: &ThumbnailRender) {
        match self {
            Self::Icon(image) => {
                image.set_paintable(Some(&render.texture));
                image.set_pixel_size(render.pixel_size);
                image.add_css_class("image-thumbnail");
            }
            Self::Picture(picture) => {
                picture.set_paintable(Some(&render.texture));
            }
        }
    }
}

impl ThumbnailValidation {
    fn from_item(item: &FileItem) -> Self {
        Self {
            size: item.size,
            modified: item.modified,
        }
    }
}

pub fn new_cache() -> ThumbnailCache {
    Rc::new(RefCell::new(ThumbnailCacheStore {
        entries: HashMap::new(),
        pending: HashSet::new(),
        queue: VecDeque::new(),
        running: false,
        generation: 0,
    }))
}

/// Drops every render whose height is not in `keep`.
///
/// Zooming the icon grid walks through a dozen sizes, and holding a texture for
/// each of them at every file in the folder adds up. The sizes still on screen
/// — the list's and the details panel's among them — are kept, so only the
/// abandoned zoom levels are paid back.
pub fn retain_sizes(thumbnail_cache: &ThumbnailCache, keep: &[i32]) {
    let mut cache = thumbnail_cache.borrow_mut();
    cache
        .entries
        .retain(|key, _| keep.contains(&key.spec.icon_size));
    discard_queued_locked(&mut cache);
}

/// Drops thumbnails queued for a listing that is being replaced.
///
/// Without this, opening a folder of photos and immediately leaving it would
/// still render every one of them before the new folder got a turn.
pub fn discard_queued(thumbnail_cache: &ThumbnailCache) {
    discard_queued_locked(&mut thumbnail_cache.borrow_mut());
}

fn discard_queued_locked(cache: &mut ThumbnailCacheStore) {
    cache.generation = cache.generation.wrapping_add(1);
    let queued = mem::take(&mut cache.queue);
    for request in queued {
        cache.pending.remove(&request.key);
    }
}

/// Whether this entry has a preview worth asking for at all.
pub fn has_preview(item: &FileItem) -> bool {
    thumbnail_identity(item).is_some()
}

/// Fills `target` from the cache if the render is already there.
///
/// Used while building a row or tile, so a thumbnail that has been seen before
/// is present in the first frame rather than appearing a moment later.
pub fn apply_cached(
    item: &FileItem,
    target: &ThumbnailTarget,
    spec: ThumbnailSpec,
    thumbnail_cache: &ThumbnailCache,
) {
    let Some((path, validation, _)) = thumbnail_identity(item) else {
        return;
    };
    let key = ThumbnailKey { path, spec };
    if let Some(cached) = cached_render(thumbnail_cache, &key, validation) {
        target.apply(&cached);
    }
}

/// Queues a render for `item`, unless one is cached or already queued.
pub fn request(
    item: &FileItem,
    target: &ThumbnailTarget,
    spec: ThumbnailSpec,
    thumbnail_cache: &ThumbnailCache,
) {
    let Some((path, validation, source)) = thumbnail_identity(item) else {
        return;
    };
    let key = ThumbnailKey { path, spec };

    if let Some(cached) = cached_render(thumbnail_cache, &key, validation) {
        target.apply(&cached);
        return;
    }

    if !thumbnail_cache.borrow_mut().pending.insert(key.clone()) {
        return;
    }

    {
        let mut cache = thumbnail_cache.borrow_mut();
        let generation = cache.generation;
        cache.queue.push_back(ThumbnailRequest {
            key,
            validation,
            source,
            target: target.clone(),
            generation,
        });
    }

    start_worker(thumbnail_cache);
}

/// Renders queued thumbnails one at a time.
///
/// Every visible tile used to start its own decode the moment the folder
/// appeared, so opening a directory of photos kicked off dozens of concurrent
/// decodes — and for videos, dozens of concurrent ffmpeg processes — which is
/// what made the window lock up. A single worker returns to the main loop
/// between items, so the view stays interactive while previews fill in.
fn start_worker(thumbnail_cache: &ThumbnailCache) {
    {
        let mut cache = thumbnail_cache.borrow_mut();
        if cache.running {
            return;
        }
        cache.running = true;
    }

    let thumbnail_cache = Rc::clone(thumbnail_cache);
    glib::MainContext::default().spawn_local(async move {
        while let Some(request) = next_request(&thumbnail_cache) {
            render_queued(&thumbnail_cache, request).await;
        }

        thumbnail_cache.borrow_mut().running = false;
    });
}

fn next_request(thumbnail_cache: &ThumbnailCache) -> Option<ThumbnailRequest> {
    let mut cache = thumbnail_cache.borrow_mut();
    while let Some(request) = cache.queue.pop_front() {
        // Stale listing, or a widget that has since left the tree: the decode
        // would be thrown away, so skip straight past it.
        if request.generation != cache.generation || !request.target.is_attached() {
            cache.pending.remove(&request.key);
            continue;
        }
        return Some(request);
    }

    None
}

async fn render_queued(thumbnail_cache: &ThumbnailCache, request: ThumbnailRequest) {
    let spec = request.key.spec;
    let render = match request.source {
        ThumbnailSource::Image => load_image(&request.key.path, spec).await,
        ThumbnailSource::Video => load_video(&request.key.path, spec).await,
    };

    let render = match render {
        Ok(render) => render,
        Err(error) => {
            tracing::debug!(%error, "failed to create thumbnail preview");
            thumbnail_cache.borrow_mut().pending.remove(&request.key);
            return;
        }
    };

    {
        let mut cache = thumbnail_cache.borrow_mut();
        cache.pending.remove(&request.key);
        cache.entries.insert(
            request.key.clone(),
            ThumbnailCacheEntry {
                validation: request.validation,
                render: render.clone(),
            },
        );
    }

    // Cached above regardless, so a tile that scrolled away still gets its
    // thumbnail for free the next time it comes back into view.
    if request.target.is_attached() {
        request.target.apply(&render);
    }
}

async fn load_image(path: &Path, spec: ThumbnailSpec) -> Result<ThumbnailRender, String> {
    let file = gio::File::for_path(path);
    let stream = file
        .read_future(glib::Priority::LOW)
        .await
        .map_err(|error| format!("failed to open image preview stream: {error}"))?;

    let pixbuf = gdk_pixbuf::Pixbuf::from_stream_at_scale_future(
        &stream,
        spec.thumbnail_width,
        spec.icon_size,
        true,
    )
    .await
    .map_err(|error| format!("failed to decode image preview: {error}"))?;

    Ok(render_from_pixbuf(&pixbuf, spec.icon_size))
}

async fn load_video(path: &Path, spec: ThumbnailSpec) -> Result<ThumbnailRender, String> {
    let bytes = extract_video_frame(path, spec).await?;
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let pixbuf = gdk_pixbuf::Pixbuf::from_stream_at_scale_future(
        &stream,
        spec.thumbnail_width,
        spec.icon_size,
        true,
    )
    .await
    .map_err(|error| format!("failed to decode video thumbnail frame: {error}"))?;

    Ok(render_from_pixbuf(&pixbuf, spec.icon_size))
}

async fn extract_video_frame(path: &Path, spec: ThumbnailSpec) -> Result<glib::Bytes, String> {
    let mut last_error = None;
    for command in video_thumbnail_commands(path, spec) {
        match run_video_thumbnail_command(command).await {
            Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
            Ok(_) => last_error = Some("video thumbnailer produced no output".to_string()),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| "no video thumbnailer command available".to_string()))
}

async fn run_video_thumbnail_command(command: Vec<OsString>) -> Result<glib::Bytes, String> {
    let argv = command
        .iter()
        .map(|argument| argument.as_os_str())
        .collect::<Vec<&OsStr>>();
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|error| format!("failed to start {}: {error}", command_name(&command)))?;

    let (stdout, stderr) = process
        .communicate_future(None)
        .await
        .map_err(|error| format!("{} failed: {error}", command_name(&command)))?;

    if !process.is_successful() {
        return Err(format!(
            "{} exited with status {}{}",
            command_name(&command),
            process.status(),
            stderr_summary(stderr.as_ref())
        ));
    }

    stdout.ok_or_else(|| format!("{} produced no output", command_name(&command)))
}

fn video_thumbnail_commands(path: &Path, spec: ThumbnailSpec) -> Vec<Vec<OsString>> {
    let path = path.as_os_str().to_os_string();
    let scale_filter = format!(
        "scale={}:{}:force_original_aspect_ratio=decrease",
        spec.thumbnail_width, spec.icon_size
    );
    let mut commands = Vec::new();

    for seek_time in ["00:00:01", "00:00:00"] {
        commands.push(
            os_strings([
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                seek_time,
                "-i",
            ])
            .into_iter()
            .chain([path.clone()])
            .chain(os_strings([
                "-an",
                "-sn",
                "-dn",
                "-frames:v",
                "1",
                "-vf",
                &scale_filter,
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "pipe:1",
            ]))
            .collect(),
        );
    }

    commands.push(
        os_strings(["ffmpegthumbnailer", "-i"])
            .into_iter()
            .chain([path])
            .chain(os_strings([
                "-o",
                "-",
                "-s",
                &spec.thumbnail_width.to_string(),
                "-q",
                "8",
                "-t",
                "10%",
            ]))
            .collect(),
    );

    commands
}

fn os_strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

fn command_name(command: &[OsString]) -> String {
    command
        .first()
        .map(|argument| argument.to_string_lossy().into_owned())
        .unwrap_or_else(|| "thumbnail command".to_string())
}

fn stderr_summary(stderr: Option<&glib::Bytes>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let summary = String::from_utf8_lossy(stderr.as_ref());
    let summary = summary.trim();
    if summary.is_empty() {
        String::new()
    } else {
        format!(": {summary}")
    }
}

fn render_from_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf, icon_size: i32) -> ThumbnailRender {
    ThumbnailRender {
        texture: gtk::gdk::Texture::for_pixbuf(pixbuf),
        pixel_size: pixbuf.width().max(icon_size),
    }
}

fn thumbnail_identity(item: &FileItem) -> Option<(PathBuf, ThumbnailValidation, ThumbnailSource)> {
    if item.kind != FileKind::File {
        return None;
    }

    let source = if is_previewable_image(&item.name) {
        ThumbnailSource::Image
    } else if is_previewable_video(&item.name) {
        ThumbnailSource::Video
    } else {
        return None;
    };

    item.uri
        .local_path()
        .ok()
        .map(|path| (path, ThumbnailValidation::from_item(item), source))
}

fn cached_render(
    thumbnail_cache: &ThumbnailCache,
    key: &ThumbnailKey,
    validation: ThumbnailValidation,
) -> Option<ThumbnailRender> {
    let mut cache = thumbnail_cache.borrow_mut();
    let cached = cache.entries.get(key)?;
    if cached.validation == validation {
        Some(cached.render.clone())
    } else {
        // The file changed under us; the render on hand is of the old one.
        cache.entries.remove(key);
        None
    }
}

pub fn is_previewable_image(name: &str) -> bool {
    let Some(extension) = name.rsplit_once('.').map(|(_, extension)| extension) else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "avif"
            | "bmp"
            | "gif"
            | "heic"
            | "heif"
            | "jpeg"
            | "jpg"
            | "png"
            | "svg"
            | "tif"
            | "tiff"
            | "webp"
    )
}

pub fn is_previewable_video(name: &str) -> bool {
    let Some(extension) = name.rsplit_once('.').map(|(_, extension)| extension) else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "3gp"
            | "avi"
            | "flv"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "ogm"
            | "ogv"
            | "webm"
            | "wmv"
    )
}

pub fn is_previewable_audio(name: &str) -> bool {
    let Some(extension) = name.rsplit_once('.').map(|(_, extension)| extension) else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "aac" | "aiff" | "alac" | "flac" | "m4a" | "mp3" | "oga" | "ogg" | "opus" | "wav" | "wma"
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::providers::ProviderUri;

    use super::*;

    fn image_item() -> FileItem {
        FileItem {
            uri: ProviderUri::local("/tmp/photo.jpg"),
            name: "photo.jpg".to_string(),
            display_name: None,
            icon: None,
            kind: FileKind::File,
            size: Some(123),
            modified: Some(UNIX_EPOCH + Duration::from_secs(42)),
            created: Some(UNIX_EPOCH + Duration::from_secs(42)),
            hidden: false,
        }
    }

    #[test]
    fn detects_common_image_extensions() {
        assert!(is_previewable_image("photo.JPG"));
        assert!(is_previewable_image("screenshot.png"));
        assert!(is_previewable_image("wallpaper.webp"));
        assert!(!is_previewable_image("clip.mp4"));
        assert!(!is_previewable_image("README"));
    }

    #[test]
    fn detects_common_video_extensions() {
        assert!(is_previewable_video("clip.MP4"));
        assert!(is_previewable_video("movie.mkv"));
        assert!(is_previewable_video("capture.webm"));
        assert!(!is_previewable_video("photo.jpg"));
        assert!(!is_previewable_video("README"));
    }

    #[test]
    fn detects_common_audio_extensions() {
        assert!(is_previewable_audio("track.MP3"));
        assert!(is_previewable_audio("voice.ogg"));
        assert!(is_previewable_audio("podcast.opus"));
        assert!(!is_previewable_audio("clip.mp4"));
        assert!(!is_previewable_audio("README"));
    }

    #[test]
    fn thumbnail_validation_tracks_size_and_modified_time() {
        let item = image_item();

        assert_eq!(
            ThumbnailValidation::from_item(&item),
            ThumbnailValidation {
                size: item.size,
                modified: item.modified,
            }
        );
    }

    #[test]
    fn only_images_and_videos_have_previews() {
        assert!(has_preview(&image_item()));

        let mut document = image_item();
        document.name = "notes.txt".to_string();
        assert!(!has_preview(&document));

        let mut folder = image_item();
        folder.kind = FileKind::Directory;
        assert!(!has_preview(&folder));
    }

    /// The list's small render and the details panel's large one are held
    /// separately, or the two views would evict each other on every selection.
    #[test]
    fn the_same_file_is_cached_once_per_size() {
        let path = PathBuf::from("/tmp/photo.jpg");
        let small = ThumbnailKey {
            path: path.clone(),
            spec: ThumbnailSpec::square(24),
        };
        let large = ThumbnailKey {
            path,
            spec: ThumbnailSpec {
                icon_size: 220,
                thumbnail_width: 256,
            },
        };

        assert_ne!(small, large);
    }

    #[test]
    fn a_square_spec_never_grows_wider_than_tall() {
        let spec = ThumbnailSpec::square(24);

        assert_eq!(spec.icon_size, 24);
        assert_eq!(spec.thumbnail_width, 24);
    }

    #[test]
    fn retaining_sizes_keeps_only_the_listed_ones() {
        let cache = new_cache();
        let render = ThumbnailValidation::from_item(&image_item());
        for icon_size in [24, 96, 220] {
            cache.borrow_mut().entries.insert(
                ThumbnailKey {
                    path: PathBuf::from("/tmp/photo.jpg"),
                    spec: ThumbnailSpec::square(icon_size),
                },
                ThumbnailCacheEntry {
                    validation: render,
                    render: ThumbnailRender {
                        // No display in tests, so a texture cannot be built;
                        // the retain path never looks at one.
                        texture: gtk::gdk::MemoryTexture::new(
                            1,
                            1,
                            gtk::gdk::MemoryFormat::R8g8b8,
                            &glib::Bytes::from_owned(vec![0, 0, 0]),
                            3,
                        )
                        .upcast(),
                        pixel_size: icon_size,
                    },
                },
            );
        }

        retain_sizes(&cache, &[24, 220]);

        let sizes: HashSet<i32> = cache
            .borrow()
            .entries
            .keys()
            .map(|key| key.spec.icon_size)
            .collect();
        assert_eq!(sizes, HashSet::from([24, 220]));
    }
}
