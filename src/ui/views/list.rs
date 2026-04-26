use gtk::prelude::*;

use crate::{
    config::ListColumns,
    providers::{FileItem, FileKind},
    ui::{
        dnd,
        views::{
            EntryContextMenuHandler, EntrySelectionHandler, FileDragHandler, FolderDropHandler,
            format_modified, format_size, image_for_item,
        },
    },
};

pub fn populate(
    list: &gtk::ListBox,
    items: &[FileItem],
    columns: &ListColumns,
    folder_drop_handler: FolderDropHandler,
    file_drag_handler: FileDragHandler,
    selection_handler: EntrySelectionHandler,
    context_menu_handler: EntryContextMenuHandler,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }

    for (index, item) in items.iter().enumerate() {
        list.append(&row_for(
            index,
            item,
            columns,
            folder_drop_handler.clone(),
            file_drag_handler.clone(),
            selection_handler.clone(),
            context_menu_handler.clone(),
        ));
    }
}

fn row_for(
    index: usize,
    item: &FileItem,
    columns: &ListColumns,
    folder_drop_handler: FolderDropHandler,
    file_drag_handler: FileDragHandler,
    selection_handler: EntrySelectionHandler,
    context_menu_handler: EntryContextMenuHandler,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(false)
        .build();

    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .css_classes(["file-row"])
        .build();

    let icon = image_for_item(item, 24);
    let name = gtk::Label::builder()
        .label(item.display_name())
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    container.append(&icon);
    container.append(&name);

    if columns.kind {
        container.append(&meta_label(item.kind.label(), 96));
    }
    if columns.size {
        container.append(&meta_label(&format_size(item), 96));
    }
    if columns.modified {
        container.append(&meta_label(&format_modified(item.modified), 148));
    }

    row.set_child(Some(&container));

    install_selection_click(&row, index, selection_handler);
    install_context_menu_click(&row, index, context_menu_handler);
    dnd::install_drag_source(&row, move |_, _| file_drag_handler(index));

    if item.kind == FileKind::Directory
        && let Ok(target_dir) = item.uri.local_path()
    {
        dnd::install_drop_target(&row, move |payload| {
            folder_drop_handler(target_dir.clone(), payload);
        });
    }

    row
}

fn install_context_menu_click(
    row: &gtk::ListBoxRow,
    index: usize,
    context_menu_handler: EntryContextMenuHandler,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    let menu_row = row.clone();
    click.connect_pressed(move |_, _, x, y| {
        context_menu_handler(index, menu_row.clone().upcast(), x, y);
    });
    row.add_controller(click);
}

fn install_selection_click(
    row: &gtk::ListBoxRow,
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
    row.add_controller(click);
}

fn meta_label(text: &str, width: i32) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .width_request(width)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label"])
        .build()
}
