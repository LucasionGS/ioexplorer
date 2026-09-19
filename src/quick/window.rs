//! The menu itself: a card at the pointer with tabs, a search entry, a
//! category bar and a grid of characters or saved GIFs.
//!
//! The layer surface covers the whole output so that a click anywhere outside
//! the card can dismiss it. The grid is a virtualised `GridView`, so a category
//! of a thousand characters costs no more to show than one of ten.
//!
//! Keyboard focus stays in the search entry throughout: typing searches, and
//! the arrows, Enter, Tab and Page Up/Down are taken before the entry sees
//! them. That also means nothing can be typed into another window while the
//! menu is open, so picks made with Shift are queued and inserted together
//! when it closes. The tag entries of the GIF tab are the exception: while one
//! has focus, keys are its own.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
};

use gtk::{cairo, gdk, gio, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use super::{
    data::{self, Item, SYMBOL_CATEGORIES},
    gifs::{
        self, Gif, Library,
        clipboard::{Detected, Found},
        parse_tags,
        thumbs::{self, Player, Thumbs},
    },
    history::{self, Entry as Clip, History},
    placement::{Placement, Screen},
    state::QuickState,
};
use crate::config::{QuickConfig, QuickTab};

/// Cells per row in the Symbols and Emoji tabs. Fixed, so arrow keys move by
/// a known amount.
const TEXT_COLUMNS: u32 = 8;
/// Cells per row in the GIF tab.
const GIF_COLUMNS: u32 = 3;
/// The largest a GIF thumbnail is drawn, in logical pixels.
const GIF_CELL: (i32, i32) = (112, 92);
pub const CARD_SIZE: (i32, i32) = (380, 460);
/// Emoji newer than this are checked against the installed fonts before they
/// are offered; older ones every emoji font has.
const TRUSTED_EMOJI_VERSION: (u8, u8) = (13, 0);

const TABS: [(QuickTab, &str); 4] = [
    (QuickTab::Symbols, "Symbols"),
    (QuickTab::Emoji, "Emoji"),
    (QuickTab::Gif, "GIF"),
    (QuickTab::Clipboard, "Clipboard"),
];
/// The most of a copied text a Clipboard row shows.
const CLIP_PREVIEW_CHARS: usize = 240;
const TONE_SAMPLES: [&str; 6] = ["✋", "✋🏻", "✋🏼", "✋🏽", "✋🏾", "✋🏿"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Category {
    Favourites,
    Recent,
    Symbols(usize),
    Emoji(&'static str),
}

impl Category {
    fn name(self) -> &'static str {
        match self {
            Self::Favourites => "Favourites",
            Self::Recent => "Recently used",
            Self::Symbols(index) => SYMBOL_CATEGORIES[index].name,
            Self::Emoji(group) => group,
        }
    }
}

/// What the menu hands over when it closes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picked {
    /// Text to type or copy: characters, and links of saved GIFs.
    pub text: String,
    /// A saved image that was put on the clipboard, to be pasted.
    pub image: Option<PathBuf>,
}

/// The widgets of the GIF tab: the offer to save what is on the clipboard, and
/// the footer's tag editing.
struct GifWidgets {
    banner: gtk::Box,
    banner_player: Rc<Player>,
    banner_title: gtk::Label,
    banner_detail: gtk::Label,
    banner_tags: gtk::Entry,
    footer_stack: gtk::Stack,
    actions: gtk::Box,
    edit_button: gtk::Button,
    edit_entry: gtk::Entry,
}

pub struct QuickMenu {
    window: gtk::ApplicationWindow,
    card: gtk::Box,
    entry: gtk::Entry,
    tab_buttons: Vec<(QuickTab, gtk::ToggleButton)>,
    category_bar: gtk::Box,
    category_buttons: RefCell<Vec<(Category, gtk::ToggleButton)>>,
    section: gtk::Label,
    grid: gtk::GridView,
    text_factory: gtk::SignalListItemFactory,
    gif_factory: gtk::SignalListItemFactory,
    clip_factory: gtk::SignalListItemFactory,
    model: gtk::StringList,
    selection: gtk::SingleSelection,
    items: RefCell<Vec<Item>>,
    preview: gtk::Label,
    name: gtk::Label,
    queue_label: gtk::Label,
    star_button: gtk::Button,
    tone_button: gtk::Button,
    gif: GifWidgets,

    config: QuickConfig,
    state: RefCell<QuickState>,
    tab: Cell<QuickTab>,
    category: Cell<Category>,
    /// Picks made with Shift, inserted together when the menu closes.
    queued: RefCell<Vec<String>>,
    /// A saved image that was put on the clipboard.
    picked_image: RefCell<Option<PathBuf>>,

    library: RefCell<Library>,
    /// The GIF tab's grid, in the order shown.
    gifs: RefCell<Vec<Gif>>,
    thumbs: Rc<Thumbs>,
    /// An unsaved image found on the clipboard.
    found: RefCell<Option<Found>>,
    /// Bumped per clipboard check, so a slow one cannot overwrite a newer one.
    detection: Cell<u32>,
    /// The file whose tags are being edited.
    editing: RefCell<Option<String>>,
    /// A GIF was dropped into another window, which counts as a pick.
    dropped: Cell<bool>,

    history: RefCell<History>,
    /// The Clipboard tab's rows, in the order shown.
    clips: RefCell<Vec<Clip>>,
    /// Watches the history, so a copy made while the menu is open shows up.
    history_monitor: RefCell<Option<gio::FileMonitor>>,
    /// Whether a server records the clipboard; looked up once per menu.
    recording: Cell<Option<bool>>,

    /// Suppresses the handlers that react to the widgets being set from code.
    updating: Cell<bool>,
    on_finish: RefCell<Option<OnFinish>>,
}

/// Called once, with everything picked, when the menu closes.
type OnFinish = Box<dyn FnOnce(Option<Picked>)>;

impl QuickMenu {
    pub fn open(
        app: &gtk::Application,
        config: QuickConfig,
        tab: QuickTab,
        placement: Placement,
        monitors: &[gdk::Monitor],
        on_finish: impl FnOnce(Option<Picked>) + 'static,
    ) -> Rc<Self> {
        install_transparency();

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Quick menu")
            .css_classes(["quick-window"])
            .decorated(false)
            .build();
        let layer_shell = gtk4_layer_shell::is_supported();
        let mut scale = 1;
        if layer_shell {
            window.init_layer_shell();
            window.set_namespace(Some("ioexplorer-quick"));
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::Exclusive);
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                window.set_anchor(edge, true);
            }
            window.set_exclusive_zone(-1);
            if let Placement::At { screen, .. } = placement {
                let monitor = monitors.get(screen);
                window.set_monitor(monitor);
                scale = monitor.map_or(1, |monitor| monitor.scale_factor().max(1));
            }
        } else {
            tracing::warn!("gtk4-layer-shell unsupported, falling back to a plain window");
            window.set_default_size(CARD_SIZE.0, CARD_SIZE.1);
        }

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["quick-card"])
            .width_request(CARD_SIZE.0)
            .height_request(CARD_SIZE.1)
            .build();
        match placement {
            Placement::At { left, top, .. } if layer_shell => {
                card.set_halign(gtk::Align::Start);
                card.set_valign(gtk::Align::Start);
                card.set_margin_start(left);
                card.set_margin_top(top);
            }
            _ if layer_shell => {
                card.set_halign(gtk::Align::Center);
                card.set_valign(gtk::Align::Center);
            }
            _ => card.set_vexpand(true),
        }
        root.append(&card);
        window.set_child(Some(&root));

        // Tabs.
        let tab_bar = gtk::Box::builder()
            .spacing(4)
            .css_classes(["quick-tabs"])
            .build();
        let mut tab_buttons: Vec<(QuickTab, gtk::ToggleButton)> = Vec::new();
        for (kind, label) in TABS {
            let button = gtk::ToggleButton::builder()
                .label(label)
                .css_classes(["quick-tab"])
                .focus_on_click(false)
                .can_focus(false)
                .build();
            if let Some((_, first)) = tab_buttons.first() {
                button.set_group(Some(first));
            }
            tab_bar.append(&button);
            tab_buttons.push((kind, button));
        }
        card.append(&tab_bar);

        let entry = gtk::Entry::builder()
            .placeholder_text("Search")
            .primary_icon_name("system-search-symbolic")
            .css_classes(["quick-entry"])
            .build();
        card.append(&entry);

        let banner = build_banner();
        card.append(&banner.0);

        let category_bar = gtk::Box::builder()
            .homogeneous(true)
            .css_classes(["quick-categories"])
            .build();
        card.append(&category_bar);

        // A wrapping label asks for its whole text as its natural width;
        // capped, it wraps within the card instead of widening it.
        let section = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(20)
            .css_classes(["quick-section", "dim-label"])
            .build();
        card.append(&section);

        let model = gtk::StringList::new(&[]);
        let selection = gtk::SingleSelection::builder()
            .model(&model)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let thumbs = Thumbs::new(((GIF_CELL.0 * scale) as u32, (GIF_CELL.1 * scale) as u32));
        let text_factory = text_factory();
        // Filled in once the menu exists, for the cells' drags to report to.
        let menu_slot: Rc<RefCell<Weak<Self>>> = Rc::default();
        let gif_factory = gif_factory(&thumbs, &menu_slot);
        let clip_factory = clip_factory(&thumbs);
        let grid = gtk::GridView::builder()
            .model(&selection)
            .factory(&text_factory)
            .min_columns(TEXT_COLUMNS)
            .max_columns(TEXT_COLUMNS)
            .single_click_activate(true)
            .can_focus(false)
            .css_classes(["quick-grid"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&grid)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .can_focus(false)
            .css_classes(["quick-scroll"])
            .build();
        card.append(&scroller);

        // Footer: what is under the pointer or selected, what is queued, and
        // the skin tone — or, in the GIF tab, the tags and what can be done
        // with them. Editing tags swaps the footer for an entry.
        let footer = gtk::Box::builder()
            .spacing(10)
            .css_classes(["quick-footer"])
            .build();
        let preview = gtk::Label::builder()
            .css_classes(["quick-preview"])
            .width_chars(2)
            .build();
        // An ellipsized label still asks for its whole text as its natural
        // width; capped, a long name is cut short instead of widening the
        // card, and it still fills the row.
        let name = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(10)
            .css_classes(["quick-name"])
            .build();
        let queue_label = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .max_width_chars(8)
            .visible(false)
            .css_classes(["quick-queue"])
            .tooltip_text("Inserted when the menu closes. Backspace removes the last one.")
            .build();
        let star_button = gtk::Button::builder()
            .css_classes(["flat", "quick-star"])
            .focus_on_click(false)
            .can_focus(false)
            .visible(false)
            .build();
        let tone_button = gtk::Button::builder()
            .css_classes(["quick-tone"])
            .focus_on_click(false)
            .can_focus(false)
            .tooltip_text("Skin tone")
            .build();
        let actions = gtk::Box::builder()
            .spacing(2)
            .visible(false)
            .css_classes(["quick-gif-actions"])
            .build();
        let edit_button = flat_icon_button("document-edit-symbolic", "Edit tags (F2)");
        let trash_button = flat_icon_button("user-trash-symbolic", "Move to trash (Delete)");
        actions.append(&edit_button);
        actions.append(&trash_button);
        footer.append(&preview);
        footer.append(&name);
        footer.append(&queue_label);
        footer.append(&star_button);
        footer.append(&tone_button);
        footer.append(&actions);

        let edit_entry = gtk::Entry::builder()
            .placeholder_text("Tags, separated by spaces or commas")
            .hexpand(true)
            .css_classes(["quick-tag-entry"])
            .build();
        let edit_done = gtk::Button::builder()
            .label("Save")
            .focus_on_click(false)
            .css_classes(["quick-save", "suggested-action"])
            .build();
        let edit_row = gtk::Box::builder()
            .spacing(6)
            .css_classes(["quick-footer"])
            .build();
        edit_row.append(&edit_entry);
        edit_row.append(&edit_done);

        let footer_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .vhomogeneous(false)
            .build();
        footer_stack.add_named(&footer, Some("info"));
        footer_stack.add_named(&edit_row, Some("edit"));
        card.append(&footer_stack);

        let (
            banner_box,
            banner_player,
            banner_title,
            banner_detail,
            banner_tags,
            banner_save,
            banner_dismiss,
        ) = banner;
        let state = QuickState::load();
        let library = Library::open(config.gif_directory_path());
        let this = Rc::new(Self {
            window,
            card,
            entry,
            tab_buttons,
            category_bar,
            category_buttons: RefCell::new(Vec::new()),
            section,
            grid,
            text_factory,
            gif_factory,
            clip_factory,
            model,
            selection,
            items: RefCell::new(Vec::new()),
            preview,
            name,
            queue_label,
            star_button,
            tone_button,
            gif: GifWidgets {
                banner: banner_box,
                banner_player,
                banner_title,
                banner_detail,
                banner_tags,
                footer_stack,
                actions,
                edit_button: edit_button.clone(),
                edit_entry,
            },
            config,
            state: RefCell::new(state),
            tab: Cell::new(tab),
            category: Cell::new(Category::Recent),
            queued: RefCell::new(Vec::new()),
            picked_image: RefCell::new(None),
            library: RefCell::new(library),
            gifs: RefCell::new(Vec::new()),
            thumbs,
            found: RefCell::new(None),
            detection: Cell::new(0),
            editing: RefCell::new(None),
            dropped: Cell::new(false),
            history: RefCell::new(History::open_default()),
            clips: RefCell::new(Vec::new()),
            history_monitor: RefCell::new(None),
            recording: Cell::new(None),
            updating: Cell::new(false),
            on_finish: RefCell::new(Some(Box::new(on_finish))),
        });

        *menu_slot.borrow_mut() = Rc::downgrade(&this);
        this.install_callbacks(&root, layer_shell);
        this.install_gif_callbacks(
            &banner_save,
            &banner_dismiss,
            &edit_button,
            &trash_button,
            &edit_done,
        );
        this.watch_history();
        this.update_tone_button();
        this.switch_tab(tab);
        this.window.present();
        this.entry.grab_focus();
        this
    }

    fn install_callbacks(self: &Rc<Self>, root: &gtk::Box, layer_shell: bool) {
        for (tab, button) in &self.tab_buttons {
            let this = Rc::downgrade(self);
            let tab = *tab;
            button.connect_toggled(move |button| {
                let Some(this) = this.upgrade() else { return };
                if button.is_active() && !this.updating.get() {
                    this.switch_tab(tab);
                }
            });
        }

        let this = Rc::downgrade(self);
        self.entry.connect_changed(move |_| {
            if let Some(this) = this.upgrade()
                && !this.updating.get()
            {
                this.refresh();
            }
        });

        // Capture phase on the window, so these are handled before the entry
        // can move its cursor with the arrows or take Tab for focus.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| match this.upgrade() {
            Some(this) => this.on_key(key, state),
            None => glib::Propagation::Proceed,
        });
        self.window.add_controller(keys);

        let this = Rc::downgrade(self);
        self.grid.connect_activate(move |_, position| {
            if let Some(this) = this.upgrade() {
                this.pick(position, shift_held());
            }
        });

        let this = Rc::downgrade(self);
        self.selection.connect_selected_notify(move |_| {
            if let Some(this) = this.upgrade() {
                this.update_footer();
            }
        });

        let this = Rc::downgrade(self);
        self.star_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.toggle_favourite();
            }
        });

        // A right-click stars what is under the pointer, which is what the
        // grid selects on hover.
        let right_click = gtk::GestureClick::new();
        right_click.set_button(gdk::BUTTON_SECONDARY);
        let this = Rc::downgrade(self);
        right_click.connect_pressed(move |gesture, _, _, _| {
            if let Some(this) = this.upgrade() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                this.toggle_favourite();
            }
        });
        self.grid.add_controller(right_click);

        let this = Rc::downgrade(self);
        self.tone_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.cycle_tone();
            }
        });

        if layer_shell {
            let gesture = gtk::GestureClick::new();
            gesture.set_button(0);
            let this = Rc::downgrade(self);
            gesture.connect_pressed(move |_, _, x, y| {
                let Some(this) = this.upgrade() else { return };
                let point = gtk::graphene::Point::new(x as f32, y as f32);
                let inside = this
                    .card
                    .compute_bounds(&this.window)
                    .is_some_and(|bounds| bounds.contains_point(&point));
                if !inside {
                    tracing::debug!(x, y, "a click outside the card closes the menu");
                    this.finish();
                }
            });
            root.add_controller(gesture);
        } else {
            let this = Rc::downgrade(self);
            self.window.connect_is_active_notify(move |window| {
                if window.is_visible()
                    && !window.is_active()
                    && let Some(this) = this.upgrade()
                {
                    this.finish();
                }
            });
        }

        let this = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(this) = this.upgrade() {
                tracing::debug!("the menu was asked to close");
                this.finish();
            }
            glib::Propagation::Proceed
        });
    }

    fn install_gif_callbacks(
        self: &Rc<Self>,
        save: &gtk::Button,
        dismiss: &gtk::Button,
        edit: &gtk::Button,
        trash: &gtk::Button,
        edit_done: &gtk::Button,
    ) {
        let this = Rc::downgrade(self);
        save.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.save_found();
            }
        });
        let this = Rc::downgrade(self);
        self.gif.banner_tags.connect_activate(move |_| {
            if let Some(this) = this.upgrade() {
                this.save_found();
            }
        });
        let this = Rc::downgrade(self);
        dismiss.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.found.borrow_mut().take();
                this.show_banner(false);
            }
        });
        let this = Rc::downgrade(self);
        edit.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.edit_tags();
            }
        });
        let this = Rc::downgrade(self);
        trash.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.trash_selected();
            }
        });
        let this = Rc::downgrade(self);
        edit_done.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.commit_tags();
            }
        });
        let this = Rc::downgrade(self);
        self.gif.edit_entry.connect_activate(move |_| {
            if let Some(this) = this.upgrade() {
                this.commit_tags();
            }
        });

        // A copy made while the menu is open, or the clipboard arriving once
        // the menu has keyboard focus, which is when Wayland hands it over.
        let this = Rc::downgrade(self);
        self.window.clipboard().connect_changed(move |_| {
            if let Some(this) = this.upgrade()
                && this.tab.get() == QuickTab::Gif
            {
                this.detect_clipboard();
            }
        });
    }

    /// The watcher records copies made while the menu is open. The folder
    /// is watched rather than the index, which is replaced by a rename.
    fn watch_history(self: &Rc<Self>) {
        let index = self.history.borrow().index_path();
        let (Some(directory), Some(index_name)) = (index.parent(), index.file_name()) else {
            return;
        };
        let _ = std::fs::create_dir_all(directory);
        let index_name = index_name.to_os_string();
        let monitor = gio::File::for_path(directory)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE);
        match monitor {
            Ok(monitor) => {
                let this = Rc::downgrade(self);
                monitor.connect_changed(move |_, file, other, event| {
                    let Some(this) = this.upgrade() else { return };
                    let is_index = |file: Option<&gio::File>| {
                        file.and_then(|file| file.basename())
                            .is_some_and(|name| name.as_os_str() == index_name)
                    };
                    let settled = matches!(
                        event,
                        gio::FileMonitorEvent::ChangesDoneHint
                            | gio::FileMonitorEvent::Created
                            | gio::FileMonitorEvent::MovedIn
                            | gio::FileMonitorEvent::Renamed
                    );
                    let touched = is_index(Some(file)) || is_index(other);
                    let open = this.on_finish.borrow().is_some();
                    if settled && touched && open && this.tab.get() == QuickTab::Clipboard {
                        this.reload_history();
                    }
                });
                *self.history_monitor.borrow_mut() = Some(monitor);
            }
            Err(error) => tracing::debug!(%error, "cannot watch the clipboard history"),
        }
    }

    fn on_key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        // The tag entries keep their keys, bar Escape, which leaves them, and
        // Ctrl+S, which saves like Enter.
        let control = state.contains(gdk::ModifierType::CONTROL_MASK);
        let save_key = control && matches!(key, gdk::Key::s | gdk::Key::S);
        if self.focus_in(&self.gif.edit_entry) {
            if key == gdk::Key::Escape {
                self.stop_editing();
                return glib::Propagation::Stop;
            }
            if save_key {
                self.commit_tags();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if self.focus_in(&self.gif.banner_tags) {
            if key == gdk::Key::Escape {
                self.entry.grab_focus();
                return glib::Propagation::Stop;
            }
            if save_key {
                self.save_found();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        // Ctrl+S reaches the offer to save from the search: to its tags,
        // where Enter or Ctrl+S again saves.
        if save_key {
            if self.gif.banner.is_visible() && self.found.borrow().is_some() {
                self.gif.banner_tags.grab_focus();
            }
            return glib::Propagation::Stop;
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let columns = i64::from(self.columns(self.tab.get()));
        let step = |delta: i64| {
            self.move_selection(delta);
            glib::Propagation::Stop
        };
        let gif_tab = self.tab.get() == QuickTab::Gif;
        let removable = gif_tab || self.tab.get() == QuickTab::Clipboard;
        match key {
            gdk::Key::Escape => {
                if self.entry.text().is_empty() {
                    self.finish();
                } else {
                    self.entry.set_text("");
                }
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => {
                let selected = self.selection.selected();
                if selected != gtk::INVALID_LIST_POSITION {
                    self.pick(selected, shift);
                } else if !shift {
                    // Nothing selected: Enter just inserts what is queued.
                    self.finish();
                }
                glib::Propagation::Stop
            }
            gdk::Key::Left | gdk::Key::KP_Left => step(-1),
            gdk::Key::Right | gdk::Key::KP_Right => step(1),
            gdk::Key::Up | gdk::Key::KP_Up => step(-columns),
            gdk::Key::Down | gdk::Key::KP_Down => step(columns),
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
                let backwards = shift || key == gdk::Key::ISO_Left_Tab;
                self.cycle_tab(if backwards { -1 } else { 1 });
                glib::Propagation::Stop
            }
            gdk::Key::Page_Up | gdk::Key::KP_Page_Up => {
                self.cycle_category(-1);
                glib::Propagation::Stop
            }
            gdk::Key::Page_Down | gdk::Key::KP_Page_Down => {
                self.cycle_category(1);
                glib::Propagation::Stop
            }
            gdk::Key::d | gdk::Key::D if control => {
                self.toggle_favourite();
                glib::Propagation::Stop
            }
            gdk::Key::F2 if gif_tab => {
                self.edit_tags();
                glib::Propagation::Stop
            }
            gdk::Key::Delete | gdk::Key::KP_Delete if removable && self.entry.text().is_empty() => {
                self.trash_selected();
                glib::Propagation::Stop
            }
            gdk::Key::BackSpace if self.entry.text().is_empty() => {
                if self.queued.borrow_mut().pop().is_some() {
                    self.update_queue();
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn focus_in(&self, widget: &impl IsA<gtk::Widget>) -> bool {
        let widget = widget.as_ref();
        gtk::prelude::GtkWindowExt::focus(&self.window)
            .is_some_and(|focus| &focus == widget || focus.is_ancestor(widget))
    }

    // -- Tabs and categories -------------------------------------------------

    fn columns(&self, tab: QuickTab) -> u32 {
        match tab {
            QuickTab::Gif => GIF_COLUMNS,
            QuickTab::Clipboard => 1,
            QuickTab::Symbols | QuickTab::Emoji => TEXT_COLUMNS,
        }
    }

    fn categories(&self, tab: QuickTab) -> Vec<Category> {
        let mut categories = Vec::new();
        if matches!(tab, QuickTab::Gif | QuickTab::Clipboard) {
            return categories;
        }
        categories.push(Category::Favourites);
        if self.config.recent_limit > 0 {
            categories.push(Category::Recent);
        }
        match tab {
            QuickTab::Symbols => {
                categories.extend((0..SYMBOL_CATEGORIES.len()).map(Category::Symbols));
            }
            QuickTab::Emoji => {
                categories.extend(data::emoji().groups.iter().copied().map(Category::Emoji));
            }
            QuickTab::Gif | QuickTab::Clipboard => {}
        }
        categories
    }

    fn switch_tab(self: &Rc<Self>, tab: QuickTab) {
        self.tab.set(tab);
        self.updating.set(true);
        for (kind, button) in &self.tab_buttons {
            button.set_active(*kind == tab);
        }
        self.updating.set(false);
        self.tone_button.set_visible(tab == QuickTab::Emoji);
        self.stop_editing();

        // GIF cells are pictures and Clipboard rows are rows, not characters:
        // the grid is emptied before its factory changes, so no cell is ever
        // bound with another tab's kind.
        let gif = tab == QuickTab::Gif;
        let clipboard = tab == QuickTab::Clipboard;
        let factory = match tab {
            QuickTab::Gif => &self.gif_factory,
            QuickTab::Clipboard => &self.clip_factory,
            QuickTab::Symbols | QuickTab::Emoji => &self.text_factory,
        };
        if self.grid.factory().as_ref() != Some(factory.upcast_ref()) {
            self.model.splice(0, self.model.n_items(), &[] as &[&str]);
            self.grid.set_factory(Some(factory));
        }
        let columns = self.columns(tab);
        self.grid.set_min_columns(1);
        self.grid.set_max_columns(columns);
        self.grid.set_min_columns(columns);
        self.category_bar.set_visible(!gif && !clipboard);
        self.preview.set_visible(!gif && !clipboard);
        self.gif.actions.set_visible(false);
        self.gif.edit_button.set_visible(gif);
        self.entry.set_placeholder_text(Some(match tab {
            QuickTab::Gif => "Search tags",
            QuickTab::Clipboard => "Search copied text",
            QuickTab::Symbols | QuickTab::Emoji => "Search",
        }));
        if clipboard {
            self.history.replace(History::open_default());
        }
        if gif {
            self.detect_clipboard();
        } else {
            self.show_banner(false);
        }

        // The category bar is rebuilt for the tab.
        while let Some(child) = self.category_bar.first_child() {
            self.category_bar.remove(&child);
        }
        let categories = self.categories(tab);
        let mut buttons: Vec<(Category, gtk::ToggleButton)> = Vec::new();
        for category in &categories {
            let button = gtk::ToggleButton::builder()
                .css_classes(["quick-category"])
                .tooltip_text(category.name())
                .focus_on_click(false)
                .can_focus(false)
                .build();
            match category {
                Category::Favourites => button.set_icon_name("starred-symbolic"),
                Category::Recent => {
                    button.set_icon_name("document-open-recent-symbolic");
                }
                Category::Symbols(index) => button.set_label(SYMBOL_CATEGORIES[*index].icon),
                Category::Emoji(group) => button.set_label(data::emoji_group_icon(group)),
            }
            if let Some((_, first)) = buttons.first() {
                button.set_group(Some(first));
            }
            let this = Rc::downgrade(self);
            let category = *category;
            button.connect_toggled(move |button| {
                let Some(this) = this.upgrade() else { return };
                if button.is_active() && !this.updating.get() {
                    this.show_category(category);
                }
            });
            self.category_bar.append(&button);
            buttons.push((category, button));
        }
        *self.category_buttons.borrow_mut() = buttons;

        // Favourites first, then recently used, skipping either while empty.
        let first = {
            let state = self.state.borrow();
            let has_favourites = !state.favourites(tab).is_empty();
            let has_recent = !state.recent(tab).is_empty();
            categories
                .iter()
                .copied()
                .find(|category| match category {
                    Category::Favourites => has_favourites,
                    Category::Recent => has_recent,
                    _ => true,
                })
                .unwrap_or(Category::Recent)
        };
        self.category.set(first);
        self.refresh();
    }

    fn cycle_tab(self: &Rc<Self>, delta: i32) {
        let current = TABS
            .iter()
            .position(|(tab, _)| *tab == self.tab.get())
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(TABS.len() as i32) as usize;
        self.switch_tab(TABS[next].0);
    }

    /// Picking a category leaves a search, the way clicking one would.
    fn show_category(&self, category: Category) {
        self.category.set(category);
        if self.entry.text().is_empty() {
            self.refresh();
        } else {
            // `changed` refreshes.
            self.entry.set_text("");
        }
    }

    fn cycle_category(&self, delta: i32) {
        let categories = self.categories(self.tab.get());
        if categories.is_empty() {
            return;
        }
        let current = categories
            .iter()
            .position(|category| *category == self.category.get())
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(categories.len() as i32) as usize;
        self.show_category(categories[next]);
    }

    // -- Content -------------------------------------------------------------

    /// Refills the grid from the search, or from the current category.
    fn refresh(&self) {
        let query = self.entry.text().trim().to_string();
        let tab = self.tab.get();
        let searching = !query.is_empty();

        let (items, section) = if tab == QuickTab::Gif {
            self.gif_items(&query)
        } else if tab == QuickTab::Clipboard {
            self.clip_items(&query)
        } else {
            let items = self.text_items(&query);
            let section = match (searching, items.is_empty()) {
                (true, true) => "No matches".to_string(),
                (true, false) => format!("Results for “{query}”"),
                (false, true) if self.category.get() == Category::Recent => {
                    "Nothing used yet".to_string()
                }
                (false, true) if self.category.get() == Category::Favourites => {
                    "No favourites yet. Ctrl+D or a right-click adds one.".to_string()
                }
                (false, _) => self.category.get().name().to_string(),
            };
            (items, section)
        };
        self.section.set_label(&section);

        self.updating.set(true);
        for (category, button) in self.category_buttons.borrow().iter() {
            button.set_active(!searching && *category == self.category.get());
        }
        self.updating.set(false);

        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        *self.items.borrow_mut() = items.clone();
        self.model.splice(0, self.model.n_items(), &texts);
        if items.is_empty() {
            self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        } else {
            self.selection.set_selected(0);
            self.grid
                .scroll_to(0, gtk::ListScrollFlags::NONE, None::<gtk::ScrollInfo>);
        }
        self.update_footer();
    }

    fn text_items(&self, query: &str) -> Vec<Item> {
        let tab = self.tab.get();
        let tone = self.state.borrow().skin_tone;
        if !query.is_empty() {
            let mut items = match tab {
                QuickTab::Symbols => data::search_symbols(query)
                    .into_iter()
                    .filter(|item| self.renders(&item.text))
                    .collect(),
                QuickTab::Emoji => {
                    let limit = self.emoji_limit();
                    data::emoji().search(query, tone, |emoji| supported(emoji, limit))
                }
                QuickTab::Gif | QuickTab::Clipboard => Vec::new(),
            };
            // Favourites first; the sort is stable, so ranked otherwise.
            let state = self.state.borrow();
            items.sort_by_key(|item| !state.is_favourite(tab, &item.text));
            return items;
        }
        let named = |texts: &[String]| -> Vec<Item> {
            texts
                .iter()
                .map(|text| Item {
                    text: text.clone(),
                    name: data::name_of(text),
                })
                .collect()
        };
        match self.category.get() {
            Category::Favourites => named(self.state.borrow().favourites(tab)),
            Category::Recent => named(self.state.borrow().recent(tab)),
            Category::Symbols(index) => SYMBOL_CATEGORIES[index]
                .items()
                .into_iter()
                .filter(|item| self.renders(&item.text))
                .collect(),
            Category::Emoji(group) => {
                let limit = self.emoji_limit();
                data::emoji().group_items(group, tone, |emoji| supported(emoji, limit))
            }
        }
    }

    /// The GIF tab's cells, whose text is the file path, and its heading.
    fn gif_items(&self, query: &str) -> (Vec<Item>, String) {
        let library = self.library.borrow();
        let gifs = if query.is_empty() {
            library.list()
        } else {
            library.search(query)
        };
        let section = match (query.is_empty(), gifs.is_empty()) {
            (false, true) => "No GIFs tagged like that".to_string(),
            (false, false) => format!("Tagged “{query}”"),
            (true, true) => {
                "Nothing saved yet. Copy a GIF or an image, then open this tab to save it."
                    .to_string()
            }
            (true, false) if gifs.iter().any(|gif| gif.favourite) => {
                "Saved, favourites first".to_string()
            }
            (true, false) => "Saved".to_string(),
        };
        let items = gifs
            .iter()
            .map(|gif| Item {
                text: gif.path.to_string_lossy().into_owned(),
                name: gif.label(),
            })
            .collect();
        *self.gifs.borrow_mut() = gifs;
        (items, section)
    }

    /// The Clipboard tab's rows and heading. Each row's text says what the
    /// row shows: its kind and whether it is pinned, then the text or the
    /// image's path (see [`clip_row`]).
    fn clip_items(&self, query: &str) -> (Vec<Item>, String) {
        let recording = self
            .recording
            .get()
            .unwrap_or_else(history::watcher_running);
        self.recording.set(Some(recording));
        let history = self.history.borrow();
        let clips = if query.is_empty() {
            history.list()
        } else {
            history.search(query)
        };
        let section = match (query.is_empty(), clips.is_empty()) {
            (false, true) => "No copied text like that".to_string(),
            (false, false) => format!("Copied text with “{query}”"),
            (true, true) if !recording => {
                "Copies are not being recorded. Run ioexplorer-quick --server with the session to keep them here."
                    .to_string()
            }
            (true, true) => "Nothing copied yet".to_string(),
            (true, false) if !recording => {
                "Copied earlier (not recording now)".to_string()
            }
            (true, false) => "Copied recently".to_string(),
        };
        let items = clips
            .iter()
            .map(|clip| Item {
                text: clip_row(clip, &history),
                name: String::new(),
            })
            .collect();
        *self.clips.borrow_mut() = clips;
        (items, section)
    }

    /// Rereads the history after the watcher wrote to it. A new copy lands
    /// on top and is selected, unless something further down was: that
    /// stays selected, and the list is left where it was scrolled.
    fn reload_history(&self) {
        let selected = self.selection.selected();
        let kept = (selected != 0)
            .then(|| self.selected_clip().map(|clip| clip.id))
            .flatten();
        self.history.replace(History::open_default());
        self.refresh();
        if let Some(id) = kept {
            let position = self.clips.borrow().iter().position(|clip| clip.id == id);
            if let Some(position) = position {
                self.selection.set_selected(position as u32);
            }
        }
    }

    fn selected_clip(&self) -> Option<Clip> {
        if self.tab.get() != QuickTab::Clipboard {
            return None;
        }
        self.clips
            .borrow()
            .get(self.selection.selected() as usize)
            .cloned()
    }

    fn select_clip(&self, id: u64) {
        let position = self.clips.borrow().iter().position(|clip| clip.id == id);
        if let Some(position) = position {
            self.select(position as u32);
        }
    }

    fn move_selection(&self, delta: i64) {
        let count = i64::from(self.model.n_items());
        if count == 0 {
            return;
        }
        let next = match self.selection.selected() {
            gtk::INVALID_LIST_POSITION => 0,
            selected => (i64::from(selected) + delta).clamp(0, count - 1) as u32,
        };
        self.select(next);
    }

    fn select(&self, position: u32) {
        self.selection.set_selected(position);
        self.grid.scroll_to(
            position,
            gtk::ListScrollFlags::NONE,
            None::<gtk::ScrollInfo>,
        );
    }

    fn update_footer(&self) {
        let selected = self.selection.selected() as usize;
        let favourite = self.selected_is_favourite();
        self.star_button.set_visible(favourite.is_some());
        let favourite = favourite.unwrap_or_default();
        // Characters, not icons: not every icon theme has an empty star.
        self.star_button
            .set_label(if favourite { "★" } else { "☆" });
        self.star_button.set_tooltip_text(Some(if favourite {
            "Remove from favourites (Ctrl+D)"
        } else {
            "Add to favourites (Ctrl+D)"
        }));
        if self.tab.get() == QuickTab::Clipboard {
            let clip = self.clips.borrow().get(selected).cloned();
            self.gif.actions.set_visible(clip.is_some());
            self.name.set_tooltip_text(None);
            let Some(clip) = clip else {
                self.name.set_label("");
                return;
            };
            let what = match &clip.text {
                Some(text) => {
                    let characters = text.chars().count();
                    format!(
                        "{characters} character{}",
                        if characters == 1 { "" } else { "s" }
                    )
                }
                None => "Image".to_string(),
            };
            self.name.set_label(&format!(
                "{what} · {}",
                history::ago(history::now_secs(), clip.copied)
            ));
            return;
        }
        if self.tab.get() == QuickTab::Gif {
            let gifs = self.gifs.borrow();
            let gif = gifs.get(selected);
            self.gif.actions.set_visible(gif.is_some());
            self.name
                .set_label(&gif.map(gif_caption).unwrap_or_default());
            self.name
                .set_tooltip_text(gif.and_then(|gif| gif.source.as_deref()));
            return;
        }
        self.name.set_tooltip_text(None);
        let items = self.items.borrow();
        match items.get(selected) {
            Some(item) => {
                self.preview.set_label(&item.text);
                self.name.set_label(&item.name);
            }
            None => {
                self.preview.set_label("");
                self.name.set_label("");
            }
        }
    }

    /// Whether the selected character or GIF is a favourite; `None` when
    /// nothing is selected.
    fn selected_is_favourite(&self) -> Option<bool> {
        let selected = self.selection.selected() as usize;
        let tab = self.tab.get();
        if tab == QuickTab::Gif {
            return self.gifs.borrow().get(selected).map(|gif| gif.favourite);
        }
        if tab == QuickTab::Clipboard {
            return self.clips.borrow().get(selected).map(|clip| clip.pinned);
        }
        let items = self.items.borrow();
        let item = items.get(selected)?;
        Some(self.state.borrow().is_favourite(tab, &item.text))
    }

    /// Stars the selection, or unstars it, and keeps it selected wherever
    /// that moves it.
    fn toggle_favourite(&self) {
        let tab = self.tab.get();
        let selected = self.selection.selected();
        if tab == QuickTab::Gif {
            let Some(gif) = self.selected_gif() else {
                return;
            };
            if let Err(error) = self.library.borrow_mut().toggle_favourite(&gif.file_name) {
                tracing::warn!(%error, "cannot save the favourite");
                return;
            }
            // Favourites sort first, so it moves.
            self.refresh();
            self.select_path(&gif.path);
            return;
        }
        if tab == QuickTab::Clipboard {
            let Some(clip) = self.selected_clip() else {
                return;
            };
            if let Err(error) = self.history.borrow_mut().toggle_pin(clip.id) {
                tracing::warn!(%error, "cannot pin the copy");
                return;
            }
            self.refresh();
            self.select_clip(clip.id);
            return;
        }
        let Some(item) = self.items.borrow().get(selected as usize).cloned() else {
            return;
        };
        self.state.borrow_mut().toggle_favourite(tab, &item.text);
        self.state.borrow().save();
        let reorders = !self.entry.text().is_empty() || self.category.get() == Category::Favourites;
        if !reorders {
            self.update_footer();
            return;
        }
        self.refresh();
        let position = self
            .items
            .borrow()
            .iter()
            .position(|shown| shown.text == item.text)
            .map_or(selected, |position| position as u32);
        let count = self.model.n_items();
        if count > 0 {
            self.select(position.min(count - 1));
        }
    }

    fn update_queue(&self) {
        let queued = self.queued.borrow().concat();
        self.queue_label.set_visible(!queued.is_empty());
        self.queue_label.set_label(&queued);
    }

    fn cycle_tone(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.skin_tone = (state.skin_tone + 1) % TONE_SAMPLES.len() as u8;
        }
        self.state.borrow().save();
        self.update_tone_button();
        let selected = self.selection.selected();
        self.refresh();
        if selected < self.model.n_items() {
            self.selection.set_selected(selected);
        }
    }

    fn update_tone_button(&self) {
        let tone = usize::from(self.state.borrow().skin_tone).min(TONE_SAMPLES.len() - 1);
        self.tone_button.set_label(TONE_SAMPLES[tone]);
    }

    fn renders(&self, text: &str) -> bool {
        text_renders(text)
    }

    fn emoji_limit(&self) -> Option<(u8, u8)> {
        fonts_emoji_limit()
    }

    // -- GIFs ----------------------------------------------------------------

    /// Checks the clipboard for an image to offer, in the background.
    fn detect_clipboard(self: &Rc<Self>) {
        let generation = self.detection.get().wrapping_add(1);
        self.detection.set(generation);
        let clipboard = self.window.clipboard();
        // Its own copy of the index: the check may be downloading for a while,
        // and must not hold the menu's library borrowed through that.
        let library = Library::open(self.library.borrow().directory().to_path_buf());
        let this = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let detected = gifs::clipboard::detect(&clipboard, &library).await;
            let Some(this) = this.upgrade() else { return };
            if this.detection.get() != generation
                || this.tab.get() != QuickTab::Gif
                || this.on_finish.borrow().is_none()
            {
                return;
            }
            match detected {
                Detected::New(found) => this.offer_found(found).await,
                Detected::Saved(file_name) => {
                    tracing::debug!(%file_name, "the copied image is saved already");
                    this.found.borrow_mut().take();
                    this.show_banner(false);
                }
                Detected::Nothing => {
                    this.found.borrow_mut().take();
                    this.show_banner(false);
                }
            }
        });
    }

    async fn offer_found(self: &Rc<Self>, found: Found) {
        let animated = found.kind == gifs::ImageKind::Gif;
        self.gif.banner_title.set_label(if animated {
            "Save the copied GIF?"
        } else {
            "Save the copied image?"
        });
        let detail = match &found.source {
            Some(source) => host_of(source).unwrap_or(source).to_string(),
            None => "From the clipboard".to_string(),
        };
        self.gif.banner_detail.set_label(&detail);
        self.gif
            .banner_detail
            .set_tooltip_text(found.source.as_deref());
        self.gif.banner_tags.set_text("");
        self.gif.banner_player.show(None);
        let bytes = found.bytes.clone();
        *self.found.borrow_mut() = Some(found);
        self.show_banner(true);

        let thumb = thumbs::decode_bytes(bytes, (56, 56)).await;
        if self.found.borrow().is_some() {
            self.gif.banner_player.show(thumb);
        }
    }

    fn show_banner(&self, visible: bool) {
        self.gif.banner.set_visible(visible);
        if !visible && self.focus_in(&self.gif.banner_tags) {
            self.entry.grab_focus();
        }
    }

    fn save_found(&self) {
        let Some(found) = self.found.borrow_mut().take() else {
            return;
        };
        let tags = parse_tags(&self.gif.banner_tags.text());
        let result =
            self.library
                .borrow_mut()
                .save(&found.bytes, found.kind, found.source.clone(), tags);
        match result {
            Ok(path) => {
                self.show_banner(false);
                self.entry.grab_focus();
                if self.entry.text().is_empty() {
                    self.refresh();
                } else {
                    // `changed` refreshes.
                    self.entry.set_text("");
                }
                self.select_path(&path);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot save the image");
                self.gif
                    .banner_title
                    .set_label(&format!("Could not save it: {error}"));
                *self.found.borrow_mut() = Some(found);
            }
        }
    }

    fn select_path(&self, path: &Path) {
        let position = self.gifs.borrow().iter().position(|gif| gif.path == path);
        if let Some(position) = position {
            self.select(position as u32);
        }
    }

    fn selected_gif(&self) -> Option<Gif> {
        if self.tab.get() != QuickTab::Gif {
            return None;
        }
        self.gifs
            .borrow()
            .get(self.selection.selected() as usize)
            .cloned()
    }

    fn edit_tags(&self) {
        let Some(gif) = self.selected_gif() else {
            return;
        };
        self.gif.edit_entry.set_text(&gif.tags.join(" "));
        *self.editing.borrow_mut() = Some(gif.file_name);
        self.gif.footer_stack.set_visible_child_name("edit");
        self.gif.edit_entry.grab_focus();
    }

    fn commit_tags(&self) {
        let Some(file_name) = self.editing.borrow_mut().take() else {
            return;
        };
        let tags = parse_tags(&self.gif.edit_entry.text());
        if let Err(error) = self.library.borrow_mut().set_tags(&file_name, tags) {
            tracing::warn!(%error, "cannot save the tags");
        }
        self.stop_editing();
        let path = self.library.borrow().directory().join(&file_name);
        let selected = self.selection.selected();
        self.refresh();
        // Retagging can take it out of the search it was found by.
        if self.gifs.borrow().iter().any(|gif| gif.path == path) {
            self.select_path(&path);
        } else if selected < self.model.n_items() {
            self.select(selected);
        }
    }

    fn stop_editing(&self) {
        self.editing.borrow_mut().take();
        self.gif.footer_stack.set_visible_child_name("info");
        if self.focus_in(&self.gif.edit_entry) {
            self.entry.grab_focus();
        }
    }

    fn trash_selected(&self) {
        let selected = self.selection.selected();
        if let Some(clip) = self.selected_clip() {
            if let Err(error) = self.history.borrow_mut().remove(clip.id) {
                tracing::warn!(%error, "cannot remove the copy");
                return;
            }
            self.refresh();
            let count = self.model.n_items();
            if count > 0 {
                self.select(selected.min(count - 1));
            }
            return;
        }
        let Some(gif) = self.selected_gif() else {
            return;
        };
        if let Err(error) = self.library.borrow_mut().remove(&gif.file_name) {
            tracing::warn!(%error, "cannot remove the image");
            return;
        }
        self.thumbs.forget(&gif.path);
        self.refresh();
        let count = self.model.n_items();
        if count > 0 {
            self.select(selected.min(count - 1));
        }
    }

    // -- Dragging ------------------------------------------------------------

    /// The surface covers the whole output and would take the drop itself;
    /// for the drag, it takes input over the card only, so the window under
    /// the pointer gets it.
    fn drag_started(&self) {
        let (Some(surface), Some(bounds)) = (
            self.window.surface(),
            self.card.compute_bounds(&self.window),
        ) else {
            return;
        };
        let card = cairo::RectangleInt::new(
            bounds.x().floor() as i32,
            bounds.y().floor() as i32,
            bounds.width().ceil() as i32,
            bounds.height().ceil() as i32,
        );
        surface.set_input_region(&cairo::Region::create_rectangle(&card));
    }

    /// A drop that landed closes the menu, as a pick would; a cancelled one
    /// gives the surface its input back.
    fn drag_ended(&self, path: &Path, dropped: bool) {
        if dropped {
            if let Some(file_name) = path.file_name().and_then(|name| name.to_str())
                && let Err(error) = self.library.borrow_mut().touch(file_name)
            {
                tracing::warn!(%error, "cannot record the use of the image");
            }
            self.dropped.set(true);
            self.finish();
            return;
        }
        let Some(surface) = self.window.surface() else {
            return;
        };
        let whole = cairo::RectangleInt::new(0, 0, surface.width(), surface.height());
        surface.set_input_region(&cairo::Region::create_rectangle(&whole));
    }

    // -- Finishing -----------------------------------------------------------

    fn pick(&self, position: u32, keep_open: bool) {
        if self.tab.get() == QuickTab::Gif {
            self.pick_gif(position, keep_open);
            return;
        }
        if self.tab.get() == QuickTab::Clipboard {
            self.pick_clip(position, keep_open);
            return;
        }
        let Some(item) = self.items.borrow().get(position as usize).cloned() else {
            return;
        };
        self.state
            .borrow_mut()
            .record(self.tab.get(), &item.text, self.config.recent_limit);
        self.queued.borrow_mut().push(item.text);
        if keep_open {
            self.update_queue();
        } else {
            self.finish();
        }
    }

    /// A GIF saved from a link goes in as that link, which chat applications
    /// embed animated; anything else is put on the clipboard as the image.
    fn pick_gif(&self, position: u32, keep_open: bool) {
        let Some(gif) = self.gifs.borrow().get(position as usize).cloned() else {
            return;
        };
        if let Err(error) = self.library.borrow_mut().touch(&gif.file_name) {
            tracing::warn!(%error, "cannot record the use of the image");
        }
        match (&gif.source, self.config.gif_prefer_link) {
            (Some(link), true) => {
                self.queued.borrow_mut().push(link.clone());
                if keep_open {
                    self.update_queue();
                } else {
                    self.finish();
                }
            }
            // An image cannot be queued: the clipboard holds one at a time.
            _ => match gifs::offer(&gif) {
                Ok(()) => {
                    *self.picked_image.borrow_mut() = Some(gif.path.clone());
                    self.finish();
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot put the image on the clipboard");
                    self.section
                        .set_label(&format!("Could not copy the image: {error}"));
                }
            },
        }
    }

    /// A copied text goes in like a character; a copied image goes back on
    /// the clipboard and is pasted, like a saved GIF.
    fn pick_clip(&self, position: u32, keep_open: bool) {
        let Some(clip) = self.clips.borrow().get(position as usize).cloned() else {
            return;
        };
        if let Err(error) = self.history.borrow_mut().touch(clip.id) {
            tracing::warn!(%error, "cannot record the use of the copy");
        }
        if let Some(text) = clip.text {
            self.queued.borrow_mut().push(text);
            if keep_open {
                self.update_queue();
            } else {
                self.finish();
            }
            return;
        }
        let Some(path) = self.history.borrow().image_path(&clip) else {
            return;
        };
        let offered = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                gifs::ImageKind::sniff(&bytes).ok_or_else(|| "not an image".to_string())
            })
            .and_then(|kind| gifs::offer_image(&path, kind));
        match offered {
            Ok(()) => {
                *self.picked_image.borrow_mut() = Some(path);
                self.finish();
            }
            Err(error) => {
                tracing::warn!(%error, "cannot put the image on the clipboard");
                self.section
                    .set_label(&format!("Could not copy the image: {error}"));
            }
        }
    }

    /// Closes the menu, handing over whatever was picked. Safe to call more
    /// than once; only the first call does anything.
    fn finish(&self) {
        let Some(on_finish) = self.on_finish.borrow_mut().take() else {
            return;
        };
        let picked = Picked {
            text: self.queued.borrow().concat(),
            image: self.picked_image.borrow().clone(),
        };
        if !picked.text.is_empty() {
            self.state.borrow().save();
        }
        self.window.set_visible(false);
        self.window.destroy();
        let anything = !picked.text.is_empty() || picked.image.is_some() || self.dropped.get();
        on_finish(anything.then_some(picked));
    }

    pub fn close(&self) {
        self.finish();
    }
}

fn text_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        // An inscription, not a label: it asks for no more width than the
        // column has, so a wide glyph such as ⟷ is clipped instead of
        // widening every column and the card with them.
        let label = gtk::Inscription::builder()
            .css_classes(["quick-cell"])
            .xalign(0.5)
            .yalign(0.5)
            .min_chars(1)
            .nat_chars(1)
            .build();
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(label), Some(text)) = (
            item.child().and_downcast::<gtk::Inscription>(),
            item.item().and_downcast::<gtk::StringObject>(),
        ) else {
            return;
        };
        label.set_text(Some(&text.string()));
    });
    factory
}

