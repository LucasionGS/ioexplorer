use std::{path::PathBuf, rc::Rc};

use gtk::prelude::*;

pub type RenameAction = Rc<dyn Fn(PathBuf)>;
pub type DeleteAction = Rc<dyn Fn(Vec<PathBuf>)>;
pub type ClipboardAction = Rc<dyn Fn(Vec<PathBuf>)>;
pub type ViewAction = Rc<dyn Fn()>;
pub type MenuAction = Rc<dyn Fn()>;

#[derive(Clone)]
pub struct BookmarkAction {
    label: String,
    activate: MenuAction,
}

impl BookmarkAction {
    pub fn new(label: impl Into<String>, activate: MenuAction) -> Self {
        Self {
            label: label.into(),
            activate,
        }
    }

    fn context_action(&self) -> ContextMenuAction {
        let activate = Rc::clone(&self.activate);
        ContextMenuAction::new(
            self.label.clone(),
            Some("user-bookmarks-symbolic"),
            false,
            Rc::new(move || activate()),
        )
    }
}

pub trait ContextMenuContext {
    fn actions(&self) -> Vec<ContextMenuAction>;
}

pub struct ContextMenuAction {
    label: String,
    icon_name: Option<&'static str>,
    destructive: bool,
    activate: Rc<dyn Fn()>,
}

impl ContextMenuAction {
    pub fn new(
        label: impl Into<String>,
        icon_name: Option<&'static str>,
        destructive: bool,
        activate: Rc<dyn Fn()>,
    ) -> Self {
        Self {
            label: label.into(),
            icon_name,
            destructive,
            activate,
        }
    }
}

pub struct ContextMenu;

impl ContextMenu {
    pub fn popup_at<C>(parent: &impl IsA<gtk::Widget>, x: f64, y: f64, context: &C)
    where
        C: ContextMenuContext,
    {
        let actions = context.actions();
        if actions.is_empty() {
            return;
        }

        let menu = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();

        let popover = gtk::Popover::builder()
            .autohide(true)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .pointing_to(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1))
            .child(&menu)
            .css_classes(["context-menu"])
            .build();

        for action in actions {
            menu.append(&action_button(&popover, action));
        }

        popover.set_parent(parent);
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    }
}

pub struct EmptySpaceContext {
    paste: MenuAction,
    new_folder: MenuAction,
    bookmark: BookmarkAction,
}

impl EmptySpaceContext {
    pub fn new(paste: MenuAction, new_folder: MenuAction, bookmark: BookmarkAction) -> Self {
        Self {
            paste,
            new_folder,
            bookmark,
        }
    }
}

impl ContextMenuContext for EmptySpaceContext {
    fn actions(&self) -> Vec<ContextMenuAction> {
        let paste = Rc::clone(&self.paste);
        let new_folder = Rc::clone(&self.new_folder);

        vec![
            ContextMenuAction::new(
                "Paste",
                Some("edit-paste-symbolic"),
                false,
                Rc::new(move || paste()),
            ),
            ContextMenuAction::new(
                "New Folder",
                Some("folder-new-symbolic"),
                false,
                Rc::new(move || new_folder()),
            ),
            self.bookmark.context_action(),
        ]
    }
}

pub struct SidebarBookmarkContext {
    remove: MenuAction,
}

impl SidebarBookmarkContext {
    pub fn new(remove: MenuAction) -> Self {
        Self { remove }
    }
}

impl ContextMenuContext for SidebarBookmarkContext {
    fn actions(&self) -> Vec<ContextMenuAction> {
        let remove = Rc::clone(&self.remove);

        vec![ContextMenuAction::new(
            "Remove Bookmark",
            Some("user-bookmarks-symbolic"),
            false,
            Rc::new(move || remove()),
        )]
    }
}

pub enum FileEntryContext {
    Single(FileSingleSelectionContext),
    Multi(FileMultiSelectionContext),
}

impl FileEntryContext {
    pub fn for_paths(
        paths: Vec<PathBuf>,
        view: Option<ViewAction>,
        bookmark: Option<BookmarkAction>,
        copy: ClipboardAction,
        cut: ClipboardAction,
        rename: RenameAction,
        delete: DeleteAction,
    ) -> Option<Self> {
        match paths.len() {
            0 => None,
            1 => Some(Self::Single(FileSingleSelectionContext {
                path: paths[0].clone(),
                view,
                bookmark,
                copy,
                cut,
                rename,
                delete,
            })),
            _ => Some(Self::Multi(FileMultiSelectionContext {
                paths,
                view,
                copy,
                cut,
                delete,
            })),
        }
    }
}

