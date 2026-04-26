use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    time::{Duration, SystemTime},
};

use gdk_pixbuf::prelude::*;
use gio::prelude::{AppInfoExt, FileExt, FileExtManual, FileMonitorExt};
use gtk::prelude::*;
use url::Url;

use crate::{
    bookmarks,
    config::{AppConfig, ViewMode, clamp_icon_size},
    providers::{FileItem, FileKind, Provider, ProviderError, ProviderUri, local::LocalProvider},
    state::AppState,
    ui::{
        computer::ComputerPage,
        context_menu, dnd,
        settings::SettingsPage,
        sidebar::{Sidebar, SidebarSection},
        topbar::TopBar,
        views,
    },
};

const MOUSE_BUTTON_BACK: u32 = 8;
const MOUSE_BUTTON_FORWARD: u32 = 9;
const FOLDER_MONITOR_DEBOUNCE_MS: u64 = 250;
const IMAGE_VIEWER_MIN_ZOOM: f64 = 1.0;
const IMAGE_VIEWER_MAX_ZOOM: f64 = 8.0;
const IMAGE_VIEWER_ZOOM_STEP: f64 = 1.15;
const ICON_VIEW_ZOOM_STEP: i32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppPage {
    Files,
    Computer,
    Settings,
}

pub struct AppWindow {
    window: gtk::ApplicationWindow,
    provider: LocalProvider,
    config: AppConfig,
    current_uri: RefCell<ProviderUri>,
    back_stack: RefCell<Vec<ProviderUri>>,
    forward_stack: RefCell<Vec<ProviderUri>>,
    entries: RefCell<Vec<FileItem>>,
    view_mode: Cell<ViewMode>,
    show_hidden: Cell<bool>,
    icon_size: Cell<i32>,
    topbar: TopBar,
    sidebar: Sidebar,
    stack: gtk::Stack,
    computer_page: ComputerPage,
    settings_page: SettingsPage,
    list_box: gtk::ListBox,
    flow_box: gtk::FlowBox,
    grid_scroll: gtk::ScrolledWindow,
    selected_indices: Rc<RefCell<BTreeSet<usize>>>,
    anchor_index: Rc<Cell<Option<usize>>>,
    status_label: gtk::Label,
    filter_bar: gtk::Box,
    filter_label: gtk::Label,
    filter_clear_button: gtk::Button,
    all_entries: RefCell<Vec<FileItem>>,
    filter_text: RefCell<String>,
    pending_filter: RefCell<Option<glib::SourceId>>,
    clipboard_paths: RefCell<Vec<PathBuf>>,
    clipboard_operation: Cell<Option<FileClipboardOperation>>,
    folder_monitor: RefCell<Option<gio::FileMonitor>>,
    pending_folder_reload: RefCell<Option<glib::SourceId>>,
    mount_monitor: gio::UnixMountMonitor,
    active_page: Cell<AppPage>,
    bookmarks: RefCell<Vec<PathBuf>>,
    thumbnail_cache: views::icon::ThumbnailCache,
    pending_visible_thumbnail_load: Cell<bool>,
}

impl AppWindow {
    pub fn new(app: &gtk::Application, config: AppConfig) -> Rc<Self> {
        let provider = LocalProvider::new();
        let start_uri = home_uri().unwrap_or_else(|| provider.root());
        let state = AppState::load(&config);
        let topbar = TopBar::new(state.layout, state.show_hidden);
        let bookmarks = bookmarks::load();
        let sidebar = Sidebar::new(config.sidebar_width, &bookmarks);
        let computer_page = ComputerPage::new();
        let settings_page = SettingsPage::new(state.layout, state.show_hidden, state.icon_size);
        let selected_indices = Rc::new(RefCell::new(BTreeSet::new()));
        let anchor_index = Rc::new(Cell::new(None::<usize>));
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(false)
            .css_classes(["content-list"])
            .build();
        let flow_box = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(false)
            .valign(gtk::Align::Start)
            .max_children_per_line(12)
            .row_spacing(2)
            .column_spacing(2)
            .css_classes(["content-grid"])
            .build();

        let list_scroll = gtk::ScrolledWindow::builder()
            .child(&list_box)
            .css_classes(["content-scroll"])
            .build();
        let grid_scroll = gtk::ScrolledWindow::builder()
            .child(&flow_box)
            .css_classes(["content-scroll"])
            .build();

        let list_rubberband = rubberband_widget();
        let list_overlay = gtk::Overlay::builder().child(&list_scroll).build();
        list_overlay.add_overlay(&list_rubberband);
        let clear_selection = clear_selection_handler(&selected_indices, &list_box, &flow_box);
        let apply_selection = apply_selection_handler(&selected_indices, &list_box, &flow_box);
        install_list_empty_space_selection(
            &list_overlay,
            &list_rubberband,
            &list_box,
            clear_selection.clone(),
            apply_selection.clone(),
        );

        let grid_rubberband = rubberband_widget();
        let grid_overlay = gtk::Overlay::builder().child(&grid_scroll).build();
        grid_overlay.add_overlay(&grid_rubberband);
        install_grid_empty_space_selection(
            &grid_overlay,
            &grid_rubberband,
            &flow_box,
            clear_selection,
            apply_selection,
        );

        let stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.set_focusable(true);
        stack.add_named(&list_overlay, Some("list"));
        stack.add_named(&grid_overlay, Some("icon"));
        stack.add_named(&computer_page.root, Some("computer"));
        stack.add_named(&settings_page.root, Some("settings"));

        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["status-label"])
            .build();