/// Cells that play their GIF. Each cell owns a [`Player`] for its lifetime;
/// binding points it at another image once that image is decoded. A cell can
/// be dragged into another window as its file, which chat applications upload
/// animated, where a pasted GIF arrives still.
fn gif_factory(
    thumbs: &Rc<Thumbs>,
    menu: &Rc<RefCell<Weak<QuickMenu>>>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let players: Rc<RefCell<HashMap<usize, Rc<Player>>>> = Rc::default();
    let key = |item: &gtk::ListItem| item.as_ptr() as usize;

    let setup_players = Rc::clone(&players);
    let menu = Rc::clone(menu);
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .width_request(GIF_CELL.0)
            .height_request(GIF_CELL.1)
            .css_classes(["quick-gif"])
            .build();
        item.set_child(Some(&picture));
        picture.add_controller(gif_drag_source(item, &menu));
        setup_players
            .borrow_mut()
            .insert(key(item), Player::new(picture));
    });

    let teardown_players = Rc::clone(&players);
    factory.connect_teardown(move |_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            teardown_players.borrow_mut().remove(&key(item));
        }
    });

    let thumbs = Rc::clone(thumbs);
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(path) = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|path| path.string())
        else {
            return;
        };
        let Some(player) = players.borrow().get(&key(item)).cloned() else {
            return;
        };
        player.show(None);
        // By the time it is decoded the cell may show something else.
        let item = item.downgrade();
        let expected = path.clone();
        thumbs.get(Path::new(path.as_str()), move |thumb| {
            let still_bound = item.upgrade().is_some_and(|item| {
                item.item()
                    .and_downcast::<gtk::StringObject>()
                    .is_some_and(|current| current.string() == expected)
            });
            if still_bound {
                player.show(thumb);
            }
        });
    });
    factory
}

