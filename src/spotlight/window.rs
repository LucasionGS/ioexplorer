//! The spotlight overlay window.

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

use gtk::prelude::*;

use crate::{
    config::SpotlightConfig,
    launcher::{
        app_index::{IconRef, LiveAppIndex},
        frecency::{self, Frecency},
        icons, spawn,
    },
    spotlight::{
        ToggleRequest,
        ai::{
            AiError, AiEvent, AiSession, Block, ChatMessage, Provider, Role, markdown,
            tools::{self, AppSummary, ToolCall, ToolDef, ToolKind, ToolOutcome, ToolRunner},
        },
        custom_results::{CustomResult, CustomResultsRunner, ResultsEvent},
        file_search::{FileHit, FileSearch, SearchEvent},
        image_cache,
        keys::{self, Action},
        layout,
        prefixes::{FIRST_PAGE, Prefix, PrefixKind, PrefixTable, build_results_line},
        preview::{self, Preview, PreviewEvent, PreviewKind, PreviewLoader},
        query::{self, Query},
        results::{self, Activation, SpotlightResult},
        runtime::SpotlightRuntime,
        ssh::{self, SshHost},
        vpn::{self, VpnSource, VpnState},
        windows::{self, OpenWindow, WindowSource},
    },
};

/// A live conversation. Plain data — no GObjects — so borrows stay short.
struct ChatState {
    provider: Provider,
    label: String,
    history: Vec<ChatMessage>,
    streaming: bool,
    /// Resolved once when the conversation opens. The tool list renders at the
    /// very front of the prompt, so rebuilding it per turn would invalidate the
    /// prompt cache on every round.
    tools: Vec<ToolDef>,
    web_search: bool,
    /// Resolved with the tools and for the same reason: it renders at the front
    /// of the prompt, so it must not change between rounds.
    system: Option<String>,
    /// Tool rounds used so far, against `max_rounds`.
    rounds: usize,
    /// This provider's round budget, from `max_tool_rounds`.
    max_rounds: usize,
    /// How long a single command may run before it is stopped.
    command_timeout: Duration,
    /// Server-side `pause_turn` resumes used, against [`MAX_PAUSE_RESUMES`].
    resumes: usize,
    /// Self-issued "carry on" prompts used, against [`MAX_CONTINUATIONS`].
    continuations: usize,
    /// The call waiting on the approval card.
    pending: Option<PendingApproval>,
    /// Calls from this round that have not run yet.
    ///
    /// A queue rather than a single slot because the API may send several
    /// `tool_use` blocks in one turn, and *every* one needs a `tool_result` in
    /// the single user turn that follows — a missing pairing is rejected. The
    /// request asks for one at a time, so this is normally length 1; it exists
    /// so that guarantee is not the only thing keeping the loop correct.
    queue: std::collections::VecDeque<ToolCall>,
    /// `tool_result` blocks for calls already answered this round.
    results: Vec<Block>,
}

/// A side-effecting call the user has not yet decided on.
struct PendingApproval {
    call: ToolCall,
    kind: ToolKind,
    /// What will actually run — the expanded command, not the template.
    expanded: String,
}

/// One tick drains the toggle socket, the filesystem walker and the AI stream.
const TICK_INTERVAL: Duration = Duration::from_millis(24);
/// Height at which the result list starts scrolling instead of growing.
const MAX_LIST_HEIGHT: i32 = 420;
/// Height at which the transcript starts scrolling instead of growing.
const MAX_CHAT_HEIGHT: i32 = 520;
const ROW_ICON_SIZE: i32 = 28;
/// Width of the preview panel beside the card.
const PREVIEW_WIDTH: i32 = 380;
/// Gap between the preview panel and the card.
const PREVIEW_GAP: i32 = 14;
/// Largest square a preview image is drawn into. `GtkImage` scales the picture
/// to fit while keeping its aspect ratio, so one number bounds both dimensions
/// — which also stops a large photograph from dictating the panel's size.
const PREVIEW_IMAGE_SIZE: i32 = 340;
/// Height at which preview text starts scrolling instead of growing.
const MAX_PREVIEW_HEIGHT: i32 = 460;
/// Height at which a preview caption starts scrolling instead of growing, so a
/// window with an absurd title cannot push the panel off the bottom of the
/// screen. A normal caption is a few lines and never reaches this.
const MAX_CAPTION_HEIGHT: i32 = 120;
/// Quiet time before a preview image is loaded. Holding an arrow key would
/// otherwise decode — and for a remote image, download — every row passed over.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(120);
/// How long a window list stays fresh. Long enough that typing filters one
/// snapshot rather than spawning a compositor query per keystroke, short enough
/// that a window opened a moment ago is still found.
const WINDOW_LIST_TTL: Duration = Duration::from_millis(1_500);
/// How long the VPN's state stays fresh. Longer than the window list: a
/// connection changes on the scale of seconds, and the query is two subprocesses
/// rather than one socket round-trip.
const VPN_STATE_TTL: Duration = Duration::from_secs(5);
/// Cap on `pause_turn` resumes. The server-side tool loop hit its own iteration
/// limit and wants to continue; this bounds how often we let it.
const MAX_PAUSE_RESUMES: usize = 5;
/// Cap on self-issued continuations after a reply hit the token limit.
///
/// Low on purpose: a truncated answer is worth finishing, but a model that keeps
/// filling its whole budget is not converging on anything, and each continuation
/// resends the entire conversation.
const MAX_CONTINUATIONS: usize = 3;
/// Fallback command timeout, for the impossible case of a tool running with no
/// conversation behind it.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// What the model is told to make it finish a reply that hit the token limit.
const CONTINUE_PROMPT: &str = "Your reply was cut off at the token limit. Carry on from exactly where you \
     stopped, without repeating or summarising what you already wrote.";
/// Rows reachable via Alt+1..9.
const QUICK_PICK_ROWS: usize = 9;
/// Shown at the end of the streaming message so it reads as still going.
const STREAM_CARET: char = '▍';
/// Header, status line, footer and the card's own padding — everything above
/// and below the scrolling area.
const CARD_CHROME_HEIGHT: i32 = 200;
/// The tallest the card can ever get. Used to place it, so the offset never
/// depends on how tall the card happens to be right now.
const MAX_CARD_HEIGHT: i32 = MAX_CHAT_HEIGHT + CARD_CHROME_HEIGHT;

const FOOTER_SEARCH_KEYS: &[(&str, &str)] = &[
    ("↑↓", "Navigate"),
    ("Enter", "Open"),
    ("Tab", "Complete"),
    ("?", "Prefixes"),
    ("Esc", "Close"),
];

const FOOTER_CHAT_KEYS: &[(&str, &str)] = &[
    ("Enter", "Send"),
    ("Ctrl+C", "Stop"),
    ("Ctrl+Y", "Copy"),
    ("Esc", "Back"),
];

