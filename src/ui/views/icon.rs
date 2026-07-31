use gtk::prelude::*;

use crate::{
    providers::{FileItem, FileKind},
    ui::{
        dnd,
        views::{
            EntryContextMenuHandler, EntrySelectionHandler, FileDragHandler, FolderDropHandler,
            image_for_item,
            thumbnail::{self, ThumbnailCache, ThumbnailSpec, ThumbnailTarget},
        },
    },
};

/// How much wider than tall a tile's preview may be. A tile is already wider
/// than its icon to leave room for the name, so a landscape photo can use that
/// width instead of being letterboxed into a square.
const TILE_EXTRA_WIDTH: i32 = 56;

#[derive(Clone)]
pub struct IconViewOptions {
    pub icon_size: i32,
    pub thumbnail_cache: ThumbnailCache,
}

impl IconViewOptions {
    fn spec(&self) -> ThumbnailSpec {
        ThumbnailSpec {
            icon_size: self.icon_size,
            thumbnail_width: self.icon_size + TILE_EXTRA_WIDTH,
        }
    }
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
    // The tiles these were queued against are about to be destroyed.
    thumbnail::discard_queued(&options.thumbnail_cache);

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
        .width_request(icon_size + TILE_EXTRA_WIDTH)
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

    thumbnail::apply_cached(
        item,
        &ThumbnailTarget::icon(&icon),
        options.spec(),
        &options.thumbnail_cache,
    );

    install_selection_click(&tile, index, selection_handler);
    install_context_menu_click(&tile, index, context_menu_handler);
    dnd::install_drag_source(&tile, move |_, _| file_drag_handler(index));

    if item.kind == FileKind::Directory
        && let Ok(target_dir) = item.uri.local_path()
    {
        dnd::install_drop_target(&tile, move |payload| {
            folder_drop_handler(target_dir.clone(), payload);
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
    let spec = options.spec();

    let mut index = 0;
    while let Some(child) = flow.child_at_index(index) {
        if flow_child_intersects_y(&child, flow, visible_top, visible_bottom)
            && let Some(item) = items.get(index as usize)
            && let Some(icon) = flow_child_icon(&child)
        {
            thumbnail::request(
                item,
                &ThumbnailTarget::icon(&icon),
                spec,
                &options.thumbnail_cache,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::views::thumbnail::new_cache;

    #[test]
    fn a_tile_preview_may_be_wider_than_the_icon_size() {
        let options = IconViewOptions {
            icon_size: 128,
            thumbnail_cache: new_cache(),
        };

        let spec = options.spec();

        assert_eq!(spec.icon_size, 128);
        assert_eq!(spec.thumbnail_width, 128 + TILE_EXTRA_WIDTH);
    }
}
