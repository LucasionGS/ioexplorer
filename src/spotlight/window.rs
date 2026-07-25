//! The spotlight overlay window.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc,
    time::Duration,
};

use gtk::prelude::*;

use crate::{
    config::SpotlightConfig,
    launcher::{
        app_index::LiveAppIndex,
        frecency::{self, Frecency},
        icons, spawn,
    },
    spotlight::{
        ToggleRequest,
        file_search::{FileHit, FileSearch, SearchEvent},
        keys::{self, Action},
        layout,
        prefixes::{Prefix, PrefixKind, PrefixTable},
        query::{self, Query},
        results::{self, Activation, SpotlightResult},
    },
};

/// One tick drains both the toggle socket and the filesystem walker.
const TICK_INTERVAL: Duration = Duration::from_millis(24);
/// Height at which the result list starts scrolling instead of growing.
const MAX_LIST_HEIGHT: i32 = 420;
const ROW_ICON_SIZE: i32 = 28;
/// Rows reachable via Alt+1..9.
const QUICK_PICK_ROWS: usize = 9;

pub struct SpotlightWindow {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    card: gtk::Box,
    entry: gtk::Entry,
    prefix_badge: gtk::Label,
    hint_label: gtk::Label,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    empty_label: gtk::Label,
    config: SpotlightConfig,
    prefix_table: PrefixTable,
    live_index: Rc<LiveAppIndex>,
    frecency: RefCell<Frecency>,
    file_search: FileSearch,
    file_hits: RefCell<Vec<FileHit>>,
    /// Set when the walker hit one of its bounds, so the UI can say so.
    file_search_truncated: Cell<bool>,
    results: RefCell<Vec<SpotlightResult>>,
    selected: Cell<usize>,
    /// Guards `connect_row_selected` against our own `select_row` calls.
    updating: Cell<bool>,
    server_mode: bool,
    layer_shell: bool,
}

impl SpotlightWindow {
    pub fn new(
        app: &gtk::Application,
        config: SpotlightConfig,
        prefix_table: PrefixTable,
        server_mode: bool,
    ) -> Rc<Self> {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .width_request(config.clamped_width())
            .css_classes(["spotlight-surface"])
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .css_classes(["spotlight-header"])
            .build();
        header.append(
            &gtk::Image::builder()
                .icon_name("edit-find-symbolic")
                .pixel_size(18)
                .css_classes(["spotlight-search-icon"])
                .build(),
        );

        let prefix_badge = gtk::Label::builder()
            .visible(false)
            .css_classes(["spotlight-prefix-badge"])
            .build();
        header.append(&prefix_badge);

        // A plain Entry, not a SearchEntry: SearchEntry delays its changed
        // signal and claims Escape and Ctrl+G, all of which fight this window.
        let entry = gtk::Entry::builder()
            .placeholder_text("Search apps, folders and commands")
            .hexpand(true)
            .has_frame(false)
            .css_classes(["spotlight-entry"])
            .build();
        header.append(&entry);

        let hint_label = gtk::Label::builder()
            .visible(false)
            .css_classes(["spotlight-hint", "dim-label"])
            .build();
        header.append(&hint_label);
        card.append(&header);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(true)
            .css_classes(["spotlight-results"])
            .build();

        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(MAX_LIST_HEIGHT)
            .propagate_natural_height(true)
            .css_classes(["spotlight-scroll"])
            .build();
        card.append(&scroller);

        let empty_label = gtk::Label::builder()
            .label("No results")
            .xalign(0.0)
            .visible(false)
            .css_classes(["spotlight-empty", "dim-label"])
            .build();
        card.append(&empty_label);
        card.append(&footer());

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        root.append(&card);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("IoExplorer Spotlight")
            .child(&root)
            .default_width(config.clamped_width())
            .build();
        window.set_decorated(false);
        window.add_css_class("spotlight-window");

        let layer_shell = layout::configure_layer_shell(&window);
        if layer_shell {
            root.add_css_class("spotlight-backdrop");
        } else {
            window.set_resizable(false);
        }

        let this = Rc::new(Self {
            app: app.clone(),
            window,
            card,
            entry,
            prefix_badge,
            hint_label,
            list,
            scroller,
            empty_label,
            config,
            prefix_table,
            live_index: LiveAppIndex::new(),
            frecency: RefCell::new(Frecency::load()),
            file_search: FileSearch::new(),
            file_hits: RefCell::new(Vec::new()),
            file_search_truncated: Cell::new(false),
            results: RefCell::new(Vec::new()),
            selected: Cell::new(0),
            updating: Cell::new(false),
            server_mode,
            layer_shell,
        });

        this.install_callbacks(&root);
        this.rebuild();
        this
    }

