use std::rc::Rc;

use gtk::prelude::*;

use crate::{
    config::ListColumns,
    providers::{FileItem, FileKind},
    sorting::{SortKey, SortOrder},
    ui::{
        dnd,
        views::{
            EntryContextMenuHandler, EntrySelectionHandler, FileDragHandler, FolderDropHandler,
            format_size, format_timestamp, image_for_item,
            thumbnail::{self, ThumbnailCache, ThumbnailSpec, ThumbnailTarget},
        },
    },
};

/// A column header being clicked, carrying what that column sorts by.
pub type ColumnSortHandler = Rc<dyn Fn(SortKey)>;

/// The row's leading icon, which the header has to skip past to line its first
/// title up with the names below it, and the size a row's preview is rendered
/// at.
pub const ICON_SIZE: i32 = 24;

/// A row's preview is square, unlike a tile's: the rows have to stay a uniform
/// height, and a wide thumbnail would push every name in the column across.
fn thumbnail_spec() -> ThumbnailSpec {
    ThumbnailSpec::square(ICON_SIZE)
}
const COLUMN_SPACING: i32 = 12;
/// The row container's own start margin plus the margin `.content-list row`
/// gives every row. The header sits outside the list — above the scrolled
/// window, so it stays put — and so has to reproduce that inset by hand.
const HEADER_MARGIN: i32 = 16;

/// A metadata column: the title its header shows, the width header and cell
/// both request, what the cell reads off an entry, and what clicking the header
/// sorts by. One definition per column, so a header can never drift out of
/// alignment with the cells under it.
#[derive(Clone, Copy)]
struct MetaColumn {
    title: &'static str,
    width: i32,
    sort: SortKey,
    value: fn(&FileItem) -> String,
}

fn kind_value(item: &FileItem) -> String {
    item.kind.label().to_string()
}

fn size_value(item: &FileItem) -> String {
    format_size(item)
}

fn modified_value(item: &FileItem) -> String {
    format_timestamp(item.modified)
}

fn created_value(item: &FileItem) -> String {
    format_timestamp(item.created)
}

const KIND_COLUMN: MetaColumn = MetaColumn {
    title: "Kind",
    width: 96,
    // The kind itself is Folder/File/Link, which "folders first" already
    // separates out. Extension is the ordering someone clicking a type column
    // is actually after: every .txt together, every .rs together.
    sort: SortKey::Extension,
    value: kind_value,
};

const SIZE_COLUMN: MetaColumn = MetaColumn {
    title: "Size",
    width: 96,
    sort: SortKey::Size,
    value: size_value,
};

const MODIFIED_COLUMN: MetaColumn = MetaColumn {
    title: "Modified",
    width: 148,
    sort: SortKey::Modified,
    value: modified_value,
};

const CREATED_COLUMN: MetaColumn = MetaColumn {
    title: "Created",
    width: 148,
    sort: SortKey::Created,
    value: created_value,
};

fn meta_columns(columns: &ListColumns) -> Vec<MetaColumn> {
    let mut enabled = Vec::new();
    if columns.kind {
        enabled.push(KIND_COLUMN);
    }
    if columns.size {
        enabled.push(SIZE_COLUMN);
    }
    if columns.modified {
        enabled.push(MODIFIED_COLUMN);
    }
    if columns.created {
        enabled.push(CREATED_COLUMN);
    }
    enabled
}

/// The empty header bar. [`populate_header`] fills it, and refills it whenever
/// the columns or the sort change.
pub fn header_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(COLUMN_SPACING)
        .margin_start(HEADER_MARGIN)
        .margin_end(HEADER_MARGIN)
        .css_classes(["list-header"])
        .build()
}

pub fn populate_header(
    header: &gtk::Box,
    columns: &ListColumns,
    order: SortOrder,
    sort_handler: ColumnSortHandler,
) {
    while let Some(child) = header.first_child() {
        child.unparent();
    }

    // Stands in for the row icon, so "Name" starts where the names do.
    header.append(
        &gtk::Box::builder()
            .width_request(ICON_SIZE)
            .orientation(gtk::Orientation::Horizontal)
            .build(),
    );
    header.append(&header_button(
        "Name",
        SortKey::Name,
        None,
        order,
        sort_handler.clone(),
    ));

    for column in meta_columns(columns) {
        header.append(&header_button(
            column.title,
            column.sort,
            Some(column.width),
            order,
            sort_handler.clone(),
        ));
    }
}

fn header_button(
    title: &str,
    sort: SortKey,
    width: Option<i32>,
    order: SortOrder,
    sort_handler: ColumnSortHandler,
) -> gtk::Button {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    content.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );

    let active = order.key == sort;
    if active {
        content.append(
            &gtk::Image::builder()
                .icon_name(if order.descending {
                    "pan-down-symbolic"
                } else {
                    "pan-up-symbolic"
                })
                .pixel_size(12)
                .build(),
        );
    }

    let button = gtk::Button::builder()
        .child(&content)
        .tooltip_text(format!("Sort by {}", sort.label()))
        .css_classes(["list-header-button"])
        .build();
    button.set_focusable(false);
    if active {
        button.add_css_class("list-header-active");
    }
    match width {
        Some(width) => button.set_width_request(width),
        None => button.set_hexpand(true),
    }

    button.connect_clicked(move |_| sort_handler(sort));
    button
}

