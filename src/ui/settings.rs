use std::{cell::RefCell, path::Path, rc::Rc};

use gtk::prelude::*;

use crate::{
    config::{CustomActionConfig, MAX_ICON_SIZE, MIN_ICON_SIZE, ViewMode},
    theme::ThemeSettings,
};

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
    pub theme_window_background_button: ThemeColorControl,
    pub theme_panel_background_button: ThemeColorControl,
    pub theme_muted_background_button: ThemeColorControl,
    pub theme_accent_button: ThemeColorControl,
    pub theme_selection_button: ThemeColorControl,
    pub theme_text_button: ThemeColorControl,
    pub theme_border_button: ThemeColorControl,
    pub theme_radius_scale: gtk::Scale,
    pub theme_radius_value_label: gtk::Label,
    pub theme_reset_button: gtk::Button,
    pub theme_css_path_label: gtk::Label,
    pub action_add_button: gtk::Button,
    pub actions_list: gtk::ListBox,
}

type ThemeColorChangedCallback = Box<dyn Fn()>;

#[derive(Clone)]
pub struct ThemeColorControl {
    button: gtk::Button,
    swatch: gtk::DrawingArea,
    rgba: Rc<RefCell<gtk::gdk::RGBA>>,
    callbacks: Rc<RefCell<Vec<ThemeColorChangedCallback>>>,
}

impl ThemeColorControl {
    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    pub fn rgba(&self) -> gtk::gdk::RGBA {
        *self.rgba.borrow()
    }

    pub fn set_rgba(&self, rgba: &gtk::gdk::RGBA) {
        *self.rgba.borrow_mut() = *rgba;
        self.swatch.queue_draw();
    }

    pub fn connect_rgba_changed(&self, callback: impl Fn() + 'static) {
        self.callbacks.borrow_mut().push(Box::new(callback));
    }

    fn notify_changed(&self) {
        for callback in self.callbacks.borrow().iter() {
            callback();
        }
    }
}

impl SettingsPage {
    pub fn new(
        layout: ViewMode,
        show_hidden: bool,
        icon_size: i32,
        theme_settings: &ThemeSettings,
        theme_css_path: Option<&Path>,
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

        let theme_window_background_button = color_button(
            &theme_settings.window_background,
            "Window Background (supports transparency)",
        );
        let theme_panel_background_button = color_button(
            &theme_settings.panel_background,
            "Panel Background (supports transparency)",
        );
        let theme_muted_background_button = color_button(
            &theme_settings.muted_background,
            "Muted Background (supports transparency)",
        );
        let theme_accent_button =
            color_button(&theme_settings.accent, "Accent (supports transparency)");
        let theme_selection_button = color_button(
            &theme_settings.selection,
            "Selection (supports transparency)",
        );
        let theme_text_button = color_button(&theme_settings.text, "Text (supports transparency)");
        let theme_border_button =
            color_button(&theme_settings.border, "Border (supports transparency)");

        let theme_radius_adjustment = gtk::Adjustment::new(
            f64::from(theme_settings.corner_radius),
            0.0,
            18.0,
            1.0,
            3.0,
            0.0,
        );
        let theme_radius_scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&theme_radius_adjustment)
            .draw_value(false)
            .hexpand(true)
            .build();
        let theme_radius_value_label = gtk::Label::builder()
            .label(format!("{}px", theme_settings.corner_radius))
            .width_chars(6)
            .xalign(1.0)
            .css_classes(["settings-value"])
            .build();
        let theme_radius_controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .hexpand(true)
            .build();
        theme_radius_controls.append(&theme_radius_scale);
        theme_radius_controls.append(&theme_radius_value_label);

        let theme_reset_button = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Reset Theme")
            .css_classes(["toolbar-button"])
            .build();
        theme_reset_button.set_focusable(false);
        let theme_css_path_label = gtk::Label::builder()
            .label(theme_css_path_label(theme_css_path))
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .css_classes(["settings-value"])
            .build();

