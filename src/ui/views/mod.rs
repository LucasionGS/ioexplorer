pub mod icon;
pub mod list;
pub mod thumbnail;

use std::{
    path::PathBuf,
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use gtk::prelude::*;

use crate::{
    providers::{FileIcon, FileItem, FileKind},
    ui::dnd::DropPayload,
};

pub type FolderDropHandler = Rc<dyn Fn(PathBuf, DropPayload)>;
pub type FileDragHandler = Rc<dyn Fn(usize) -> Vec<PathBuf>>;
pub type EntrySelectionHandler = Rc<dyn Fn(usize, gtk::gdk::ModifierType)>;
pub type EntryContextMenuHandler = Rc<dyn Fn(usize, gtk::Widget, f64, f64)>;

pub fn format_bytes(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", size, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_size(item: &FileItem) -> String {
    if item.kind == FileKind::Directory {
        return String::new();
    }

    let Some(size) = item.size else {
        return String::new();
    };

    format_bytes(size)
}

pub fn format_timestamp(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return String::new();
    };

    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return String::new();
    };

    glib::DateTime::from_unix_local(duration.as_secs() as i64)
        .ok()
        .and_then(|datetime| datetime.format("%Y-%m-%d %H:%M").ok())
        .map(|formatted| formatted.to_string())
        .unwrap_or_default()
}

pub fn clear_box_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        child.unparent();
    }
}

pub fn image_for_item(item: &FileItem, pixel_size: i32) -> gtk::Image {
    let image = gtk::Image::builder()
        .pixel_size(pixel_size)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    set_image_for_item(&image, item, pixel_size);
    image
}

/// The widget an entry's icon goes in, and the `gtk::Image` inside it.
///
/// A bare image for anything that is not a link, so the common case pays for no
/// extra widget in a ten-thousand-file listing. A link gets a `gtk::Overlay`
/// with a corner emblem: GTK4 CSS has no `::after` to badge a widget with, and
/// the thumbnail worker later swaps the paintable on this exact image — so the
/// emblem has to be a sibling of it rather than part of it.
///
/// Callers keep the returned image: it is what a `ThumbnailTarget` wraps.
pub fn icon_widget_for_item(item: &FileItem, pixel_size: i32) -> (gtk::Widget, gtk::Image) {
    let image = image_for_item(item, pixel_size);
    let Some(link) = item.link.as_ref() else {
        return (image.clone().upcast(), image);
    };

    let overlay = gtk::Overlay::builder().child(&image).build();
    overlay.add_overlay(&link_emblem(pixel_size, link.resolved.is_none()));
    (overlay.upcast(), image)
}

/// The `gtk::Image` inside whatever [`icon_widget_for_item`] built.
///
/// One place knows both shapes. The lazy-thumbnail passes look their target
/// back up out of the widget tree, and a plain `first_child()` downcast would
/// return `None` for a link — loading nothing, with no error to notice.
pub fn entry_icon(widget: &gtk::Widget) -> Option<gtk::Image> {
    if let Ok(image) = widget.clone().downcast::<gtk::Image>() {
        return Some(image);
    }
    widget
        .clone()
        .downcast::<gtk::Overlay>()
        .ok()?
        .child()?
        .downcast::<gtk::Image>()
        .ok()
}

/// Points an always-present emblem at whatever `item` currently is.
///
/// The desktop re-reads an item into an existing tile rather than rebuilding
/// it, so its emblem has to exist before it is needed and be hidden when it is
/// not — a conditional overlay could never grow one when a file is replaced by
/// a link. One extra hidden widget per desktop icon, of which there are dozens.
pub fn set_link_emblem(emblem: &gtk::Image, item: &FileItem, pixel_size: i32) {
    let Some(link) = item.link.as_ref() else {
        emblem.set_visible(false);
        return;
    };

    let broken = link.resolved.is_none();
    emblem.set_icon_name(Some(emblem_icon_name(broken)));
    emblem.set_pixel_size(emblem_size(pixel_size));
    if broken {
        emblem.add_css_class("link-emblem-broken");
    } else {
        emblem.remove_css_class("link-emblem-broken");
    }
    emblem.set_visible(true);
}

fn emblem_icon_name(broken: bool) -> &'static str {
    if broken {
        "dialog-warning-symbolic"
    } else {
        "emblem-symbolic-link-symbolic"
    }
}

fn link_emblem(pixel_size: i32, broken: bool) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(emblem_icon_name(broken))
        .pixel_size(emblem_size(pixel_size))
        .halign(gtk::Align::Start)
        .valign(gtk::Align::End)
        .css_classes(if broken {
            ["link-emblem", "link-emblem-broken"].as_slice()
        } else {
            ["link-emblem"].as_slice()
        })
        .build()
}

/// A third of the icon, bounded at both ends: below 12px a symbolic stops being
/// readable, and past 32px the badge starts covering what it is badging.
fn emblem_size(pixel_size: i32) -> i32 {
    (pixel_size / 3).clamp(12, 32)
}

pub fn set_image_for_item(image: &gtk::Image, item: &FileItem, pixel_size: i32) {
    match &item.icon {
        Some(FileIcon::Path(path)) => image.set_from_file(Some(path)),
        Some(FileIcon::Themed(icon_name)) => image.set_icon_name(Some(icon_name)),
        None => image.set_icon_name(Some(item.kind.icon_name())),
    }
    image.set_pixel_size(pixel_size);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge scales with the icon but never off either end of the range —
    /// the desktop's icons and the list's 24px rows share this one function.
    #[test]
    fn an_emblem_stays_a_fraction_of_the_icon() {
        assert_eq!(emblem_size(24), 12);
        assert_eq!(emblem_size(96), 32);
        assert_eq!(emblem_size(150), 32);
    }
}