/// A Clipboard row's model text: `t` or `i` for a text or an image, upper
/// case when pinned, then `:` and a preview of the text or the image's path.
fn clip_row(clip: &Clip, history: &History) -> String {
    let pinned = clip.pinned;
    match &clip.text {
        Some(text) => {
            let flat: String = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(CLIP_PREVIEW_CHARS)
                .collect();
            format!("{}:{flat}", if pinned { 'T' } else { 't' })
        }
        None => {
            let path = history.image_path(clip).unwrap_or_default();
            format!("{}:{}", if pinned { 'I' } else { 'i' }, path.display())
        }
    }
}

/// Rows of the Clipboard tab: a copied text, two lines of it, or a copied
/// image, playing if it moves; a star on the pinned ones.
fn clip_factory(thumbs: &Rc<Thumbs>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let players: Rc<RefCell<HashMap<usize, Rc<Player>>>> = Rc::default();
    let key = |item: &gtk::ListItem| item.as_ptr() as usize;

    let setup_players = Rc::clone(&players);
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::builder()
            .spacing(10)
            .css_classes(["quick-clip"])
            .build();
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .css_classes(["quick-clip-image"])
            .build();
        let frame = framed(&picture, GIF_CELL.0, 56);
        frame.set_halign(gtk::Align::Start);
        // Capped, like the section label, so a long line wraps and ellipsizes
        // within the card instead of widening it.
        let label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(20)
            .css_classes(["quick-clip-text"])
            .build();
        let pin = gtk::Image::builder()
            .icon_name("starred-symbolic")
            .valign(gtk::Align::Start)
            .css_classes(["quick-clip-pin"])
            .build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        row.append(&frame);
        row.append(&label);
        row.append(&spacer);
        row.append(&pin);
        item.set_child(Some(&row));
        setup_players
            .borrow_mut()
            .insert(key(item), Player::new(picture));
    });

    let teardown_players = Rc::clone(&players);
    factory.connect_teardown(move |_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            teardown_players.borrow_mut().remove(&key(item));
        }
    });

    let thumbs = Rc::clone(thumbs);
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(row), Some(encoded)) = (
            item.child().and_downcast::<gtk::Box>(),
            item.item().and_downcast::<gtk::StringObject>(),
        ) else {
            return;
        };
        let Some(player) = players.borrow().get(&key(item)).cloned() else {
            return;
        };
        let (Some(frame), Some(label), Some(pin)) = (
            row.first_child(),
            row.first_child()
                .and_then(|frame| frame.next_sibling())
                .and_downcast::<gtk::Label>(),
            row.last_child().and_downcast::<gtk::Image>(),
        ) else {
            return;
        };
        let encoded = encoded.string();
        let Some((kind, payload)) = encoded.split_once(':') else {
            return;
        };
        pin.set_visible(kind == "T" || kind == "I");
        player.show(None);
        let image = kind.eq_ignore_ascii_case("i");
        frame.set_visible(image);
        label.set_visible(!image);
        if !image {
            label.set_label(payload);
            return;
        }
        let item = item.downgrade();
        let expected = encoded.to_string();
        thumbs.get(Path::new(payload), move |thumb| {
            let still_bound = item.upgrade().is_some_and(|item| {
                item.item()
                    .and_downcast::<gtk::StringObject>()
                    .is_some_and(|current| current.string() == expected)
            });
            if still_bound {
                player.show(thumb);
            }
        });
    });
    factory
}

