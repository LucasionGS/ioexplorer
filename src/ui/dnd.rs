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
    Uris(Vec<String>),
    Texture(gdk::Texture),
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
    if let Some(payload) = read_file_list_payload(drop).await {
        return Some(payload);
    }
    if let Some(payload) = read_uri_payload(drop).await {
        return Some(payload);
    }
    if let Some(payload) = read_texture_payload(drop).await {
        return Some(payload);
    }
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

async fn read_uri_payload(drop: &gdk::Drop) -> Option<(DropPayload, gdk::DragAction)> {
    if !drop_has_text_payload(drop) {
        return None;
    }

    let value = drop
        .read_value_future(String::static_type(), glib::Priority::DEFAULT)
        .await
        .ok()?;
    let text = value.get::<String>().ok()?;
    let uris = extract_uris_from_text(&text);
    if uris.is_empty() {
        return None;
    }

    Some((DropPayload::Uris(uris), gdk::DragAction::COPY))
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

fn drop_has_supported_payload(drop: &gdk::Drop) -> bool {
    let formats = drop.formats();
    formats.contains_type(gdk::FileList::static_type())
        || drop_has_text_payload(drop)
        || drop_has_texture_payload(drop)
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
    use super::extract_uris_from_text;

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