        let filter_icon = gtk::Image::builder()
            .icon_name("edit-find-symbolic")
            .build();
        let filter_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["filter-label"])
            .build();
        let filter_clear_button = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat"])
            .build();
        filter_clear_button.set_focusable(false);
        let filter_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["filter-bar"])
            .visible(false)
            .build();
        filter_bar.append(&filter_icon);
        filter_bar.append(&filter_label);
        filter_bar.append(&filter_clear_button);

        let body = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar.root)
            .end_child(&stack)
            .resize_start_child(false)
            .shrink_start_child(false)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .build();
        root.append(&topbar.root);
        root.append(&body);
        root.append(&filter_bar);
        root.append(&status_label);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(format!("IoExplorer - {}", provider.name()))
            .default_width(1120)
            .default_height(760)
            .child(&root)
            .build();

        let this = Rc::new(Self {
            window,
            provider,
            current_uri: RefCell::new(start_uri.clone()),
            back_stack: RefCell::new(Vec::new()),
            forward_stack: RefCell::new(Vec::new()),
            entries: RefCell::new(Vec::new()),
            view_mode: Cell::new(state.layout),
            show_hidden: Cell::new(state.show_hidden),
            icon_size: Cell::new(state.icon_size),
            config,
            topbar,
            sidebar,
            stack,
            computer_page,
            settings_page,
            list_box,
            flow_box,
            grid_scroll,
            selected_indices,
            anchor_index,
            status_label,
            filter_bar,
            filter_label,
            filter_clear_button,
            all_entries: RefCell::new(Vec::new()),
            filter_text: RefCell::new(String::new()),
            pending_filter: RefCell::new(None),
            clipboard_paths: RefCell::new(Vec::new()),
            clipboard_operation: Cell::new(None),
            folder_monitor: RefCell::new(None),
            pending_folder_reload: RefCell::new(None),
            mount_monitor: gio::UnixMountMonitor::get(),
            active_page: Cell::new(AppPage::Files),
            bookmarks: RefCell::new(bookmarks),
            thumbnail_cache: views::icon::new_thumbnail_cache(),
            pending_visible_thumbnail_load: Cell::new(false),
        });

        this.setup_callbacks();
        this.apply_view_mode(this.view_mode.get());
        this.load_uri(start_uri, false);
        this
    }

    pub fn present(&self) {
        self.window.present();
        self.focus_content();
        let stack = self.stack.clone();
        glib::idle_add_local_once(move || {
            stack.grab_focus();
        });
    }

    fn focus_content(&self) {
        self.stack.grab_focus();
    }

    pub fn navigate_to_path(self: &Rc<Self>, path: PathBuf) {
        self.load_uri(ProviderUri::local(path), true);
    }

    pub fn open_paths(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let Some(first_path) = paths.first().cloned() else {
            return;
        };

        if first_path.is_dir() {
            self.navigate_to_path(first_path);
        } else {
            self.reveal_paths(paths);
        }
    }

    pub fn reveal_paths(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let Some(first_path) = paths.first() else {
            return;
        };
        let folder = reveal_folder_for_path(first_path);

        self.load_uri(ProviderUri::local(&folder), true);
        self.select_paths_in_current_folder(&folder, &paths);
    }

    fn setup_callbacks(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.topbar.path_entry.connect_activate(move |entry| {
            this.navigate_from_input(entry.text().as_str());
        });

        let this = Rc::clone(self);
        self.topbar
            .location_button
            .connect_clicked(move |_| this.show_location_entry());

        let this = Rc::clone(self);
        self.topbar
            .back_button
            .connect_clicked(move |_| this.go_back());

        let this = Rc::clone(self);
        self.topbar
            .forward_button
            .connect_clicked(move |_| this.go_forward());

        let this = Rc::clone(self);
        self.topbar.up_button.connect_clicked(move |_| this.go_up());

        let this = Rc::clone(self);
        self.topbar
            .refresh_button
            .connect_clicked(move |_| this.refresh());

        let this = Rc::clone(self);
        self.topbar
            .new_folder_button
            .connect_clicked(move |_| this.create_folder());

        let this = Rc::clone(self);
        self.topbar.list_button.connect_toggled(move |button| {
            if button.is_active() {
                this.apply_view_mode(ViewMode::List);
            }
        });

        let this = Rc::clone(self);
        self.topbar.icon_button.connect_toggled(move |button| {
            if button.is_active() {
                this.apply_view_mode(ViewMode::Icon);
            }
        });

        let this = Rc::clone(self);
        self.topbar
            .show_hidden_button
            .connect_toggled(move |button| this.set_show_hidden(button.is_active()));

        let this = Rc::clone(self);
        self.sidebar
            .computer_button
            .connect_clicked(move |_| this.show_computer_page());

        let this = Rc::clone(self);
        self.sidebar
            .settings_button
            .connect_clicked(move |_| this.show_settings_page());

        let this = Rc::clone(self);
        self.settings_page
            .show_hidden_check
            .connect_toggled(move |button| this.set_show_hidden(button.is_active()));

        let this = Rc::clone(self);
        self.settings_page
            .list_button
            .connect_toggled(move |button| {
                if button.is_active() {
                    this.apply_view_mode(ViewMode::List);
                }
            });

        let this = Rc::clone(self);
        self.settings_page
            .icon_button
            .connect_toggled(move |button| {
                if button.is_active() {
                    this.apply_view_mode(ViewMode::Icon);
                }
            });

        let this = Rc::clone(self);
        self.settings_page
            .icon_size_down_button
            .connect_clicked(move |_| this.change_icon_size(-ICON_VIEW_ZOOM_STEP));

        let this = Rc::clone(self);
        self.settings_page
            .icon_size_up_button
            .connect_clicked(move |_| this.change_icon_size(ICON_VIEW_ZOOM_STEP));

        let this = Rc::clone(self);
        self.settings_page
            .icon_size_scale
            .connect_value_changed(move |scale| {
                this.set_icon_size(scale.value().round() as i32);
            });

        let this = Rc::clone(self);
        self.computer_page
            .list
            .connect_row_activated(move |_, row| {
                if let Some(path) = this
                    .computer_page
                    .mount_path_at(row.index().max(0) as usize)
                {
                    this.load_uri(ProviderUri::local(path), true);
                }
            });

        let this = Rc::clone(self);
        self.mount_monitor
            .connect_mounts_changed(move |_| this.refresh_computer_page_if_visible());

        let this = Rc::clone(self);
        self.mount_monitor
            .connect_mountpoints_changed(move |_| this.refresh_computer_page_if_visible());

        let this = Rc::clone(self);
        self.sidebar.list.connect_row_activated(move |_, row| {
            if let Some(place) = this.sidebar.place_at(row.index() as usize) {
                this.load_uri(place.uri.clone(), true);
            }
        });

        let sidebar_right_click = gtk::GestureClick::new();
        sidebar_right_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let sidebar_list = self.sidebar.list.clone();
        let this = Rc::clone(self);
        sidebar_right_click.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = picked_list_box_row(&sidebar_list, x, y) else {
                return;
            };
            let Some(place) = this.sidebar.place_at(row.index().max(0) as usize) else {
                return;
            };
            if !place.is_bookmark {
                return;
            }
            let Ok(path) = place.uri.local_path() else {
                return;
            };

            gesture.set_state(gtk::EventSequenceState::Claimed);
            this.show_sidebar_bookmark_context_menu(path, sidebar_list.clone().upcast(), x, y);
        });
        self.sidebar.list.add_controller(sidebar_right_click);

        let this = Rc::clone(self);
        self.list_box.connect_row_activated(move |_, row| {
            // Capture the item now; defer activation so the signal handler returns
            // before we potentially rebuild the list (prevents stale-pointer crash).
            let item = this
                .entries
                .borrow()
                .get(row.index().max(0) as usize)
                .cloned();
            if let Some(item) = item {
                let this = Rc::clone(&this);
                glib::idle_add_local_once(move || this.activate_entry(item));
            }
        });

        let this = Rc::clone(self);
        self.flow_box.connect_child_activated(move |_, child| {
            let item = this
                .entries
                .borrow()
                .get(child.index().max(0) as usize)
                .cloned();
            if let Some(item) = item {
                let this = Rc::clone(&this);
                glib::idle_add_local_once(move || this.activate_entry(item));
            }
        });

        let this = Rc::clone(self);
        dnd::install_drop_target(&self.stack, move |payload| {
            this.transfer_dropped_payload(payload);
        });

        let this = Rc::clone(self);
        self.grid_scroll
            .vadjustment()
            .connect_value_changed(move |_| this.queue_visible_thumbnail_load());

        let this = Rc::clone(self);
        self.grid_scroll
            .vadjustment()
            .connect_page_size_notify(move |_| this.queue_visible_thumbnail_load());

        self.install_icon_view_zoom_controls();

        let right_click = gtk::GestureClick::new();
        right_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let stack = self.stack.clone();
        let this = Rc::clone(self);
        right_click.connect_pressed(move |_, _, x, y| {
            if this.active_page.get() != AppPage::Files {
                return;
            }

            if hit_widget_type(&stack, x, y, gtk::ListBoxRow::static_type())
                || hit_widget_type(&stack, x, y, gtk::FlowBoxChild::static_type())
            {
                return;
            }

            this.show_empty_space_context_menu(stack.clone().upcast(), x, y);
        });
        self.stack.add_controller(right_click);

        self.install_mouse_history_actions();
        self.install_keyboard_actions();

        let this = Rc::clone(self);
        self.filter_clear_button.connect_clicked(move |_| {
            this.clear_filter();
        });
    }

    fn install_keyboard_actions(self: &Rc<Self>) {
        let controller = gtk::EventControllerKey::new();
        let this = Rc::clone(self);
        controller.connect_key_pressed(move |_, key, _, state| {
            let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            match key {
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
                    if this.file_shortcuts_enabled() && this.is_filtering() =>
                {
                    let item = this
                        .selected_indices
                        .try_borrow()
                        .ok()
                        .and_then(|sel| sel.iter().next().copied())
                        .and_then(|idx| this.entries.borrow().get(idx).cloned());
                    if let Some(item) = item {
                        this.clear_filter();
                        this.activate_entry(item);
                    }
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
                    if this.file_shortcuts_enabled() && !this.is_filtering() =>
                {
                    this.activate_selection();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Up | gtk::gdk::Key::KP_Up if this.file_shortcuts_enabled() => {
                    this.move_selection_by(-1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Down | gtk::gdk::Key::KP_Down if this.file_shortcuts_enabled() => {
                    this.move_selection_by(1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Left | gtk::gdk::Key::KP_Left
                    if this.file_shortcuts_enabled() && this.view_mode.get() == ViewMode::Icon =>
                {
                    this.move_selection_by(-1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Right | gtk::gdk::Key::KP_Right
                    if this.file_shortcuts_enabled() && this.view_mode.get() == ViewMode::Icon =>
                {
                    this.move_selection_by(1);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::F5 => {
                    this.refresh();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::BackSpace if alt => {
                    this.go_up();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::BackSpace
                    if this.file_shortcuts_enabled() && !this.is_filtering() =>
                {
                    this.go_up();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::BackSpace
                    if this.file_shortcuts_enabled() && this.is_filtering() =>
                {
                    let empty = {
                        let mut filter = this.filter_text.borrow_mut();
                        filter.pop();
                        filter.is_empty()
                    };
                    if empty {
                        this.clear_filter();
                    } else {
                        this.trigger_filter();
                    }
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::F2 if this.file_shortcuts_enabled() => {
                    this.rename_selection();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Delete | gtk::gdk::Key::KP_Delete
                    if this.file_shortcuts_enabled() =>
                {
                    this.delete_selection();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::c | gtk::gdk::Key::C if this.file_shortcuts_enabled() && ctrl => {
                    this.copy_selection_to_clipboard(FileClipboardOperation::Copy);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::x | gtk::gdk::Key::X if this.file_shortcuts_enabled() && ctrl => {
                    this.copy_selection_to_clipboard(FileClipboardOperation::Cut);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::v | gtk::gdk::Key::V if this.file_shortcuts_enabled() && ctrl => {
                    this.paste_from_clipboard();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::h | gtk::gdk::Key::H if this.file_shortcuts_enabled() && ctrl => {
                    this.set_show_hidden(!this.show_hidden.get());
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::l if ctrl && this.active_page.get() == AppPage::Files => {
                    this.show_location_entry();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Escape => {
                    if this.is_filtering() {
                        this.clear_filter();
                    } else {
                        this.show_breadcrumbs();
                    }
                    glib::Propagation::Stop
                }
                _ if this.file_shortcuts_enabled() && !ctrl && !alt => {
                    if let Some(c) = key.to_unicode()
                        && !c.is_control()
                    {
                        this.filter_text.borrow_mut().push(c);
                        this.trigger_filter();
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(controller);
    }

    fn install_mouse_history_actions(self: &Rc<Self>) {
        let back_click = gtk::GestureClick::new();
        back_click.set_button(MOUSE_BUTTON_BACK);
        back_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        back_click.connect_pressed(move |gesture, _, _, _| {
            this.go_back();
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.window.add_controller(back_click);

        let forward_click = gtk::GestureClick::new();
        forward_click.set_button(MOUSE_BUTTON_FORWARD);
        forward_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        forward_click.connect_pressed(move |gesture, _, _, _| {
            this.go_forward();
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        self.window.add_controller(forward_click);
    }

    fn file_shortcuts_enabled(&self) -> bool {
        self.active_page.get() == AppPage::Files && !self.topbar.path_entry.has_focus()
    }

    fn is_filtering(&self) -> bool {
        !self.filter_text.borrow().is_empty()
    }

    fn apply_filter_to_entries(self: &Rc<Self>) {
        let filter = self.filter_text.borrow().to_lowercase();
        let filtered: Vec<FileItem> = if filter.is_empty() {
            self.all_entries.borrow().clone()
        } else {
            self.all_entries
                .borrow()
                .iter()
                .filter(|item| {
                    item.name.to_lowercase().contains(&filter)
                        || item.display_name().to_lowercase().contains(&filter)
                })
                .cloned()
                .collect()
        };
        if !filter.is_empty() {
            let total = self.all_entries.borrow().len();
            let shown = filtered.len();
            self.status_label.set_text(&format!(
                "Showing {shown} of {total} items matching \"{filter}\""
            ));
        }
        *self.entries.borrow_mut() = filtered;
        self.render_entries();
        if !filter.is_empty() && !self.entries.borrow().is_empty() {
            set_entry_selection(
                &self.selected_indices,
                &self.list_box,
                &self.flow_box,
                [0].into_iter().collect(),
            );
        }
        self.update_filter_bar();
    }

    fn update_filter_bar(&self) {
        let filter = self.filter_text.borrow();
        if filter.is_empty() {
            self.filter_bar.set_visible(false);
        } else {
            self.filter_label.set_text(&format!("Filter: {}", *filter));
            self.filter_bar.set_visible(true);
        }
    }

    fn trigger_filter(self: &Rc<Self>) {
        let count = self.all_entries.borrow().len();
        if count < 1000 {
            self.apply_filter_to_entries();
        } else {
            if let Ok(mut pending) = self.pending_filter.try_borrow_mut()
                && let Some(id) = pending.take()
            {
                id.remove();
            }
            self.update_filter_bar();
            let this = Rc::clone(self);
            let id = glib::timeout_add_local_once(Duration::from_millis(500), move || {
                *this.pending_filter.borrow_mut() = None;
                this.apply_filter_to_entries();
            });
            if let Ok(mut pending) = self.pending_filter.try_borrow_mut() {
                *pending = Some(id);
            }
        }
    }

    fn clear_filter(self: &Rc<Self>) {
        self.cancel_pending_filter();
        *self.filter_text.borrow_mut() = String::new();
        self.apply_filter_to_entries();
        self.update_full_status();
    }

    fn cancel_pending_filter(&self) {
        if let Ok(mut pending) = self.pending_filter.try_borrow_mut()
            && let Some(id) = pending.take()
        {
            id.remove();
        }
    }

    fn update_full_status(&self) {
        let uri = self.current_uri.borrow().clone();
        let count = self.all_entries.borrow().len();
        let current_kind = self
            .provider
            .metadata(&uri)
            .ok()
            .map(|item| format!(" - {}", item.kind.label()))
            .unwrap_or_default();
        let status = format!("{count} items - {}", uri.display_path()) + &current_kind;
        self.status_label.set_text(&status);
    }

    fn navigate_from_input(self: &Rc<Self>, input: &str) {
        match ProviderUri::from_str(input) {
            Ok(uri) => {
                self.show_breadcrumbs();
                self.load_uri(uri, true);
            }
            Err(error) => self.show_error(error),
        }
    }

    fn show_location_entry(&self) {
        self.topbar
            .path_entry
            .set_text(&self.current_uri.borrow().display_path());
        self.topbar.path_stack.set_visible_child_name("entry");
        self.topbar.path_entry.grab_focus();
        self.topbar.path_entry.select_region(0, -1);
    }

    fn show_breadcrumbs(&self) {
        self.topbar.path_stack.set_visible_child_name("breadcrumbs");
    }

    fn load_uri(self: &Rc<Self>, uri: ProviderUri, push_history: bool) {
        if uri.provider() != self.provider.id() {
            self.show_error(ProviderError::UnsupportedProvider(
                uri.provider().to_string(),
            ));
            return;
        }

        match self.list_visible_items(&uri) {
            Ok(items) => {
                self.active_page.set(AppPage::Files);
                self.sidebar.set_active_section(SidebarSection::Files);
                self.set_file_topbar_state();
                self.apply_view_mode(self.view_mode.get());

                if push_history {
                    self.back_stack
                        .borrow_mut()
                        .push(self.current_uri.borrow().clone());
                    self.forward_stack.borrow_mut().clear();
                }

                *self.current_uri.borrow_mut() = uri.clone();
                self.watch_current_folder(&uri);
                // Cancel any pending debounce timer and clear filter on navigation
                self.cancel_pending_filter();
                *self.filter_text.borrow_mut() = String::new();
                self.anchor_index.set(None);
                *self.all_entries.borrow_mut() = items;
                self.topbar.path_entry.set_text(&uri.display_path());
                self.render_breadcrumbs();
                self.apply_filter_to_entries();
                self.update_navigation_buttons();
                let current_kind = self
                    .provider
                    .metadata(&uri)
                    .ok()
                    .map(|item| format!(" - {}", item.kind.label()))
                    .unwrap_or_default();

                let status = format!(
                    "{} items - {}",
                    self.all_entries.borrow().len(),
                    uri.display_path()
                ) + &current_kind;
                self.status_label.set_text(&status);
                self.focus_content();
            }
            Err(error) => self.show_error(error),
        }
    }

    fn show_computer_page(self: &Rc<Self>) {
        self.active_page.set(AppPage::Computer);
        self.sidebar.set_active_section(SidebarSection::Computer);
        self.cancel_pending_filter();
        self.cancel_pending_folder_reload();
        if let Some(monitor) = self.folder_monitor.borrow_mut().take() {
            monitor.cancel();
        }
        *self.filter_text.borrow_mut() = String::new();
        self.update_filter_bar();
        clear_entry_selection(&self.selected_indices, &self.list_box, &self.flow_box);

        self.computer_page.refresh();
        self.render_computer_breadcrumb();
        self.topbar.path_entry.set_text("This PC");
        self.show_breadcrumbs();
        self.stack.set_visible_child_name("computer");
        self.set_computer_topbar_state();
        self.update_computer_status();
        self.focus_content();
    }

    fn show_settings_page(self: &Rc<Self>) {
        self.active_page.set(AppPage::Settings);
        self.sidebar.set_active_section(SidebarSection::Settings);
        self.cancel_pending_filter();
        self.cancel_pending_folder_reload();
        if let Some(monitor) = self.folder_monitor.borrow_mut().take() {
            monitor.cancel();
        }
        *self.filter_text.borrow_mut() = String::new();
        self.update_filter_bar();
        clear_entry_selection(&self.selected_indices, &self.list_box, &self.flow_box);

        self.render_settings_breadcrumb();
        self.topbar.path_entry.set_text("Settings");
        self.show_breadcrumbs();
        self.stack.set_visible_child_name("settings");
        self.set_settings_topbar_state();
        self.status_label.set_text("Settings");
        self.focus_content();
    }

    fn refresh_computer_page_if_visible(&self) {
        if self.active_page.get() == AppPage::Computer {
            self.computer_page.refresh();
            self.update_computer_status();
        }
    }

    fn update_computer_status(&self) {
        let count = self.computer_page.volume_count();
        self.status_label
            .set_text(&format!("{count} mounted volume(s) - This PC"));
    }

    fn render_computer_breadcrumb(&self) {
        views::clear_box_children(&self.topbar.breadcrumbs);

        let button = gtk::Button::builder()
            .css_classes(["breadcrumb-button"])
            .sensitive(false)
            .build();
        button.set_child(Some(&breadcrumb_content_with_icon(
            "This PC",
            "computer-symbolic",
        )));
        self.topbar.breadcrumbs.append(&button);
    }

    fn render_settings_breadcrumb(&self) {
        views::clear_box_children(&self.topbar.breadcrumbs);

        let button = gtk::Button::builder()
            .css_classes(["breadcrumb-button"])
            .sensitive(false)
            .build();
        button.set_child(Some(&breadcrumb_content_with_icon(
            "Settings",
            "preferences-system-symbolic",
        )));
        self.topbar.breadcrumbs.append(&button);
    }

    fn set_file_topbar_state(&self) {
        self.topbar.new_folder_button.set_sensitive(true);
        self.topbar.location_button.set_sensitive(true);
        self.topbar.list_button.set_sensitive(true);
        self.topbar.icon_button.set_sensitive(true);
        self.topbar.show_hidden_button.set_sensitive(true);
        self.topbar
            .show_hidden_button
            .set_active(self.show_hidden.get());
        self.settings_page.set_show_hidden(self.show_hidden.get());
        self.settings_page.set_view_mode(self.view_mode.get());
        self.settings_page.set_icon_size(self.icon_size.get());
        self.update_navigation_buttons();
    }

    fn set_computer_topbar_state(&self) {
        self.topbar.back_button.set_sensitive(false);
        self.topbar.forward_button.set_sensitive(false);
        self.topbar.up_button.set_sensitive(false);
        self.topbar.refresh_button.set_sensitive(true);
        self.topbar.new_folder_button.set_sensitive(false);
        self.topbar.location_button.set_sensitive(false);
        self.topbar.list_button.set_sensitive(false);
        self.topbar.icon_button.set_sensitive(false);
        self.topbar.show_hidden_button.set_sensitive(false);
    }

    fn set_settings_topbar_state(&self) {
        self.topbar.back_button.set_sensitive(false);
        self.topbar.forward_button.set_sensitive(false);
        self.topbar.up_button.set_sensitive(false);
        self.topbar.refresh_button.set_sensitive(false);
        self.topbar.new_folder_button.set_sensitive(false);
        self.topbar.location_button.set_sensitive(false);
        self.topbar.list_button.set_sensitive(false);
        self.topbar.icon_button.set_sensitive(false);
        self.topbar.show_hidden_button.set_sensitive(false);
        self.settings_page.set_show_hidden(self.show_hidden.get());
        self.settings_page.set_view_mode(self.view_mode.get());
        self.settings_page.set_icon_size(self.icon_size.get());
    }

    fn list_visible_items(&self, uri: &ProviderUri) -> Result<Vec<FileItem>, ProviderError> {
        let mut items = self.provider.list(uri)?;
        if !self.show_hidden.get() {
            items.retain(|item| !item.hidden);
        }
        Ok(items)
    }

    fn watch_current_folder(self: &Rc<Self>, uri: &ProviderUri) {
        self.cancel_pending_folder_reload();
        if let Some(monitor) = self.folder_monitor.borrow_mut().take() {
            monitor.cancel();
        }

        let Ok(path) = uri.local_path() else {
            return;
        };

        let file = gio::File::for_path(&path);
        let monitor = match file.monitor_directory(
            gio::FileMonitorFlags::WATCH_MOVES,
            None::<&gio::Cancellable>,
        ) {
            Ok(monitor) => monitor,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to watch folder");
                return;
            }
        };

        monitor.set_rate_limit(FOLDER_MONITOR_DEBOUNCE_MS as i32);
        let weak_self = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, event| {
            if folder_monitor_event_affects_listing(event)
                && let Some(this) = weak_self.upgrade()
            {
                this.queue_folder_reload(path.clone());
            }
        });

        *self.folder_monitor.borrow_mut() = Some(monitor);
    }

    fn queue_folder_reload(self: &Rc<Self>, watched_path: PathBuf) {
        if !self.is_current_local_path(&watched_path) {
            return;
        }

        if self.pending_folder_reload.borrow().is_some() {
            return;
        }

        let this = Rc::clone(self);
        let source_id = glib::timeout_add_local_once(
            Duration::from_millis(FOLDER_MONITOR_DEBOUNCE_MS),
            move || {
                *this.pending_folder_reload.borrow_mut() = None;
                if this.is_current_local_path(&watched_path) {
                    this.reload_current_folder_entries();
                }
            },
        );
        *self.pending_folder_reload.borrow_mut() = Some(source_id);
    }

    fn reload_current_folder_entries(self: &Rc<Self>) {
        let uri = self.current_uri.borrow().clone();
        match self.list_visible_items(&uri) {
            Ok(items) => {
                *self.all_entries.borrow_mut() = items;
                self.topbar.path_entry.set_text(&uri.display_path());
                self.render_breadcrumbs();
                self.apply_filter_to_entries();
                self.update_navigation_buttons();
                if !self.is_filtering() {
                    self.update_full_status();
                }
                self.focus_content();
            }
            Err(error) => self.show_error(error),
        }
    }

    fn cancel_pending_folder_reload(&self) {
        if let Some(source_id) = self.pending_folder_reload.borrow_mut().take() {
            source_id.remove();
        }
    }

    fn is_current_local_path(&self, path: &Path) -> bool {
        self.current_uri
            .borrow()
            .local_path()
            .is_ok_and(|current| current == path)
    }

    fn render_entries(self: &Rc<Self>) {
        let entries = self.entries.borrow().clone();
        let folder_drop_handler: views::FolderDropHandler = {
            let this = Rc::clone(self);
            Rc::new(move |target_dir, payload| {
                this.transfer_drop_payload_into_target(payload, target_dir);
            })
        };
        let list_drag_handler: views::FileDragHandler = {
            let this = Rc::clone(self);
            Rc::new(move |index| this.selected_paths_from_list_index(index))
        };
        let grid_drag_handler: views::FileDragHandler = {
            let this = Rc::clone(self);
            Rc::new(move |index| this.selected_paths_from_grid_index(index))
        };
        let selection_handler = entry_selection_handler(
            &self.selected_indices,
            &self.anchor_index,
            &self.list_box,
            &self.flow_box,
        );
        let context_menu_handler: views::EntryContextMenuHandler = {
            let this = Rc::clone(self);
            Rc::new(move |index, parent, x, y| this.show_file_context_menu(index, parent, x, y))
        };

        views::list::populate(
            &self.list_box,
            &entries,
            &self.config.list_columns,
            folder_drop_handler.clone(),
            list_drag_handler,
            selection_handler.clone(),
            context_menu_handler.clone(),
        );
        views::icon::populate(
            &self.flow_box,
            &entries,
            views::icon::IconViewOptions {
                icon_size: self.icon_size.get(),
                thumbnail_cache: Rc::clone(&self.thumbnail_cache),
            },
            folder_drop_handler,
            grid_drag_handler,
            selection_handler,
            context_menu_handler,
        );
        clear_entry_selection(&self.selected_indices, &self.list_box, &self.flow_box);
        self.queue_visible_thumbnail_load();
    }

    fn queue_visible_thumbnail_load(self: &Rc<Self>) {
        if self.pending_visible_thumbnail_load.replace(true) {
            return;
        }

        let this = Rc::clone(self);
        glib::idle_add_local_once(move || {
            this.pending_visible_thumbnail_load.set(false);
            this.load_visible_thumbnails();
        });
    }

    fn load_visible_thumbnails(&self) {
        let entries = self.entries.borrow().clone();
        views::icon::load_visible_thumbnails(
            &self.flow_box,
            &self.grid_scroll,
            &entries,
            views::icon::IconViewOptions {
                icon_size: self.icon_size.get(),
                thumbnail_cache: Rc::clone(&self.thumbnail_cache),
            },
        );
    }

    fn render_breadcrumbs(self: &Rc<Self>) {
        views::clear_box_children(&self.topbar.breadcrumbs);

        let current = self.current_uri.borrow().clone();
        let mut uri = ProviderUri::root(current.provider());
        self.append_breadcrumb("/", uri.clone());

        for component in current
            .path()
            .split('/')
            .filter(|component| !component.is_empty())
        {
            uri = uri.child(component);
            self.append_breadcrumb(component, uri.clone());
        }
    }

    fn append_breadcrumb(self: &Rc<Self>, label: &str, uri: ProviderUri) {
        let button = gtk::Button::builder()
            .css_classes(["breadcrumb-button"])
            .build();
        button.set_focusable(false);
        button.set_child(Some(&breadcrumb_content(label)));

        let this = Rc::clone(self);
        let click_uri = uri.clone();
        button.connect_clicked(move |_| this.load_uri(click_uri.clone(), true));

        if let Ok(target_dir) = uri.local_path() {
            let this = Rc::clone(self);
            dnd::install_drop_target(&button, move |payload| {
                this.transfer_drop_payload_into_target(payload, target_dir.clone());
            });
        }

        self.topbar.breadcrumbs.append(&button);
    }

    fn apply_view_mode(&self, mode: ViewMode) {
        self.view_mode.set(mode);
        match mode {
            ViewMode::List => {
                if self.active_page.get() == AppPage::Files {
                    self.stack.set_visible_child_name("list");
                }
                self.topbar.icon_button.set_active(false);
                self.topbar.list_button.set_active(true);
            }
            ViewMode::Icon => {
                if self.active_page.get() == AppPage::Files {
                    self.stack.set_visible_child_name("icon");
                }
                self.topbar.list_button.set_active(false);
                self.topbar.icon_button.set_active(true);
            }
        }
        self.settings_page.set_view_mode(mode);
        self.save_ui_state();
    }

    fn set_show_hidden(self: &Rc<Self>, show_hidden: bool) {
        if self.show_hidden.get() == show_hidden {
            return;
        }

        self.show_hidden.set(show_hidden);
        self.topbar.show_hidden_button.set_active(show_hidden);
        self.settings_page.set_show_hidden(show_hidden);
        self.save_ui_state();

        if self.active_page.get() == AppPage::Files {
            self.reload_current_folder_entries();
            self.status_label.set_text(if show_hidden {
                "Showing hidden files"
            } else {
                "Hiding hidden files"
            });
        }
    }

    fn save_ui_state(&self) {
        let state = AppState {
            layout: self.view_mode.get(),
            show_hidden: self.show_hidden.get(),
            icon_size: self.icon_size.get(),
        };
        if let Err(error) = state.save() {
            tracing::warn!(%error, "failed to save UI state");
        }
    }

    fn install_icon_view_zoom_controls(self: &Rc<Self>) {
        let scroll_controller =
            gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        scroll_controller.connect_scroll(move |scroll, _, delta_y| {
            if delta_y == 0.0
                || this.active_page.get() != AppPage::Files
                || this.view_mode.get() != ViewMode::Icon
                || !scroll
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK)
            {
                return glib::Propagation::Proceed;
            }

            if delta_y < 0.0 {
                this.change_icon_size(ICON_VIEW_ZOOM_STEP);
            } else {
                this.change_icon_size(-ICON_VIEW_ZOOM_STEP);
            }
            glib::Propagation::Stop
        });
        self.grid_scroll.add_controller(scroll_controller);
    }

    fn change_icon_size(self: &Rc<Self>, delta: i32) {
        let current = self.icon_size.get();
        self.set_icon_size(current + delta);
    }

    fn set_icon_size(self: &Rc<Self>, icon_size: i32) {
        let current = self.icon_size.get();
        let next = clamp_icon_size(icon_size);
        if next == current {
            self.settings_page.set_icon_size(next);
            return;
        }

        let selected = self.selected_indices.borrow().clone();
        self.icon_size.set(next);
        self.settings_page.set_icon_size(next);
        self.save_ui_state();
        views::icon::clear_thumbnail_cache(&self.thumbnail_cache);
        if self.active_page.get() == AppPage::Files {
            self.render_entries();
            if !selected.is_empty() {
                set_entry_selection(
                    &self.selected_indices,
                    &self.list_box,
                    &self.flow_box,
                    selected,
                );
            }
        }
        self.status_label.set_text(&format!("Icon size: {next}px"));
    }

    fn activate_entry(self: &Rc<Self>, item: FileItem) {
        if item.kind == FileKind::Directory {
            self.load_uri(item.uri, true);
        } else if is_desktop_entry_file(&item) {
            self.launch_desktop_entry(item);
        } else if let Some(uri) = item.uri.to_file_uri() {
            match gio::AppInfo::launch_default_for_uri(&uri, Option::<&gio::AppLaunchContext>::None)
            {
                Ok(()) => {}
                Err(error) => self
                    .status_label
                    .set_text(&format!("Failed to open {}: {error}", item.name)),
            }
        }
    }

    fn launch_desktop_entry(&self, item: FileItem) {
        let Ok(path) = item.uri.local_path() else {
            self.status_label
                .set_text(&format!("Cannot launch {}", item.display_name()));
            return;
        };

        let Some(app_info) = gio::DesktopAppInfo::from_filename(&path) else {
            self.status_label
                .set_text(&format!("Invalid desktop entry: {}", item.name));
            return;
        };

        match app_info.launch(&[], Option::<&gio::AppLaunchContext>::None) {
            Ok(()) => self
                .status_label
                .set_text(&format!("Launched {}", item.display_name())),
            Err(error) => self.status_label.set_text(&format!(
                "Failed to launch {}: {error}",
                item.display_name()
            )),
        }
    }

    fn go_back(self: &Rc<Self>) {
        if self.active_page.get() != AppPage::Files {
            return;
        }

        let previous = self.back_stack.borrow_mut().pop();
        let Some(previous) = previous else {
            return;
        };
        self.forward_stack
            .borrow_mut()
            .push(self.current_uri.borrow().clone());
        self.load_uri(previous, false);
    }

    fn go_forward(self: &Rc<Self>) {
        if self.active_page.get() != AppPage::Files {
            return;
        }

        let next = self.forward_stack.borrow_mut().pop();
        let Some(next) = next else {
            return;
        };
        self.back_stack
            .borrow_mut()
            .push(self.current_uri.borrow().clone());
        self.load_uri(next, false);
    }

    fn go_up(self: &Rc<Self>) {
        if self.active_page.get() != AppPage::Files {
            return;
        }

        let parent = self.current_uri.borrow().parent();
        if let Some(parent) = parent {
            self.load_uri(parent, true);
        }
    }

    fn refresh(self: &Rc<Self>) {
        if self.active_page.get() == AppPage::Computer {
            self.computer_page.refresh();
            self.update_computer_status();
            return;
        }

        if self.active_page.get() != AppPage::Files {
            return;
        }

        let uri = self.current_uri.borrow().clone();
        self.load_uri(uri, false);
    }

    fn update_navigation_buttons(&self) {
        if self.active_page.get() != AppPage::Files {
            self.topbar.back_button.set_sensitive(false);
            self.topbar.forward_button.set_sensitive(false);
            self.topbar.up_button.set_sensitive(false);
            return;
        }

        self.topbar
            .back_button
            .set_sensitive(!self.back_stack.borrow().is_empty());
        self.topbar
            .forward_button
            .set_sensitive(!self.forward_stack.borrow().is_empty());
        self.topbar
            .up_button
            .set_sensitive(self.current_uri.borrow().parent().is_some());
    }

    fn selected_paths_from_list_index(&self, index: usize) -> Vec<PathBuf> {
        if self.entries.borrow().get(index).is_none() {
            return Vec::new();
        }

        if !self
            .selected_indices
            .try_borrow()
            .map(|selected| selected.contains(&index))
            .unwrap_or(false)
        {
            set_entry_selection(
                &self.selected_indices,
                &self.list_box,
                &self.flow_box,
                [index].into_iter().collect(),
            );
        }

        self.selected_paths()
    }

    fn selected_paths_from_grid_index(&self, index: usize) -> Vec<PathBuf> {
        if self.entries.borrow().get(index).is_none() {
            return Vec::new();
        }

        if !self
            .selected_indices
            .try_borrow()
            .map(|selected| selected.contains(&index))
            .unwrap_or(false)
        {
            set_entry_selection(
                &self.selected_indices,
                &self.list_box,
                &self.flow_box,
                [index].into_iter().collect(),
            );
        }

        self.selected_paths()
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let selected_indices = self
            .selected_indices
            .try_borrow()
            .map(|selected| selected.clone())
            .unwrap_or_default();
        let entries = self.entries.borrow();

        selected_indices
            .into_iter()
            .filter_map(|index| entries.get(index).cloned())
            .filter_map(|item| item.uri.local_path().ok())
            .collect()
    }

    fn rename_selection(self: &Rc<Self>) {
        let paths = self.selected_paths();
        match paths.as_slice() {
            [] => self.status_label.set_text("No item selected"),
            [path] => self.show_rename_dialog(path.clone()),
            _ => self.status_label.set_text("Select one item to rename"),
        }
    }

    fn delete_selection(self: &Rc<Self>) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.status_label.set_text("No item selected");
            return;
        }

        self.delete_paths(paths);
    }

    fn copy_selection_to_clipboard(&self, operation: FileClipboardOperation) {
        let paths = self.selected_paths();
        self.copy_paths_to_clipboard(paths, operation);
    }

    fn copy_paths_to_clipboard(&self, paths: Vec<PathBuf>, operation: FileClipboardOperation) {
        if paths.is_empty() {
            self.status_label.set_text("No item selected");
            return;
        }

        let provider = file_clipboard_provider(&paths, operation);
        match self.window.clipboard().set_content(Some(&provider)) {
            Ok(()) => {
                *self.clipboard_paths.borrow_mut() = paths.clone();
                self.clipboard_operation.set(Some(operation));
                self.status_label.set_text(&format!(
                    "{} {} item(s) to clipboard",
                    operation.past_tense(),
                    paths.len()
                ));
            }
            Err(error) => self
                .status_label
                .set_text(&format!("Failed to copy to clipboard: {error}")),
        }
    }

    fn paste_from_clipboard(self: &Rc<Self>) {
        let target_dir = {
            let current_uri = self.current_uri.borrow();
            match current_uri.local_path() {
                Ok(path) => path,
                Err(_) => {
                    self.status_label
                        .set_text("Paste is only supported for local folders");
                    return;
                }
            }
        };

        let clipboard = self.window.clipboard();
        let this = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let value = match clipboard
                .read_value_future(gtk::gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    this.status_label
                        .set_text(&format!("Clipboard does not contain files: {error}"));
                    return;
                }
            };

            let Ok(file_list) = value.get::<gtk::gdk::FileList>() else {
                this.status_label
                    .set_text("Clipboard does not contain files");
                return;
            };
            let paths = file_list
                .files()
                .into_iter()
                .filter_map(|file| file.path())
                .collect::<Vec<_>>();
            if paths.is_empty() {
                this.status_label
                    .set_text("Clipboard does not contain files");
                return;
            }

            let operation = this.clipboard_drop_operation(&clipboard, &paths);
            this.transfer_paths_into_target(operation, paths, target_dir);
            if operation == dnd::DropOperation::Move {
                this.clipboard_paths.borrow_mut().clear();
                this.clipboard_operation.set(None);
            }
        });
    }

    fn clipboard_drop_operation(
        &self,
        clipboard: &gtk::gdk::Clipboard,
        paths: &[PathBuf],
    ) -> dnd::DropOperation {
        if clipboard.is_local()
            && same_paths(&self.clipboard_paths.borrow(), paths)
            && let Some(operation) = self.clipboard_operation.get()
        {
            return operation.drop_operation();
        }

        dnd::DropOperation::Copy
    }

    fn move_selection_by(&self, delta: i32) {
        let count = self.entries.borrow().len();
        if count == 0 {
            return;
        }
        let current = self
            .selected_indices
            .try_borrow()
            .ok()
            .and_then(|sel| sel.iter().next().copied());
        let next = match current {
            Some(idx) => (idx as i64 + i64::from(delta)).clamp(0, count as i64 - 1) as usize,
            None => {
                if delta >= 0 {
                    0
                } else {
                    count - 1
                }
            }
        };
        self.anchor_index.set(Some(next));
        set_entry_selection(
            &self.selected_indices,
            &self.list_box,
            &self.flow_box,
            [next].into_iter().collect(),
        );
    }

    fn activate_selection(self: &Rc<Self>) {
        let item = self
            .selected_indices
            .try_borrow()
            .ok()
            .and_then(|sel| sel.iter().next().copied())
            .and_then(|idx| self.entries.borrow().get(idx).cloned());
        if let Some(item) = item {
            let this = Rc::clone(self);
            glib::idle_add_local_once(move || this.activate_entry(item));
        }
    }

    fn select_paths_in_current_folder(&self, folder: &Path, paths: &[PathBuf]) {
        let selected_names = paths
            .iter()
            .filter(|path| path.parent().is_some_and(|parent| parent == folder))
            .filter_map(|path| path.file_name())
            .filter_map(|name| name.to_str())
            .collect::<BTreeSet<_>>();

        if selected_names.is_empty() {
            return;
        }

        let indices = self
            .entries
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                selected_names.contains(item.name.as_str()).then_some(index)
            })
            .collect::<BTreeSet<_>>();

        let Some(first_index) = indices.first().copied() else {
            return;
        };

        self.anchor_index.set(Some(first_index));
        set_entry_selection(
            &self.selected_indices,
            &self.list_box,
            &self.flow_box,
            indices.clone(),
        );
        self.focus_entry_at(first_index);

        if indices.len() == 1 {
            if let Some(name) = selected_names.iter().next() {
                self.status_label.set_text(&format!("Selected {name}"));
            }
        } else {
            self.status_label
                .set_text(&format!("Selected {} items", indices.len()));
        }
    }

    fn focus_entry_at(&self, index: usize) {
        let index = index as i32;
        match self.view_mode.get() {
            ViewMode::List => {
                if let Some(row) = self.list_box.row_at_index(index) {
                    row.grab_focus();
                }
            }
            ViewMode::Icon => {
                if let Some(child) = self.flow_box.child_at_index(index) {
                    child.grab_focus();
                }
            }
        }
    }

    fn show_file_context_menu(self: &Rc<Self>, index: usize, parent: gtk::Widget, x: f64, y: f64) {
        if self.entries.borrow().get(index).is_none() {
            return;
        }

        let selected_contains_index = self
            .selected_indices
            .try_borrow()
            .map(|selected| selected.contains(&index))
            .unwrap_or(false);
        if !selected_contains_index {
            set_entry_selection(
                &self.selected_indices,
                &self.list_box,
                &self.flow_box,
                [index].into_iter().collect(),
            );
        }

        let paths = self.selected_paths();
        let view: Option<context_menu::ViewAction> = self
            .entries
            .borrow()
            .get(index)
            .cloned()
            .filter(is_previewable_image_file)
            .and_then(|item| item.uri.local_path().ok())
            .map(|path| {
                let this = Rc::clone(self);
                Rc::new(move || this.show_image_viewer(path.clone())) as context_menu::ViewAction
            });
        let bookmark: Option<context_menu::BookmarkAction> = self
            .entries
            .borrow()
            .get(index)
            .cloned()
            .filter(|item| item.kind == FileKind::Directory)
            .and_then(|item| item.uri.local_path().ok())
            .map(|path| self.bookmark_action_for_folder(path));
        let rename: context_menu::RenameAction = {
            let this = Rc::clone(self);
            Rc::new(move |path| this.show_rename_dialog(path))
        };
        let delete: context_menu::DeleteAction = {
            let this = Rc::clone(self);
            Rc::new(move |paths| this.delete_paths(paths))
        };
        let copy: context_menu::ClipboardAction = {
            let this = Rc::clone(self);
            Rc::new(move |paths| this.copy_paths_to_clipboard(paths, FileClipboardOperation::Copy))
        };
        let cut: context_menu::ClipboardAction = {
            let this = Rc::clone(self);
            Rc::new(move |paths| this.copy_paths_to_clipboard(paths, FileClipboardOperation::Cut))
        };

        let Some(context) = context_menu::FileEntryContext::for_paths(
            paths, view, bookmark, copy, cut, rename, delete,
        ) else {
            return;
        };

        context_menu::ContextMenu::popup_at(&parent, x, y, &context);
    }

    fn show_image_viewer(self: &Rc<Self>, start_path: PathBuf) {
        let images = self.current_folder_images();
        let Some(start_index) = images
            .iter()
            .position(|item| item.uri.local_path().is_ok_and(|path| path == start_path))
        else {
            self.status_label.set_text("Image is no longer available");
            return;
        };

        let default_width = (self.window.width() - 96).clamp(900, 1500);
        let default_height = (self.window.height() - 96).clamp(640, 1000);
        let viewer_window = gtk::Window::builder()
            .title("Image Viewer")
            .transient_for(&self.window)
            .modal(true)
            .default_width(default_width)
            .default_height(default_height)
            .build();
        viewer_window.set_focusable(true);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["image-viewer"])
            .build();
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["image-viewer-toolbar"])
            .build();
        let previous_button = image_viewer_button("go-previous-symbolic", "Previous Image");
        let next_button = image_viewer_button("go-next-symbolic", "Next Image");
        let close_button = image_viewer_button("window-close-symbolic", "Close");
        let title_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["image-viewer-title"])
            .build();
        let counter_label = gtk::Label::builder()
            .css_classes(["dim-label", "image-viewer-counter"])
            .build();
        toolbar.append(&previous_button);
        toolbar.append(&next_button);
        toolbar.append(&title_label);
        toolbar.append(&counter_label);
        toolbar.append(&close_button);

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["image-viewer-picture"])
            .build();
        let image_scroll = gtk::ScrolledWindow::builder()
            .child(&picture)
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .css_classes(["image-viewer-scroll"])
            .build();

        root.append(&toolbar);
        root.append(&image_scroll);
        viewer_window.set_child(Some(&root));

        let images = Rc::new(images);
        let current_index = Rc::new(Cell::new(start_index));
        let image_zoom = Rc::new(Cell::new(IMAGE_VIEWER_MIN_ZOOM));
        let animation_source = Rc::new(RefCell::new(None::<glib::SourceId>));
        install_image_viewer_zoom_controls(&picture, &image_scroll, Rc::clone(&image_zoom));
        let update_view: Rc<dyn Fn()> = Rc::new({
            let viewer_window = viewer_window.clone();
            let picture = picture.clone();
            let image_scroll = image_scroll.clone();
            let title_label = title_label.clone();
            let counter_label = counter_label.clone();
            let images = Rc::clone(&images);
            let current_index = Rc::clone(&current_index);
            let image_zoom = Rc::clone(&image_zoom);
            let animation_source = Rc::clone(&animation_source);
            move || {
                image_zoom.set(IMAGE_VIEWER_MIN_ZOOM);
                apply_image_viewer_zoom(&picture, &image_scroll, image_zoom.get());
                update_image_viewer(
                    &viewer_window,
                    &picture,
                    &title_label,
                    &counter_label,
                    &images,
                    current_index.get(),
                    &animation_source,
                )
            }
        });

        let navigation_enabled = images.len() > 1;
        previous_button.set_sensitive(navigation_enabled);
        next_button.set_sensitive(navigation_enabled);
        let previous_image = image_viewer_navigation(
            Rc::clone(&images),
            Rc::clone(&current_index),
            -1,
            Rc::clone(&update_view),
        );
        let next_image = image_viewer_navigation(
            Rc::clone(&images),
            Rc::clone(&current_index),
            1,
            Rc::clone(&update_view),
        );

        let previous_from_button = Rc::clone(&previous_image);
        previous_button.connect_clicked(move |_| previous_from_button());
        let next_from_button = Rc::clone(&next_image);
        next_button.connect_clicked(move |_| next_from_button());
        let close_window = viewer_window.clone();
        let close_animation_source = Rc::clone(&animation_source);
        close_button.connect_clicked(move |_| close_window.close());
        viewer_window
            .connect_destroy(move |_| cancel_image_viewer_animation(&close_animation_source));

        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let previous_from_key = Rc::clone(&previous_image);
        let next_from_key = Rc::clone(&next_image);
        let close_window = viewer_window.clone();
        let key_picture = picture.clone();
        let key_image_scroll = image_scroll.clone();
        let key_image_zoom = Rc::clone(&image_zoom);
        key_controller.connect_key_pressed(move |_, key, _, _| match key {
            gtk::gdk::Key::Left | gtk::gdk::Key::KP_Left => {
                previous_from_key();
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Right | gtk::gdk::Key::KP_Right => {
                next_from_key();
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Escape => {
                close_window.close();
                glib::Propagation::Stop
            }
            gtk::gdk::Key::plus | gtk::gdk::Key::equal | gtk::gdk::Key::KP_Add => {
                zoom_image_viewer(
                    &key_picture,
                    &key_image_scroll,
                    &key_image_zoom,
                    IMAGE_VIEWER_ZOOM_STEP,
                    None,
                );
                glib::Propagation::Stop
            }
            gtk::gdk::Key::minus | gtk::gdk::Key::underscore | gtk::gdk::Key::KP_Subtract => {
                zoom_image_viewer(
                    &key_picture,
                    &key_image_scroll,
                    &key_image_zoom,
                    1.0 / IMAGE_VIEWER_ZOOM_STEP,
                    None,
                );
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        viewer_window.add_controller(key_controller);

        update_view();
        viewer_window.present();
        let initial_picture = picture.clone();
        let initial_image_scroll = image_scroll.clone();
        let initial_image_zoom = Rc::clone(&image_zoom);
        glib::idle_add_local_once(move || {
            apply_image_viewer_zoom(
                &initial_picture,
                &initial_image_scroll,
                initial_image_zoom.get(),
            )
        });
        viewer_window.grab_focus();
    }

    fn current_folder_images(&self) -> Vec<FileItem> {
        self.all_entries
            .borrow()
            .iter()
            .filter(|item| is_previewable_image_file(item))
            .cloned()
            .collect()
    }

    fn show_empty_space_context_menu(self: &Rc<Self>, parent: gtk::Widget, x: f64, y: f64) {
        let paste: context_menu::MenuAction = {
            let this = Rc::clone(self);
            Rc::new(move || this.paste_from_clipboard())
        };
        let new_folder: context_menu::MenuAction = {
            let this = Rc::clone(self);
            Rc::new(move || this.create_folder())
        };
        let bookmark = self.current_folder_bookmark_action();
        let context = context_menu::EmptySpaceContext::new(paste, new_folder, bookmark);

        context_menu::ContextMenu::popup_at(&parent, x, y, &context);
    }

    fn show_sidebar_bookmark_context_menu(
        self: &Rc<Self>,
        path: PathBuf,
        parent: gtk::Widget,
        x: f64,
        y: f64,
    ) {
        let remove: context_menu::MenuAction = {
            let this = Rc::clone(self);
            Rc::new(move || this.remove_bookmark(path.clone()))
        };
        let context = context_menu::SidebarBookmarkContext::new(remove);

        context_menu::ContextMenu::popup_at(&parent, x, y, &context);
    }

    fn current_folder_bookmark_action(self: &Rc<Self>) -> context_menu::BookmarkAction {
        let target_dir = self.current_uri.borrow().local_path().ok();
        match target_dir {
            Some(path) => self.bookmark_action_for_folder(path),
            None => {
                let this = Rc::clone(self);
                context_menu::BookmarkAction::new(
                    "Add Bookmark",
                    Rc::new(move || this.bookmark_current_folder()),
                )
            }
        }
    }

    fn bookmark_action_for_folder(self: &Rc<Self>, path: PathBuf) -> context_menu::BookmarkAction {
        if self.is_bookmarked(&path) {
            let this = Rc::clone(self);
            context_menu::BookmarkAction::new(
                "Remove Bookmark",
                Rc::new(move || this.remove_bookmark(path.clone())),
            )
        } else {
            let this = Rc::clone(self);
            context_menu::BookmarkAction::new(
                "Add Bookmark",
                Rc::new(move || this.add_bookmark(path.clone())),
            )
        }
    }

    fn bookmark_current_folder(self: &Rc<Self>) {
        let target_dir = {
            let current_uri = self.current_uri.borrow();
            match current_uri.local_path() {
                Ok(path) => path,
                Err(_) => {
                    self.status_label
                        .set_text("Bookmarks are only supported for local folders");
                    return;
                }
            }
        };

        self.add_bookmark(target_dir);
    }

    fn add_bookmark(self: &Rc<Self>, path: PathBuf) {
        if !path.is_dir() {
            self.status_label.set_text("Only folders can be bookmarked");
            return;
        }

        let mut next_bookmarks = self.bookmarks.borrow().clone();
        if !bookmarks::add(&mut next_bookmarks, path.clone()) {
            self.status_label
                .set_text(&format!("{} is already bookmarked", path.display()));
            return;
        }

        let success = format!("Bookmarked {}", path.display());
        self.save_bookmark_changes(next_bookmarks, success);
    }

    fn remove_bookmark(self: &Rc<Self>, path: PathBuf) {
        let mut next_bookmarks = self.bookmarks.borrow().clone();
        if !bookmarks::remove(&mut next_bookmarks, &path) {
            self.status_label
                .set_text(&format!("{} is not bookmarked", path.display()));
            return;
        }

        let success = format!("Removed bookmark {}", path.display());
        self.save_bookmark_changes(next_bookmarks, success);
    }

    fn is_bookmarked(&self, path: &Path) -> bool {
        let bookmarks = self.bookmarks.borrow();
        bookmarks::contains(bookmarks.as_slice(), path)
    }

    fn save_bookmark_changes(&self, next_bookmarks: Vec<PathBuf>, success: String) {
        match bookmarks::save(&next_bookmarks) {
            Ok(()) => {
                *self.bookmarks.borrow_mut() = next_bookmarks;
                let bookmarks = self.bookmarks.borrow();
                self.sidebar.set_bookmarks(bookmarks.as_slice());
                self.status_label.set_text(&success);
            }
            Err(error) => self
                .status_label
                .set_text(&format!("Failed to save bookmark: {error}")),
        }
    }

    fn show_rename_dialog(self: &Rc<Self>, path: PathBuf) {
        let Some(current_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            self.status_label.set_text("Cannot rename this item");
            return;
        };

        let rename_window = gtk::Window::builder()
            .title("Rename")
            .transient_for(&self.window)
            .modal(true)
            .default_width(420)
            .resizable(false)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_top(14)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .build();

        content.append(&gtk::Label::builder().label("New name").xalign(0.0).build());
        let name_entry = gtk::Entry::builder()
            .text(&current_name)
            .hexpand(true)
            .build();
        name_entry.select_region(0, -1);
        content.append(&name_entry);

        let button_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .build();
        let cancel_button = gtk::Button::builder().label("Cancel").build();
        let rename_button = gtk::Button::builder()
            .label("Rename")
            .css_classes(["suggested-action"])
            .build();
        button_row.append(&cancel_button);
        button_row.append(&rename_button);
        content.append(&button_row);
        rename_window.set_child(Some(&content));

        let this = Rc::clone(self);
        let submit = Rc::new({
            let rename_window = rename_window.clone();
            let name_entry = name_entry.clone();
            move || {
                this.rename_path(path.clone(), name_entry.text().trim().to_string());
                rename_window.close();
            }
        });

        let submit_from_button = Rc::clone(&submit);
        rename_button.connect_clicked(move |_| submit_from_button());
        name_entry.connect_activate(move |_| submit());

        let cancel_window = rename_window.clone();
        cancel_button.connect_clicked(move |_| {
            cancel_window.close();
        });

        let esc_controller = gtk::EventControllerKey::new();
        let esc_window = rename_window.clone();
        esc_controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                esc_window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        rename_window.add_controller(esc_controller);

        rename_window.present();
        name_entry.grab_focus();
    }

    fn rename_path(self: &Rc<Self>, source: PathBuf, new_name: String) {
        if new_name.is_empty() {
            self.status_label.set_text("Name cannot be empty");
            return;
        }
        if new_name == "." || new_name == ".." || new_name.contains('/') {
            self.status_label
                .set_text("Name cannot contain path separators");
            return;
        }

        let Some(parent) = source.parent() else {
            self.status_label.set_text("Cannot rename this item");
            return;
        };
        let target = parent.join(&new_name);
        if target == source {
            self.status_label.set_text("Name unchanged");
            return;
        }
        if target.exists() {
            self.status_label
                .set_text(&format!("{} already exists", target.display()));
            return;
        }

        match fs::rename(&source, &target) {
            Ok(()) => {
                self.status_label
                    .set_text(&format!("Renamed to {new_name}"));
                self.refresh();
            }
            Err(error) => self
                .status_label
                .set_text(&format!("Failed to rename {}: {error}", source.display())),
        }
    }

    fn delete_paths(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }

        let total = paths.len();
        let mut deleted = 0;
        let mut last_error = None;
        for path in paths {
            match remove_path(&path) {
                Ok(()) => deleted += 1,
                Err(error) => {
                    last_error = Some(format!("Failed to delete {}: {error}", path.display()))
                }
            }
        }

        if deleted > 0 {
            self.status_label
                .set_text(&format!("Deleted {deleted} of {total} item(s)"));
            self.refresh();
        }

        if let Some(error) = last_error {
            self.status_label.set_text(&error);
        }
    }

    fn transfer_dropped_payload(self: &Rc<Self>, payload: dnd::DropPayload) {
        if self.active_page.get() != AppPage::Files {
            self.status_label
                .set_text("Open a folder before dropping files");
            return;
        }

        let target_dir = {
            let current_uri = self.current_uri.borrow();
            match current_uri.local_path() {
                Ok(path) => path,
                Err(_) => {
                    self.status_label
                        .set_text("Drops are only supported for local folders");
                    return;
                }
            }
        };

        self.transfer_drop_payload_into_target(payload, target_dir);
    }

    fn transfer_drop_payload_into_target(
        self: &Rc<Self>,
        payload: dnd::DropPayload,
        target_dir: PathBuf,
    ) {
        match payload {
            dnd::DropPayload::LocalPaths { operation, paths } => {
                self.transfer_paths_into_target(operation, paths, target_dir);
            }
            dnd::DropPayload::Uris(uris) => self.import_uri_drop_into_target(uris, target_dir),
            dnd::DropPayload::Texture(texture) => {
                self.save_dropped_texture_into_target(texture, target_dir);
            }
        }
    }

    fn import_uri_drop_into_target(self: &Rc<Self>, uris: Vec<String>, target_dir: PathBuf) {
        let (local_paths, remote_uris) = partition_drop_uris(uris);
        if local_paths.is_empty() && remote_uris.is_empty() {
            self.status_label
                .set_text("Dropped data did not contain importable files");
            return;
        }

        if !local_paths.is_empty() {
            self.transfer_paths_into_target(
                dnd::DropOperation::Copy,
                local_paths,
                target_dir.clone(),
            );
        }

        if !remote_uris.is_empty() {
            self.import_remote_uris_into_target(remote_uris, target_dir);
        }
    }

    fn import_remote_uris_into_target(self: &Rc<Self>, uris: Vec<String>, target_dir: PathBuf) {
        let total = uris.len();
        self.status_label
            .set_text(&format!("Importing {total} dropped item(s)..."));

        let this = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let mut imported = 0;
            let mut last_error = None;
            for uri in uris {
                match copy_remote_uri_into_target(&uri, &target_dir).await {
                    Ok(_) => imported += 1,
                    Err(error) => last_error = Some(format!("Failed to import {uri}: {error}")),
                }
            }

            if imported > 0 {
                this.status_label
                    .set_text(&format!("Imported {imported} of {total} dropped item(s)"));
                this.refresh();
            }

            if let Some(error) = last_error {
                this.status_label.set_text(&error);
            }
        });
    }

    fn save_dropped_texture_into_target(
        self: &Rc<Self>,
        texture: gtk::gdk::Texture,
        target_dir: PathBuf,
    ) {
        let target = next_available_path(&target_dir.join("Dropped Image.png"));
        match texture.save_to_png(&target) {
            Ok(()) => {
                self.status_label
                    .set_text(&format!("Imported {}", target.display()));
                self.refresh();
            }
            Err(error) => self
                .status_label
                .set_text(&format!("Failed to import dropped image: {error}")),
        }
    }

    fn transfer_paths_into_target(
        self: &Rc<Self>,
        operation: dnd::DropOperation,
        paths: Vec<PathBuf>,
        target_dir: PathBuf,
    ) {
        if drop_target_is_selected(&target_dir, &paths) {
            self.status_label
                .set_text("Cannot drop onto a selected item");
            return;
        }

        let mut transferred = 0;
        let mut skipped = 0;
        for path in paths {
            let result = match operation {
                dnd::DropOperation::Copy => copy_path_into(&path, &target_dir),
                dnd::DropOperation::Move => move_path_into(&path, &target_dir),
            };

            match result {
                Ok(true) => transferred += 1,
                Ok(false) => skipped += 1,
                Err(error) => self.status_label.set_text(&format!(
                    "Failed to {} {}: {error}",
                    operation.verb(),
                    path.display()
                )),
            }
        }

        if transferred > 0 {
            self.status_label
                .set_text(&format!("{} {transferred} item(s)", operation.past_tense()));
            self.refresh();
        } else if skipped > 0 {
            self.status_label.set_text("Already in that folder");
        }
    }

    fn create_folder(self: &Rc<Self>) {
        let target_dir = {
            let current_uri = self.current_uri.borrow();
            match current_uri.local_path() {
                Ok(path) => path,
                Err(_) => {
                    self.status_label
                        .set_text("New folders are only supported locally");
                    return;
                }
            }
        };

        self.show_create_folder_dialog(target_dir);
    }

    fn show_create_folder_dialog(self: &Rc<Self>, target_dir: PathBuf) {
        let create_window = gtk::Window::builder()
            .title("New Folder")
            .transient_for(&self.window)
            .modal(true)
            .default_width(420)
            .resizable(false)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_top(14)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .build();

        content.append(
            &gtk::Label::builder()
                .label("Folder name")
                .xalign(0.0)
                .build(),
        );
        let name_entry = gtk::Entry::builder()
            .text("New Folder")
            .hexpand(true)
            .build();
        name_entry.select_region(0, -1);
        content.append(&name_entry);

        let button_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .build();
        let cancel_button = gtk::Button::builder().label("Cancel").build();
        let create_button = gtk::Button::builder()
            .label("Create")
            .css_classes(["suggested-action"])
            .build();
        button_row.append(&cancel_button);
        button_row.append(&create_button);
        content.append(&button_row);
        create_window.set_child(Some(&content));

        let this = Rc::clone(self);
        let submit = Rc::new({
            let create_window = create_window.clone();
            let name_entry = name_entry.clone();
            move || {
                if this.create_folder_named(&target_dir, name_entry.text().trim()) {
                    create_window.close();
                }
            }
        });

        let submit_from_button = Rc::clone(&submit);
        create_button.connect_clicked(move |_| submit_from_button());
        name_entry.connect_activate(move |_| submit());

        let cancel_window = create_window.clone();
        cancel_button.connect_clicked(move |_| {
            cancel_window.close();
        });

        let esc_controller = gtk::EventControllerKey::new();
        let esc_window = create_window.clone();
        esc_controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                esc_window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        create_window.add_controller(esc_controller);

        create_window.present();
        name_entry.grab_focus();
    }

    fn create_folder_named(self: &Rc<Self>, target_dir: &Path, name: &str) -> bool {
        let target = match new_folder_target(target_dir, name) {
            Ok(target) => target,
            Err(message) => {
                self.status_label.set_text(message);
                return false;
            }
        };

        match fs::create_dir(&target) {
            Ok(()) => {
                self.status_label
                    .set_text(&format!("Created {}", target.display()));
                self.refresh();
                true
            }
            Err(error) => {
                self.status_label
                    .set_text(&format!("Failed to create folder: {error}"));
                false
            }
        }
    }

    fn show_error(&self, error: ProviderError) {
        self.status_label.set_text(&format!("{error}"));
    }
}

fn home_uri() -> Option<ProviderUri> {
    directories::UserDirs::new().map(|dirs| ProviderUri::local(dirs.home_dir()))
}

async fn copy_remote_uri_into_target(uri: &str, target_dir: &Path) -> Result<PathBuf, glib::Error> {
    let file_name = dropped_file_name_for_uri(uri);
    let target = next_available_path(&target_dir.join(file_name));
    let source_file = gio::File::for_uri(uri);
    let target_file = gio::File::for_path(&target);
    let (copy, _progress) = source_file.copy_future(
        &target_file,
        gio::FileCopyFlags::NONE,
        glib::Priority::DEFAULT,
    );
    copy.await?;
    Ok(target)
}

fn partition_drop_uris(uris: Vec<String>) -> (Vec<PathBuf>, Vec<String>) {
    let mut local_paths = Vec::new();
    let mut remote_uris = Vec::new();

    for uri in uris {
        let trimmed = uri.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(path) = drop_uri_to_local_path(trimmed) {
            local_paths.push(path);
        } else if is_remote_drop_uri(trimmed) {
            remote_uris.push(trimmed.to_string());
        }
    }

    (local_paths, remote_uris)
}

fn drop_uri_to_local_path(uri: &str) -> Option<PathBuf> {
    if let Ok(url) = Url::parse(uri)
        && url.scheme() == "file"
    {
        return url.to_file_path().ok();
    }

    let path = PathBuf::from(uri);
    path.is_absolute().then_some(path)
}

fn is_remote_drop_uri(uri: &str) -> bool {
    Url::parse(uri)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn dropped_file_name_for_uri(uri: &str) -> String {
    let candidate = Url::parse(uri)
        .ok()
        .and_then(|url| {
            url.path_segments().and_then(|segments| {
                segments
                    .rev()
                    .find(|segment| !segment.is_empty())
                    .map(percent_decode_path_segment)
            })
        })
        .unwrap_or_else(|| "Dropped File".to_string());

    sanitize_dropped_file_name(&candidate, "Dropped File")
}

fn sanitize_dropped_file_name(name: &str, fallback: &str) -> String {
    let sanitized = name
        .chars()
        .map(|character| match character {
            '/' | '\\' | '\0' => '_',
            character => character,
        })
        .collect::<String>()
        .trim()
        .to_string();

    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn percent_decode_path_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
        {
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

type ClearSelectionHandler = Rc<dyn Fn()>;
type ApplySelectionHandler = Rc<dyn Fn(BTreeSet<usize>)>;

fn clear_selection_handler(
    selected_indices: &Rc<RefCell<BTreeSet<usize>>>,
    list: &gtk::ListBox,
    flow: &gtk::FlowBox,
) -> ClearSelectionHandler {
    let selected_indices = Rc::clone(selected_indices);
    let list = list.clone();
    let flow = flow.clone();
    Rc::new(move || clear_entry_selection(&selected_indices, &list, &flow))
}

fn apply_selection_handler(
    selected_indices: &Rc<RefCell<BTreeSet<usize>>>,
    list: &gtk::ListBox,
    flow: &gtk::FlowBox,
) -> ApplySelectionHandler {
    let selected_indices = Rc::clone(selected_indices);
    let list = list.clone();
    let flow = flow.clone();
    Rc::new(move |indices| set_entry_selection(&selected_indices, &list, &flow, indices))
}

fn entry_selection_handler(
    selected_indices: &Rc<RefCell<BTreeSet<usize>>>,
    anchor_index: &Rc<Cell<Option<usize>>>,
    list: &gtk::ListBox,
    flow: &gtk::FlowBox,
) -> views::EntrySelectionHandler {
    let selected_indices = Rc::clone(selected_indices);
    let anchor_index = Rc::clone(anchor_index);
    let list = list.clone();
    let flow = flow.clone();
    Rc::new(move |index, state| {
        let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);

        let Ok(mut selected) = selected_indices.try_borrow_mut() else {
            return;
        };

        if shift {
            // Range select from anchor to clicked index.
            // Ctrl+Shift adds the range without clearing existing selection.
            let anchor = anchor_index.get().unwrap_or(index);
            let lo = anchor.min(index);
            let hi = anchor.max(index);
            if !ctrl {
                selected.clear();
            }
            selected.extend(lo..=hi);
            // Anchor stays fixed during shift-click
        } else if ctrl {
            // Toggle single item; move anchor to this item
            if !selected.remove(&index) {
                selected.insert(index);
            }
            anchor_index.set(Some(index));
        } else {
            // Plain click: select only this item and set anchor
            selected.clear();
            selected.insert(index);
            anchor_index.set(Some(index));
        }
        drop(selected);

        sync_entry_selection(&selected_indices, &list, &flow);
    })
}

fn clear_entry_selection(
    selected_indices: &Rc<RefCell<BTreeSet<usize>>>,
    list: &gtk::ListBox,
    flow: &gtk::FlowBox,
) {
    let Ok(mut selected) = selected_indices.try_borrow_mut() else {
        return;
    };
    selected.clear();
    drop(selected);

    sync_entry_selection(selected_indices, list, flow);
}

fn set_entry_selection(
    selected_indices: &Rc<RefCell<BTreeSet<usize>>>,
    list: &gtk::ListBox,
    flow: &gtk::FlowBox,
    indices: BTreeSet<usize>,
) {
    let Ok(mut selected) = selected_indices.try_borrow_mut() else {
        return;
    };
    *selected = indices;
    drop(selected);

    sync_entry_selection(selected_indices, list, flow);
}

fn sync_entry_selection(
    selected_indices: &Rc<RefCell<BTreeSet<usize>>>,
    list: &gtk::ListBox,
    flow: &gtk::FlowBox,
) {
    let selected_indices = selected_indices
        .try_borrow()
        .map(|selected| selected.clone())
        .unwrap_or_default();

    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        set_entry_selected_class(&row, selected_indices.contains(&(index as usize)));
        index += 1;
    }

    let mut index = 0;
    while let Some(child) = flow.child_at_index(index) {
        set_entry_selected_class(&child, selected_indices.contains(&(index as usize)));
        index += 1;
    }
}

fn set_entry_selected_class(widget: &impl IsA<gtk::Widget>, selected: bool) {
    if selected {
        widget.add_css_class("entry-selected");
    } else {
        widget.remove_css_class("entry-selected");
    }
}

#[derive(Default)]
struct RubberbandState {
    active: bool,
    start_x: f64,
    start_y: f64,
    has_dragged: bool,
    suppress_click_clear: bool,
}

fn rubberband_widget() -> gtk::Box {
    let rubberband = gtk::Box::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .css_classes(["rubberband-selection"])
        .visible(false)
        .build();
    rubberband.set_can_target(false);
    rubberband
}

fn install_list_empty_space_selection(
    overlay: &gtk::Overlay,
    rubberband: &gtk::Box,
    list: &gtk::ListBox,
    clear_selection: ClearSelectionHandler,
    apply_selection: ApplySelectionHandler,
) {
    let state = Rc::new(RefCell::new(RubberbandState::default()));
    install_empty_space_click(
        overlay,
        gtk::ListBoxRow::static_type(),
        Rc::clone(&state),
        clear_selection.clone(),
    );
    install_list_rubberband(
        overlay,
        rubberband,
        list,
        state,
        clear_selection,
        apply_selection,
    );
}

fn install_grid_empty_space_selection(
    overlay: &gtk::Overlay,
    rubberband: &gtk::Box,
    flow: &gtk::FlowBox,
    clear_selection: ClearSelectionHandler,
    apply_selection: ApplySelectionHandler,
) {
    let state = Rc::new(RefCell::new(RubberbandState::default()));
    install_empty_space_click(
        overlay,
        gtk::FlowBoxChild::static_type(),
        Rc::clone(&state),
        clear_selection.clone(),
    );
    install_grid_rubberband(
        overlay,
        rubberband,
        flow,
        state,
        clear_selection,
        apply_selection,
    );
}

fn install_empty_space_click(
    overlay: &gtk::Overlay,
    item_type: glib::types::Type,
    state: Rc<RefCell<RubberbandState>>,
    clear_selection: ClearSelectionHandler,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    let hit_overlay = overlay.clone();
    click.connect_released(move |_, _, x, y| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        if state.suppress_click_clear {
            state.suppress_click_clear = false;
            return;
        }
        drop(state);

        if !hit_widget_type(&hit_overlay, x, y, item_type) {
            clear_selection();
        }
    });
    overlay.add_controller(click);
}

fn install_list_rubberband(
    overlay: &gtk::Overlay,
    rubberband: &gtk::Box,
    list: &gtk::ListBox,
    state: Rc<RefCell<RubberbandState>>,
    clear_selection: ClearSelectionHandler,
    apply_selection: ApplySelectionHandler,
) {
    install_rubberband(
        overlay,
        rubberband,
        gtk::ListBoxRow::static_type(),
        state,
        clear_selection,
        {
            let overlay = overlay.clone();
            let list = list.clone();
            move |selection_rect| {
                select_list_rows_in_rect(&list, &overlay, selection_rect, &apply_selection)
            }
        },
    );
}

fn install_grid_rubberband(
    overlay: &gtk::Overlay,
    rubberband: &gtk::Box,
    flow: &gtk::FlowBox,
    state: Rc<RefCell<RubberbandState>>,
    clear_selection: ClearSelectionHandler,
    apply_selection: ApplySelectionHandler,
) {
    install_rubberband(
        overlay,
        rubberband,
        gtk::FlowBoxChild::static_type(),
        state,
        clear_selection,
        {
            let overlay = overlay.clone();
            let flow = flow.clone();
            move |selection_rect| {
                select_flow_children_in_rect(&flow, &overlay, selection_rect, &apply_selection)
            }
        },
    );
}

fn install_rubberband<S>(
    overlay: &gtk::Overlay,
    rubberband: &gtk::Box,
    item_type: glib::types::Type,
    state: Rc<RefCell<RubberbandState>>,
    clear_selection: ClearSelectionHandler,
    select_in_rect: S,
) where
    S: Fn(&gtk::graphene::Rect) + 'static,
{
    let select_in_rect: Rc<dyn Fn(&gtk::graphene::Rect)> = Rc::new(select_in_rect);
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);

    let begin_overlay = overlay.clone();
    let begin_rubberband = rubberband.clone();
    let begin_state = Rc::clone(&state);
    drag.connect_drag_begin(move |drag, x, y| {
        if hit_widget_type(&begin_overlay, x, y, item_type) {
            if let Ok(mut state) = begin_state.try_borrow_mut() {
                state.active = false;
                state.has_dragged = false;
                state.suppress_click_clear = false;
            }
            begin_rubberband.set_visible(false);
            drag.set_state(gtk::EventSequenceState::Denied);
            return;
        }

        drag.set_state(gtk::EventSequenceState::Claimed);
        let Ok(mut state) = begin_state.try_borrow_mut() else {
            return;
        };
        state.active = true;
        state.start_x = x;
        state.start_y = y;
        state.has_dragged = false;
        state.suppress_click_clear = false;
        begin_rubberband.set_visible(false);
        clear_selection();
    });

    let update_overlay = overlay.clone();
    let update_rubberband = rubberband.clone();
    let update_state = Rc::clone(&state);
    let update_select_in_rect = Rc::clone(&select_in_rect);
    drag.connect_drag_update(move |_, offset_x, offset_y| {
        let (start_x, start_y) = {
            let Ok(mut state) = update_state.try_borrow_mut() else {
                return;
            };
            if !state.active {
                return;
            }

            if offset_x.abs() > 3.0 || offset_y.abs() > 3.0 {
                state.has_dragged = true;
                state.suppress_click_clear = true;
            }

            if !state.has_dragged {
                return;
            }

            (state.start_x, state.start_y)
        };

        let selection_rect = rubberband_rect(start_x, start_y, offset_x, offset_y);
        update_rubberband_widget(&update_rubberband, &update_overlay, &selection_rect);
        update_select_in_rect(&selection_rect);
    });

    let end_overlay = overlay.clone();
    let end_rubberband = rubberband.clone();
    let end_state = Rc::clone(&state);
    let end_select_in_rect = Rc::clone(&select_in_rect);
    drag.connect_drag_end(move |_, offset_x, offset_y| {
        let maybe_rect = {
            let Ok(mut state) = end_state.try_borrow_mut() else {
                return;
            };
            if !state.active {
                return;
            }

            state.active = false;
            let rect = if state.has_dragged {
                state.suppress_click_clear = true;
                Some(rubberband_rect(
                    state.start_x,
                    state.start_y,
                    offset_x,
                    offset_y,
                ))
            } else {
                None
            };
            state.has_dragged = false;
            rect
        };

        if let Some(selection_rect) = maybe_rect {
            end_select_in_rect(&selection_rect);
        }
        end_rubberband.set_visible(false);
        end_overlay.queue_draw();
    });

    overlay.add_controller(drag);
}

fn hit_widget_type(
    widget: &impl IsA<gtk::Widget>,
    x: f64,
    y: f64,
    widget_type: glib::types::Type,
) -> bool {
    widget
        .pick(x, y, gtk::PickFlags::DEFAULT)
        .and_then(|widget| widget.ancestor(widget_type))
        .is_some()
}

fn picked_list_box_row(widget: &impl IsA<gtk::Widget>, x: f64, y: f64) -> Option<gtk::ListBoxRow> {
    widget
        .pick(x, y, gtk::PickFlags::DEFAULT)?
        .ancestor(gtk::ListBoxRow::static_type())?
        .downcast::<gtk::ListBoxRow>()
        .ok()
}

fn rubberband_rect(
    start_x: f64,
    start_y: f64,
    offset_x: f64,
    offset_y: f64,
) -> gtk::graphene::Rect {
    let current_x = start_x + offset_x;
    let current_y = start_y + offset_y;
    let x = start_x.min(current_x) as f32;
    let y = start_y.min(current_y) as f32;
    let width = (start_x - current_x).abs().max(1.0) as f32;
    let height = (start_y - current_y).abs().max(1.0) as f32;
    gtk::graphene::Rect::new(x, y, width, height)
}

fn update_rubberband_widget(
    rubberband: &gtk::Box,
    overlay: &gtk::Overlay,
    rect: &gtk::graphene::Rect,
) {
    let Some((x, y, width, height)) = clipped_widget_rect(rect, overlay.width(), overlay.height())
    else {
        rubberband.set_visible(false);
        return;
    };

    rubberband.set_margin_start(x);
    rubberband.set_margin_top(y);
    rubberband.set_width_request(width);
    rubberband.set_height_request(height);
    rubberband.set_visible(true);
}

fn clipped_widget_rect(
    rect: &gtk::graphene::Rect,
    max_width: i32,
    max_height: i32,
) -> Option<(i32, i32, i32, i32)> {
    let left = rect.x().max(0.0);
    let top = rect.y().max(0.0);
    let right = (rect.x() + rect.width()).min(max_width as f32);
    let bottom = (rect.y() + rect.height()).min(max_height as f32);

    if right <= left || bottom <= top {
        return None;
    }

    Some((
        left.round() as i32,
        top.round() as i32,
        (right - left).round().max(1.0) as i32,
        (bottom - top).round().max(1.0) as i32,
    ))
}

fn select_list_rows_in_rect(
    list: &gtk::ListBox,
    overlay: &gtk::Overlay,
    selection_rect: &gtk::graphene::Rect,
    apply_selection: &ApplySelectionHandler,
) {
    let mut selected = BTreeSet::new();
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if widget_intersects_rect(&row, overlay, selection_rect) {
            selected.insert(index as usize);
        }
        index += 1;
    }

    apply_selection(selected);
}

fn select_flow_children_in_rect(
    flow: &gtk::FlowBox,
    overlay: &gtk::Overlay,
    selection_rect: &gtk::graphene::Rect,
    apply_selection: &ApplySelectionHandler,
) {
    let mut selected = BTreeSet::new();
    let mut index = 0;
    while let Some(child) = flow.child_at_index(index) {
        if widget_intersects_rect(&child, overlay, selection_rect) {
            selected.insert(index as usize);
        }
        index += 1;
    }

    apply_selection(selected);
}

fn widget_intersects_rect(
    widget: &impl IsA<gtk::Widget>,
    target: &impl IsA<gtk::Widget>,
    selection_rect: &gtk::graphene::Rect,
) -> bool {
    widget
        .compute_bounds(target)
        .and_then(|bounds| bounds.intersection(selection_rect))
        .is_some()
}

#[derive(Clone, Copy)]
enum FileClipboardOperation {
    Copy,
    Cut,
}

impl FileClipboardOperation {
    fn gnome_action(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Cut => "cut",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Cut => "Cut",
        }
    }

    fn drop_operation(self) -> dnd::DropOperation {
        match self {
            Self::Copy => dnd::DropOperation::Copy,
            Self::Cut => dnd::DropOperation::Move,
        }
    }
}

fn file_clipboard_provider(
    paths: &[PathBuf],
    operation: FileClipboardOperation,
) -> gtk::gdk::ContentProvider {
    let files = paths.iter().map(gio::File::for_path).collect::<Vec<_>>();
    let file_list = gtk::gdk::FileList::from_array(&files);
    let file_list_provider = gtk::gdk::ContentProvider::for_value(&file_list.to_value());

    let gnome_payload = file_clipboard_payload(paths, operation);
    let gnome_bytes = glib::Bytes::from_owned(gnome_payload.into_bytes());
    let gnome_provider =
        gtk::gdk::ContentProvider::for_bytes("x-special/gnome-copied-files", &gnome_bytes);

    let uri_payload = file_uri_list_payload(paths);
    let uri_bytes = glib::Bytes::from_owned(uri_payload.into_bytes());
    let uri_provider = gtk::gdk::ContentProvider::for_bytes("text/uri-list", &uri_bytes);

    gtk::gdk::ContentProvider::new_union(&[file_list_provider, gnome_provider, uri_provider])
}

fn file_clipboard_payload(paths: &[PathBuf], operation: FileClipboardOperation) -> String {
    let mut payload = operation.gnome_action().to_string();
    for path in paths {
        payload.push('\n');
        payload.push_str(&file_uri_for_path(path));
    }
    payload.push('\n');
    payload
}

fn file_uri_list_payload(paths: &[PathBuf]) -> String {
    let mut payload = String::new();
    for path in paths {
        payload.push_str(&file_uri_for_path(path));
        payload.push_str("\r\n");
    }
    payload
}

fn file_uri_for_path(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

fn same_paths(left: &[PathBuf], right: &[PathBuf]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort();
    right.sort();
    left == right
}

fn is_desktop_entry_file(item: &FileItem) -> bool {
    item.kind == FileKind::File && item.name.to_ascii_lowercase().ends_with(".desktop")
}

fn is_previewable_image_file(item: &FileItem) -> bool {
    item.kind == FileKind::File
        && views::icon::is_previewable_image(&item.name)
        && item.uri.local_path().is_ok()
}

fn reveal_folder_for_path(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn image_viewer_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .css_classes(["image-viewer-button"])
        .build();
    button.set_focusable(false);
    button
}

fn image_viewer_navigation(
    images: Rc<Vec<FileItem>>,
    current_index: Rc<Cell<usize>>,
    delta: isize,
    update_view: Rc<dyn Fn()>,
) -> Rc<dyn Fn()> {
    Rc::new(move || {
        if images.len() <= 1 {
            return;
        }

        current_index.set(wrapped_image_index(
            images.len(),
            current_index.get(),
            delta,
        ));
        update_view();
    })
}

fn install_image_viewer_zoom_controls(
    picture: &gtk::Picture,
    image_scroll: &gtk::ScrolledWindow,
    image_zoom: Rc<Cell<f64>>,
) {
    let pointer_anchor = Rc::new(Cell::new(None::<(f64, f64)>));
    let motion_controller = gtk::EventControllerMotion::new();
    motion_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let enter_pointer_anchor = Rc::clone(&pointer_anchor);
    motion_controller.connect_enter(move |_, x, y| enter_pointer_anchor.set(Some((x, y))));
    let motion_pointer_anchor = Rc::clone(&pointer_anchor);
    motion_controller.connect_motion(move |_, x, y| motion_pointer_anchor.set(Some((x, y))));
    let leave_pointer_anchor = Rc::clone(&pointer_anchor);
    motion_controller.connect_leave(move |_| leave_pointer_anchor.set(None));
    image_scroll.add_controller(motion_controller);

    let scroll_controller =
        gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let scroll_picture = picture.clone();
    let scroll_image_scroll = image_scroll.clone();
    let scroll_image_zoom = Rc::clone(&image_zoom);
    let scroll_pointer_anchor = Rc::clone(&pointer_anchor);
    scroll_controller.connect_scroll(move |_, _, delta_y| {
        if delta_y == 0.0 {
            return glib::Propagation::Proceed;
        }

        let factor = if delta_y < 0.0 {
            IMAGE_VIEWER_ZOOM_STEP
        } else {
            1.0 / IMAGE_VIEWER_ZOOM_STEP
        };
        zoom_image_viewer(
            &scroll_picture,
            &scroll_image_scroll,
            &scroll_image_zoom,
            factor,
            scroll_pointer_anchor.get(),
        );
        glib::Propagation::Stop
    });
    image_scroll.add_controller(scroll_controller);

    let pan_origin = Rc::new(Cell::new(None::<(f64, f64)>));
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);

    let begin_image_scroll = image_scroll.clone();
    let begin_pan_origin = Rc::clone(&pan_origin);
    drag.connect_drag_begin(move |drag, _, _| {
        if !image_viewer_can_pan(&begin_image_scroll) {
            begin_pan_origin.set(None);
            drag.set_state(gtk::EventSequenceState::Denied);
            return;
        }

        let horizontal = begin_image_scroll.hadjustment();
        let vertical = begin_image_scroll.vadjustment();
        begin_pan_origin.set(Some((horizontal.value(), vertical.value())));
        drag.set_state(gtk::EventSequenceState::Claimed);
    });

    let update_image_scroll = image_scroll.clone();
    let update_pan_origin = Rc::clone(&pan_origin);
    drag.connect_drag_update(move |_, offset_x, offset_y| {
        let Some((start_x, start_y)) = update_pan_origin.get() else {
            return;
        };

        let horizontal = update_image_scroll.hadjustment();
        let vertical = update_image_scroll.vadjustment();
        set_adjustment_value(&horizontal, start_x - offset_x);
        set_adjustment_value(&vertical, start_y - offset_y);
    });

    let end_pan_origin = Rc::clone(&pan_origin);
    drag.connect_drag_end(move |_, _, _| end_pan_origin.set(None));
    picture.add_controller(drag);
}

fn zoom_image_viewer(
    picture: &gtk::Picture,
    image_scroll: &gtk::ScrolledWindow,
    image_zoom: &Rc<Cell<f64>>,
    factor: f64,
    anchor: Option<(f64, f64)>,
) {
    let current_zoom = image_zoom.get();
    let next_zoom = (current_zoom * factor).clamp(IMAGE_VIEWER_MIN_ZOOM, IMAGE_VIEWER_MAX_ZOOM);
    if (next_zoom - current_zoom).abs() < f64::EPSILON {
        return;
    }

    let horizontal = image_scroll.hadjustment();
    let vertical = image_scroll.vadjustment();
    let (old_width, old_height) = image_viewer_zoom_size(image_scroll, current_zoom);
    let (new_width, new_height) = image_viewer_zoom_size(image_scroll, next_zoom);
    let (viewport_x, viewport_y) = image_viewer_zoom_anchor(anchor, &horizontal, &vertical);
    let content_x = horizontal.value() + viewport_x;
    let content_y = vertical.value() + viewport_y;

    image_zoom.set(next_zoom);
    apply_image_viewer_zoom(picture, image_scroll, next_zoom);

    let horizontal_target = scaled_viewer_position(content_x, old_width, new_width) - viewport_x;
    let vertical_target = scaled_viewer_position(content_y, old_height, new_height) - viewport_y;
    glib::idle_add_local_once(move || {
        set_adjustment_value(&horizontal, horizontal_target);
        set_adjustment_value(&vertical, vertical_target);
    });
}

fn apply_image_viewer_zoom(picture: &gtk::Picture, image_scroll: &gtk::ScrolledWindow, zoom: f64) {
    if image_scroll.width() <= 1 || image_scroll.height() <= 1 {
        picture.set_size_request(-1, -1);
        return;
    }

    let (width, height) = image_viewer_zoom_size(image_scroll, zoom);
    picture.set_size_request(
        width.round().max(1.0) as i32,
        height.round().max(1.0) as i32,
    );
}

fn image_viewer_zoom_size(image_scroll: &gtk::ScrolledWindow, zoom: f64) -> (f64, f64) {
    (
        image_viewer_viewport_width(image_scroll) * zoom,
        image_viewer_viewport_height(image_scroll) * zoom,
    )
}

fn image_viewer_zoom_anchor(
    anchor: Option<(f64, f64)>,
    horizontal: &gtk::Adjustment,
    vertical: &gtk::Adjustment,
) -> (f64, f64) {
    let fallback_x = horizontal.page_size() / 2.0;
    let fallback_y = vertical.page_size() / 2.0;
    let Some((x, y)) = anchor else {
        return (fallback_x, fallback_y);
    };

    (
        x.clamp(0.0, horizontal.page_size().max(0.0)),
        y.clamp(0.0, vertical.page_size().max(0.0)),
    )
}

fn image_viewer_viewport_width(image_scroll: &gtk::ScrolledWindow) -> f64 {
    f64::from(image_scroll.width().max(1))
}

fn image_viewer_viewport_height(image_scroll: &gtk::ScrolledWindow) -> f64 {
    f64::from(image_scroll.height().max(1))
}

fn scaled_viewer_position(position: f64, old_size: f64, new_size: f64) -> f64 {
    if old_size <= 0.0 {
        return position;
    }

    position * (new_size / old_size)
}

fn image_viewer_can_pan(image_scroll: &gtk::ScrolledWindow) -> bool {
    adjustment_can_scroll(&image_scroll.hadjustment())
        || adjustment_can_scroll(&image_scroll.vadjustment())
}

fn adjustment_can_scroll(adjustment: &gtk::Adjustment) -> bool {
    adjustment.upper() - adjustment.page_size() > adjustment.lower()
}

fn set_adjustment_value(adjustment: &gtk::Adjustment, value: f64) {
    let maximum = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    adjustment.set_value(value.clamp(adjustment.lower(), maximum));
}

fn update_image_viewer(
    viewer_window: &gtk::Window,
    picture: &gtk::Picture,
    title_label: &gtk::Label,
    counter_label: &gtk::Label,
    images: &[FileItem],
    current_index: usize,
    animation_source: &Rc<RefCell<Option<glib::SourceId>>>,
) {
    cancel_image_viewer_animation(animation_source);

    let Some(item) = images.get(current_index) else {
        return;
    };
    let Ok(path) = item.uri.local_path() else {
        return;
    };

    let title = item.display_name();
    viewer_window.set_title(Some(title));
    title_label.set_text(title);
    counter_label.set_text(&format!("{} / {}", current_index + 1, images.len()));
    if is_gif_image_path(&path) && show_gif_in_picture(picture, &path, Rc::clone(animation_source))
    {
        return;
    }

    picture.set_file(Some(&gio::File::for_path(path)));
}

fn show_gif_in_picture(
    picture: &gtk::Picture,
    path: &Path,
    animation_source: Rc<RefCell<Option<glib::SourceId>>>,
) -> bool {
    let Ok(animation) = gdk_pixbuf::PixbufAnimation::from_file(path) else {
        return false;
    };

    if animation.is_static_image() {
        let Some(pixbuf) = animation.static_image() else {
            return false;
        };
        set_picture_pixbuf(picture, &pixbuf);
        return true;
    }

    let iter = animation.iter(Some(SystemTime::now()));
    set_picture_pixbuf(picture, &iter.pixbuf());
    schedule_gif_frame(picture.clone(), iter, animation_source);
    true
}

fn schedule_gif_frame(
    picture: gtk::Picture,
    iter: gdk_pixbuf::PixbufAnimationIter,
    animation_source: Rc<RefCell<Option<glib::SourceId>>>,
) {
    let source_cell = Rc::clone(&animation_source);
    let source_id = glib::timeout_add_local_once(gif_frame_delay(&iter), move || {
        if picture.root().is_none() {
            *source_cell.borrow_mut() = None;
            return;
        }

        iter.advance(SystemTime::now());
        set_picture_pixbuf(&picture, &iter.pixbuf());
        schedule_gif_frame(picture, iter, source_cell);
    });
    *animation_source.borrow_mut() = Some(source_id);
}

fn gif_frame_delay(iter: &gdk_pixbuf::PixbufAnimationIter) -> Duration {
    let delay = iter
        .delay_time()
        .unwrap_or_else(|| Duration::from_millis(100));
    delay.max(Duration::from_millis(20))
}

fn set_picture_pixbuf(picture: &gtk::Picture, pixbuf: &gdk_pixbuf::Pixbuf) {
    picture.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(pixbuf)));
}

fn cancel_image_viewer_animation(animation_source: &Rc<RefCell<Option<glib::SourceId>>>) {
    if let Some(source_id) = animation_source.borrow_mut().take() {
        source_id.remove();
    }
}

fn is_gif_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
}

fn wrapped_image_index(len: usize, current_index: usize, delta: isize) -> usize {
    let len = len as isize;
    (current_index as isize + delta).rem_euclid(len) as usize
}

fn folder_monitor_event_affects_listing(event: gio::FileMonitorEvent) -> bool {
    matches!(
        event,
        gio::FileMonitorEvent::Changed
            | gio::FileMonitorEvent::ChangesDoneHint
            | gio::FileMonitorEvent::Deleted
            | gio::FileMonitorEvent::Created
            | gio::FileMonitorEvent::AttributeChanged
            | gio::FileMonitorEvent::Unmounted
            | gio::FileMonitorEvent::Moved
            | gio::FileMonitorEvent::Renamed
            | gio::FileMonitorEvent::MovedIn
            | gio::FileMonitorEvent::MovedOut
    )
}

impl dnd::DropOperation {
    fn verb(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Move => "Moved",
        }
    }
}

fn breadcrumb_content(label: &str) -> gtk::Box {
    if label == "/" {
        return breadcrumb_content_with_icon("", "drive-harddisk-symbolic");
    }

    let icon_name = if label == "home" {
        "user-home-symbolic"
    } else {
        "folder-symbolic"
    };
    breadcrumb_content_with_icon(label, icon_name)
}

fn breadcrumb_content_with_icon(label: &str, icon_name: &str) -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .valign(gtk::Align::Center)
        .build();

    content.append(
        &gtk::Image::builder()
            .icon_name(icon_name)
            .pixel_size(16)
            .build(),
    );

    if !label.is_empty() {
        content.append(
            &gtk::Label::builder()
                .label(label)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
    }

    content
}

fn copy_path_into(source: &Path, target_dir: &Path) -> std::io::Result<bool> {
    let Some(name) = source.file_name() else {
        return Ok(false);
    };

    if source.is_dir() && target_dir.starts_with(source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot copy a folder into itself",
        ));
    }

    let target = next_available_path(&target_dir.join(name));
    copy_path_to(source, &target).map(|()| true)
}

fn drop_target_is_selected(target_dir: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path == target_dir)
}

fn move_path_into(source: &Path, target_dir: &Path) -> std::io::Result<bool> {
    let Some(name) = source.file_name() else {
        return Ok(false);
    };

    if source.parent() == Some(target_dir) {
        return Ok(false);
    }

    if source.is_dir() && target_dir.starts_with(source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot move a folder into itself",
        ));
    }

    let target = next_available_path(&target_dir.join(name));
    match fs::rename(source, &target) {
        Ok(()) => Ok(true),
        Err(error) if is_cross_device_move(&error) => {
            copy_path_to(source, &target)?;
            remove_path(source)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn copy_path_to(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        copy_dir_recursive(source, target)
    } else {
        fs::copy(source, target).map(|_| ())
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn is_cross_device_move(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(18)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let child_target = target.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &child_target)?;
        } else {
            fs::copy(&path, &child_target)?;
        }
    }
    Ok(())
}

fn new_folder_target(target_dir: &Path, name: &str) -> Result<PathBuf, &'static str> {
    if name.is_empty() {
        return Err("Name cannot be empty");
    }
    if name == "." || name == ".." || name.contains('/') {
        return Err("Name cannot contain path separators");
    }

    let target = target_dir.join(name);
    if target.exists() {
        return Err("A folder or file with that name already exists");
    }

    Ok(target)
}

fn next_available_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("copy");
    let extension = path.extension().and_then(|extension| extension.to_str());

    for index in 2.. {
        let name = match extension {
            Some(extension) => format!("{stem} {index}.{extension}"),
            None => format!("{stem} {index}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::ErrorKind,
        path::{Path, PathBuf},
    };

    use crate::providers::{FileItem, FileKind, ProviderUri};
    use tempfile::tempdir;

    use super::{
        FileClipboardOperation, copy_path_into, drop_target_is_selected, dropped_file_name_for_uri,
        file_clipboard_payload, file_uri_list_payload, folder_monitor_event_affects_listing,
        is_desktop_entry_file, is_gif_image_path, move_path_into, new_folder_target,
        partition_drop_uris,
    };

    #[test]
    fn detects_desktop_entry_files_for_launching() {
        let item = FileItem {
            uri: ProviderUri::local("/tmp/example.desktop"),
            name: "example.desktop".to_string(),
            display_name: Some("Example".to_string()),
            icon: None,
            kind: FileKind::File,
            size: Some(100),
            modified: None,
            hidden: false,
        };

        assert!(is_desktop_entry_file(&item));
    }

    #[test]
    fn detects_gif_paths_for_animation_viewer() {
        assert!(is_gif_image_path(Path::new("/tmp/clip.GIF")));
        assert!(!is_gif_image_path(Path::new("/tmp/photo.png")));
    }

    #[test]
    fn file_clipboard_payload_marks_cut_operation() {
        let paths = [PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.txt")];

        assert_eq!(
            file_clipboard_payload(&paths, FileClipboardOperation::Cut),
            "cut\nfile:///tmp/a.txt\nfile:///tmp/b.txt\n"
        );
    }

    #[test]
    fn uri_list_payload_uses_crlf_separators() {
        let paths = [PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.txt")];

        assert_eq!(
            file_uri_list_payload(&paths),
            "file:///tmp/a.txt\r\nfile:///tmp/b.txt\r\n"
        );
    }

    #[test]
    fn partitions_local_and_remote_drop_uris() {
        let (local, remote) = partition_drop_uris(vec![
            "file:///tmp/My%20Photo.png".to_string(),
            "/tmp/plain-path.txt".to_string(),
            "https://example.com/image.jpg".to_string(),
        ]);

        assert_eq!(
            local,
            vec![
                PathBuf::from("/tmp/My Photo.png"),
                PathBuf::from("/tmp/plain-path.txt"),
            ]
        );
        assert_eq!(remote, vec!["https://example.com/image.jpg".to_string()]);
    }

    #[test]
    fn infers_filename_for_remote_drop_uri() {
        assert_eq!(
            dropped_file_name_for_uri("https://example.com/assets/My%20Photo.png?size=large"),
            "My Photo.png"
        );
        assert_eq!(
            dropped_file_name_for_uri("https://example.com/"),
            "Dropped File"
        );
    }

    #[test]
    fn builds_new_folder_target_from_requested_name() {
        let temp_dir = tempdir().expect("temp dir");

        assert_eq!(
            new_folder_target(temp_dir.path(), "Projects").expect("valid folder name"),
            temp_dir.path().join("Projects")
        );
    }

    #[test]
    fn rejects_invalid_new_folder_names() {
        let temp_dir = tempdir().expect("temp dir");
        fs::write(temp_dir.path().join("exists"), "already here").expect("existing file");

        assert!(new_folder_target(temp_dir.path(), "").is_err());
        assert!(new_folder_target(temp_dir.path(), ".").is_err());
        assert!(new_folder_target(temp_dir.path(), "..").is_err());
        assert!(new_folder_target(temp_dir.path(), "parent/child").is_err());
        assert!(new_folder_target(temp_dir.path(), "exists").is_err());
    }

    #[test]
    fn folder_monitor_events_trigger_listing_updates() {
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::Created
        ));
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::Deleted
        ));
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::Renamed
        ));
        assert!(folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::AttributeChanged
        ));
        assert!(!folder_monitor_event_affects_listing(
            gio::FileMonitorEvent::PreUnmount
        ));
    }

    #[test]
    fn rejects_copying_folder_into_itself() {
        let temp_dir = tempdir().expect("temp dir");
        let source = temp_dir.path().join("folder");
        fs::create_dir(&source).expect("source folder");

        let error = copy_path_into(&source, &source).expect_err("self-copy should fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn moves_file_into_target_directory() {
        let temp_dir = tempdir().expect("temp dir");
        let source = temp_dir.path().join("note.txt");
        let target_dir = temp_dir.path().join("target");
        fs::write(&source, "hello").expect("source file");
        fs::create_dir(&target_dir).expect("target dir");

        assert!(move_path_into(&source, &target_dir).expect("move file"));
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(target_dir.join("note.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn moving_to_same_parent_is_noop() {
        let temp_dir = tempdir().expect("temp dir");
        let source = temp_dir.path().join("note.txt");
        fs::write(&source, "hello").expect("source file");

        assert!(!move_path_into(&source, temp_dir.path()).expect("same-parent move"));
        assert_eq!(fs::read_to_string(source).unwrap(), "hello");
    }

    #[test]
    fn detects_selected_drop_target() {
        let target_dir = PathBuf::from("/tmp/selected-folder");
        let dragged_paths = vec![PathBuf::from("/tmp/other-file.txt"), target_dir.clone()];

        assert!(drop_target_is_selected(&target_dir, &dragged_paths));
        assert!(!drop_target_is_selected(
            Path::new("/tmp/unselected-folder"),
            &dragged_paths
        ));
    }
}