pub struct SpotlightWindow {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    card: gtk::Box,
    /// Carries the top margin and holds the card between the preview panel and
    /// its mirroring spacer.
    stage: gtk::Box,
    preview_panel: gtk::Box,
    preview_spacer: gtk::Box,
    preview_stack: gtk::Stack,
    preview_image: gtk::Image,
    /// Details written under the artwork on the image page.
    preview_caption: gtk::Label,
    /// The caption's clamping wrapper — this is what gets shown or hidden, since
    /// hiding the label alone would leave its container occupying space.
    preview_caption_view: gtk::ScrolledWindow,
    preview_label: gtk::Label,
    preview_status: gtk::Label,
    entry: gtk::Entry,
    prefix_badge: gtk::Label,
    hint_label: gtk::Label,
    spinner: gtk::Spinner,
    body: gtk::Stack,
    footers: gtk::Stack,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    chat_scroller: gtk::ScrolledWindow,
    transcript: gtk::Box,
    status_label: gtk::Label,
    empty_label: gtk::Label,
    /// The config section and everything derived from it. Held behind a
    /// `RefCell` so an external edit to `config.toml` can swap it, and read via
    /// [`SpotlightWindow::runtime`], which hands out an `Rc` snapshot rather
    /// than a borrow guard — nothing here may be borrowed across a callback.
    runtime: RefCell<Rc<SpotlightRuntime>>,
    ai_session: AiSession,
    /// Runs read-only tools off the main thread.
    tool_runner: ToolRunner,
    /// The approval card currently in the transcript, so it can be removed once
    /// a decision is made. Kept here rather than in `ChatState`, which is plain
    /// data by design.
    approval_card: RefCell<Option<gtk::Widget>>,
    /// The live conversation, kept in memory so hide/show resumes it.
    chat: RefCell<Option<ChatState>>,
    /// The label currently receiving deltas; `None` when not streaming.
    streaming_label: RefCell<Option<gtk::Label>>,
    /// The label a just-completed reply landed in, awaiting Markdown rendering.
    finished_label: RefCell<Option<gtk::Label>>,
    /// Deltas are accumulated here and flushed once per tick.
    streaming_text: RefCell<String>,
    mode: Cell<keys::Mode>,
    /// Whether the transcript should stay pinned to the bottom as it grows.
    /// Cleared when the user scrolls away, restored when they scroll back.
    chat_follow: Cell<bool>,
    /// Last margin `apply_top_offset` returned, to skip redundant re-layout.
    applied_top_margin: Cell<i32>,
    /// Last surface height seen, so a layout pass we caused ourselves does not
    /// trigger another one.
    last_window_height: Cell<i32>,
    /// Re-entrancy guard: `reflow` sets a margin, which can synchronously run
    /// another layout pass. Without this the two can ping-pong.
    reflowing: Cell<bool>,
    live_index: Rc<LiveAppIndex>,
    frecency: RefCell<Frecency>,
    file_search: FileSearch,
    file_hits: RefCell<Vec<FileHit>>,
    /// Set when the walker hit one of its bounds, so the UI can say so.
    file_search_truncated: Cell<bool>,
    /// Background source of the open-window list.
    window_source: WindowSource,
    /// The last list the compositor reported. Kept between keystrokes so typing
    /// filters a snapshot instead of re-querying the compositor per character.
    open_windows: RefCell<Vec<OpenWindow>>,
    /// When the current list was *asked for*, not when it arrived — so a slow
    /// reply cannot cause a second request to pile up behind the first.
    windows_requested: Cell<Option<Instant>>,
    /// Whether a reply has ever landed, which is what separates "still asking"
    /// from "genuinely nothing open".
    windows_loaded: Cell<bool>,
    /// Why the last compositor query failed, so the prefix can say so instead of
    /// looking like it is still waiting.
    windows_error: RefCell<Option<String>>,
    /// The hosts read from the user's SSH config, held for as long as the window
    /// stays open so typing filters a snapshot instead of re-reading the file.
    ssh_hosts: RefCell<Vec<SshHost>>,
    /// Whether the config has been read since the window was last shown.
    ssh_loaded: Cell<bool>,
    /// Why reading it failed, so the prefix can say so rather than looking like
    /// an empty config.
    ssh_error: RefCell<Option<String>>,
    /// Background source of the VPN's state.
    vpn_source: VpnSource,
    /// What the VPN client last reported. Kept between keystrokes so typing
    /// filters a snapshot of the location list rather than re-running the client.
    vpn_state: RefCell<Option<VpnState>>,
    /// When that state was *asked for*, so a slow client cannot have a second
    /// request pile up behind the first.
    vpn_requested: Cell<Option<Instant>>,
    /// Why the last query failed, so the prefix can say so instead of looking
    /// like it is still asking.
    vpn_error: RefCell<Option<String>>,
    custom_results: CustomResultsRunner,
    custom_rows: RefCell<Vec<CustomResult>>,
    custom_error: RefCell<Option<String>>,
    /// The command line the current rows belong to. Rebuilds fire for reasons
    /// other than typing — an app-index change, a redraw after a drain — and
    /// restarting on those would reset the debounce forever.
    custom_line: RefCell<Option<String>>,
    custom_pending: Cell<bool>,
    /// The page a paginated prefix is showing.
    custom_page: Cell<i64>,
    /// Prefix and query the current page belongs to. A different one means the
    /// user is asking a new question, which starts again at page one.
    custom_page_key: RefCell<Option<String>>,
    /// Set when the *user* changed page, to skip the typing debounce once —
    /// a deliberate keypress should not wait as though it were a keystroke.
    custom_immediate: Cell<bool>,
    preview_loader: PreviewLoader,
    /// The preview currently on screen, so an unchanged one is not rebuilt —
    /// which would restart its crossfade on every keystroke.
    preview_shown: RefCell<Option<Preview>>,
    /// Bumped on every preview change; the debounce timeout drops itself when
    /// it no longer matches, which is cheaper than tracking `SourceId`s.
    preview_generation: Cell<u64>,
    /// The row under the pointer, which takes precedence over the selected one.
    hovered: Cell<Option<usize>>,
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
        runtime: Rc<SpotlightRuntime>,
        server_mode: bool,
    ) -> Rc<Self> {
        let config = &runtime.config;
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

        let spinner = gtk::Spinner::builder()
            .visible(false)
            .css_classes(["spotlight-chat-spinner"])
            .build();
        header.append(&spinner);

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

        let transcript = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .css_classes(["spotlight-chat-transcript"])
            .build();
        let chat_scroller = gtk::ScrolledWindow::builder()
            .child(&transcript)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(MAX_CHAT_HEIGHT)
            .propagate_natural_height(true)
            .css_classes(["spotlight-chat-scroll"])
            .build();

        // Crossfade, never slide: a slide animates the stack's allocation and
        // fights `apply_top_offset`, which shows up as the card jumping.
        let body = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .css_classes(["spotlight-body"])
            .build();
        body.add_named(&scroller, Some("results"));
        body.add_named(&chat_scroller, Some("chat"));
        body.set_visible_child_name("results");
        card.append(&body);

        let empty_label = gtk::Label::builder()
            .label("No results")
            .xalign(0.0)
            .visible(false)
            .css_classes(["spotlight-empty", "dim-label"])
            .build();
        card.append(&empty_label);

        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .visible(false)
            .wrap(true)
            .css_classes(["spotlight-chat-status", "dim-label"])
            .build();
        card.append(&status_label);

        let footers = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .build();
        footers.add_named(&footer(FOOTER_SEARCH_KEYS), Some("results"));
        footers.add_named(&footer(FOOTER_CHAT_KEYS), Some("chat"));
        footers.set_visible_child_name("results");
        card.append(&footers);

        let preview_image = gtk::Image::builder()
            .pixel_size(PREVIEW_IMAGE_SIZE)
            .css_classes(["spotlight-preview-image"])
            .build();

        let preview_label = gtk::Label::builder()
            .xalign(0.0)
            .yalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            // Never selectable: a selectable label takes focus, and the entry
            // losing focus mid-search is the bug that ate follow-up keystrokes.
            .selectable(false)
            .css_classes(["spotlight-preview-text"])
            .build();
        let preview_text_scroller = gtk::ScrolledWindow::builder()
            .child(&preview_label)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(MAX_PREVIEW_HEIGHT)
            .propagate_natural_height(true)
            .build();

        let preview_status = gtk::Label::builder()
            .wrap(true)
            .css_classes(["spotlight-preview-status", "dim-label"])
            .build();

        let preview_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(90)
            .build();
        let preview_caption = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(false)
            .css_classes(["spotlight-preview-caption", "dim-label"])
            .build();
        // Load-bearing, not decoration. A wrapping `GtkLabel` reports its
        // *unwrapped* single-line width as its natural width, and the panel's
        // `width_request` is only a minimum — so a long window title widened the
        // whole panel to 619px instead of wrapping inside it. A `GtkScrolledWindow`
        // does not propagate its child's natural width, which pins the panel at
        // `PREVIEW_WIDTH` and makes the text wrap downwards. Measured: without
        // this, a Chrome title gives 619px and an unbreakable URL 705px.
        //
        // `max_width_chars` also works but is a character count against whatever
        // font is in use, so a wider font would leak past the edge again.
        let preview_caption_view = gtk::ScrolledWindow::builder()
            .child(&preview_caption)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(MAX_CAPTION_HEIGHT)
            .propagate_natural_height(true)
            .visible(false)
            .build();

        // The artwork and its details share one page so they crossfade together;
        // as two pages they would fade against each other.
        let preview_figure = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        preview_figure.append(&preview_image);
        preview_figure.append(&preview_caption_view);

        preview_stack.add_named(&preview_figure, Some("image"));
        preview_stack.add_named(&preview_text_scroller, Some("text"));
        preview_stack.add_named(&preview_status, Some("status"));
        preview_stack.set_visible_child_name("status");

        let preview_panel = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Start)
            .width_request(PREVIEW_WIDTH)
            .visible(false)
            .css_classes(["spotlight-preview"])
            .build();
        preview_panel.append(&preview_stack);

        // Mirrors the panel's width on the other side so the card stays exactly
        // centred. Without it the whole row re-centres as previews come and go,
        // and the search entry slides sideways under the user's cursor.
        let preview_spacer = gtk::Box::builder()
            .width_request(PREVIEW_WIDTH)
            .visible(false)
            .build();

        let stage = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(PREVIEW_GAP)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .build();
        stage.append(&preview_panel);
        stage.append(&card);
        stage.append(&preview_spacer);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        root.append(&stage);

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
            stage,
            preview_panel,
            preview_spacer,
            preview_stack,
            preview_image,
            preview_caption,
            preview_caption_view,
            preview_label,
            preview_status,
            entry,
            prefix_badge,
            hint_label,
            spinner,
            body,
            footers,
            list,
            scroller,
            chat_scroller,
            transcript,
            status_label,
            empty_label,
            runtime: RefCell::new(runtime),
            ai_session: AiSession::new(),
            tool_runner: ToolRunner::new(),
            approval_card: RefCell::new(None),
            chat: RefCell::new(None),
            streaming_label: RefCell::new(None),
            finished_label: RefCell::new(None),
            streaming_text: RefCell::new(String::new()),
            mode: Cell::new(keys::Mode::Search),
            chat_follow: Cell::new(true),
            applied_top_margin: Cell::new(0),
            last_window_height: Cell::new(0),
            reflowing: Cell::new(false),
            live_index: LiveAppIndex::new(),
            frecency: RefCell::new(Frecency::load()),
            file_search: FileSearch::new(),
            file_hits: RefCell::new(Vec::new()),
            file_search_truncated: Cell::new(false),
            window_source: WindowSource::new(windows::detect()),
            open_windows: RefCell::new(Vec::new()),
            windows_requested: Cell::new(None),
            windows_loaded: Cell::new(false),
            windows_error: RefCell::new(None),
            ssh_hosts: RefCell::new(Vec::new()),
            ssh_loaded: Cell::new(false),
            ssh_error: RefCell::new(None),
            vpn_source: VpnSource::new(),
            vpn_state: RefCell::new(None),
            vpn_requested: Cell::new(None),
            vpn_error: RefCell::new(None),
            custom_results: CustomResultsRunner::new(),
            custom_rows: RefCell::new(Vec::new()),
            custom_error: RefCell::new(None),
            custom_line: RefCell::new(None),
            custom_pending: Cell::new(false),
            custom_page: Cell::new(FIRST_PAGE),
            custom_page_key: RefCell::new(None),
            custom_immediate: Cell::new(false),
            preview_loader: PreviewLoader::new(),
            preview_shown: RefCell::new(None),
            preview_generation: Cell::new(0),
            hovered: Cell::new(None),
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
            let preview_panel = self.preview_panel.clone();
            gesture.connect_pressed(move |_, _, x, y| {
                let point = gtk::graphene::Point::new(x as f32, y as f32);
                // The card and the panel, never the stage: the stage also spans
                // the invisible spacer balancing the panel, and a click on what
                // looks like empty backdrop should still dismiss.
                let inside = [&card, &preview_panel].into_iter().any(|widget| {
                    widget.is_visible()
                        && widget
                            .compute_bounds(&this.window)
                            .is_some_and(|bounds| bounds.contains_point(&point))
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

        // Sticky-bottom scrolling for the transcript.
        //
        // `changed` fires when the content size updates — that is, once the
        // streaming label has actually re-laid-out — which is the only moment
        // the new bottom is known. Scrolling from the flush itself, or from an
        // idle callback, races that layout and lands short.
        let adjustment = self.chat_scroller.vadjustment();

        let this = Rc::clone(self);
        adjustment.connect_changed(move |adjustment| {
            if this.chat_follow.get() {
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            }
        });

        // `value_changed` fires only when someone actually scrolls, so growth
        // alone never disturbs the flag — following stops when the user scrolls
        // up and resumes when they come back to the bottom.
        let this = Rc::clone(self);
        adjustment.connect_value_changed(move |adjustment| {
            this.chat_follow.set(is_at_bottom(adjustment));
        });

        let this = Rc::clone(self);
        self.window.connect_map(move |_| this.reflow());

        let this = Rc::clone(self);
        self.window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                return;
            };
            let this = Rc::clone(&this);
            // Follows a monitor or resolution change — but *only* on a real
            // size change. Reflowing on every layout pass is a feedback loop:
            // `reflow` sets a margin, which requests another layout, which
            // measures a card whose height has meanwhile changed, and GTK
            // eventually gives up with "layout continuously requested".
            //
            // The surface is anchored to all four edges, so its height changes
            // only when the output does — never because of our own margin.
            surface.connect_layout(move |_, _, height| {
                if this.last_window_height.replace(height) != height {
                    this.reflow();
                }
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

    /// The current config and the tables built from it.
    ///
    /// Returns a snapshot rather than a borrow guard, so callers are free to
    /// hold it across anything that might reload the config underneath them.
    fn runtime(&self) -> Rc<SpotlightRuntime> {
        Rc::clone(&self.runtime.borrow())
    }

    /// Re-applies the top offset, skipping the work when nothing moved.
    ///
    /// Setting a margin requests a layout, and a layout can call back into
    /// here — the guard stops that becoming a loop.
    fn reflow(&self) {
        if self.reflowing.replace(true) {
            return;
        }
        let margin = layout::apply_top_offset(
            &self.window,
            &self.stage,
            self.runtime().config.clamped_top_ratio(),
            MAX_CARD_HEIGHT,
        );
        self.applied_top_margin.set(margin);
        self.reflowing.set(false);
    }

    /// Installs the single tick that drains the toggle socket, the walker and
    /// the AI stream.
    pub fn install_tick(self: &Rc<Self>, receiver: Option<mpsc::Receiver<ToggleRequest>>) {
        let this = Rc::clone(self);
        glib::timeout_add_local(TICK_INTERVAL, move || {
            if let Some(receiver) = &receiver {
                while receiver.try_recv().is_ok() {
                    this.toggle();
                }
            }
            this.drain_file_search();
            this.drain_windows();
            this.drain_vpn();
            this.drain_custom_results();
            this.drain_previews();
            this.drain_ai();
            this.drain_tools();
            glib::ControlFlow::Continue
        });
    }

    fn on_key(
        self: &Rc<Self>,
        key: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        match keys::resolve(key, state, self.mode.get()) {
            Action::Close => {
                self.close();
                glib::Propagation::Stop
            }
            Action::Back => {
                self.leave_chat();
                glib::Propagation::Stop
            }
            Action::Send => {
                self.send_follow_up();
                glib::Propagation::Stop
            }
            Action::Cancel => {
                self.cancel_stream();
                glib::Propagation::Stop
            }
            Action::CopyLast => {
                self.copy_last_reply();
                glib::Propagation::Stop
            }
            Action::Approve => {
                self.resolve_approval(true);
                glib::Propagation::Stop
            }
            Action::Deny => {
                self.resolve_approval(false);
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
            // Consumed either way. Letting Alt+Left through when nothing is
            // paginated would move the entry's cursor instead, which reads as
            // the shortcut misfiring rather than as it not applying.
            Action::Page(delta) => {
                self.change_page(delta);
                glib::Propagation::Stop
            }
            Action::Pass => {
                // A selectable transcript label is focusable, so clicking one
                // moves focus off the entry and the next keystroke goes nowhere.
                //
                // This runs in the capture phase, before the entry sees the key,
                // and plain `grab_focus` on a GtkEntry *selects all its text* —
                // which would make every keystroke replace the whole reply
                // instead of appending to it.
                if self.mode.get() == keys::Mode::Chat && !self.entry.has_focus() {
                    self.entry.grab_focus_without_selecting();
                }
                glib::Propagation::Proceed
            }
        }
    }

    // -- results -----------------------------------------------------------

    fn rebuild(self: &Rc<Self>) {
        // Load-bearing: `entry.set_text` fires `connect_changed`, and that
        // happens on every send, on enter/leave chat, and on show/hide. Without
        // this guard each follow-up keystroke re-renders a hidden results list.
        if self.mode.get() != keys::Mode::Search {
            return;
        }

        let rt = self.runtime();
        let raw = self.entry.text().to_string();
        let parsed = query::parse(&raw, &rt.prefixes);
        let limit = rt.config.clamped_result_limit();

        let mut active_prefix: Option<&Prefix> = None;
        let mut results = match &parsed.query {
            Query::Empty => {
                self.cancel_async_results();
                results::default_results(
                    "",
                    &self.live_index.snapshot(),
                    &[],
                    &self.frecency.borrow(),
                    frecency::now_secs(),
                    limit,
                )
            }
            Query::Plain(text) => {
                self.cancel_async_results();
                self.ensure_windows();
                let open = self.open_windows.borrow();
                let searchable: &[OpenWindow] = match rt.config.windows.in_search {
                    true => &open,
                    false => &[],
                };
                results::default_results(
                    text,
                    &self.live_index.snapshot(),
                    searchable,
                    &self.frecency.borrow(),
                    frecency::now_secs(),
                    limit,
                )
            }
            Query::Prefixed { key, arg } => {
                let Some(prefix) = rt.prefixes.get(key) else {
                    return;
                };
                active_prefix = Some(prefix);

                match &prefix.kind {
                    PrefixKind::FileSearch => {
                        self.cancel_custom_results();
                        self.start_file_search(arg);
                        self.file_results(limit)
                    }
                    PrefixKind::Windows => {
                        self.cancel_async_results();
                        self.ensure_windows();
                        self.window_rows(prefix, arg, limit)
                    }
                    PrefixKind::Ssh => {
                        self.cancel_async_results();
                        self.ensure_ssh_hosts();
                        self.ssh_rows(prefix, arg, limit)
                    }
                    PrefixKind::Software => {
                        self.cancel_async_results();
                        self.software_rows(&rt, &prefix.key, arg, limit)
                    }
                    PrefixKind::Vpn(provider) => {
                        self.cancel_async_results();
                        self.ensure_vpn(*provider);
                        self.vpn_rows(prefix, arg, *provider, limit)
                    }
                    PrefixKind::CustomResults {
                        command,
                        action,
                        delay,
                        terminal,
                        icon_size,
                        paginated,
                    } => {
                        self.file_search.cancel();
                        self.sync_page(key, arg, *paginated);
                        self.start_custom_results(command, *delay, arg);
                        self.custom_results(
                            prefix,
                            arg,
                            action.as_deref(),
                            *terminal,
                            *icon_size,
                            limit,
                        )
                    }
                    _ => {
                        self.cancel_async_results();
                        results::prefixed_results(prefix, arg, &rt.prefixes, limit)
                    }
                }
            }
        };

        // A plain query also offers what it could install, below everything that
        // is already here.
        if let Query::Plain(text) = &parsed.query
            && rt.config.software.in_search
            && let Some(key) = software_prefix_key(&rt.prefixes)
        {
            results.extend(results::software_search_results(
                key,
                text,
                &rt.software,
                &self.live_index.snapshot(),
                rt.config.software.keep_open,
                limit,
            ));
            results.truncate(limit);
        }

        if let Some(key) = &parsed.hint
            && let Some(prefix) = rt.prefixes.get(key)
        {
            results.insert(0, results::hint_result(prefix));
            results.truncate(limit);
        }

        // A plain query with a `default = true` provider also offers to ask it.
        if let Query::Plain(text) = &parsed.query
            && !text.trim().is_empty()
            && let Some((index, provider)) = rt
                .ai_providers
                .iter()
                .enumerate()
                .find(|(_, provider)| provider.default)
        {
            results.push(results::default_ai_result(
                index,
                &provider.label,
                &provider.icon,
                text,
            ));
        }

        self.update_header(active_prefix, parsed.hint.as_deref());
        self.render(results);
    }

    fn update_header(&self, prefix: Option<&Prefix>, hint: Option<&str>) {
        match prefix {
            Some(prefix) => {
                // The page rides on the badge rather than getting a widget of
                // its own: it is only ever meaningful next to the prefix it
                // belongs to, and the header has no room to spare.
                let page = self.custom_page.get();
                match self.custom_page_key.borrow().is_some() && page > FIRST_PAGE {
                    true => self
                        .prefix_badge
                        .set_text(&format!("{} · page {page}", prefix.label)),
                    false => self.prefix_badge.set_text(&prefix.label),
                }
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
        // The rows about to be built are different ones, so an index recorded
        // against the old list means nothing now.
        self.hovered.set(None);

        for (index, result) in results.iter().enumerate() {
            let row = row_for(result, index);
            if result.preview.is_some() {
                self.watch_hover(&row, index);
            }
            self.list.append(&row);
        }

        self.empty_label.set_visible(results.is_empty());
        if self.mode.get() == keys::Mode::Search {
            self.body.set_visible_child_name("results");
        }
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

        self.refresh_preview();
    }

    /// Makes a row show its preview while the pointer is over it.
    fn watch_hover(self: &Rc<Self>, row: &gtk::ListBoxRow, index: usize) {
        let motion = gtk::EventControllerMotion::new();

        let this = Rc::clone(self);
        motion.connect_enter(move |_, _, _| {
            this.hovered.set(Some(index));
            this.refresh_preview();
        });

        let this = Rc::clone(self);
        motion.connect_leave(move |_| {
            // Guarded: leaving a row the pointer has already left elsewhere
            // would otherwise clear whichever row it has just entered.
            if this.hovered.get() == Some(index) {
                this.hovered.set(None);
                this.refresh_preview();
            }
        });

        row.add_controller(motion);
    }

    // -- preview panel -----------------------------------------------------

    /// Points the panel at whichever row the user is currently indicating.
    ///
    /// The pointer wins over the keyboard: hovering is a deliberate act and is
    /// always the more recent one, and the selection is still visible in the
    /// list, so nothing is lost by showing what is under the cursor.
    fn refresh_preview(self: &Rc<Self>) {
        if self.mode.get() != keys::Mode::Search {
            self.show_preview(None);
            return;
        }

        let index = self.hovered.get().unwrap_or_else(|| self.selected.get());
        let preview = self
            .results
            .borrow()
            .get(index)
            .and_then(|result| result.preview.clone());
        self.show_preview(preview);
    }

    fn show_preview(self: &Rc<Self>, preview: Option<Preview>) {
        if *self.preview_shown.borrow() == preview {
            return;
        }
        *self.preview_shown.borrow_mut() = preview.clone();

        // Supersedes any pending debounce and any in-flight download.
        let generation = self.preview_generation.get().wrapping_add(1);
        self.preview_generation.set(generation);
        self.preview_loader.cancel();

        let Some(preview) = preview.filter(|_| self.preview_fits()) else {
            self.preview_panel.set_visible(false);
            self.preview_spacer.set_visible(false);
            return;
        };

        self.preview_panel.set_visible(true);
        self.preview_spacer.set_visible(true);

        self.set_preview_caption(preview.caption.as_deref());

        match preview.kind {
            PreviewKind::Text => {
                self.preview_label.set_text(&preview.content);
                self.preview_stack.set_visible_child_name("text");
            }
            // An icon resolves through the icon theme, so there is nothing to
            // download and nothing to wait for — debouncing it would only make
            // the panel arrive late.
            PreviewKind::Icon => {
                self.set_preview_icon(&IconRef::from_icon_name(preview.content.clone()));
            }
            // A file already on disk costs a decode, not a round trip, so it
            // still waits out the debounce — holding an arrow key past twenty
            // photographs should not decode twenty photographs.
            PreviewKind::Image => {
                self.preview_status.set_text("Loading…");
                self.preview_stack.set_visible_child_name("status");

                let this = Rc::clone(self);
                glib::timeout_add_local_once(PREVIEW_DEBOUNCE, move || {
                    if this.preview_generation.get() == generation {
                        this.load_preview_image(&preview.content);
                    }
                });
            }
        }
    }

    /// Shows an image preview, downloading it first when it is remote.
    fn load_preview_image(self: &Rc<Self>, content: &str) {
        if !image_cache::is_remote(content) {
            let path = match content.starts_with("file://") {
                true => gio::File::for_uri(content).path(),
                false => Some(PathBuf::from(content)),
            };
            match path {
                Some(path) => self.set_preview_image(&path),
                None => {
                    self.preview_status.set_text("Preview unavailable");
                    self.preview_stack.set_visible_child_name("status");
                }
            }
            return;
        }

        match preview::cached_image(content) {
            Some(path) => self.set_preview_image(&path),
            None => {
                self.preview_loader.start(content.to_string());
            }
        }
    }

    /// Draws artwork from the icon theme rather than the filesystem.
    fn set_preview_icon(&self, icon: &IconRef) {
        match gio::Icon::for_string(&icon.0) {
            Ok(gicon) => self.preview_image.set_from_gicon(&gicon),
            Err(_) => self.preview_image.set_icon_name(Some(&icon.0)),
        }
        // Same trap as `set_from_file`: setting the source resets the storage
        // type and drops the pixel size with it, leaving a 16px icon.
        self.preview_image.set_pixel_size(PREVIEW_IMAGE_SIZE);
        self.preview_stack.set_visible_child_name("image");
    }

    fn set_preview_caption(&self, caption: Option<&str>) {
        match caption {
            Some(caption) => {
                self.preview_caption.set_text(caption);
                self.preview_caption_view.set_visible(true);
            }
            None => self.preview_caption_view.set_visible(false),
        }
    }

    fn set_preview_image(&self, path: &Path) {
        self.preview_image.set_from_file(Some(path));
        // `set_from_file` resets the storage type, which drops the pixel size
        // with it — without this the picture falls back to a 16px icon.
        self.preview_image.set_pixel_size(PREVIEW_IMAGE_SIZE);
        self.preview_stack.set_visible_child_name("image");
    }

    fn drain_previews(self: &Rc<Self>) {
        for event in self.preview_loader.drain() {
            match event {
                PreviewEvent::Ready { path, .. } => self.set_preview_image(&path),
                PreviewEvent::Failed { .. } => {
                    self.preview_status.set_text("Preview unavailable");
                    self.preview_stack.set_visible_child_name("status");
                }
            }
        }
    }

    /// Whether the output is wide enough for a panel on each side of the card.
    ///
    /// The spacer means the preview costs twice its own width. On a narrow
    /// screen that would push the card off-centre or off-screen entirely, and a
    /// launcher that cannot be read is worse than one without previews.
    fn preview_fits(&self) -> bool {
        let available = self.window.width();
        available == 0
            || available >= self.runtime().config.clamped_width() + 2 * (PREVIEW_WIDTH + PREVIEW_GAP)
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
        let rt = self.runtime();
        let parsed = query::parse(&self.entry.text(), &rt.prefixes);
        let Query::Prefixed { key, .. } = &parsed.query else {
            return;
        };
        let Some(prefix) = rt.prefixes.get(key) else {
            return;
        };
        if !matches!(prefix.kind, PrefixKind::FileSearch) {
            return;
        }

        let selected = self.selected.get();
        let results = self.file_results(rt.config.clamped_result_limit());
        self.render(results);
        self.select(selected);
    }

    // -- open windows ------------------------------------------------------

    /// Asks the compositor for the window list unless a fresh one is already in
    /// hand. Cheap enough to call from `rebuild`, which runs on every keystroke.
    fn ensure_windows(&self) {
        if !self.window_source.available() {
            return;
        }
        let fresh = self
            .windows_requested
            .get()
            .is_some_and(|at| at.elapsed() < WINDOW_LIST_TTL);
        if fresh {
            return;
        }

        // Stamped before the request, not after the reply: otherwise every
        // keystroke during a slow query would start another one.
        self.windows_requested.set(Some(Instant::now()));
        self.window_source.refresh();
    }

    /// Forgets the cached list, so the next `ensure_windows` really asks.
    fn invalidate_windows(&self) {
        self.windows_requested.set(None);
    }

    fn drain_windows(self: &Rc<Self>) {
        let Some(reply) = self.window_source.drain() else {
            return;
        };

        match reply {
            Ok(windows) => {
                *self.open_windows.borrow_mut() = windows;
                *self.windows_error.borrow_mut() = None;
            }
            // The previous list described a compositor state we can no longer
            // confirm, so showing it would offer windows that may be gone.
            Err(error) => {
                self.open_windows.borrow_mut().clear();
                *self.windows_error.borrow_mut() = Some(error);
            }
        }
        self.windows_loaded.set(true);
        // The rows on screen were built without this list — or from an older one
        // — so they have to be rebuilt against it.
        self.rebuild();
    }

    /// The rows for the window-switcher prefix, or one row explaining why there
    /// are none. A blank list would read as a bug.
    fn window_rows(&self, prefix: &Prefix, arg: &str, limit: usize) -> Vec<SpotlightResult> {
        if !self.window_source.available() {
            return vec![results::windows_notice(
                prefix,
                "Switching windows needs Hyprland or sway",
            )];
        }

        if let Some(error) = self.windows_error.borrow().as_deref() {
            return vec![results::windows_notice(prefix, error)];
        }

        let open = self.open_windows.borrow();
        if open.is_empty() {
            let note = match self.windows_loaded.get() {
                true => "No windows are open",
                false => "Asking the compositor…",
            };
            return vec![results::windows_notice(prefix, note)];
        }

        let rows = results::window_results(arg, &open, &self.live_index.snapshot(), limit);
        match rows.is_empty() {
            true => vec![results::windows_notice(prefix, "No window matches")],
            false => rows,
        }
    }

    // -- ssh hosts ---------------------------------------------------------

    /// Reads the SSH config once per time the window is shown.
    ///
    /// Inline rather than on a worker thread, unlike the window list: this is a
    /// few kilobytes from local disk with no subprocess that could hang, and the
    /// result is reused for every keystroke that follows.
    fn ensure_ssh_hosts(&self) {
        if self.ssh_loaded.get() {
            return;
        }
        // Set first, so a config that fails to read is not retried on every
        // keystroke for as long as the prefix is open.
        self.ssh_loaded.set(true);

        match ssh::load() {
            Ok(hosts) => {
                *self.ssh_hosts.borrow_mut() = hosts;
                *self.ssh_error.borrow_mut() = None;
            }
            Err(error) => {
                tracing::warn!(%error, "failed to read the ssh config");
                self.ssh_hosts.borrow_mut().clear();
                *self.ssh_error.borrow_mut() = Some(error);
            }
        }
    }

    /// Forgets the host list, so the next `ensure_ssh_hosts` really reads it.
    fn invalidate_ssh_hosts(&self) {
        self.ssh_loaded.set(false);
    }

    /// The rows for the software prefix: categories, or the apps inside one.
    fn software_rows(
        &self,
        rt: &SpotlightRuntime,
        prefix_key: &str,
        arg: &str,
        limit: usize,
    ) -> Vec<SpotlightResult> {
        results::software_results(
            prefix_key,
            arg,
            &rt.software,
            rt.config.software.keep_open,
            &self.frecency.borrow(),
            frecency::now_secs(),
            limit,
        )
    }

    /// The rows for the ssh prefix, or one row explaining why there are none.
    fn ssh_rows(&self, prefix: &Prefix, arg: &str, limit: usize) -> Vec<SpotlightResult> {
        if let Some(error) = self.ssh_error.borrow().as_deref() {
            return vec![results::ssh_notice(prefix, error)];
        }

        let hosts = self.ssh_hosts.borrow();
        let rows = results::ssh_results(
            prefix,
            arg,
            &hosts,
            &self.frecency.borrow(),
            frecency::now_secs(),
            limit,
        );
        if !rows.is_empty() {
            return rows;
        }

        // Nothing matched, which means one of two quite different things: an
        // empty config, or a query that no host matches and that ssh could not
        // be handed either.
        let note = match hosts.is_empty() {
            true => "No hosts in ~/.ssh/config · type a host to connect",
            false => "No host matches · type a host to connect",
        };
        vec![results::ssh_notice(prefix, note)]
    }

    // -- vpn ---------------------------------------------------------------

    /// Asks the VPN client for its state unless a fresh answer is already in
    /// hand. Cheap enough to call from `rebuild`, which runs on every keystroke.
    fn ensure_vpn(&self, provider: vpn::Provider) {
        let fresh = self
            .vpn_requested
            .get()
            .is_some_and(|at| at.elapsed() < VPN_STATE_TTL);
        if fresh {
            return;
        }

        // Stamped before the request, not after the reply, so a slow client does
        // not collect one query per keystroke.
        self.vpn_requested.set(Some(Instant::now()));
        self.vpn_source.refresh(provider);
    }

    /// Forgets the cached state, so the next `ensure_vpn` really asks.
    fn invalidate_vpn(&self) {
        self.vpn_requested.set(None);
    }

    fn drain_vpn(self: &Rc<Self>) {
        let Some(reply) = self.vpn_source.drain() else {
            return;
        };

        match reply {
            Ok(state) => {
                *self.vpn_state.borrow_mut() = Some(state);
                *self.vpn_error.borrow_mut() = None;
            }
            // The previous state described a connection we can no longer
            // confirm, so keeping it on screen would claim something untrue
            // about the user's traffic.
            Err(error) => {
                *self.vpn_state.borrow_mut() = None;
                *self.vpn_error.borrow_mut() = Some(error);
            }
        }
        self.rebuild();
    }

    /// The rows for the VPN prefix, or one row explaining why there are none.
    fn vpn_rows(
        &self,
        prefix: &Prefix,
        arg: &str,
        provider: vpn::Provider,
        limit: usize,
    ) -> Vec<SpotlightResult> {
        if let Some(error) = self.vpn_error.borrow().as_deref() {
            return vec![results::vpn_notice(prefix, error)];
        }

        let state = self.vpn_state.borrow();
        let Some(state) = state.as_ref() else {
            return vec![results::vpn_notice(
                prefix,
                &format!("Asking {}…", provider.label()),
            )];
        };

        let rows = results::vpn_results(
            prefix,
            arg,
            provider,
            state,
            &self.frecency.borrow(),
            frecency::now_secs(),
            limit,
        );
        if !rows.is_empty() {
            return rows;
        }

        // The action row always matches an empty query, so an empty list means a
        // query that matched nothing — unless the client listed no locations at
        // all, which for every provider so far means there is no session to list
        // them for.
        let note = match state.locations.is_empty() {
            true if !state.status.logged_in => "Log in to your VPN client to pick a location",
            true => "The client listed no locations",
            false => "No location matches",
        };
        vec![results::vpn_notice(prefix, note)]
    }

    // -- custom results ----------------------------------------------------

    /// Cancels both asynchronous result sources at once.
    fn cancel_async_results(&self) {
        self.file_search.cancel();
        self.cancel_custom_results();
    }

    fn cancel_custom_results(&self) {
        self.custom_results.cancel();
        self.custom_rows.borrow_mut().clear();
        *self.custom_error.borrow_mut() = None;
        *self.custom_line.borrow_mut() = None;
        self.custom_pending.set(false);
    }

    /// The rows for a `get_results` prefix: whatever the command last returned,
    /// or a single row standing in for its state.
    fn custom_results(
        &self,
        prefix: &Prefix,
        arg: &str,
        action: Option<&str>,
        terminal: bool,
        icon_size: i32,
        limit: usize,
    ) -> Vec<SpotlightResult> {
        if let Some(error) = self.custom_error.borrow().as_deref() {
            return vec![results::custom_results_error(error)];
        }
        if arg.trim().is_empty() {
            return vec![results::custom_results_notice(prefix, "Type to search")];
        }

        let rows = self.custom_rows.borrow();
        if rows.is_empty() && self.custom_pending.get() {
            return vec![results::custom_results_notice(prefix, "Searching…")];
        }
        // Past the first page an empty result is the end of the list, not a
        // query that matched nothing — saying "No results" there would read as
        // though the search itself had failed.
        if rows.is_empty() && self.custom_page.get() > FIRST_PAGE {
            return vec![results::custom_results_notice(
                prefix,
                "No more results — Alt+← for the previous page",
            )];
        }

        results::custom_result_rows(prefix, &rows, action, terminal, icon_size, limit)
    }

    fn start_custom_results(&self, command: &str, delay: Duration, arg: &str) {
        let arg = arg.trim();
        let line = build_results_line(command, arg, self.custom_page.get());
        if self.custom_line.borrow().as_deref() == Some(line.as_str()) {
            return;
        }
        // Only a run that actually starts consumes it, or the flag would leak
        // into whichever keystroke came next.
        let delay = match self.custom_immediate.replace(false) {
            true => Duration::ZERO,
            false => delay,
        };

        self.custom_rows.borrow_mut().clear();
        *self.custom_error.borrow_mut() = None;
        *self.custom_line.borrow_mut() = Some(line.clone());

        if arg.is_empty() {
            self.custom_results.cancel();
            self.custom_pending.set(false);
            return;
        }

        self.custom_pending.set(true);
        self.custom_results.start(line, delay);
    }

    /// Keeps the page tied to the question being asked.
    ///
    /// Editing the query, or switching to another prefix, starts again at page
    /// one — page 4 of a search the user has since retyped is meaningless, and
    /// carrying it over would silently hide the first three pages of results.
    fn sync_page(&self, key: &str, arg: &str, paginated: bool) {
        // NUL cannot occur in either half, so the join is unambiguous.
        let page_key = paginated.then(|| format!("{key}\0{}", arg.trim()));
        if *self.custom_page_key.borrow() == page_key {
            return;
        }

        *self.custom_page_key.borrow_mut() = page_key;
        self.custom_page.set(FIRST_PAGE);
    }

    /// Steps a paginated prefix forward or back, if one is active.
    fn change_page(self: &Rc<Self>, delta: i32) {
        if !self.paginated_query() {
            return;
        }

        let page = (self.custom_page.get() + i64::from(delta)).max(FIRST_PAGE);
        if page == self.custom_page.get() {
            return;
        }

        self.custom_page.set(page);
        self.custom_immediate.set(true);
        self.rebuild();
    }

    /// Whether what is in the entry right now can be paged.
    fn paginated_query(&self) -> bool {
        if self.mode.get() == keys::Mode::Chat {
            return false;
        }

        let rt = self.runtime();
        let parsed = query::parse(&self.entry.text(), &rt.prefixes);
        let Query::Prefixed { key, arg } = &parsed.query else {
            return false;
        };
        // A prefix with nothing typed after it has not run anything to page.
        !arg.trim().is_empty()
            && matches!(
                rt.prefixes.get(key).map(|prefix| &prefix.kind),
                Some(PrefixKind::CustomResults {
                    paginated: true,
                    ..
                })
            )
    }

    fn drain_custom_results(self: &Rc<Self>) {
        let events = self.custom_results.drain();
        if events.is_empty() {
            return;
        }

        for event in events {
            match event {
                ResultsEvent::Ready { results, .. } => {
                    *self.custom_rows.borrow_mut() = results;
                    *self.custom_error.borrow_mut() = None;
                }
                ResultsEvent::Failed { error, .. } => {
                    tracing::warn!(%error, "a spotlight get_results command failed");
                    self.custom_rows.borrow_mut().clear();
                    *self.custom_error.borrow_mut() = Some(error);
                }
            }
        }
        self.custom_pending.set(false);

        // Only re-render while a get_results prefix is still active.
        let rt = self.runtime();
        let parsed = query::parse(&self.entry.text(), &rt.prefixes);
        let Query::Prefixed { key, arg } = &parsed.query else {
            return;
        };
        let Some(prefix) = rt.prefixes.get(key) else {
            return;
        };
        let PrefixKind::CustomResults {
            action,
            terminal,
            icon_size,
            ..
        } = &prefix.kind
        else {
            return;
        };

        let selected = self.selected.get();
        let results = self.custom_results(
            prefix,
            arg,
            action.as_deref(),
            *terminal,
            *icon_size,
            rt.config.clamped_result_limit(),
        );
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
        // Opening a chat keeps the window up, so it returns before `close()`.
        if let Activation::AskAi { provider, prompt } = &activation {
            self.enter_chat(*provider, prompt);
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
            Activation::FocusWindow(handle) => {
                windows::activate(self.window_source.compositor(), handle)
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
            // Both are handled before `perform` is reached.
            Activation::Replace(_) | Activation::AskAi { .. } | Activation::Inert => Ok(()),
        }
    }

    // -- chat --------------------------------------------------------------

    /// Switches to the transcript and sends the first prompt.
    ///
    /// A prompt from the results list always starts a fresh conversation;
    /// re-entering without one resumes whatever is already there.
    fn enter_chat(self: &Rc<Self>, provider_index: usize, prompt: &str) {
        let rt = self.runtime();
        let Some(provider) = rt.ai_providers.get(provider_index) else {
            return;
        };

        let restarting = self
            .chat
            .borrow()
            .as_ref()
            .is_none_or(|chat| chat.provider != provider.provider);
        if restarting {
            *self.chat.borrow_mut() = Some(ChatState {
                provider: provider.provider.clone(),
                label: provider.label.clone(),
                history: Vec::new(),
                streaming: false,
                tools: provider.tools.clone(),
                web_search: provider.web_search,
                system: provider.system.clone(),
                rounds: 0,
                max_rounds: provider.max_tool_rounds,
                command_timeout: provider.command_timeout,
                resumes: 0,
                continuations: 0,
                pending: None,
                queue: std::collections::VecDeque::new(),
                results: Vec::new(),
            });
            icons::clear_box_children(&self.transcript);
        }

        self.set_mode(keys::Mode::Chat);
        self.prefix_badge.set_text(&provider.label);
        self.prefix_badge.set_visible(true);
        self.send_prompt(prompt.to_string());
    }

    /// Returns to the results list, cancelling any stream.
    ///
    /// Never routes to `close()` — that quits the process in one-shot mode.
    fn leave_chat(self: &Rc<Self>) {
        // Approval counts as being in the chat: Escape is bound to declining
        // there, so reaching this at all means the user wants out entirely.
        if self.mode.get() == keys::Mode::Search {
            self.close();
            return;
        }

        self.cancel_stream();
        self.set_mode(keys::Mode::Search);
        self.set_entry_text("");
        self.rebuild();
        self.reflow();
    }

    fn set_mode(self: &Rc<Self>, mode: keys::Mode) {
        self.mode.set(mode);
        // The panel belongs to the result list; the chat replaces it.
        self.refresh_preview();
        match mode {
            // Approval is the chat surface with a card on it — same widgets,
            // different key table.
            keys::Mode::Chat | keys::Mode::Approval => {
                self.body.set_visible_child_name("chat");
                self.footers.set_visible_child_name("chat");
                self.empty_label.set_visible(false);
                self.entry.set_placeholder_text(Some("Reply…"));
                self.hint_label.set_visible(false);
            }
            keys::Mode::Search => {
                self.body.set_visible_child_name("results");
                self.footers.set_visible_child_name("results");
                self.entry
                    .set_placeholder_text(Some("Search apps, folders and commands"));
                self.status_label.set_visible(false);
                self.set_streaming(false);
            }
        }
    }

    fn send_follow_up(self: &Rc<Self>) {
        let prompt = self.entry.text().trim().to_string();
        if prompt.is_empty() {
            return;
        }
        self.send_prompt(prompt);
    }

    /// Appends the prompt to the transcript and starts a request.
    fn send_prompt(self: &Rc<Self>, prompt: String) {
        // A second send supersedes the first; the old worker sees the bumped
        // generation and unwinds on its own.
        self.finalize_streaming_label();

        let sent = self
            .chat
            .borrow_mut()
            .as_mut()
            .map(|chat| {
                chat.history.push(ChatMessage::user(prompt.clone()));
                // A new prompt is a new turn: every budget starts again.
                chat.rounds = 0;
                chat.resumes = 0;
                chat.continuations = 0;
            })
            .is_some();
        if !sent {
            return;
        }

        self.append_message("You", &prompt, "spotlight-chat-user");
        self.set_entry_text("");
        // Appending to the transcript can move focus; take it back so the next
        // follow-up can be typed straight away.
        self.entry.grab_focus_without_selecting();
        self.status_label.set_visible(false);
        self.start_turn();
        self.scroll_chat_to_bottom();
    }

    /// Sends the conversation as it currently stands.
    ///
    /// Every request goes through here — the first prompt, a follow-up, the
    /// continuation after a tool result, and a `pause_turn` resume — so the tool
    /// set and the streaming flag can never be set one way in one path and
    /// another way in the next.
    fn start_turn(self: &Rc<Self>) {
        let Some(turn) = self.chat.borrow_mut().as_mut().map(|chat| {
            chat.streaming = true;
            crate::spotlight::ai::Turn {
                provider: chat.provider.clone(),
                history: chat.history.clone(),
                tools: chat.tools.clone(),
                web_search: chat.web_search,
                system: chat.system.clone(),
            }
        }) else {
            return;
        };

        self.set_streaming(true);
        self.ai_session.start(turn);
    }

    /// Stops generating but stays in the transcript, keeping partial text.
    fn cancel_stream(self: &Rc<Self>) {
        // Also reachable while a tool is running or awaiting approval, when no
        // HTTP request is in flight — stopping has to work there too, and that
        // is exactly when someone is most likely to want out.
        let interrupted = self.abandon_tools();
        if !self.is_streaming() && !interrupted {
            return;
        }
        self.ai_session.cancel();
        self.finalize_streaming_label();
        // A stopped reply is still worth formatting — the partial is what the
        // user chose to keep.
        self.render_markdown();
        self.set_streaming(false);
        self.note("Stopped.");
    }

    fn copy_last_reply(self: &Rc<Self>) {
        let last = self.chat.borrow().as_ref().and_then(|chat| {
            chat.history
                .iter()
                .rev()
                .find(|message| matches!(message.role, crate::spotlight::ai::Role::Assistant))
                .map(ChatMessage::text)
        });

        let Some(text) = last else {
            return;
        };
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&text);
            self.note("Copied the last reply.");
        }
    }

    fn is_streaming(&self) -> bool {
        self.chat
            .borrow()
            .as_ref()
            .is_some_and(|chat| chat.streaming)
    }

    fn set_streaming(&self, streaming: bool) {
        if let Some(chat) = self.chat.borrow_mut().as_mut() {
            chat.streaming = streaming;
        }
        self.spinner.set_visible(streaming);
        self.spinner.set_spinning(streaming);
    }

    /// Drains this tick's stream events, issuing exactly one `set_text`.
    fn drain_ai(self: &Rc<Self>) {
        let events = self.ai_session.drain();
        if events.is_empty() {
            return;
        }

        let mut grew = false;
        for event in events {
            match event {
                AiEvent::Started { .. } => self.set_streaming(true),
                AiEvent::Thinking { .. } => {
                    // Accurate on Claude: reasoning can run for seconds with no
                    // visible output, and this is the only signal of it.
                    self.status_label.set_text("Thinking…");
                    self.status_label.set_visible(true);
                }
                AiEvent::Delta { text, .. } => {
                    self.streaming_text.borrow_mut().push_str(&text);
                    grew = true;
                }
                AiEvent::ToolRequested { text, calls, .. } => {
                    self.begin_tool_round(&text, calls);
                    // The turn is not over — the transcript continues once the
                    // results are back — so no `finish_stream` here.
                    return;
                }
                AiEvent::Done { stop_reason, .. } => self.finish_stream(stop_reason),
                AiEvent::Failed { error, .. } => self.fail_stream(&error),
            }
        }

        if grew {
            self.status_label.set_visible(false);
            self.flush_streaming_text();
        }
    }

    /// Writes the accumulated text in one shot — Pango re-lays-out the whole
    /// label on every `set_text`, so doing it per delta would be wasteful.
    fn flush_streaming_text(self: &Rc<Self>) {
        let text = self.streaming_text.borrow().clone();
        if text.is_empty() {
            return;
        }

        let label = self.streaming_label.borrow().clone();
        let label = match label {
            Some(label) => label,
            None => {
                let label = self.append_message(&self.chat_label(), "", "spotlight-chat-assistant");
                *self.streaming_label.borrow_mut() = Some(label.clone());
                label
            }
        };

        // No scrolling here: setting the text triggers a re-layout, and the
        // adjustment's `changed` handler pins the bottom once that lands.
        label.set_text(&format!("{text}{STREAM_CARET}"));
    }

    fn finish_stream(self: &Rc<Self>, stop_reason: Option<String>) {
        let text = self.finalize_streaming_label();
        self.set_streaming(false);
        // Formatting is applied now rather than per delta: a half-arrived
        // `**bold` would otherwise render as a literal `**` until its closing
        // pair showed up, which reads worse than plain text.
        self.render_markdown();

        if !text.is_empty()
            && let Some(chat) = self.chat.borrow_mut().as_mut()
        {
            chat.history.push(ChatMessage::assistant(text));
        }

        match stop_reason.as_deref() {
            // The reply ran out of budget mid-sentence. Rather than leaving a
            // half-answer standing, the loop prompts itself to finish it — the
            // one case where "not done yet" is unambiguous enough to act on
            // without asking.
            Some("max_tokens") => {
                let continuing = self
                    .chat
                    .borrow_mut()
                    .as_mut()
                    .map(|chat| {
                        chat.continuations += 1;
                        // A new user turn rather than a prefilled assistant one:
                        // the API rejects a trailing assistant message with
                        // trailing whitespace, and a streamed reply routinely
                        // ends in some.
                        let carry_on = chat.continuations <= MAX_CONTINUATIONS;
                        if carry_on {
                            chat.history
                                .push(ChatMessage::user(CONTINUE_PROMPT.to_string()));
                        }
                        carry_on
                    })
                    .unwrap_or(false);
                if continuing {
                    self.note("Continuing…");
                    self.start_turn();
                    return;
                }
                self.note("Cut off at the token limit — raise `max_tokens` in [[spotlight.ai]].");
            }
            // The server-side tool loop hit its own iteration cap and wants to
            // carry on. The assistant turn has just been appended, so re-sending
            // the conversation is the documented way to resume it.
            Some("pause_turn") => {
                let resuming = self
                    .chat
                    .borrow_mut()
                    .as_mut()
                    .map(|chat| {
                        chat.resumes += 1;
                        chat.resumes <= MAX_PAUSE_RESUMES
                    })
                    .unwrap_or(false);
                if resuming {
                    self.status_label.set_text("Still working…");
                    self.status_label.set_visible(true);
                    self.start_turn();
                    return;
                }
                self.note(&format!("Stopped after {MAX_PAUSE_RESUMES} continuations."));
            }
            _ => self.status_label.set_visible(false),
        }
        self.reflow();
    }

    // -- tool use ----------------------------------------------------------

    /// The model asked for tools. Records the assistant turn and starts working
    /// through the calls.
    fn begin_tool_round(self: &Rc<Self>, text: &str, calls: Vec<ToolCall>) {
        self.finalize_streaming_label();
        self.render_markdown();
        // The spinner deliberately stays on. The request is over, but the turn
        // is not, and a command may take a minute — with nothing spinning the
        // overlay reads as finished, and `Ctrl+C`, which tests this same flag,
        // would decline to stop anything.

        let over_budget = {
            let mut chat = self.chat.borrow_mut();
            let Some(chat) = chat.as_mut() else {
                return;
            };

            // The assistant turn carries what it said *and* what it asked for.
            // Dropping the text would lose the model's own reasoning from the
            // conversation; dropping the blocks would orphan the results.
            let mut blocks = Vec::new();
            if !text.trim().is_empty() {
                blocks.push(Block::Text(text.to_string()));
            }
            for call in &calls {
                blocks.push(Block::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
            }
            chat.history.push(ChatMessage {
                role: Role::Assistant,
                blocks,
            });

            chat.rounds += 1;
            chat.results.clear();
            chat.queue = calls.into_iter().collect();
            (chat.rounds > chat.max_rounds).then_some(chat.max_rounds)
        };

        if let Some(limit) = over_budget {
            // Every call still needs an answer, or the follow-up request is
            // rejected outright — so refuse them rather than dropping them.
            self.answer_remaining("Tool round limit reached; nothing was run.");
            // Recorded but deliberately *not* sent. Sending would give the model
            // another turn, in which it would ask for another tool and land back
            // here — a loop of API calls that only stops when the user notices.
            self.commit_tool_results();
            self.set_streaming(false);
            self.note(&format!("Stopped after {limit} tool rounds."));
            return;
        }

        self.advance_tools();
    }

    /// Runs the next queued call, or sends the results when none are left.
    fn advance_tools(self: &Rc<Self>) {
        let next = self
            .chat
            .borrow_mut()
            .as_mut()
            .and_then(|chat| chat.queue.pop_front());

        let Some(call) = next else {
            return self.send_tool_results();
        };
        self.run_or_ask(call);
    }

    /// Hands the accumulated results back as one user turn and continues.
    fn send_tool_results(self: &Rc<Self>) {
        if self.commit_tool_results() {
            self.start_turn();
        }
    }

    /// Moves this round's results into the history. Returns whether there were
    /// any.
    ///
    /// Separate from sending them because the round-limit path has to record the
    /// answers without granting another turn: the API rejects a later request
    /// that leaves a `tool_use` unanswered, so the results must land in the
    /// history even when the loop stops here.
    fn commit_tool_results(self: &Rc<Self>) -> bool {
        self.chat
            .borrow_mut()
            .as_mut()
            .map(|chat| {
                let blocks = std::mem::take(&mut chat.results);
                if blocks.is_empty() {
                    return false;
                }
                // One user turn carrying every result from this round: splitting
                // them across turns is rejected when several calls were made.
                chat.history.push(ChatMessage {
                    role: Role::User,
                    blocks,
                });
                true
            })
            .unwrap_or(false)
    }

    /// Decides how a single call is handled: run it, or ask first.
    fn run_or_ask(self: &Rc<Self>, call: ToolCall) {
        let definition = self.chat.borrow().as_ref().and_then(|chat| {
            chat.tools
                .iter()
                .find(|tool| tool.name == call.name)
                .cloned()
        });

        let Some(definition) = definition else {
            // The model named a tool that is not declared. Say so rather than
            // hanging: it can correct itself on the next round.
            return self.finish_tool(
                call.id,
                ToolOutcome::Error(format!("no tool named '{}'", call.name)),
            );
        };

        if definition.effect == tools::Effect::ReadOnly {
            self.append_tool_note(&format!("Running {}…", definition.name));
            self.tool_runner.start(
                call.id,
                definition.kind.clone(),
                call.input,
                self.app_snapshot(),
            );
            return;
        }

        let expanded = tools::preview(&definition, &call);
        if !definition.needs_approval() {
            // `confirm = "never"` on this specific tool — the user's explicit,
            // per-tool choice, not a general bypass.
            return self.dispatch_side_effecting(
                call.id,
                &definition.name,
                &definition.kind,
                &call.input,
                &expanded,
            );
        }

        // Nothing is happening until the user decides, so the spinner stops —
        // the one point in a tool round where waiting is on them, not on us.
        self.set_streaming(false);
        self.show_approval_card(&definition.name, &expanded);
        if let Some(chat) = self.chat.borrow_mut().as_mut() {
            chat.pending = Some(PendingApproval {
                call,
                kind: definition.kind.clone(),
                expanded,
            });
        }
        self.set_mode(keys::Mode::Approval);
    }

    /// Applies the user's decision on the pending call.
    fn resolve_approval(self: &Rc<Self>, approved: bool) {
        let pending = self
            .chat
            .borrow_mut()
            .as_mut()
            .and_then(|chat| chat.pending.take());
        let Some(pending) = pending else {
            return;
        };

        if let Some(card) = self.approval_card.borrow_mut().take() {
            self.transcript.remove(&card);
        }
        self.set_mode(keys::Mode::Chat);
        // The wait is ours again, whichever way they decided.
        self.set_streaming(true);

        if !approved {
            // A denial is reported as a result, not as silence — the model can
            // then adapt instead of waiting for an answer that never arrives.
            let outcome = ToolOutcome::Error("The user declined to run this.".to_string());
            self.append_tool_note(&format!("{}: {}", pending.call.name, outcome.text()));
            return self.finish_tool(pending.call.id, outcome);
        }

        self.dispatch_side_effecting(
            pending.call.id,
            &pending.call.name,
            &pending.kind,
            &pending.call.input,
            &pending.expanded,
        );
    }

    /// Runs an approved side-effecting call by whichever route its kind needs.
    ///
    /// A command goes to the worker with its output captured, and answers later
    /// through [`Self::drain_tools`]. Anything else is a spawn that returns at
    /// once and is answered here. The split matters: a command's *output is the
    /// answer*, so reporting "Started: uname -a" and moving on would leave the
    /// model to guess at — or ask the user about — something it just ran.
    fn dispatch_side_effecting(
        self: &Rc<Self>,
        id: String,
        name: &str,
        kind: &ToolKind,
        input: &serde_json::Value,
        expanded: &str,
    ) {
        if tools::is_command(kind) {
            let timeout = self
                .chat
                .borrow()
                .as_ref()
                .map(|chat| chat.command_timeout)
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT);
            self.append_tool_note(&format!("$ {expanded}"));
            self.tool_runner
                .start_command(id, expanded.to_string(), timeout);
            return;
        }

        let outcome = tools::run_side_effecting(kind, input, expanded);
        self.append_tool_note(&format!("{name}: {}", outcome.text()));
        self.finish_tool(id, outcome);
    }

    /// Records one call's answer and moves on.
    fn finish_tool(self: &Rc<Self>, id: String, outcome: ToolOutcome) {
        if let Some(chat) = self.chat.borrow_mut().as_mut() {
            chat.results.push(Block::ToolResult {
                tool_use_id: id,
                content: outcome.text().to_string(),
                is_error: outcome.is_error(),
            });
        }
        self.advance_tools();
    }

    /// Answers every still-queued call with the same refusal.
    fn answer_remaining(self: &Rc<Self>, reason: &str) {
        if let Some(chat) = self.chat.borrow_mut().as_mut() {
            while let Some(call) = chat.queue.pop_front() {
                chat.results.push(Block::ToolResult {
                    tool_use_id: call.id,
                    content: reason.to_string(),
                    is_error: true,
                });
            }
        }
        self.send_tool_results();
    }

    fn drain_tools(self: &Rc<Self>) {
        for event in self.tool_runner.drain() {
            // Tool output is untrusted text — a file or a web page the model
            // read. `append_tool_note` sets it as a label's plain text, so no
            // markup in it can be interpreted.
            let summary = summarize(event.outcome.text());
            self.append_tool_note(&summary);
            self.finish_tool(event.id, event.outcome);
        }
    }

    /// Drops any in-flight or pending tool work. Returns whether there was any.
    ///
    /// The queue and the accumulated results go with it: a half-answered round
    /// cannot be resumed, because the API needs every `tool_use` from that
    /// assistant turn answered in one following user turn.
    fn abandon_tools(self: &Rc<Self>) -> bool {
        self.tool_runner.cancel();

        if let Some(card) = self.approval_card.borrow_mut().take() {
            self.transcript.remove(&card);
        }
        if self.mode.get() == keys::Mode::Approval {
            self.set_mode(keys::Mode::Chat);
        }

        let had_work = self
            .chat
            .borrow()
            .as_ref()
            .map(|chat| chat.pending.is_some() || !chat.queue.is_empty())
            .unwrap_or(false);

        self.close_open_tool_round("The user stopped this before it ran.");
        had_work
    }

    /// Answers every outstanding call in the current round, then closes it.
    ///
    /// Load-bearing rather than tidiness. By the time a round can be abandoned
    /// its assistant turn — carrying the `tool_use` blocks — is already in the
    /// history, and the API rejects any later request that leaves one of them
    /// without a matching `tool_result`. Dropping the queue instead of
    /// answering it would leave the conversation permanently un-sendable, and
    /// the failure would not surface until the user's *next* message.
    fn close_open_tool_round(&self, reason: &str) {
        let mut chat = self.chat.borrow_mut();
        let Some(chat) = chat.as_mut() else {
            return;
        };

        let outstanding = match chat.history.last() {
            Some(message) if message.role == Role::Assistant => message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    Block::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };

        chat.pending = None;
        chat.queue.clear();

        if outstanding.is_empty() {
            chat.results.clear();
            return;
        }

        let answered = chat
            .results
            .iter()
            .filter_map(|block| match block {
                Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        let mut blocks = std::mem::take(&mut chat.results);
        for id in outstanding {
            if !answered.contains(&id) {
                blocks.push(Block::ToolResult {
                    tool_use_id: id,
                    content: reason.to_string(),
                    is_error: true,
                });
            }
        }
        chat.history.push(ChatMessage {
            role: Role::User,
            blocks,
        });
    }

    /// Applications as plain data, so a worker can filter them without touching
    /// GTK.
    fn app_snapshot(&self) -> Vec<AppSummary> {
        self.live_index
            .snapshot()
            .entries()
            .iter()
            .map(|entry| AppSummary {
                desktop_id: entry.desktop_id.clone(),
                name: entry.name.clone(),
                description: entry.comment.clone(),
            })
            .collect()
    }

    fn fail_stream(self: &Rc<Self>, error: &AiError) {
        // A refusal arrives after any content blocks, so discard the partial
        // rather than leaving a half-answer standing above the explanation.
        if matches!(error, AiError::Refused { .. })
            && let Some(label) = self.streaming_label.borrow_mut().take()
        {
            self.transcript
                .remove(&label.parent().unwrap_or_else(|| label.clone().upcast()));
        }
        self.finalize_streaming_label();
        // Formats whatever arrived before the failure, and — importantly —
        // consumes `finished_label` so a later reply cannot render into this
        // now-stale one.
        self.render_markdown();
        self.set_streaming(false);
        self.append_message("Error", &error.to_string(), "spotlight-chat-error");
        self.scroll_chat_to_bottom();
        self.reflow();
    }

    /// Strips the caret and detaches the streaming label, returning its text.
    ///
    /// The label is handed to `finished_label` so [`Self::render_markdown`] can
    /// replace it once the reply is known to be complete.
    fn finalize_streaming_label(self: &Rc<Self>) -> String {
        let text = std::mem::take(&mut *self.streaming_text.borrow_mut());
        if let Some(label) = self.streaming_label.borrow_mut().take() {
            label.set_text(&text);
            *self.finished_label.borrow_mut() = Some(label);
        }
        text
    }

    fn chat_label(&self) -> String {
        self.chat
            .borrow()
            .as_ref()
            .map(|chat| chat.label.clone())
            .unwrap_or_else(|| "Assistant".to_string())
    }

    /// Replaces the just-finished reply's plain label with rendered Markdown.
    ///
    /// Leaves the transcript untouched when the reply has no block structure
    /// worth rendering, so a one-line answer keeps its single selectable label.
    fn render_markdown(self: &Rc<Self>) {
        let Some(label) = self.finished_label.borrow_mut().take() else {
            return;
        };
        let Some(row) = label.parent().and_downcast::<gtk::Box>() else {
            return;
        };

        let text = label.text().to_string();
        let blocks = markdown::parse(&text);
        if blocks.is_empty() {
            return;
        }

        row.remove(&label);
        for block in blocks {
            row.append(&widget_for_block(&block));
        }
    }

    /// Appends a role caption plus a body label, returning the body label so a
    /// streaming reply can keep writing into it.
    fn append_message(&self, role: &str, text: &str, css_class: &str) -> gtk::Label {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["spotlight-chat-row", css_class])
            .build();
        row.append(
            &gtk::Label::builder()
                .label(role)
                .xalign(0.0)
                .css_classes(["spotlight-chat-role"])
                .build(),
        );

        let body = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["spotlight-chat-text"])
            .build();
        // `selectable` makes a label focusable, so clicking one would otherwise
        // pull focus out of the entry. Text stays mouse-selectable; the click
        // just no longer moves keyboard focus.
        body.set_focus_on_click(false);
        row.append(&body);
        self.transcript.append(&row);
        body
    }

    /// A one-line entry in the transcript recording tool activity.
    ///
    /// Set as a label's plain text, never as markup: this carries tool output,
    /// which is a file or a web page the model read and is therefore untrusted.
    fn append_tool_note(&self, text: &str) {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["spotlight-chat-tool"])
            .build();
        label.set_focus_on_click(false);
        self.transcript.append(&label);
    }

    /// The approval card: what is about to run, and how to answer.
    ///
    /// Shows the **expanded** command — the actual argv the shell would see —
    /// not the template it came from. A card that showed `playerctl {query}`
    /// would hide exactly the part an injected argument controls.
    fn show_approval_card(self: &Rc<Self>, name: &str, expanded: &str) {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .css_classes(["spotlight-chat-approval"])
            .build();

        card.append(
            &gtk::Label::builder()
                .label(format!("Run {name}?"))
                .xalign(0.0)
                .css_classes(["spotlight-chat-role"])
                .build(),
        );

        let command = gtk::Label::builder()
            .label(expanded)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::Char)
            .selectable(true)
            .css_classes(["spotlight-chat-code"])
            .build();
        command.set_focus_on_click(false);
        card.append(&command);

        card.append(
            &gtk::Label::builder()
                .label("Enter to run · Esc to decline")
                .xalign(0.0)
                .css_classes(["spotlight-chat-status", "dim-label"])
                .build(),
        );

        self.transcript.append(&card);
        *self.approval_card.borrow_mut() = Some(card.upcast());
        self.scroll_chat_to_bottom();
    }

    fn note(&self, text: &str) {
        self.status_label.set_text(text);
        self.status_label.set_visible(true);
    }

    /// Jumps to the bottom and resumes following, whatever the user last did.
    fn scroll_chat_to_bottom(self: &Rc<Self>) {
        self.chat_follow.set(true);
        let adjustment = self.chat_scroller.vadjustment();
        // Deferred so the freshly appended row is measured first.
        glib::idle_add_local_once(move || {
            adjustment.set_value(adjustment.upper() - adjustment.page_size());
        });
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
        // Windows open and close while the launcher is hidden, so whatever was
        // cached from the previous session says nothing about this one. The same
        // goes for the ssh config, which the user may have just been editing.
        self.invalidate_windows();
        self.invalidate_ssh_hosts();
        // And the VPN may have been connected or dropped since — including by
        // the last row this launcher activated.
        self.invalidate_vpn();
        self.rebuild();
        self.reflow();
        self.window.present();

        let entry = self.entry.clone();
        glib::idle_add_local_once(move || {
            entry.grab_focus();
        });
    }

    /// Adopts a reloaded config, re-resolving everything derived from it.
    ///
    /// The width applies straight away because it costs nothing while hidden,
    /// but the layout and the result list are only redone when the window is on
    /// screen: the daemon spends most of its life hidden, [`show`](Self::show)
    /// redoes both anyway, and [`reflow`](Self::reflow) reads a surface height
    /// that means nothing for an unmapped window.
    ///
    /// A running conversation survives: [`ChatState`] holds cloned provider
    /// data rather than an index into the swapped-out list.
    pub fn apply_config(self: &Rc<Self>, config: SpotlightConfig) {
        let runtime = SpotlightRuntime::resolve(config);
        let width = runtime.config.clamped_width();
        *self.runtime.borrow_mut() = runtime;

        self.card.set_width_request(width);
        self.window.set_default_width(width);

        if self.window.is_visible() {
            self.reflow();
            self.rebuild();
        }
    }

    /// Hiding keeps the conversation: reopening and re-entering chat resumes it.
    /// The daemon holds it in memory only — nothing is written to disk, and it
    /// is gone on restart.
    pub fn hide(&self) {
        self.file_search.cancel();
        self.file_hits.borrow_mut().clear();
        self.cancel_custom_results();
        // A stream would otherwise keep appending into a hidden transcript.
        self.ai_session.cancel();
        // And a half-finished tool round would strand the conversation: its
        // assistant turn is already recorded, so reopening would resume a chat
        // whose `tool_use` blocks can never be answered.
        self.tool_runner.cancel();
        self.close_open_tool_round("Interrupted before this could run.");
        if let Some(chat) = self.chat.borrow_mut().as_mut() {
            chat.streaming = false;
        }
        if let Some(card) = self.approval_card.borrow_mut().take() {
            self.transcript.remove(&card);
        }
        self.spinner.set_spinning(false);
        // Also drops the pending debounce, whose generation check now fails.
        self.preview_loader.cancel();
        self.preview_generation
            .set(self.preview_generation.get().wrapping_add(1));
        *self.preview_shown.borrow_mut() = None;
        self.hovered.set(None);
        self.preview_panel.set_visible(false);
        self.preview_spacer.set_visible(false);
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

/// The key the software prefix actually ended up on, or `None` when it is not
/// in the table at all.
///
/// Read from the table rather than from the config, so a prefix the user
/// disabled or took over for something else does not leave plain queries
/// pointing at a key that no longer opens the catalog.
fn software_prefix_key(prefixes: &PrefixTable) -> Option<&str> {
    prefixes
        .all()
        .iter()
        .find(|prefix| prefix.kind == PrefixKind::Software)
        .map(|prefix| prefix.key.as_str())
}

fn footer(keys: &[(&str, &str)]) -> gtk::Box {
    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .css_classes(["spotlight-footer"])
        .build();

    for (keycap, description) in keys.iter().copied() {
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

/// Builds the widget for one Markdown block.
///
/// Every text run goes through `markdown::inline_markup`, which escapes XML —
/// so a reply containing `<span …>` renders as those characters rather than
/// becoming real Pango markup.
fn widget_for_block(block: &markdown::Block) -> gtk::Widget {
    match block {
        markdown::Block::Heading { level, text } => {
            let class = match level {
                1 => "spotlight-chat-h1",
                2 => "spotlight-chat-h2",
                _ => "spotlight-chat-h3",
            };
            markup_label(text, &["spotlight-chat-heading", class]).upcast()
        }
        markdown::Block::Paragraph(text) => markup_label(text, &["spotlight-chat-text"]).upcast(),
        markdown::Block::Quote(text) => {
            markup_label(text, &["spotlight-chat-text", "spotlight-chat-quote"]).upcast()
        }
        markdown::Block::Bullet { indent, text } => {
            list_row("•", *indent, text, &["spotlight-chat-text"]).upcast()
        }
        markdown::Block::Numbered {
            indent,
            marker,
            text,
        } => list_row(
            &format!("{marker}."),
            *indent,
            text,
            &["spotlight-chat-text"],
        )
        .upcast(),
        markdown::Block::Code { body, .. } => code_block(body).upcast(),
        markdown::Block::Rule => gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["spotlight-chat-rule"])
            .build()
            .upcast(),
    }
}

fn markup_label(text: &str, css_classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .css_classes(css_classes.to_vec())
        .build();
    label.set_markup(&markdown::inline_markup(text));
    label.set_focus_on_click(false);
    label
}

/// A bullet or number in its own column so wrapped text stays aligned under
/// the text, not under the marker.
fn list_row(marker: &str, indent: usize, text: &str, css_classes: &[&str]) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start((indent as i32).min(8) * 3)
        .css_classes(["spotlight-chat-list-row"])
        .build();
    row.append(
        &gtk::Label::builder()
            .label(marker)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .css_classes(["spotlight-chat-bullet"])
            .build(),
    );

    let body = markup_label(text, css_classes);
    body.set_hexpand(true);
    row.append(&body);
    row
}

/// Code scrolls horizontally rather than wrapping — character-wrapped code is
/// unreadable, and letting it size the card would blow the layout out.
fn code_block(body: &str) -> gtk::ScrolledWindow {
    let label = gtk::Label::builder()
        .label(body)
        .xalign(0.0)
        .wrap(false)
        .selectable(true)
        .css_classes(["spotlight-chat-code-text"])
        .build();
    label.set_focus_on_click(false);

    gtk::ScrolledWindow::builder()
        .child(&label)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .css_classes(["spotlight-chat-code"])
        .build()
}

/// Generous slack: a partly-rendered final line should still count as "at the
/// bottom", or a fast stream would keep unsticking itself.
/// Longest tool output echoed into the transcript. The full text still goes to
/// the model; this is only what the user sees, and a 200 KB file pasted into the
/// chat helps nobody.
const MAX_TOOL_NOTE: usize = 300;

/// A one-line summary of tool output for the transcript.
fn summarize(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_TOOL_NOTE {
        return flat;
    }
    let head = flat.chars().take(MAX_TOOL_NOTE).collect::<String>();
    format!("{head}…")
}

const AUTOSCROLL_SLACK: f64 = 48.0;

fn is_at_bottom(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - AUTOSCROLL_SLACK
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

    if let Some(trailing) = &result.trailing_icon {
        let trailing = icons::image_for(trailing, result.trailing_icon_size);
        trailing.add_css_class("spotlight-row-trailing-icon");
        trailing.set_valign(gtk::Align::Center);
        content.append(&trailing);
    }

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