/// Drags the file of whatever GIF `item` shows.
fn gif_drag_source(item: &gtk::ListItem, menu: &Rc<RefCell<Weak<QuickMenu>>>) -> gtk::DragSource {
    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY)
        .build();
    // What the current drag carries, and whether it was cancelled; a drag
    // that ends without being cancelled was dropped.
    let dragging: Rc<RefCell<Option<PathBuf>>> = Rc::default();
    let cancelled = Rc::new(Cell::new(false));

    let item = item.downgrade();
    let prepare_dragging = Rc::clone(&dragging);
    let prepare_cancelled = Rc::clone(&cancelled);
    source.connect_prepare(move |_, _, _| {
        let path = item
            .upgrade()?
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|path| PathBuf::from(path.string().as_str()))?;
        let files = gdk::FileList::from_array(&[gio::File::for_path(&path)]);
        *prepare_dragging.borrow_mut() = Some(path);
        prepare_cancelled.set(false);
        Some(gdk::ContentProvider::for_value(&files.to_value()))
    });

    let begin_menu = Rc::clone(menu);
    source.connect_drag_begin(move |source, _| {
        if let Some(picture) = source.widget().and_downcast::<gtk::Picture>()
            && let Some(frame) = picture.paintable()
        {
            let (width, height) = (frame.intrinsic_width(), frame.intrinsic_height());
            source.set_icon(Some(&frame), width / 2, height / 2);
        }
        if let Some(menu) = begin_menu.borrow().upgrade() {
            menu.drag_started();
        }
    });

    let cancel_flag = Rc::clone(&cancelled);
    source.connect_drag_cancel(move |_, _, reason| {
        tracing::debug!(?reason, "the drag was cancelled");
        cancel_flag.set(true);
        false
    });

    let end_menu = Rc::clone(menu);
    source.connect_drag_end(move |_, _, _| {
        let Some(path) = dragging.borrow_mut().take() else {
            return;
        };
        // Deferred: the menu may close, and take this cell with it.
        let menu = end_menu.borrow().clone();
        let dropped = !cancelled.get();
        glib::idle_add_local_once(move || {
            if let Some(menu) = menu.upgrade() {
                menu.drag_ended(&path, dropped);
            }
        });
    });
    source
}

