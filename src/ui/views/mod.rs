pub mod icon;
pub mod list;

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

pub fn format_modified(time: Option<SystemTime>) -> String {
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

pub fn set_image_for_item(image: &gtk::Image, item: &FileItem, pixel_size: i32) {
    match &item.icon {
        Some(FileIcon::Path(path)) => image.set_from_file(Some(path)),
        Some(FileIcon::Themed(icon_name)) => image.set_icon_name(Some(icon_name)),
        None => image.set_icon_name(Some(item.kind.icon_name())),
    }
    image.set_pixel_size(pixel_size);
}