        let theme_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        theme_header.append(
            &gtk::Label::builder()
                .label("Theme")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["settings-row-label"])
                .build(),
        );
        theme_header.append(&theme_reset_button);

        let theme = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["settings-tab"])
            .build();
        theme.append(&theme_header);
        theme.append(&settings_row(
            "Window",
            theme_window_background_button.widget(),
        ));
        theme.append(&settings_row(
            "Panels",
            theme_panel_background_button.widget(),
        ));
        theme.append(&settings_row(
            "Muted",
            theme_muted_background_button.widget(),
        ));
        theme.append(&settings_row("Accent", theme_accent_button.widget()));
        theme.append(&settings_row("Selection", theme_selection_button.widget()));
        theme.append(&settings_row("Text", theme_text_button.widget()));
        theme.append(&settings_row("Border", theme_border_button.widget()));
        theme.append(&settings_row("Corner radius", &theme_radius_controls));
        theme.append(&settings_row("Generated CSS", &theme_css_path_label));

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
        tabs.add_titled(&theme, Some("theme"), "Theme");
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
            theme_window_background_button,
            theme_panel_background_button,
            theme_muted_background_button,
            theme_accent_button,
            theme_selection_button,
            theme_text_button,
            theme_border_button,
            theme_radius_scale,
            theme_radius_value_label,
            theme_reset_button,
            theme_css_path_label,
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

    pub fn theme_settings(&self) -> ThemeSettings {
        ThemeSettings {
            window_background: self.theme_window_background_button.rgba(),
            panel_background: self.theme_panel_background_button.rgba(),
            muted_background: self.theme_muted_background_button.rgba(),
            accent: self.theme_accent_button.rgba(),
            selection: self.theme_selection_button.rgba(),
            text: self.theme_text_button.rgba(),
            border: self.theme_border_button.rgba(),
            corner_radius: self.theme_radius_scale.value().round() as i32,
        }
    }

    pub fn set_theme_settings(&self, settings: &ThemeSettings) {
        self.theme_window_background_button
            .set_rgba(&settings.window_background);
        self.theme_panel_background_button
            .set_rgba(&settings.panel_background);
        self.theme_muted_background_button
            .set_rgba(&settings.muted_background);
        self.theme_accent_button.set_rgba(&settings.accent);
        self.theme_selection_button.set_rgba(&settings.selection);
        self.theme_text_button.set_rgba(&settings.text);
        self.theme_border_button.set_rgba(&settings.border);
        self.theme_radius_scale
            .set_value(f64::from(settings.corner_radius));
        self.theme_radius_value_label
            .set_text(&format!("{}px", settings.corner_radius));
    }

    pub fn set_theme_css_path(&self, path: Option<&Path>) {
        self.theme_css_path_label
            .set_text(&theme_css_path_label(path));
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

fn color_button(color: &gtk::gdk::RGBA, tooltip: &str) -> ThemeColorControl {
    let rgba = Rc::new(RefCell::new(*color));
    let callbacks = Rc::new(RefCell::new(Vec::<ThemeColorChangedCallback>::new()));
    let swatch = gtk::DrawingArea::builder()
        .content_width(42)
        .content_height(22)
        .build();
    let draw_rgba = Rc::clone(&rgba);
    swatch.set_draw_func(move |_, cr, width, height| {
        draw_transparency_grid(cr, width, height);
        let color = draw_rgba.borrow();
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
        let _ = cr.fill();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.34);
        cr.rectangle(0.5, 0.5, f64::from(width) - 1.0, f64::from(height) - 1.0);
        let _ = cr.stroke();
    });

    let button = gtk::Button::builder()
        .tooltip_text(tooltip)
        .css_classes(["settings-color-button"])
        .child(&swatch)
        .build();
    button.set_focusable(false);

    let control = ThemeColorControl {
        button,
        swatch,
        rgba,
        callbacks,
    };

    let click_control = control.clone();
    control
        .button
        .connect_clicked(move |_| open_theme_color_dialog(&click_control));

    control
}

fn draw_transparency_grid(cr: &gtk::cairo::Context, width: i32, height: i32) {
    let tile = 6.0;
    let columns = (f64::from(width) / tile).ceil() as i32;
    let rows = (f64::from(height) / tile).ceil() as i32;

    for row in 0..rows {
        for column in 0..columns {
            let shade = if (row + column) % 2 == 0 { 0.78 } else { 0.48 };
            cr.set_source_rgb(shade, shade, shade);
            cr.rectangle(f64::from(column) * tile, f64::from(row) * tile, tile, tile);
            let _ = cr.fill();
        }
    }
}

#[allow(deprecated)]
fn open_theme_color_dialog(control: &ThemeColorControl) {
    let parent = control
        .button
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok());
    let dialog = gtk::Dialog::builder()
        .title("Choose Theme Color")
        .modal(true)
        .resizable(true)
        .default_width(520)
        .default_height(440)
        .build();
    if let Some(parent) = parent.as_ref() {
        dialog.set_transient_for(Some(parent));
    }
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Select", gtk::ResponseType::Ok);
    dialog.set_default_response(gtk::ResponseType::Ok);

    let chooser = gtk::ColorChooserWidget::builder()
        .show_editor(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    chooser.set_size_request(440, 320);
    chooser.set_use_alpha(true);
    chooser.set_rgba(&control.rgba());

    let content = dialog.content_area();
    content.set_spacing(0);
    content.set_hexpand(true);
    content.set_vexpand(true);
    content.append(&chooser);

    let original = control.rgba();
    let live_control = control.clone();
    chooser.connect_rgba_notify(move |chooser| {
        live_control.set_rgba(&chooser.rgba());
        live_control.notify_changed();
    });

    let response_control = control.clone();
    dialog.connect_response(move |dialog, response| {
        if should_revert_theme_color_response(response) {
            response_control.set_rgba(&original);
            response_control.notify_changed();
        }
        dialog.close();
    });
    dialog.present();
}

fn should_revert_theme_color_response(response: gtk::ResponseType) -> bool {
    response == gtk::ResponseType::Cancel
}

fn theme_css_path_label(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "Not configured".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_color_select_responses_do_not_revert() {
        assert!(!should_revert_theme_color_response(gtk::ResponseType::Ok));
        assert!(!should_revert_theme_color_response(
            gtk::ResponseType::Accept
        ));
        assert!(!should_revert_theme_color_response(
            gtk::ResponseType::Apply
        ));
        assert!(!should_revert_theme_color_response(gtk::ResponseType::None));
        assert!(!should_revert_theme_color_response(
            gtk::ResponseType::Close
        ));
        assert!(!should_revert_theme_color_response(
            gtk::ResponseType::DeleteEvent
        ));
    }

    #[test]
    fn theme_color_cancel_response_reverts() {
        assert!(should_revert_theme_color_response(
            gtk::ResponseType::Cancel
        ));
    }
}
