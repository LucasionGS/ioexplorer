use gtk::prelude::*;

use crate::config::ViewMode;

pub struct TopBar {
    pub root: gtk::Box,
    pub back_button: gtk::Button,
    pub forward_button: gtk::Button,
    pub up_button: gtk::Button,
    pub refresh_button: gtk::Button,
    pub new_folder_button: gtk::Button,
    pub location_button: gtk::Button,
    pub path_stack: gtk::Stack,
    pub breadcrumbs: gtk::Box,
    pub path_entry: gtk::Entry,
    pub list_button: gtk::ToggleButton,
    pub icon_button: gtk::ToggleButton,
    pub show_hidden_button: gtk::ToggleButton,
    pub details_button: gtk::ToggleButton,
}

impl TopBar {
    pub fn new(default_view: ViewMode, show_hidden: bool) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["topbar"])
            .build();

        let back_button = icon_button("go-previous-symbolic", "Back");
        let forward_button = icon_button("go-next-symbolic", "Forward");
        let up_button = icon_button("go-up-symbolic", "Up");
        let refresh_button = icon_button("view-refresh-symbolic", "Refresh");
        let new_folder_button = icon_button("folder-new-symbolic", "New Folder");
        let location_button = icon_button("document-edit-symbolic", "Edit Location");
        let search_button = icon_button("edit-find-symbolic", "Search");
        search_button.set_sensitive(false);

        let breadcrumbs = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .hexpand(true)
            .css_classes(["breadcrumbs"])
            .build();

        let path_entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Type a path or file:// URI")
            .css_classes(["path-entry"])
            .build();

        let path_stack = gtk::Stack::builder()
            .hexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .css_classes(["path-stack"])
            .build();
        path_stack.add_named(&breadcrumbs, Some("breadcrumbs"));
        path_stack.add_named(&path_entry, Some("entry"));
        path_stack.set_visible_child_name("breadcrumbs");

        let list_button = gtk::ToggleButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text("List View")
            .build();
        list_button.set_focusable(false);
        let icon_button = gtk::ToggleButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Icon View")
            .build();
        icon_button.set_focusable(false);
        let show_hidden_button = gtk::ToggleButton::builder()
            .icon_name("view-hidden-symbolic")
            .tooltip_text("Show Hidden Files")
            .active(show_hidden)
            .build();
        show_hidden_button.set_focusable(false);
        let details_button = gtk::ToggleButton::builder()
            .icon_name("dialog-information-symbolic")
            .tooltip_text("Details Pane")
            .active(true)
            .build();
        details_button.set_focusable(false);

        match default_view {
            ViewMode::List => list_button.set_active(true),
            ViewMode::Icon => icon_button.set_active(true),
        }

        let nav_group = toolbar_group(&[
            back_button.upcast_ref(),
            forward_button.upcast_ref(),
            up_button.upcast_ref(),
        ]);
        let file_group =
            toolbar_group(&[refresh_button.upcast_ref(), new_folder_button.upcast_ref()]);
        let utility_group =
            toolbar_group(&[location_button.upcast_ref(), search_button.upcast_ref()]);
        let view_group = toolbar_group(&[
            list_button.upcast_ref(),
            icon_button.upcast_ref(),
            show_hidden_button.upcast_ref(),
            details_button.upcast_ref(),
        ]);

        root.append(&nav_group);
        root.append(&file_group);
        root.append(&path_stack);
        root.append(&utility_group);
        root.append(&view_group);

        Self {
            root,
            back_button,
            forward_button,
            up_button,
            refresh_button,
            new_folder_button,
            location_button,
            path_stack,
            breadcrumbs,
            path_entry,
            list_button,
            icon_button,
            show_hidden_button,
            details_button,
        }
    }
}

fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .css_classes(["toolbar-button"])
        .build();
    button.set_focusable(false);
    button
}

fn toolbar_group(widgets: &[&gtk::Widget]) -> gtk::Box {
    let group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .css_classes(["toolbar-group"])
        .build();

    for widget in widgets {
        group.append(*widget);
    }

    group
}