type Banner = (
    gtk::Box,
    Rc<Player>,
    gtk::Label,
    gtk::Label,
    gtk::Entry,
    gtk::Button,
    gtk::Button,
);

/// The offer to save an image found on the clipboard, above the GIF grid.
fn build_banner() -> Banner {
    let banner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .css_classes(["quick-banner"])
        .build();

    let top = gtk::Box::builder().spacing(10).build();
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .css_classes(["quick-banner-picture"])
        .build();
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    let title = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(16)
        .css_classes(["quick-banner-title"])
        .build();
    let detail = gtk::Label::builder()
        .xalign(0.0)
        .max_width_chars(16)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["quick-banner-detail", "dim-label"])
        .build();
    text.append(&title);
    text.append(&detail);
    let dismiss = flat_icon_button("window-close-symbolic", "Not now");
    dismiss.set_valign(gtk::Align::Start);
    top.append(&framed(&picture, 56, 56));
    top.append(&text);
    top.append(&dismiss);

    let bottom = gtk::Box::builder().spacing(6).build();
    let tags = gtk::Entry::builder()
        .placeholder_text("Tags (Ctrl+S)")
        .tooltip_text("Tags, separated by spaces or commas")
        .hexpand(true)
        .css_classes(["quick-tag-entry"])
        .build();
    let save = gtk::Button::builder()
        .label("Save")
        .tooltip_text("Save (Enter or Ctrl+S in the tags)")
        .focus_on_click(false)
        .css_classes(["quick-save", "suggested-action"])
        .build();
    bottom.append(&tags);
    bottom.append(&save);

    banner.append(&top);
    banner.append(&bottom);
    let player = Player::new(picture);
    (banner, player, title, detail, tags, save, dismiss)
}

