use gtk::prelude::*;

use crate::{config::ViewMode, sorting::SortOrder, ui::sort_menu::SortMenu};

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
    pub search_entry: gtk::SearchEntry,
    pub list_button: gtk::ToggleButton,
    pub icon_button: gtk::ToggleButton,
    pub sort_menu: SortMenu,
    pub show_hidden_button: gtk::ToggleButton,
    pub details_button: gtk::ToggleButton,
}

impl TopBar {
    pub fn new(default_view: ViewMode, show_hidden: bool, sort: SortOrder) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["topbar"])
            .build();

        let back_button = icon_button("go-previous-symbolic", "Back (Alt+Left)");
        let forward_button = icon_button("go-next-symbolic", "Forward (Alt+Right)");
        let up_button = icon_button("go-up-symbolic", "Up (Alt+Up)");
        let refresh_button = icon_button("view-refresh-symbolic", "Refresh");
        let new_folder_button = icon_button("folder-new-symbolic", "New Folder");
        let location_button = icon_button("document-edit-symbolic", "Edit Location");
        for button in [&refresh_button, &location_button] {
            button.add_css_class("address-button");
        }

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

        let address = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .hexpand(true)
            .css_classes(["address-bar"])
            .build();
        address.append(&path_stack);
        address.append(&location_button);
        address.append(&refresh_button);

        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search folder")
            .width_chars(14)
            .css_classes(["header-search"])
            .build();
        search_entry.set_tooltip_text(Some("Search this folder (Ctrl+F)"));

        let list_button = gtk::ToggleButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text("List View")
            .build();
        list_button.set_focusable(false);
        let icon_button = gtk::ToggleButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Icon View")
            .group(&list_button)
            .build();
        icon_button.set_focusable(false);
        let sort_menu = SortMenu::new(sort);
        let show_hidden_button = gtk::ToggleButton::builder()
            .icon_name("view-reveal-symbolic")
            .tooltip_text("Show Hidden Files")
            .active(show_hidden)
            .build();
        show_hidden_button.set_focusable(false);
        let details_button = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
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
        let view_switcher = toolbar_group(&[list_button.upcast_ref(), icon_button.upcast_ref()]);
        view_switcher.add_css_class("view-switcher");
        let view_group = toolbar_group(&[
            new_folder_button.upcast_ref(),
            sort_menu.button.upcast_ref(),
            show_hidden_button.upcast_ref(),
            details_button.upcast_ref(),
        ]);

        root.append(&nav_group);
        root.append(&address);
        root.append(&search_entry);
        root.append(&view_switcher);
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
            search_entry,
            list_button,
            icon_button,
            sort_menu,
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
        .spacing(2)
        .css_classes(["toolbar-group"])
        .build();

    for widget in widgets {
        group.append(*widget);
    }

    group
}