    fn install_callbacks(self: &Rc<Self>, root: &gtk::Box) {
        let this = Rc::clone(self);
        self.entry.connect_changed(move |_| this.rebuild());

        // Capture phase on the window, so these keys are handled before the
        // focused GtkText can consume Return, Tab or Escape.
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        controller.connect_key_pressed(move |_, key, _, state| this.on_key(key, state));
        self.window.add_controller(controller);

        let this = Rc::clone(self);
        self.list.connect_row_activated(move |_, row| {
            this.activate(row.index().max(0) as usize, false);
        });

        let this = Rc::clone(self);
        self.list.connect_row_selected(move |_, row| {
            if this.updating.get() {
                return;
            }
            if let Some(row) = row {
                this.selected.set(row.index().max(0) as usize);
            }
        });

        if self.layer_shell {
            // The overlay covers the whole output, so clicking the backdrop is
            // the only way to dismiss it with the pointer.
            let gesture = gtk::GestureClick::new();
            let this = Rc::clone(self);
            let card = self.card.clone();
            gesture.connect_pressed(move |_, _, x, y| {
                let inside = card.compute_bounds(&this.window).is_some_and(|bounds| {
                    bounds.contains_point(&gtk::graphene::Point::new(x as f32, y as f32))
                });
                if !inside {
                    this.close();
                }
            });
            root.add_controller(gesture);
        } else {
            let this = Rc::clone(self);
            self.window.connect_is_active_notify(move |window| {
                if window.is_visible() && !window.is_active() {
                    this.close();
                }
            });
        }

        let this = Rc::clone(self);
        self.window.connect_map(move |window| {
            layout::apply_top_offset(window, &this.card, this.config.clamped_top_ratio());
        });

        let this = Rc::clone(self);
        self.window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                return;
            };
            let this = Rc::clone(&this);
            let window = window.clone();
            // Re-run on every layout so the offset follows a monitor or
            // resolution change rather than sticking to the first output.
            surface.connect_layout(move |_, _, _| {
                layout::apply_top_offset(&window, &this.card, this.config.clamped_top_ratio());
            });
        });

        let this = Rc::clone(self);
        self.window.connect_close_request(move |_| {
            if this.server_mode {
                this.hide();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });

        let this = Rc::clone(self);
        self.live_index.connect_changed(move |_| this.rebuild());
    }

    /// Installs the single tick that drains the toggle socket and the walker.
    pub fn install_tick(self: &Rc<Self>, receiver: Option<mpsc::Receiver<ToggleRequest>>) {
        let this = Rc::clone(self);
        glib::timeout_add_local(TICK_INTERVAL, move || {
            if let Some(receiver) = &receiver {
                while receiver.try_recv().is_ok() {
                    this.toggle();
                }
            }
            this.drain_file_search();
            glib::ControlFlow::Continue
        });
    }

    fn on_key(
        self: &Rc<Self>,
        key: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        match keys::resolve(key, state) {
            Action::Close => {
                self.close();
                glib::Propagation::Stop
            }
            Action::Move(delta) => {
                self.move_selection(delta);
                glib::Propagation::Stop
            }
            Action::Activate { secondary } => {
                self.activate(self.selected.get(), secondary);
                glib::Propagation::Stop
            }
            Action::Complete => {
                self.complete();
                glib::Propagation::Stop
            }
            Action::Pick(index) => {
                self.activate(index, false);
                glib::Propagation::Stop
            }
            Action::Pass => glib::Propagation::Proceed,
        }
    }

    // -- results -----------------------------------------------------------

    fn rebuild(self: &Rc<Self>) {
        let raw = self.entry.text().to_string();
        let parsed = query::parse(&raw, &self.prefix_table);
        let limit = self.config.clamped_result_limit();

        let mut active_prefix: Option<&Prefix> = None;
        let mut results = match &parsed.query {
            Query::Empty => {
                self.file_search.cancel();
                results::default_results(
                    "",
                    &self.live_index.snapshot(),
                    &self.frecency.borrow(),
                    frecency::now_secs(),
                    limit,
                )
            }
            Query::Plain(text) => {
                self.file_search.cancel();
                results::default_results(
                    text,
                    &self.live_index.snapshot(),
                    &self.frecency.borrow(),
                    frecency::now_secs(),
                    limit,
                )
            }
            Query::Prefixed { key, arg } => {
                let Some(prefix) = self.prefix_table.get(key) else {
                    return;
                };
                active_prefix = Some(prefix);

                if matches!(prefix.kind, PrefixKind::FileSearch) {
                    self.start_file_search(arg);
                    self.file_results(limit)
                } else {
                    self.file_search.cancel();
                    results::prefixed_results(prefix, arg, &self.prefix_table, limit)
                }
            }
        };

        if let Some(key) = &parsed.hint
            && let Some(prefix) = self.prefix_table.get(key)
        {
            results.insert(0, results::hint_result(prefix));
            results.truncate(limit);
        }

        self.update_header(active_prefix, parsed.hint.as_deref());
        self.render(results);
    }

    fn update_header(&self, prefix: Option<&Prefix>, hint: Option<&str>) {
        match prefix {
            Some(prefix) => {
                self.prefix_badge.set_text(&prefix.label);
                self.prefix_badge.set_visible(true);
            }
            None => self.prefix_badge.set_visible(false),
        }

        match hint {
            Some(_) => {
                self.hint_label.set_text("Tab");
                self.hint_label.set_visible(true);
            }
            None => self.hint_label.set_visible(false),
        }
    }

    fn render(self: &Rc<Self>, results: Vec<SpotlightResult>) {
        self.updating.set(true);
        icons::clear_list_rows(&self.list);

        for (index, result) in results.iter().enumerate() {
            self.list.append(&row_for(result, index));
        }

        let is_empty = results.is_empty();
        self.empty_label.set_visible(is_empty);
        self.scroller.set_visible(!is_empty);
        *self.results.borrow_mut() = results;
        self.updating.set(false);

        self.select(0);
    }

    fn select(self: &Rc<Self>, index: usize) {
        let count = self.results.borrow().len();
        if count == 0 {
            self.selected.set(0);
            return;
        }

        let index = index.min(count - 1);
        self.selected.set(index);

        self.updating.set(true);
        let row = self.list.row_at_index(index as i32);
        self.list.select_row(row.as_ref());
        self.updating.set(false);

        if let Some(row) = row {
            let scroller = self.scroller.clone();
            let list = self.list.clone();
            // Deferred so the row has a current allocation to scroll to.
            glib::idle_add_local_once(move || scroll_into_view(&scroller, &list, &row));
        }
    }

    fn move_selection(self: &Rc<Self>, delta: i32) {
        let count = self.results.borrow().len();
        if count == 0 {
            return;
        }

        let current = self.selected.get() as i64;
        let target = match delta {
            i32::MIN => 0,
            i32::MAX => count as i64 - 1,
            delta => (current + i64::from(delta)).clamp(0, count as i64 - 1),
        };

        self.select(target as usize);
    }

    // -- filesystem search -------------------------------------------------

    /// The current filesystem hits, plus a note when the walk was cut short.
    fn file_results(&self, limit: usize) -> Vec<SpotlightResult> {
        let mut results = results::file_hit_results(&self.file_hits.borrow(), limit);
        if self.file_search_truncated.get() && !results.is_empty() {
            results.push(results::truncation_notice(results.len()));
        }
        results
    }

    fn start_file_search(&self, arg: &str) {
        self.file_hits.borrow_mut().clear();
        self.file_search_truncated.set(false);

        if arg.trim().is_empty() {
            self.file_search.cancel();
            return;
        }

        let mut roots = results::places()
            .into_iter()
            .map(|(_, path, _)| path)
            .collect::<Vec<_>>();
        roots.extend(crate::bookmarks::load());
        self.file_search.start(arg.trim(), roots);
    }

    fn drain_file_search(self: &Rc<Self>) {
        let events = self.file_search.drain();
        if events.is_empty() {
            return;
        }

        for event in events {
            match event {
                SearchEvent::Batch { hits, .. } => self.file_hits.borrow_mut().extend(hits),
                SearchEvent::Done { truncated, .. } => self.file_search_truncated.set(truncated),
            }
        }

        // Only re-render while the file-search prefix is still active.
        let parsed = query::parse(&self.entry.text(), &self.prefix_table);
        let Query::Prefixed { key, .. } = &parsed.query else {
            return;
        };
        let Some(prefix) = self.prefix_table.get(key) else {
            return;
        };
        if !matches!(prefix.kind, PrefixKind::FileSearch) {
            return;
        }

        let selected = self.selected.get();
        let results = self.file_results(self.config.clamped_result_limit());
        self.render(results);
        self.select(selected);
    }

    // -- activation --------------------------------------------------------

    fn complete(self: &Rc<Self>) {
        let completion = self
            .results
            .borrow()
            .get(self.selected.get())
            .and_then(|result| result.completion.clone());

        if let Some(text) = completion {
            self.set_entry_text(&text);
        }
    }

    fn set_entry_text(self: &Rc<Self>, text: &str) {
        self.entry.set_text(text);
        self.entry.set_position(-1);
    }

    fn activate(self: &Rc<Self>, index: usize, secondary: bool) {
        let Some(result) = self.results.borrow().get(index).cloned() else {
            return;
        };

        let activation = if secondary {
            result.secondary.clone().unwrap_or(result.primary.clone())
        } else {
            result.primary.clone()
        };

        if let Activation::Replace(text) = &activation {
            self.set_entry_text(text);
            return;
        }
        if matches!(activation, Activation::Inert) {
            return;
        }

        if let Err(error) = self.perform(&activation) {
            tracing::warn!(%error, "failed to activate spotlight result");
            return;
        }

        if let Some(key) = &result.frecency_key {
            let mut frecency = self.frecency.borrow_mut();
            frecency.record(key, frecency::now_secs());
            if let Err(error) = frecency.save() {
                tracing::warn!(%error, "failed to persist launch history");
            }
        }

        self.close();
    }

    fn perform(&self, activation: &Activation) -> Result<(), String> {
        match activation {
            Activation::LaunchApp(desktop_id) => {
                crate::launcher::app_index::launch_desktop_id(desktop_id)
            }
            Activation::OpenPath(path) => spawn::launch_in_ioexplorer(path)
                .map_err(|error| format!("failed to open {}: {error}", path.display())),
            Activation::RunShell(line) => spawn::spawn_shell_line(line, "ioexplorer-spotlight")
                .map_err(|error| format!("failed to run command: {error}")),
            Activation::RunInTerminal(line) => spawn::spawn_in_terminal(line)
                .map_err(|error| format!("failed to open a terminal: {error}")),
            Activation::CopyText(text) => {
                let display = gtk::gdk::Display::default()
                    .ok_or_else(|| "no display available for the clipboard".to_string())?;
                display.clipboard().set_text(text);
                Ok(())
            }
            Activation::Replace(_) | Activation::Inert => Ok(()),
        }
    }

    // -- visibility --------------------------------------------------------

    pub fn toggle(self: &Rc<Self>) {
        if self.window.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    pub fn show(self: &Rc<Self>) {
        self.entry.set_text("");
        self.rebuild();
        layout::apply_top_offset(&self.window, &self.card, self.config.clamped_top_ratio());
        self.window.present();

        let entry = self.entry.clone();
        glib::idle_add_local_once(move || {
            entry.grab_focus();
        });
    }

    pub fn hide(&self) {
        self.file_search.cancel();
        self.file_hits.borrow_mut().clear();
        self.entry.set_text("");
        self.window.set_visible(false);
    }

    fn close(&self) {
        // Always hide first: a full-output overlay that fails to disappear
        // swallows every click on the desktop.
        if self.server_mode {
            self.hide();
        } else {
            self.window.set_visible(false);
            self.app.quit();
        }
    }
}