/// `picture` in a box of exactly `width` by `height`. A picture asks for as
/// much width as its image's shape needs at the height it gets, so a wide
/// image outside a scroller would widen the card; the frame asks only for its
/// own size and fits the image inside.
fn framed(picture: &gtk::Picture, width: i32, height: i32) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(picture)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .width_request(width)
        .height_request(height)
        .can_focus(false)
        .css_classes(["quick-frame"])
        .build()
}

fn flat_icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .focus_on_click(false)
        .can_focus(false)
        .css_classes(["flat", "quick-icon-button"])
        .build()
}

/// The footer's line for a saved GIF: its tags or name, and where it goes in
/// as a link.
fn gif_caption(gif: &Gif) -> String {
    match gif.source.as_deref().and_then(host_of) {
        Some(host) => format!("{} · {host}", gif.label()),
        None => gif.label(),
    }
}

/// `media.tenor.com` from `https://media.tenor.com/a/b.gif`.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then_some(host)
}

fn supported(emoji: &data::Emoji, limit: Option<(u8, u8)>) -> bool {
    limit.is_none_or(|limit| emoji.version < limit)
}

fn shift_held() -> bool {
    gdk::Display::default()
        .and_then(|display| display.default_seat())
        .and_then(|seat| seat.keyboard())
        .is_some_and(|keyboard| {
            keyboard
                .modifier_state()
                .contains(gdk::ModifierType::SHIFT_MASK)
        })
}

