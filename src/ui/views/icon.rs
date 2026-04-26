use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    rc::Rc,
    time::SystemTime,
};

use gio::prelude::*;
use gtk::prelude::*;

use crate::{
    providers::{FileItem, FileKind},
    ui::{
        dnd,
        views::{
            EntryContextMenuHandler, EntrySelectionHandler, FileDragHandler, FolderDropHandler,
            image_for_item,
        },
    },
};

pub type ThumbnailCache = Rc<RefCell<ThumbnailCacheStore>>;

pub struct ThumbnailCacheStore {
    entries: HashMap<PathBuf, ThumbnailCacheEntry>,
    pending: HashSet<PathBuf>,
}

#[derive(Clone)]
pub struct ThumbnailCacheEntry {
    validation: ThumbnailValidation,
    texture: gtk::gdk::Texture,
    pixel_size: i32,
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

struct ThumbnailRender {
    texture: gtk::gdk::Texture,
    pixel_size: i32,
}

impl ThumbnailValidation {
    fn from_item(item: &FileItem) -> Self {
        Self {
            size: item.size,
            modified: item.modified,
        }
    }
}

pub fn new_thumbnail_cache() -> ThumbnailCache {
    Rc::new(RefCell::new(ThumbnailCacheStore {
        entries: HashMap::new(),
        pending: HashSet::new(),
    }))
}

#[derive(Clone)]
pub struct IconViewOptions {
    pub icon_size: i32,
    pub thumbnail_cache: ThumbnailCache,
}

pub fn populate(
    flow: &gtk::FlowBox,
    items: &[FileItem],
    options: IconViewOptions,
    folder_drop_handler: FolderDropHandler,
    file_drag_handler: FileDragHandler,
    selection_handler: EntrySelectionHandler,
    context_menu_handler: EntryContextMenuHandler,
) {
    while let Some(child) = flow.child_at_index(0) {
        flow.remove(&child);
    }

    for (index, item) in items.iter().enumerate() {
        flow.insert(
            &tile_for(
                index,
                item,
                options.clone(),
                folder_drop_handler.clone(),
                file_drag_handler.clone(),
                selection_handler.clone(),
                context_menu_handler.clone(),
            ),
            -1,
        );
    }
}

fn tile_for(
    index: usize,
    item: &FileItem,
    options: IconViewOptions,
    folder_drop_handler: FolderDropHandler,
    file_drag_handler: FileDragHandler,
    selection_handler: EntrySelectionHandler,
    context_menu_handler: EntryContextMenuHandler,
) -> gtk::Box {
    let icon_size = options.icon_size;
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .width_request(icon_size + 56)
        .css_classes(["file-tile"])
        .build();

    let icon = image_for_item(item, icon_size);
    icon.add_css_class("file-tile-icon");
    let label = gtk::Label::builder()
        .label(item.display_name())
        .justify(gtk::Justification::Center)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(2)
        .max_width_chars(18)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Center)
        .build();

    tile.append(&icon);
    tile.append(&label);

    apply_cached_thumbnail(item, &icon, Rc::clone(&options.thumbnail_cache));

    install_selection_click(&tile, index, selection_handler);
    install_context_menu_click(&tile, index, context_menu_handler);
    dnd::install_drag_source(&tile, move |_, _| file_drag_handler(index));

    if item.kind == FileKind::Directory
        && let Ok(target_dir) = item.uri.local_path()
    {
        dnd::install_drop_target(&tile, move |operation, paths| {
            folder_drop_handler(target_dir.clone(), operation, paths);
        });
    }

    tile
}

pub fn load_visible_thumbnails(
    flow: &gtk::FlowBox,
    scroll: &gtk::ScrolledWindow,
    items: &[FileItem],
    options: IconViewOptions,
) {
    let adjustment = scroll.vadjustment();
    let page_size = adjustment.page_size();
    let overscan = page_size.max(f64::from(options.icon_size) * 2.0);
    let visible_top = (adjustment.value() - overscan).max(0.0) as f32;
    let visible_bottom = (adjustment.value() + page_size + overscan) as f32;
    let thumbnail_width = options.icon_size + 56;

    let mut index = 0;
    while let Some(child) = flow.child_at_index(index) {
        if flow_child_intersects_y(&child, flow, visible_top, visible_bottom)
            && let Some(item) = items.get(index as usize)
            && let Some(icon) = flow_child_icon(&child)
        {
            request_thumbnail(
                item,
                &icon,
                options.icon_size,
                thumbnail_width,
                Rc::clone(&options.thumbnail_cache),
            );
        }
        index += 1;
    }
}

fn flow_child_intersects_y(
    child: &gtk::FlowBoxChild,
    flow: &gtk::FlowBox,
    visible_top: f32,
    visible_bottom: f32,
) -> bool {
    child.compute_bounds(flow).is_some_and(|bounds| {
        bounds.y() <= visible_bottom && bounds.y() + bounds.height() >= visible_top
    })
}

fn flow_child_icon(child: &gtk::FlowBoxChild) -> Option<gtk::Image> {
    child
        .child()?
        .downcast::<gtk::Box>()
        .ok()?
        .first_child()?
        .downcast::<gtk::Image>()
        .ok()
}

fn install_context_menu_click(
    tile: &gtk::Box,
    index: usize,
    context_menu_handler: EntryContextMenuHandler,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    let menu_tile = tile.clone();
    click.connect_pressed(move |_, _, x, y| {
        context_menu_handler(index, menu_tile.clone().upcast(), x, y);
    });
    tile.add_controller(click);
}

fn install_selection_click(
    tile: &gtk::Box,
    index: usize,
    selection_handler: EntrySelectionHandler,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.connect_released(move |click, n_press, _, _| {
        if n_press == 1 {
            selection_handler(index, click.current_event_state());
        }
    });
    tile.add_controller(click);
}