fn footer() -> gtk::Box {
    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .css_classes(["spotlight-footer"])
        .build();

    for (keycap, description) in [
        ("↑↓", "Navigate"),
        ("Enter", "Open"),
        ("Tab", "Complete"),
        ("?", "Prefixes"),
        ("Esc", "Close"),
    ] {
        let group = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(5)
            .build();
        group.append(
            &gtk::Label::builder()
                .label(keycap)
                .css_classes(["spotlight-footer-key"])
                .build(),
        );
        group.append(
            &gtk::Label::builder()
                .label(description)
                .css_classes(["spotlight-footer-label", "dim-label"])
                .build(),
        );
        footer.append(&group);
    }

    footer
}

fn row_for(result: &SpotlightResult, index: usize) -> gtk::ListBoxRow {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();

    let icon = icons::image_for(&result.icon, ROW_ICON_SIZE);
    icon.add_css_class("spotlight-row-icon");
    content.append(&icon);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(&result.title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["spotlight-row-title"])
            .build(),
    );
    if !result.subtitle.is_empty() {
        text.append(
            &gtk::Label::builder()
                .label(&result.subtitle)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["spotlight-row-subtitle", "dim-label"])
                .build(),
        );
    }
    content.append(&text);

    if index < QUICK_PICK_ROWS {
        content.append(
            &gtk::Label::builder()
                .label(format!("Alt+{}", index + 1))
                .valign(gtk::Align::Center)
                .css_classes(["spotlight-row-accel", "dim-label"])
                .build(),
        );
    }

    let row = gtk::ListBoxRow::builder()
        .child(&content)
        .selectable(true)
        .activatable(true)
        .css_classes(["spotlight-row"])
        .build();
    // Focus must never leave the entry, so rows are never focusable.
    row.set_focusable(false);
    row
}

fn scroll_into_view(scroller: &gtk::ScrolledWindow, list: &gtk::ListBox, row: &gtk::ListBoxRow) {
    // Bounds relative to the list, which is the coordinate space the scrolled
    // window's vertical adjustment works in.
    let Some(bounds) = row.compute_bounds(list) else {
        return;
    };
    let adjustment = scroller.vadjustment();
    let top = f64::from(bounds.y());
    let bottom = f64::from(bounds.y() + bounds.height());

    if top < adjustment.value() {
        adjustment.set_value(top);
    } else if bottom > adjustment.value() + adjustment.page_size() {
        adjustment.set_value(bottom - adjustment.page_size());
    }
}