/// Output areas, in global logical coordinates, in the order of `monitors`.
pub fn screens(monitors: &[gdk::Monitor]) -> Vec<Screen> {
    monitors
        .iter()
        .map(|monitor| {
            let geometry = monitor.geometry();
            Screen {
                x: geometry.x(),
                y: geometry.y(),
                width: geometry.width(),
                height: geometry.height(),
            }
        })
        .collect()
}

thread_local! {
    /// Whether each text draws with the installed fonts, rather than as a box
    /// with its code point in. Kept for the life of the process, which for a
    /// server is every menu it opens.
    static RENDERS: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    /// The oldest emoji version the fonts cannot draw; `None` inside when they
    /// draw them all, and `None` outside until it is worked out.
    static EMOJI_LIMIT: Cell<Option<Option<(u8, u8)>>> = const { Cell::new(None) };
    /// Lays out the texts [`text_renders`] judges.
    static MEASURE: gtk::Label = gtk::Label::new(None);
    static TRANSPARENT: Cell<bool> = const { Cell::new(false) };
}

/// Whether `text` draws with the installed fonts. Symbols come from all of
/// Unicode, and a grid of boxes with code points in them helps nobody.
fn text_renders(text: &str) -> bool {
    if let Some(known) = RENDERS.with_borrow(|renders| renders.get(text).copied()) {
        return known;
    }
    let renders =
        MEASURE.with(|label| label.create_pango_layout(Some(text)).unknown_glyphs_count() == 0);
    RENDERS.with_borrow_mut(|known| known.insert(text.to_string(), renders));
    renders
}

