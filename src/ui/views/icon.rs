use std::{cell::RefCell, rc::Rc};

use gtk::prelude::*;

use crate::{
    config::IconStyle,
    providers::{FileItem, FileKind},
    ui::{
        dnd,
        views::{
            EntryContextMenuHandler, EntrySelectionHandler, FileDragHandler, FolderDropHandler,
            entry_icon, icon_widget_for_item,
            thumbnail::{self, ThumbnailCache, ThumbnailSpec, ThumbnailTarget},
        },
    },
};

/// How much wider than tall a tile's preview may be. A tile is already wider
/// than its icon to leave room for the name, so a landscape photo can use that
/// width instead of being letterboxed into a square.
pub const TILE_EXTRA_WIDTH: i32 = 56;

#[derive(Clone)]
pub struct IconViewOptions {
    pub icon_size: i32,
    pub thumbnail_cache: ThumbnailCache,
    pub icon_style: IconStyle,
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
        let tile = tile_for(
            index,
            item,
            options.clone(),
            folder_drop_handler.clone(),
            file_drag_handler.clone(),
            selection_handler.clone(),
            context_menu_handler.clone(),
        );
        // The flow box hands each line's spare width to its cells, so a cell
        // left to fill would be wider in a folder of three files than in one
        // of thirty. Centred, the cell keeps the tile's own size, and with it
        // the hover and selection fill drawn on it.
        let cell = gtk::FlowBoxChild::builder()
            .child(&tile)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .build();
        flow.insert(&cell, -1);
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
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .width_request(icon_size + TILE_EXTRA_WIDTH)
        .css_classes(["file-tile"])
        .build();

    let (icon_widget, icon) = icon_widget_for_item(item, icon_size, options.icon_style);
    icon.add_css_class("file-tile-icon");
    // The tile is as wide as its slot whatever the name, so one long name no
    // longer widens every column of the grid. The name wraps to two lines and
    // ellipsizes; selecting the tile spells it out, and hovering shows it in a
    // balloon (see `install_name_balloon`).
    let label = gtk::Label::builder()
        .label(item.display_name())
        .justify(gtk::Justification::Center)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(2)
        .max_width_chars(1)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Fill)
        .css_classes(["file-tile-label"])
        .build();

    tile.append(&icon_widget);
    tile.append(&label);

    thumbnail::apply_cached(
        item,
        &ThumbnailTarget::icon(&icon),
        options.spec(),
        &options.thumbnail_cache,
    );

    if item.kind == FileKind::BrokenLink {
        tile.add_css_class("file-broken-link");
    }

    install_name_balloon(&tile, &label);
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

/// Spells out a selected tile's whole name in place, or folds it back to two
/// ellipsized lines.
pub fn set_tile_name_expanded(child: &gtk::FlowBoxChild, expanded: bool) {
    let Some(label) = child
        .child()
        .and_then(|tile| tile.last_child())
        .and_downcast::<gtk::Label>()
    else {
        return;
    };
    label.set_ellipsize(if expanded {
        gtk::pango::EllipsizeMode::None
    } else {
        gtk::pango::EllipsizeMode::End
    });
}

/// Shows a cut-off name whole in a balloon under the tile while the pointer
/// is on it. A balloon rather than growing the tile, which would reflow the
/// grid under the pointer as it moves across it.
fn install_name_balloon(tile: &gtk::Box, label: &gtk::Label) {
    // Made on the first hover only: a folder of thousands of files should
    // not build a popover per tile up front.
    let balloon = Rc::new(RefCell::new(None::<gtk::Popover>));
    let motion = gtk::EventControllerMotion::new();

    let label = label.clone();
    let enter_tile = tile.clone();
    let enter_balloon = Rc::clone(&balloon);
    motion.connect_enter(move |_, _, _| {
        let cut_off =
            label.ellipsize() != gtk::pango::EllipsizeMode::None && label.layout().is_ellipsized();
        if !cut_off {
            return;
        }

        let popover = enter_balloon
            .borrow_mut()
            .get_or_insert_with(|| name_balloon(&enter_tile, &label.label()))
            .clone();
        // Just under the tile rather than over the name: a balloon under the
        // pointer could take the pointer from the tile, closing it again.
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            0,
            enter_tile.height() - 6,
            enter_tile.width(),
            1,
        )));
        popover.popup();
    });
    motion.connect_leave(move |_| {
        if let Some(popover) = balloon.borrow().as_ref() {
            popover.popdown();
        }
    });
    tile.add_controller(motion);
}

fn name_balloon(tile: &gtk::Box, name: &str) -> gtk::Popover {
    let popover = gtk::Popover::builder()
        .child(
            &gtk::Label::builder()
                .label(name)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(28)
                .build(),
        )
        .autohide(false)
        .has_arrow(false)
        .can_focus(false)
        .can_target(false)
        .position(gtk::PositionType::Bottom)
        .css_classes(["file-name-balloon"])
        .build();
    popover.set_parent(tile);
    let parented = popover.clone();
    tile.connect_destroy(move |_| parented.unparent());
    popover
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
    let first = child.child()?.downcast::<gtk::Box>().ok()?.first_child()?;
    entry_icon(&first)
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
            icon_style: IconStyle::default(),
        };

        let spec = options.spec();

        assert_eq!(spec.icon_size, 128);
        assert_eq!(spec.thumbnail_width, 128 + TILE_EXTRA_WIDTH);
    }
}