fn request_thumbnail(
    item: &FileItem,
    icon: &gtk::Image,
    icon_size: i32,
    thumbnail_width: i32,
    thumbnail_cache: ThumbnailCache,
) {
    let Some((path, validation, source)) = thumbnail_identity(item) else {
        return;
    };

    if let Some(cached) = cached_thumbnail(&thumbnail_cache, &path, validation) {
        apply_thumbnail(icon, &cached.texture, cached.pixel_size);
        return;
    }

    if !mark_thumbnail_pending(&thumbnail_cache, &path) {
        return;
    }

    let icon = icon.clone();
    glib::MainContext::default().spawn_local(async move {
        let render = match source {
            ThumbnailSource::Image => load_image_thumbnail(&path, icon_size, thumbnail_width).await,
            ThumbnailSource::Video => load_video_thumbnail(&path, icon_size, thumbnail_width).await,
        };

        let render = match render {
            Ok(render) => render,
            Err(error) => {
                tracing::debug!(%error, "failed to create thumbnail preview");
                unmark_thumbnail_pending(&thumbnail_cache, &path);
                return;
            }
        };
        let mut cache = thumbnail_cache.borrow_mut();
        cache.pending.remove(&path);
        cache.entries.insert(
            path,
            ThumbnailCacheEntry {
                validation,
                texture: render.texture.clone(),
                pixel_size: render.pixel_size,
            },
        );

        if icon.root().is_some() {
            apply_thumbnail(&icon, &render.texture, render.pixel_size);
        }
    });
}

async fn load_image_thumbnail(
    path: &Path,
    icon_size: i32,
    thumbnail_width: i32,
) -> Result<ThumbnailRender, String> {
    let file = gio::File::for_path(path);
    let stream = file
        .read_future(glib::Priority::LOW)
        .await
        .map_err(|error| format!("failed to open image preview stream: {error}"))?;

    let pixbuf =
        gdk_pixbuf::Pixbuf::from_stream_at_scale_future(&stream, thumbnail_width, icon_size, true)
            .await
            .map_err(|error| format!("failed to decode image preview: {error}"))?;

    Ok(render_from_pixbuf(&pixbuf, icon_size))
}

async fn load_video_thumbnail(
    path: &Path,
    icon_size: i32,
    thumbnail_width: i32,
) -> Result<ThumbnailRender, String> {
    let bytes = extract_video_thumbnail_bytes(path, icon_size, thumbnail_width).await?;
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let pixbuf =
        gdk_pixbuf::Pixbuf::from_stream_at_scale_future(&stream, thumbnail_width, icon_size, true)
            .await
            .map_err(|error| format!("failed to decode video thumbnail frame: {error}"))?;

    Ok(render_from_pixbuf(&pixbuf, icon_size))
}

async fn extract_video_thumbnail_bytes(
    path: &Path,
    icon_size: i32,
    thumbnail_width: i32,
) -> Result<glib::Bytes, String> {
    let mut last_error = None;
    for command in video_thumbnail_commands(path, icon_size, thumbnail_width) {
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

fn video_thumbnail_commands(
    path: &Path,
    icon_size: i32,
    thumbnail_width: i32,
) -> Vec<Vec<OsString>> {
    let path = path.as_os_str().to_os_string();
    let scale_filter =
        format!("scale={thumbnail_width}:{icon_size}:force_original_aspect_ratio=decrease");
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
                &thumbnail_width.to_string(),
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

fn apply_cached_thumbnail(item: &FileItem, icon: &gtk::Image, thumbnail_cache: ThumbnailCache) {
    let Some((path, validation, _)) = thumbnail_identity(item) else {
        return;
    };
    if let Some(cached) = cached_thumbnail(&thumbnail_cache, &path, validation) {
        apply_thumbnail(icon, &cached.texture, cached.pixel_size);
    }
}

fn cached_thumbnail(
    thumbnail_cache: &ThumbnailCache,
    path: &PathBuf,
    validation: ThumbnailValidation,
) -> Option<ThumbnailCacheEntry> {
    let mut cache = thumbnail_cache.borrow_mut();
    let cached = cache.entries.get(path)?;
    if cached.validation == validation {
        Some(cached.clone())
    } else {
        cache.entries.remove(path);
        None
    }
}

fn mark_thumbnail_pending(thumbnail_cache: &ThumbnailCache, path: &Path) -> bool {
    thumbnail_cache
        .borrow_mut()
        .pending
        .insert(path.to_path_buf())
}

fn unmark_thumbnail_pending(thumbnail_cache: &ThumbnailCache, path: &Path) {
    thumbnail_cache.borrow_mut().pending.remove(path);
}

fn apply_thumbnail(icon: &gtk::Image, texture: &gtk::gdk::Texture, pixel_size: i32) {
    icon.set_paintable(Some(texture));
    icon.set_pixel_size(pixel_size);
    icon.add_css_class("image-thumbnail");
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::providers::{FileItem, FileKind, ProviderUri};

    use super::{ThumbnailValidation, is_previewable_image, is_previewable_video};

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
    fn thumbnail_validation_tracks_size_and_modified_time() {
        let modified = UNIX_EPOCH + Duration::from_secs(42);
        let item = FileItem {
            uri: ProviderUri::local("/tmp/photo.jpg"),
            name: "photo.jpg".to_string(),
            display_name: None,
            icon: None,
            kind: FileKind::File,
            size: Some(123),
            modified: Some(modified),
            hidden: false,
        };

        assert_eq!(
            ThumbnailValidation::from_item(&item),
            ThumbnailValidation {
                size: Some(123),
                modified: Some(modified),
            }
        );
    }
}