/// The oldest emoji version the fonts cannot draw. Judged by the versions'
/// lone code points, which a font either has or draws as a box; sequences
/// cannot be judged that way, but arrive together with those.
fn fonts_emoji_limit() -> Option<(u8, u8)> {
    if let Some(limit) = EMOJI_LIMIT.get() {
        return limit;
    }
    let limit = data::emoji()
        .versions()
        .into_iter()
        .filter(|(version, _)| *version > TRUSTED_EMOJI_VERSION)
        .find(|(_, members)| members.iter().any(|emoji| !text_renders(emoji.glyph)))
        .map(|(version, _)| version);
    if let Some(version) = limit {
        tracing::debug!(?version, "the installed fonts stop at this emoji version");
    }
    EMOJI_LIMIT.set(Some(limit));
    limit
}

/// Does ahead of time what the first menu would otherwise do as it opens:
/// reads the character tables, checks which the fonts can draw, and draws
/// once, which starts the renderer and loads the fonts. For a server, before
/// anyone is waiting.
pub fn warm_up() {
    data::unicode();
    fonts_emoji_limit();
    for category in SYMBOL_CATEGORIES {
        for item in category.items() {
            text_renders(&item.text);
        }
    }
    if !gtk4_layer_shell::is_supported() {
        return;
    }

    // A pixel in a corner, below every window, that takes no keys: nothing
    // anyone can see or type into, gone after its first frame.
    install_transparency();
    let sample = gtk::Label::builder()
        .label("→ ★ € ∑ 😀 👍🏽")
        .css_classes(["quick-cell"])
        .build();
    let window = gtk::Window::builder()
        .css_classes(["quick-window"])
        .decorated(false)
        .default_width(1)
        .default_height(1)
        .child(&sample)
        .build();
    window.init_layer_shell();
    window.set_namespace(Some("ioexplorer-quick-warm-up"));
    window.set_layer(Layer::Background);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_opacity(0.0);
    window.connect_map(|window| {
        let window = window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            window.destroy();
        });
    });
    window.present();
}

/// The surface covers the whole output, so it must stay see-through whatever
/// a theme does to `window`. Like the desktop surface it keeps the faintest
/// tint, so a click beside the card reaches it and closes the menu. Once per
/// process: a server opens many menus.
fn install_transparency() {
    if TRANSPARENT.replace(true) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".quick-window {\n\
           background-color: rgba(0, 0, 0, 0.004);\n\
           background-image: none;\n\
           box-shadow: none;\n\
         }\n\
         .quick-window > box {\n\
           background: none;\n\
         }\n",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 2,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_read_from_links() {
        assert_eq!(
            host_of("https://media.tenor.com/a/b.gif"),
            Some("media.tenor.com")
        );
        assert_eq!(host_of("http://x.test?y"), Some("x.test"));
        assert_eq!(host_of("not a link"), None);
    }
}
