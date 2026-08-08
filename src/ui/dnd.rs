use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gtk::{gdk, prelude::*};

thread_local! {
    static INTERNAL_DRAG_PATHS: RefCell<Option<Vec<PathBuf>>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DropOperation {
    Copy,
    Move,
}

pub enum DropPayload {
    LocalPaths {
        operation: DropOperation,
        paths: Vec<PathBuf>,
    },
    /// Raw bytes handed over by the drag source itself, so no refetch is needed.
    Data {
        bytes: glib::Bytes,
        mime_type: String,
        suggested_name: Option<String>,
    },
    Uris(Vec<String>),
    Texture(gdk::Texture),
}

/// Mime types we accept as a direct byte transfer, in the order we prefer them.
/// Anything else advertised as `image/*` is accepted too, just with lower priority.
///
/// `application/octet-stream` earns its place here: browsers hand a dragged image
/// over under that type rather than an `image/*` one, with the real name in a
/// `name=` parameter. Refusing it is what used to push these drops onto the
/// refetch path.
const PREFERRED_BINARY_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/avif",
    "image/gif",
    "image/tiff",
    "image/bmp",
    "image/svg+xml",
    "image/x-icon",
    "application/pdf",
    "application/octet-stream",
];

/// Text flavors that carry a URL, best first. `text/html` is deliberately absent:
/// it holds the whole dragged fragment, so a dragged image yields both the `<img
/// src>` and the enclosing `<a href>` page link, and we cannot tell which is which.
const URL_MIME_TYPES: &[&str] = &[
    "text/uri-list",
    "text/x-moz-url",
    "text/plain;charset=utf-8",
    "text/plain",
];

/// The paths of the drag currently in flight, if it started in this process.
///
/// A drop target that has to tell "the user rearranged their own icons" from
/// "something arrived from elsewhere" needs to know this *synchronously*, while
/// deciding which action to advertise — reading the payload is async and lands
/// far too late for that.
pub fn internal_drag_paths() -> Option<Vec<PathBuf>> {
    INTERNAL_DRAG_PATHS.with(|paths| paths.borrow().clone())
}

pub fn install_drag_source<W, F>(widget: &W, selected_paths: F)
where
    W: IsA<gtk::Widget>,
    F: Fn(f64, f64) -> Vec<PathBuf> + 'static,
{
    let selected_paths = Rc::new(selected_paths);
    let drag_source = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY | gdk::DragAction::MOVE)
        .build();
    drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);

    drag_source.connect_prepare(move |_, x, y| {
        let paths = selected_paths(x, y);
        if paths.is_empty() {
            return None;
        }

        INTERNAL_DRAG_PATHS.with(|drag_paths| {
            *drag_paths.borrow_mut() = Some(paths.clone());
        });

        let files = paths.iter().map(gio::File::for_path).collect::<Vec<_>>();
        let file_list = gdk::FileList::from_array(&files);
        Some(gdk::ContentProvider::for_value(&file_list.to_value()))
    });

    drag_source.connect_drag_end(|_, _, _| clear_internal_drag());
    drag_source.connect_drag_cancel(|_, _, _| {
        clear_internal_drag();
        false
    });

    widget.add_controller(drag_source);
}

pub fn install_drop_target<W, F>(widget: &W, on_drop: F)
where
    W: IsA<gtk::Widget>,
    F: Fn(DropPayload) + 'static,
{
    let on_drop = Rc::new(on_drop);
    let drop_target =
        gtk::DropTargetAsync::new(None, gdk::DragAction::COPY | gdk::DragAction::MOVE);

    drop_target.connect_accept(|_, drop| drop_has_supported_payload(drop));
    drop_target.connect_drag_enter(|_, drop, _, _| preferred_drop_action(drop));
    drop_target.connect_drag_motion(|_, drop, _, _| preferred_drop_action(drop));
    drop_target.connect_drop(move |target, drop, _, _| {
        if !drop_has_supported_payload(drop) {
            target.reject_drop(drop);
            return false;
        }

        let on_drop = Rc::clone(&on_drop);
        let drop = drop.clone();
        glib::MainContext::default().spawn_local(async move {
            match read_drop_payload(&drop).await {
                Some((payload, action)) => {
                    on_drop(payload);
                    drop.finish(action);
                }
                None => drop.finish(gdk::DragAction::empty()),
            }
        });
        true
    });

    widget.add_controller(drop_target);
}