impl ContextMenuContext for FileEntryContext {
    fn actions(&self) -> Vec<ContextMenuAction> {
        match self {
            Self::Single(context) => context.actions(),
            Self::Multi(context) => context.actions(),
        }
    }
}

pub struct FileSingleSelectionContext {
    path: PathBuf,
    view: Option<ViewAction>,
    bookmark: Option<BookmarkAction>,
    copy: ClipboardAction,
    cut: ClipboardAction,
    rename: RenameAction,
    delete: DeleteAction,
}

impl ContextMenuContext for FileSingleSelectionContext {
    fn actions(&self) -> Vec<ContextMenuAction> {
        let view = self.view.as_ref().map(|view| {
            let view = Rc::clone(view);
            ContextMenuAction::new(
                "View",
                Some("image-x-generic-symbolic"),
                false,
                Rc::new(move || view()),
            )
        });
        let bookmark = self.bookmark.as_ref().map(BookmarkAction::context_action);
        let copy_paths = vec![self.path.clone()];
        let copy = Rc::clone(&self.copy);
        let cut_paths = vec![self.path.clone()];
        let cut = Rc::clone(&self.cut);
        let rename_path = self.path.clone();
        let rename = Rc::clone(&self.rename);
        let delete_paths = vec![self.path.clone()];
        let delete = Rc::clone(&self.delete);

        let mut actions = Vec::new();
        actions.extend(view);
        actions.extend(bookmark);
        actions.extend([
            ContextMenuAction::new(
                "Copy",
                Some("edit-copy-symbolic"),
                false,
                Rc::new(move || copy(copy_paths.clone())),
            ),
            ContextMenuAction::new(
                "Cut",
                Some("edit-cut-symbolic"),
                false,
                Rc::new(move || cut(cut_paths.clone())),
            ),
            ContextMenuAction::new(
                "Rename",
                Some("edit-rename-symbolic"),
                false,
                Rc::new(move || rename(rename_path.clone())),
            ),
            ContextMenuAction::new(
                "Delete",
                Some("user-trash-symbolic"),
                true,
                Rc::new(move || delete(delete_paths.clone())),
            ),
        ]);
        actions
    }
}

pub struct FileMultiSelectionContext {
    paths: Vec<PathBuf>,
    view: Option<ViewAction>,
    copy: ClipboardAction,
    cut: ClipboardAction,
    delete: DeleteAction,
}

impl ContextMenuContext for FileMultiSelectionContext {
    fn actions(&self) -> Vec<ContextMenuAction> {
        let view = self.view.as_ref().map(|view| {
            let view = Rc::clone(view);
            ContextMenuAction::new(
                "View",
                Some("image-x-generic-symbolic"),
                false,
                Rc::new(move || view()),
            )
        });
        let copy_paths = self.paths.clone();
        let copy = Rc::clone(&self.copy);
        let copy_label = format!("Copy {} Items", copy_paths.len());
        let cut_paths = self.paths.clone();
        let cut = Rc::clone(&self.cut);
        let cut_label = format!("Cut {} Items", cut_paths.len());
        let delete_paths = self.paths.clone();
        let delete = Rc::clone(&self.delete);
        let delete_label = format!("Delete {} Items", delete_paths.len());

        let mut actions = Vec::new();
        actions.extend(view);
        actions.extend([
            ContextMenuAction::new(
                copy_label,
                Some("edit-copy-symbolic"),
                false,
                Rc::new(move || copy(copy_paths.clone())),
            ),
            ContextMenuAction::new(
                cut_label,
                Some("edit-cut-symbolic"),
                false,
                Rc::new(move || cut(cut_paths.clone())),
            ),
            ContextMenuAction::new(
                delete_label,
                Some("user-trash-symbolic"),
                true,
                Rc::new(move || delete(delete_paths.clone())),
            ),
        ]);
        actions
    }
}

