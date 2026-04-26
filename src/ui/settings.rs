use std::rc::Rc;

use gtk::prelude::*;

use crate::config::{CustomActionConfig, MAX_ICON_SIZE, MIN_ICON_SIZE, ViewMode};

#[derive(Clone)]
pub struct ActionEditorCallbacks {
    pub edit: Rc<dyn Fn(usize)>,
    pub delete: Rc<dyn Fn(usize)>,
    pub move_up: Rc<dyn Fn(usize)>,
    pub move_down: Rc<dyn Fn(usize)>,
}

pub struct SettingsPage {
    pub root: gtk::ScrolledWindow,
    pub show_hidden_check: gtk::CheckButton,
    pub list_button: gtk::ToggleButton,
    pub icon_button: gtk::ToggleButton,
    pub icon_size_down_button: gtk::Button,
    pub icon_size_up_button: gtk::Button,
    pub icon_size_scale: gtk::Scale,
    pub icon_size_value_label: gtk::Label,
    pub action_add_button: gtk::Button,
    pub actions_list: gtk::ListBox,
}

impl SettingsPage {
    pub fn new(
        layout: ViewMode,
        show_hidden: bool,
        icon_size: i32,
        actions: &[CustomActionConfig],
    ) -> Self {
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

        let action_add_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Action")
            .css_classes(["toolbar-button"])
            .build();
        action_add_button.set_focusable(false);
        let actions_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        actions_header.append(
            &gtk::Label::builder()
                .label("Custom actions")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["settings-row-label"])
                .build(),
        );
        actions_header.append(&action_add_button);

        let actions_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["settings-actions-list"])
            .build();
        let actions_tab = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["settings-tab"])
            .build();
        actions_tab.append(&actions_header);
        actions_tab.append(&actions_list);

        let tabs = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        tabs.add_titled(&general, Some("general"), "General");
        tabs.add_titled(&view, Some("view"), "View");
        tabs.add_titled(&actions_tab, Some("actions"), "Actions");

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
            action_add_button,
            actions_list,
        };
        page.set_view_mode(layout);
        page.set_actions(actions);
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

    pub fn set_actions(&self, actions: &[CustomActionConfig]) {
        while let Some(row) = self.actions_list.row_at_index(0) {
            self.actions_list.remove(&row);
        }

        if actions.is_empty() {
            self.actions_list.append(&action_empty_row());
            return;
        }

        for action in actions {
            self.actions_list.append(&action_row(action));
        }
    }

    pub fn set_action_editor(
        &self,
        actions: &[CustomActionConfig],
        callbacks: ActionEditorCallbacks,
    ) {
        while let Some(row) = self.actions_list.row_at_index(0) {
            self.actions_list.remove(&row);
        }

        if actions.is_empty() {
            self.actions_list.append(&action_empty_row());
            return;
        }

        for (index, action) in actions.iter().enumerate() {
            self.actions_list.append(&action_editor_row(
                index,
                action,
                actions.len(),
                callbacks.clone(),
            ));
        }
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

fn action_row(action: &CustomActionConfig) -> gtk::ListBoxRow {
    action_row_with_child(action, None)
}

fn action_editor_row(
    index: usize,
    action: &CustomActionConfig,
    action_count: usize,
    callbacks: ActionEditorCallbacks,
) -> gtk::ListBoxRow {
    let move_up_button = action_icon_button("go-up-symbolic", "Move Up");
    move_up_button.set_sensitive(index > 0);
    let move_up = Rc::clone(&callbacks.move_up);
    move_up_button.connect_clicked(move |_| move_up(index));

    let move_down_button = action_icon_button("go-down-symbolic", "Move Down");
    move_down_button.set_sensitive(index + 1 < action_count);
    let move_down = Rc::clone(&callbacks.move_down);
    move_down_button.connect_clicked(move |_| move_down(index));

    let edit_button = action_icon_button("document-edit-symbolic", "Edit Action");
    let edit = Rc::clone(&callbacks.edit);
    edit_button.connect_clicked(move |_| edit(index));

    let delete_button = action_icon_button("user-trash-symbolic", "Delete Action");
    delete_button.add_css_class("destructive-action");
    let delete = Rc::clone(&callbacks.delete);
    delete_button.connect_clicked(move |_| delete(index));

    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .valign(gtk::Align::Start)
        .css_classes(["settings-action-controls"])
        .build();
    controls.append(&move_up_button);
    controls.append(&move_down_button);
    controls.append(&edit_button);
    controls.append(&delete_button);

    let controls: gtk::Widget = controls.upcast();
    action_row_with_child(action, Some(&controls))
}

fn action_row_with_child(
    action: &CustomActionConfig,
    trailing: Option<&gtk::Widget>,
) -> gtk::ListBoxRow {
    let title = gtk::Label::builder()
        .label(&action.label)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["settings-action-title"])
        .build();
    let command = gtk::Label::builder()
        .label(&action.command)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["settings-action-command"])
        .build();
    let filters = gtk::Label::builder()
        .label(action_filters_label(action))
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["settings-value"])
        .build();
    let run_mode = gtk::Label::builder()
        .label(action_run_mode_label(action))
        .xalign(0.0)
        .css_classes(["settings-value"])
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .css_classes(["settings-action-details"])
        .build();
    content.append(&title);
    content.append(&command);
    content.append(&run_mode);
    content.append(&filters);

    let row_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .css_classes(["settings-action-row"])
        .build();
    row_content.append(&content);
    if let Some(trailing) = trailing {
        row_content.append(trailing);
    }

    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&row_content)
        .build()
}

fn action_empty_row() -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label("No custom actions configured")
        .xalign(0.0)
        .css_classes(["settings-value", "settings-action-row"])
        .build();
    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&label)
        .build()
}

fn action_filters_label(action: &CustomActionConfig) -> String {
    if action.filters.is_empty() {
        "Filters: all files and folders".to_string()
    } else {
        format!("Filters: {}", action.filters.join(", "))
    }
}

fn action_run_mode_label(action: &CustomActionConfig) -> &'static str {
    if action.run_on_each {
        "Run: each selected entry"
    } else {
        "Run: once with all selected entries"
    }
}

fn action_icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .css_classes(["flat"])
        .build();
    button.set_focusable(false);
    button
}