async fn read_drop_payload(drop: &gdk::Drop) -> Option<(DropPayload, gdk::DragAction)> {
    tracing::info!(offered = ?offered_mime_types(drop), "received a drop");

    if let Some(payload) = read_file_list_payload(drop).await {
        return Some(payload);
    }

    // Browsers advertise a dragged image both as a URL and as the bytes they
    // already hold. Always prefer the bytes: refetching the URL is a fresh
    // request carrying none of the browser's session, so anything behind a login
    // fails there while the bytes were sitting in the drag the whole time.
    if let Some(payload) = read_binary_payload(drop).await {
        return Some(payload);
    }
    if let Some(payload) = read_texture_payload(drop).await {
        return Some(payload);
    }

    let uris = read_text(drop)
        .await
        .map(|text| extract_uris_from_text(&text))
        .unwrap_or_default();
    if !uris.is_empty() {
        tracing::debug!(?uris, "drop fell back to refetching a url");
        return Some((DropPayload::Uris(uris), gdk::DragAction::COPY));
    }

    tracing::warn!("drop offered no payload we could read");
    None
}

async fn read_file_list_payload(drop: &gdk::Drop) -> Option<(DropPayload, gdk::DragAction)> {
    if !drop.formats().contains_type(gdk::FileList::static_type()) {
        return None;
    }

    let value = drop
        .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
        .await
        .ok()?;
    let file_list = value.get::<gdk::FileList>().ok()?;
    let paths = file_list
        .files()
        .into_iter()
        .filter_map(|file| file.path())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }

    let operation = if is_internal_drag(&paths) {
        DropOperation::Move
    } else {
        DropOperation::Copy
    };
    let action = match operation {
        DropOperation::Copy => gdk::DragAction::COPY,
        DropOperation::Move => gdk::DragAction::MOVE,
    };

    Some((DropPayload::LocalPaths { operation, paths }, action))
}

/// Reads the dropped URL, asking for a URL-bearing flavor by name.
///
/// Letting GDK satisfy a plain `String` instead would often serve `text/html`,
/// whose markup yields both the image and the page linking to it — and importing
/// the page link is how a perfectly valid image drop ends up fetching a 404.
async fn read_text(drop: &gdk::Drop) -> Option<String> {
    let offered = offered_mime_types(drop);
    let preferred = URL_MIME_TYPES.iter().find_map(|preferred| {
        offered
            .iter()
            .find(|mime_type| base_mime_type(mime_type) == base_mime_type(preferred))
    });

    if let Some(mime_type) = preferred
        && let Ok((stream, _)) = drop
            .read_future(&[mime_type.as_str()], glib::Priority::DEFAULT)
            .await
        && let Some(bytes) = read_stream_to_end(&stream).await
    {
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }

    if !drop_has_text_payload(drop) {
        return None;
    }

    let value = drop
        .read_value_future(String::static_type(), glib::Priority::DEFAULT)
        .await
        .ok()?;
    value.get::<String>().ok()
}

async fn read_binary_payload(drop: &gdk::Drop) -> Option<(DropPayload, gdk::DragAction)> {
    let mime_type = preferred_binary_mime_type(drop)?;
    let (stream, negotiated) = drop
        .read_future(&[mime_type.as_str()], glib::Priority::DEFAULT)
        .await
        .inspect_err(|error| tracing::warn!(%mime_type, %error, "could not read dropped bytes"))
        .ok()?;

    let bytes = read_stream_to_end(&stream).await?;
    if bytes.is_empty() {
        return None;
    }

    let suggested_name =
        mime_name_parameter(&mime_type).or_else(|| mime_name_parameter(&negotiated));
    tracing::debug!(
        %negotiated,
        size = bytes.len(),
        ?suggested_name,
        "took dropped bytes from the source"
    );

    Some((
        DropPayload::Data {
            bytes,
            mime_type: base_mime_type(&negotiated),
            suggested_name,
        },
        gdk::DragAction::COPY,
    ))
}