pub struct ListViewOptions {
    pub columns: ListColumns,
    pub thumbnail_cache: ThumbnailCache,
}

/// What every row in one pass shares, resolved once rather than per row.
struct RowContext<'a> {
    columns: &'a [MetaColumn],
    thumbnail_cache: &'a ThumbnailCache,
}

pub fn populate(
    list: &gtk::ListBox,
    items: &[FileItem],
    options: &ListViewOptions,
    folder_drop_handler: FolderDropHandler,
    file_drag_handler: FileDragHandler,
    selection_handler: EntrySelectionHandler,
    context_menu_handler: EntryContextMenuHandler,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }

    let columns = meta_columns(&options.columns);
    let context = RowContext {
        columns: &columns,
        thumbnail_cache: &options.thumbnail_cache,
    };
    for (index, item) in items.iter().enumerate() {
        list.append(&row_for(
            index,
            item,
            &context,
            folder_drop_handler.clone(),
            file_drag_handler.clone(),
            selection_handler.clone(),
            context_menu_handler.clone(),
        ));
    }
}

/// Queues previews for the rows on screen.
///
/// The same treatment the grid gets: a folder of ten thousand photos should
/// only decode the two dozen the user can actually see.
pub fn load_visible_thumbnails(
    list: &gtk::ListBox,
    scroll: &gtk::ScrolledWindow,
    items: &[FileItem],
    thumbnail_cache: &ThumbnailCache,
) {
    let adjustment = scroll.vadjustment();
    let page_size = adjustment.page_size();
    let overscan = page_size.max(f64::from(ICON_SIZE) * 8.0);
    let visible_top = (adjustment.value() - overscan).max(0.0) as f32;
    let visible_bottom = (adjustment.value() + page_size + overscan) as f32;
    let spec = thumbnail_spec();

    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if row_intersects_y(&row, list, visible_top, visible_bottom)
            && let Some(item) = items.get(index as usize)
            && let Some(icon) = row_icon(&row)
        {
            thumbnail::request(item, &ThumbnailTarget::icon(&icon), spec, thumbnail_cache);
        }
        index += 1;
    }
}

fn row_intersects_y(
    row: &gtk::ListBoxRow,
    list: &gtk::ListBox,
    visible_top: f32,
    visible_bottom: f32,
) -> bool {
    row.compute_bounds(list).is_some_and(|bounds| {
        bounds.y() <= visible_bottom && bounds.y() + bounds.height() >= visible_top
    })
}

fn row_icon(row: &gtk::ListBoxRow) -> Option<gtk::Image> {
    row.child()?
        .downcast::<gtk::Box>()
        .ok()?
        .first_child()?
        .downcast::<gtk::Image>()
        .ok()
}

fn row_for(
    index: usize,
    item: &FileItem,
    context: &RowContext<'_>,
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
        .spacing(COLUMN_SPACING)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .css_classes(["file-row"])
        .build();

    let icon = image_for_item(item, ICON_SIZE);
    icon.add_css_class("file-row-icon");
    thumbnail::apply_cached(
        item,
        &ThumbnailTarget::icon(&icon),
        thumbnail_spec(),
        context.thumbnail_cache,
    );
    let name = gtk::Label::builder()
        .label(item.display_name())
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    container.append(&icon);
    container.append(&name);

    for column in context.columns {
        container.append(&meta_label(&(column.value)(item), column.width));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn all_columns() -> ListColumns {
        ListColumns {
            size: true,
            kind: true,
            modified: true,
            created: true,
        }
    }

    #[test]
    fn columns_appear_in_a_fixed_order() {
        let titles: Vec<_> = meta_columns(&all_columns())
            .into_iter()
            .map(|column| column.title)
            .collect();

        assert_eq!(titles, ["Kind", "Size", "Modified", "Created"]);
    }

    #[test]
    fn a_disabled_column_is_dropped_without_shifting_the_rest() {
        let columns = ListColumns {
            size: false,
            ..all_columns()
        };

        let titles: Vec<_> = meta_columns(&columns)
            .into_iter()
            .map(|column| column.title)
            .collect();

        assert_eq!(titles, ["Kind", "Modified", "Created"]);
    }

    /// Two headers offering the same key would leave the sort arrow drawn twice.
    #[test]
    fn every_column_sorts_by_a_different_key() {
        let columns = meta_columns(&all_columns());

        for (index, column) in columns.iter().enumerate() {
            assert_ne!(
                column.sort,
                SortKey::Name,
                "{} duplicates Name",
                column.title
            );
            assert!(
                !columns[..index]
                    .iter()
                    .any(|earlier| earlier.sort == column.sort),
                "{} duplicates an earlier column",
                column.title
            );
        }
    }
}
