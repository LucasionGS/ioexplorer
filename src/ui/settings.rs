use gtk::prelude::*;

use crate::config::{MAX_ICON_SIZE, MIN_ICON_SIZE, ViewMode};

pub struct SettingsPage {
    pub root: gtk::ScrolledWindow,
    pub show_hidden_check: gtk::CheckButton,
    pub list_button: gtk::ToggleButton,
    pub icon_button: gtk::ToggleButton,
    pub icon_size_down_button: gtk::Button,
    pub icon_size_up_button: gtk::Button,
    pub icon_size_scale: gtk::Scale,
    pub icon_size_value_label: gtk::Label,
}

impl SettingsPage {
    pub fn new(layout: ViewMode, show_hidden: bool, icon_size: i32) -> Self {
        let show_hidden_check = gtk::CheckButton::builder()
            .label("Show hidden files")
            .active(show_hidden)
            .build();

        let general = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["settings-tab"])
            .build();
        general.append(&settings_row("Files", &show_hidden_check));

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
        let layout_group = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .css_classes(["toolbar-group"])
            .build();
        layout_group.append(&list_button);
        layout_group.append(&icon_button);

        let icon_size_down_button = icon_button_control("zoom-out-symbolic", "Smaller");
        let icon_size_up_button = icon_button_control("zoom-in-symbolic", "Larger");
        let icon_size_adjustment = gtk::Adjustment::new(
            f64::from(icon_size),
            f64::from(MIN_ICON_SIZE),
            f64::from(MAX_ICON_SIZE),
            1.0,
            16.0,
            0.0,
        );
        let icon_size_scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&icon_size_adjustment)
            .draw_value(false)
            .hexpand(true)
            .build();
        icon_size_scale.add_mark(f64::from(MIN_ICON_SIZE), gtk::PositionType::Bottom, None);
        icon_size_scale.add_mark(f64::from(MAX_ICON_SIZE), gtk::PositionType::Bottom, None);
        let icon_size_value_label = gtk::Label::builder()
            .label(format!("{icon_size}px"))
            .width_chars(6)
            .xalign(1.0)
            .css_classes(["settings-value"])
            .build();
        let icon_size_controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .hexpand(true)
            .build();
        icon_size_controls.append(&icon_size_down_button);
        icon_size_controls.append(&icon_size_scale);
        icon_size_controls.append(&icon_size_up_button);
        icon_size_controls.append(&icon_size_value_label);

        let view = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["settings-tab"])
            .build();
        view.append(&settings_row("Layout", &layout_group));
        view.append(&settings_row("Icon size", &icon_size_controls));

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["settings-tab"])
            .build();

        let tabs = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        tabs.add_titled(&general, Some("general"), "General");
        tabs.add_titled(&view, Some("view"), "View");
        tabs.add_titled(&actions, Some("actions"), "Actions");

        let switcher = gtk::StackSwitcher::builder()
            .stack(&tabs)
            .halign(gtk::Align::Start)
            .css_classes(["settings-tabs"])
            .build();

        let title = gtk::Label::builder()
            .label("Settings")
            .xalign(0.0)
            .css_classes(["settings-title"])
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .css_classes(["settings-page"])
            .build();
        content.append(&title);
        content.append(&switcher);
        content.append(&tabs);

        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .css_classes(["content-scroll", "settings-page-scroll"])
            .build();

        let page = Self {
            root,
            show_hidden_check,
            list_button,
            icon_button,
            icon_size_down_button,
            icon_size_up_button,
            icon_size_scale,
            icon_size_value_label,
        };
        page.set_view_mode(layout);
        page
    }

    pub fn set_show_hidden(&self, show_hidden: bool) {
        self.show_hidden_check.set_active(show_hidden);
    }

    pub fn set_view_mode(&self, layout: ViewMode) {
        self.list_button.set_active(layout == ViewMode::List);
        self.icon_button.set_active(layout == ViewMode::Icon);
    }

    pub fn set_icon_size(&self, icon_size: i32) {
        self.icon_size_scale.set_value(f64::from(icon_size));
        self.icon_size_value_label
            .set_text(&format!("{icon_size}px"));
    }
}

fn settings_row(label: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let label = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["settings-row-label"])
        .build();
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .css_classes(["settings-row"])
        .build();
    row.append(&label);
    row.append(control);
    row
}

fn icon_button_control(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .css_classes(["toolbar-button"])
        .build();
    button.set_focusable(false);
    button
}
