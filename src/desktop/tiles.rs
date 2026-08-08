//! One icon on the desktop.
//!
//! Deliberately not `views::icon::tile_for`: that builds for a `FlowBox` which
//! sizes its children, and it wires selection by *index* into a listing. A
//! desktop tile is placed absolutely and identified by file name, because a
//! folder change must not silently re-point anything at a different file.

use gtk::prelude::*;

use crate::{
    providers::FileItem,
    ui::views::{
        image_for_item,
        thumbnail::{self, ThumbnailCache, ThumbnailSpec, ThumbnailTarget},
    },
};

use super::layout::GridMetrics;

/// A built tile, kept so a reload can update it in place rather than rebuild it.
#[derive(Clone)]
pub struct Tile {
    pub root: gtk::Box,
    pub image: gtk::Image,
    pub label: gtk::Label,
}

pub fn spec(icon_size: i32, metrics: GridMetrics) -> ThumbnailSpec {
    ThumbnailSpec {
        icon_size,
        thumbnail_width: metrics.tile_width,
    }
}

pub fn build(
    item: &FileItem,
    icon_size: i32,
    metrics: GridMetrics,
    label_backdrop: bool,
    thumbnails: &ThumbnailCache,
) -> Tile {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .width_request(metrics.tile_width)
        .height_request(metrics.tile_height)
        .css_classes(["desktop-tile"])
        .build();

    let image = image_for_item(item, icon_size);
    image.add_css_class("file-tile-icon");

    let label = gtk::Label::builder()
        .label(item.display_name())
        .justify(gtk::Justification::Center)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(2)
        .max_width_chars(14)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .css_classes(["desktop-tile-label"])
        .build();
    if label_backdrop {
        label.add_css_class("label-backdrop");
    }

    root.append(&image);
    root.append(&label);

    // Synchronous, so a thumbnail already in the cache is on screen in the
    // first frame rather than flickering in from the placeholder icon.
    thumbnail::apply_cached(
        item,
        &ThumbnailTarget::icon(&image),
        spec(icon_size, metrics),
        thumbnails,
    );

    Tile { root, image, label }
}

impl Tile {
    /// Re-reads `item` into an existing tile. Used for entries that survived a
    /// reload: a `.desktop` file's icon or name can change without the file
    /// appearing or disappearing.
    pub fn update(&self, item: &FileItem, icon_size: i32) {
        crate::ui::views::set_image_for_item(&self.image, item, icon_size);
        self.image.add_css_class("file-tile-icon");
        self.label.set_label(item.display_name());
    }

    pub fn set_selected(&self, selected: bool) {
        if selected {
            self.root.add_css_class("entry-selected");
        } else {
            self.root.remove_css_class("entry-selected");
        }
    }

    pub fn request_thumbnail(
        &self,
        item: &FileItem,
        icon_size: i32,
        metrics: GridMetrics,
        thumbnails: &ThumbnailCache,
    ) {
        thumbnail::request(
            item,
            &ThumbnailTarget::icon(&self.image),
            spec(icon_size, metrics),
            thumbnails,
        );
    }
}