/// Picks the mime type to take the bytes from, returned verbatim so it can be
/// handed straight back to the source: matching ignores parameters, but reading
/// has to ask for the exact string that was advertised.
fn preferred_binary_mime_type(drop: &gdk::Drop) -> Option<String> {
    let offered = offered_mime_types(drop);

    PREFERRED_BINARY_MIME_TYPES
        .iter()
        .find_map(|preferred| {
            offered
                .iter()
                .find(|mime_type| base_mime_type(mime_type) == *preferred)
        })
        .or_else(|| {
            offered
                .iter()
                .find(|mime_type| base_mime_type(mime_type).starts_with("image/"))
        })
        .cloned()
}

/// The mime type without its parameters, lowercased: `image/JPEG; q=1` -> `image/jpeg`.
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// The `name=` parameter of a mime type, which is where a browser puts the
/// original filename when it hands over bytes as `application/octet-stream`.
fn mime_name_parameter(mime_type: &str) -> Option<String> {
    let parameters = mime_type.split(';').skip(1);
    for parameter in parameters {
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("name") {
            let value = value.trim().trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

async fn read_texture_payload(drop: &gdk::Drop) -> Option<(DropPayload, gdk::DragAction)> {
    if !drop_has_texture_payload(drop) {
        return None;
    }

    let value = drop
        .read_value_future(gdk::Texture::static_type(), glib::Priority::DEFAULT)
        .await
        .ok()?;
    let texture = value.get::<gdk::Texture>().ok()?;
    Some((DropPayload::Texture(texture), gdk::DragAction::COPY))
}

async fn read_stream_to_end(stream: &gio::InputStream) -> Option<glib::Bytes> {
    let sink = gio::MemoryOutputStream::new_resizable();
    sink.splice_future(
        stream,
        gio::OutputStreamSpliceFlags::CLOSE_SOURCE | gio::OutputStreamSpliceFlags::CLOSE_TARGET,
        glib::Priority::DEFAULT,
    )
    .await
    .ok()?;

    Some(sink.steal_as_bytes())
}

fn offered_mime_types(drop: &gdk::Drop) -> Vec<String> {
    drop.formats()
        .mime_types()
        .iter()
        .map(|mime_type| mime_type.to_string())
        .collect()
}

fn drop_has_supported_payload(drop: &gdk::Drop) -> bool {
    let formats = drop.formats();
    formats.contains_type(gdk::FileList::static_type())
        || drop_has_text_payload(drop)
        || drop_has_texture_payload(drop)
        || preferred_binary_mime_type(drop).is_some()
}

fn drop_has_text_payload(drop: &gdk::Drop) -> bool {
    let formats = drop.formats();
    formats.contains_type(String::static_type())
        || formats.contain_mime_type("text/uri-list")
        || formats.contain_mime_type("text/plain")
        || formats.contain_mime_type("text/html")
}

fn drop_has_texture_payload(drop: &gdk::Drop) -> bool {
    let formats = drop.formats();
    formats.contains_type(gdk::Texture::static_type())
        || formats
            .mime_types()
            .iter()
            .any(|mime_type| mime_type.as_str().starts_with("image/"))
}

fn preferred_drop_action(drop: &gdk::Drop) -> gdk::DragAction {
    if !drop_has_supported_payload(drop) {
        return gdk::DragAction::empty();
    }

    if drop.drag().is_some() && drop.actions().contains(gdk::DragAction::MOVE) {
        gdk::DragAction::MOVE
    } else if drop.actions().contains(gdk::DragAction::COPY) {
        gdk::DragAction::COPY
    } else {
        drop.actions() & (gdk::DragAction::COPY | gdk::DragAction::MOVE)
    }
}

fn extract_uris_from_text(text: &str) -> Vec<String> {
    let mut uris = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| looks_like_drop_uri(line))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    extract_html_attribute_uris(text, "src", &mut uris);
    extract_html_attribute_uris(text, "href", &mut uris);
    uris.sort();
    uris.dedup();
    uris
}

fn extract_html_attribute_uris(text: &str, attribute: &str, uris: &mut Vec<String>) {
    let mut remaining = text;
    let pattern = format!("{attribute}=");
    while let Some(index) = remaining.to_ascii_lowercase().find(&pattern) {
        remaining = &remaining[index + pattern.len()..];
        let mut chars = remaining.chars();
        let quote = chars.next().unwrap_or_default();
        if quote != '\'' && quote != '"' {
            continue;
        }
        let value_start = quote.len_utf8();
        let Some(value_end) = remaining[value_start..].find(quote) else {
            break;
        };
        let candidate = &remaining[value_start..value_start + value_end];
        if looks_like_drop_uri(candidate) {
            uris.push(candidate.to_string());
        }
        remaining = &remaining[value_start + value_end + quote.len_utf8()..];
    }
}

fn looks_like_drop_uri(value: &str) -> bool {
    value.starts_with("file://")
        || value.starts_with("http://")
        || value.starts_with("https://")
        || PathBuf::from(value).is_absolute()
}

fn clear_internal_drag() {
    INTERNAL_DRAG_PATHS.with(|drag_paths| {
        *drag_paths.borrow_mut() = None;
    });
}

fn is_internal_drag(paths: &[PathBuf]) -> bool {
    INTERNAL_DRAG_PATHS.with(|drag_paths| {
        let drag_paths = drag_paths.borrow();
        let Some(internal_paths) = drag_paths.as_ref() else {
            return false;
        };

        if internal_paths.len() != paths.len() {
            return false;
        }

        let mut internal_paths = internal_paths.clone();
        let mut drop_paths = paths.to_vec();
        internal_paths.sort();
        drop_paths.sort();
        internal_paths == drop_paths
    })
}

#[cfg(test)]
mod tests {
    use super::{base_mime_type, extract_uris_from_text, mime_name_parameter};

    #[test]
    fn compares_mime_types_without_their_parameters() {
        assert_eq!(
            base_mime_type("application/octet-stream;name=\"177109.jpg\""),
            "application/octet-stream"
        );
        assert_eq!(base_mime_type(" image/JPEG "), "image/jpeg");
    }

    #[test]
    fn takes_the_original_filename_from_the_mime_parameters() {
        assert_eq!(
            mime_name_parameter("application/octet-stream;name=\"177109.jpg\"").as_deref(),
            Some("177109.jpg")
        );
        assert_eq!(
            mime_name_parameter("application/octet-stream; NAME=photo.png").as_deref(),
            Some("photo.png")
        );
        assert_eq!(mime_name_parameter("image/png"), None);
        assert_eq!(
            mime_name_parameter("application/octet-stream;name=\"\""),
            None
        );
        // A valueless parameter must not hide the one we are after.
        assert_eq!(
            mime_name_parameter("application/octet-stream;inline;name=\"a.png\"").as_deref(),
            Some("a.png")
        );
    }

    #[test]
    fn parses_text_uri_list_comments_and_crlf() {
        let uris = extract_uris_from_text(
            "# copied files\r\nfile:///tmp/photo.png\r\nhttps://example.com/image.jpg\r\n",
        );

        assert_eq!(
            uris,
            vec![
                "file:///tmp/photo.png".to_string(),
                "https://example.com/image.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn extracts_browser_image_sources_from_html() {
        let uris = extract_uris_from_text(
            r#"<a href="https://example.com/page"><img src="https://example.com/image.webp"></a>"#,
        );

        assert_eq!(
            uris,
            vec![
                "https://example.com/image.webp".to_string(),
                "https://example.com/page".to_string(),
            ]
        );
    }
}