fn action_button(popover: &gtk::Popover, action: ContextMenuAction) -> gtk::Button {
    let button = gtk::Button::builder()
        .halign(gtk::Align::Fill)
        .css_classes(["context-menu-item"])
        .build();
    if action.destructive {
        button.add_css_class("destructive-action");
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .halign(gtk::Align::Fill)
        .build();

    if let Some(icon_name) = action.icon_name {
        content.append(
            &gtk::Image::builder()
                .icon_name(icon_name)
                .pixel_size(16)
                .build(),
        );
    }

    content.append(
        &gtk::Label::builder()
            .label(action.label)
            .xalign(0.0)
            .hexpand(true)
            .build(),
    );
    button.set_child(Some(&content));

    let popover = popover.clone();
    let activate = Rc::clone(&action.activate);
    button.connect_clicked(move |_| {
        popover.popdown();
        activate();
    });

    button
}

#[cfg(test)]
mod tests {
    use super::{
        BookmarkAction, ClipboardAction, ContextMenuContext, DeleteAction, EmptySpaceContext,
        FileEntryContext, MenuAction, RenameAction, SidebarBookmarkContext, ViewAction,
    };
    use std::{path::PathBuf, rc::Rc};

    #[test]
    fn single_file_context_includes_clipboard_rename_and_delete() {
        let context = FileEntryContext::for_paths(
            vec![PathBuf::from("/tmp/file.txt")],
            None,
            None,
            noop_clipboard(),
            noop_clipboard(),
            noop_rename(),
            noop_delete(),
        )
        .expect("single context");

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Copy", "Cut", "Rename", "Delete"]);
    }

    #[test]
    fn image_file_context_includes_view_first() {
        let context = FileEntryContext::for_paths(
            vec![PathBuf::from("/tmp/photo.png")],
            Some(noop_view()),
            None,
            noop_clipboard(),
            noop_clipboard(),
            noop_rename(),
            noop_delete(),
        )
        .expect("single context");

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["View", "Copy", "Cut", "Rename", "Delete"]);
    }

    #[test]
    fn single_folder_context_can_include_bookmark() {
        let context = FileEntryContext::for_paths(
            vec![PathBuf::from("/tmp/folder")],
            None,
            Some(noop_bookmark_action("Add Bookmark")),
            noop_clipboard(),
            noop_clipboard(),
            noop_rename(),
            noop_delete(),
        )
        .expect("single context");

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Add Bookmark", "Copy", "Cut", "Rename", "Delete"]);
    }

    #[test]
    fn bookmarked_folder_context_can_include_remove_bookmark() {
        let context = FileEntryContext::for_paths(
            vec![PathBuf::from("/tmp/folder")],
            None,
            Some(noop_bookmark_action("Remove Bookmark")),
            noop_clipboard(),
            noop_clipboard(),
            noop_rename(),
            noop_delete(),
        )
        .expect("single context");

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            ["Remove Bookmark", "Copy", "Cut", "Rename", "Delete"]
        );
    }

    #[test]
    fn multi_file_context_includes_clipboard_and_delete() {
        let context = FileEntryContext::for_paths(
            vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.txt")],
            None,
            None,
            noop_clipboard(),
            noop_clipboard(),
            noop_rename(),
            noop_delete(),
        )
        .expect("multi context");

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Copy 2 Items", "Cut 2 Items", "Delete 2 Items"]);
    }

    #[test]
    fn empty_space_context_includes_paste_and_new_folder() {
        let context = EmptySpaceContext::new(
            noop_menu_action(),
            noop_menu_action(),
            noop_bookmark_action("Add Bookmark"),
        );

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Paste", "New Folder", "Add Bookmark"]);
    }

    #[test]
    fn sidebar_bookmark_context_removes_bookmark() {
        let context = SidebarBookmarkContext::new(noop_menu_action());

        let labels = context
            .actions()
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Remove Bookmark"]);
    }

    fn noop_menu_action() -> MenuAction {
        Rc::new(|| {})
    }

    fn noop_bookmark_action(label: &str) -> BookmarkAction {
        BookmarkAction::new(label, noop_menu_action())
    }

    fn noop_view() -> ViewAction {
        Rc::new(|| {})
    }

    fn noop_clipboard() -> ClipboardAction {
        Rc::new(|_| {})
    }

    fn noop_rename() -> RenameAction {
        Rc::new(|_| {})
    }

    fn noop_delete() -> DeleteAction {
        Rc::new(|_| {})
    }
}
