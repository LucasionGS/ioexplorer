use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    env, fs,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

use directories::{ProjectDirs, UserDirs};
use gio::prelude::*;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing_subscriber::{EnvFilter, fmt};

use crate::{config::AppConfig, theme};

const START_MENU_APP_ID: &str = "io.github.ionix.IoExplorer.StartMenu";
const START_MENU_SOCKET: &str = "start-menu.sock";
const PINNED_APP_LIMIT: usize = 12;
const SEARCH_RESULT_LIMIT: usize = 18;
const PANEL_WIDTH: i32 = 700;
const PANEL_HEIGHT: i32 = 760;

pub fn run() -> glib::ExitCode {
    init_logging();

    let args = match StartMenuArgs::parse(env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return glib::ExitCode::FAILURE;
        }
    };

    if args.server {
        return run_server();
    }

    if send_toggle_request(ToggleRequest {
        placement: args.placement,
    })
    .is_ok()
    {
        return glib::ExitCode::SUCCESS;
    }

    run_one_shot(args.placement)
}

fn run_server() -> glib::ExitCode {
    let (listener, _socket_guard) = match bind_start_menu_socket() {
        Ok(Some(listener)) => listener,
        Ok(None) => {
            tracing::info!("ioexplorer-start server already running");
            return glib::ExitCode::SUCCESS;
        }
        Err(error) => {
            tracing::error!(%error, "failed to start ioexplorer-start server");
            return glib::ExitCode::FAILURE;
        }
    };
    let receiver = spawn_server_listener(listener);

    run_application(
        LaunchMode::Server(Rc::new(RefCell::new(Some(receiver)))),
        StartPlacement::default(),
    )
}

fn run_one_shot(placement: StartPlacement) -> glib::ExitCode {
    run_application(LaunchMode::OneShot, placement)
}

fn run_application(mode: LaunchMode, placement: StartPlacement) -> glib::ExitCode {
    let argv0 = env::args()
        .next()
        .unwrap_or_else(|| "ioexplorer-start".to_string());
    let app = gtk::Application::builder()
        .application_id(START_MENU_APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_startup(|_| {
        let config = AppConfig::load();
        theme::install(&config);
    });

    app.connect_activate(move |app| {
        let window = StartMenuWindow::new(app, mode.is_server(), placement);
        if let LaunchMode::Server(receiver) = &mode
            && let Some(receiver) = receiver.borrow_mut().take()
        {
            window.install_server_listener(receiver);
        }

        if mode.is_server() {
            window.hide_menu();
        } else {
            window.show_menu(placement);
        }
    });

    app.run_with_args(&[argv0])
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

#[derive(Clone)]
enum LaunchMode {
    Server(Rc<RefCell<Option<mpsc::Receiver<ToggleRequest>>>>),
    OneShot,
}

impl LaunchMode {
    fn is_server(&self) -> bool {
        matches!(self, Self::Server(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StartMenuArgs {
    server: bool,
    placement: StartPlacement,
}

impl StartMenuArgs {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut server = false;
        let mut horizontal = None;
        let mut vertical = None;
        let mut center = false;

        for arg in args {
            match arg.as_str() {
                "--server" => server = true,
                "--left" => set_once(&mut horizontal, HorizontalPlacement::Left, "horizontal")?,
                "--right" => {
                    set_once(&mut horizontal, HorizontalPlacement::Right, "horizontal")?
                }
                "--top" => set_once(&mut vertical, VerticalPlacement::Top, "vertical")?,
                "--bottom" => set_once(&mut vertical, VerticalPlacement::Bottom, "vertical")?,
                "--center" => center = true,
                "--help" => {
                    return Err(
                        "Usage: ioexplorer-start [--server] [--left|--right|--center] [--top|--bottom|--center]"
                            .to_string(),
                    )
                }
                _ => return Err(format!("Unknown argument: {arg}")),
            }
        }

        if center {
            horizontal.get_or_insert(HorizontalPlacement::Center);
            vertical.get_or_insert(VerticalPlacement::Center);
        }

        Ok(Self {
            server,
            placement: StartPlacement {
                horizontal: horizontal.unwrap_or(HorizontalPlacement::Center),
                vertical: vertical.unwrap_or(VerticalPlacement::Bottom),
            },
        })
    }
}

fn set_once<T: Copy>(slot: &mut Option<T>, value: T, label: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        Err(format!("{label} placement specified more than once"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToggleRequest {
    placement: StartPlacement,
}

impl ToggleRequest {
    fn serialize(self) -> String {
        format!(
            "toggle {} {}\n",
            self.placement.horizontal.as_str(),
            self.placement.vertical.as_str()
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        (parts.next()? == "toggle").then_some(())?;
        let horizontal = HorizontalPlacement::parse(parts.next()?)?;
        let vertical = VerticalPlacement::parse(parts.next()?)?;
        parts.next().is_none().then_some(Self {
            placement: StartPlacement {
                horizontal,
                vertical,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StartPlacement {
    horizontal: HorizontalPlacement,
    vertical: VerticalPlacement,
}

impl Default for StartPlacement {
    fn default() -> Self {
        Self {
            horizontal: HorizontalPlacement::Center,
            vertical: VerticalPlacement::Bottom,
        }
    }
}

impl StartPlacement {
    fn halign(self) -> gtk::Align {
        match self.horizontal {
            HorizontalPlacement::Left => gtk::Align::Start,
            HorizontalPlacement::Center => gtk::Align::Center,
            HorizontalPlacement::Right => gtk::Align::End,
        }
    }

    fn valign(self) -> gtk::Align {
        match self.vertical {
            VerticalPlacement::Top => gtk::Align::Start,
            VerticalPlacement::Center => gtk::Align::Center,
            VerticalPlacement::Bottom => gtk::Align::End,
        }
    }

    fn left_anchor(self) -> bool {
        matches!(self.horizontal, HorizontalPlacement::Left)
    }

    fn right_anchor(self) -> bool {
        matches!(self.horizontal, HorizontalPlacement::Right)
    }

    fn top_anchor(self) -> bool {
        matches!(self.vertical, VerticalPlacement::Top)
    }

    fn bottom_anchor(self) -> bool {
        matches!(self.vertical, VerticalPlacement::Bottom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HorizontalPlacement {
    Left,
    Center,
    Right,
}

impl HorizontalPlacement {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "left" => Some(Self::Left),
            "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerticalPlacement {
    Top,
    Center,
    Bottom,
}

impl VerticalPlacement {
    fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Center => "center",
            Self::Bottom => "bottom",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "top" => Some(Self::Top),
            "center" => Some(Self::Center),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
}

fn bind_start_menu_socket() -> io::Result<Option<(UnixListener, SocketFileGuard)>> {
    let path = socket_path().ok_or_else(|| io::Error::other("missing socket path"))?;
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return Err(error);
    }

    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Ok(None);
        }

        let _ = fs::remove_file(&path);
    }

    let listener = UnixListener::bind(&path)?;
    Ok(Some((listener, SocketFileGuard { path })))
}

fn socket_path() -> Option<PathBuf> {
    let project_dirs = ProjectDirs::from("io.github", "ionix", "ioexplorer");
    project_dirs
        .as_ref()
        .and_then(|dirs| dirs.runtime_dir().map(|dir| dir.join(START_MENU_SOCKET)))
        .or_else(|| {
            project_dirs
                .as_ref()
                .and_then(|dirs| dirs.state_dir().map(|dir| dir.join(START_MENU_SOCKET)))
        })
        .or_else(|| Some(env::temp_dir().join("ioexplorer-start.sock")))
}

fn send_toggle_request(request: ToggleRequest) -> io::Result<()> {
    let path = socket_path().ok_or_else(|| io::Error::other("missing socket path"))?;
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(request.serialize().as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn spawn_server_listener(listener: UnixListener) -> mpsc::Receiver<ToggleRequest> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = String::new();
                    match stream.read_to_string(&mut request) {
                        Ok(_) => {
                            if let Some(request) = ToggleRequest::parse(&request) {
                                let _ = sender.send(request);
                            }
                        }
                        Err(error) => tracing::warn!(%error, "failed to read start-menu request"),
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "start-menu listener stopped");
                    break;
                }
            }
        }
    });
    receiver
}

struct SocketFileGuard {
    path: PathBuf,
}

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone)]
struct StartEntry {
    id: Option<String>,
    title: String,
    subtitle: String,
    search_text: String,
    icon: StartIcon,
    action: StartAction,
}

impl StartEntry {
    fn matches(&self, query: &str) -> bool {
        self.search_text.contains(query)
    }
}

#[derive(Clone)]
enum StartIcon {
    GIcon(gio::Icon),
    IconName(String),
}

#[derive(Clone)]
enum StartAction {
    LaunchApp(gio::AppInfo),
    OpenPath(PathBuf),
}

impl StartAction {
    fn launch(&self) -> Result<(), String> {
        match self {
            Self::LaunchApp(app) => app
                .launch(&[], None::<&gio::AppLaunchContext>)
                .map_err(|error| format!("failed to launch app: {error}")),
            Self::OpenPath(path) => launch_in_ioexplorer(path)
                .map_err(|error| format!("failed to open {}: {error}", path.display())),
        }
    }
}

struct StartMenuWindow {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    surface: gtk::Box,
    search_entry: gtk::SearchEntry,
    content_stack: gtk::Stack,
    results_box: gtk::Box,
    empty_label: gtk::Label,
    all_entries: Vec<StartEntry>,
    server_mode: bool,
    placement: Cell<StartPlacement>,
    power_menu_visible: Rc<Cell<bool>>,
}

impl StartMenuWindow {
    fn new(app: &gtk::Application, server_mode: bool, placement: StartPlacement) -> Rc<Self> {
        let all_entries = all_start_entries();
        let pinned_entries = pinned_entries(&all_entries);
        let recommended_entries = recommended_entries();

        let surface = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .width_request(PANEL_WIDTH)
            .height_request(PANEL_HEIGHT)
            .css_classes(["start-menu-surface"])
            .build();
        surface.set_halign(placement.halign());
        surface.set_valign(placement.valign());

        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search apps and folders")
            .hexpand(true)
            .css_classes(["start-menu-search"])
            .build();
        surface.append(&search_entry);

        let home_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        home_content.append(&section_header("Pinned", "Installed applications"));

        let pinned_grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(false)
            .max_children_per_line(3)
            .row_spacing(10)
            .column_spacing(10)
            .css_classes(["start-menu-grid"])
            .build();
        home_content.append(&pinned_grid);

        home_content.append(&section_header("Recommended", "Folders and places"));
        let recommended_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .build();
        home_content.append(&recommended_box);

        let home_scroll = gtk::ScrolledWindow::builder()
            .child(&home_content)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .css_classes(["start-menu-scroll"])
            .build();

        let results_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        let empty_label = gtk::Label::builder()
            .label("No results")
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .css_classes(["start-menu-empty", "dim-label"])
            .build();
        let search_scroll = gtk::ScrolledWindow::builder()
            .child(&results_box)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .css_classes(["start-menu-scroll"])
            .build();

        let content_stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .build();
        content_stack.add_named(&home_scroll, Some("home"));
        content_stack.add_named(&search_scroll, Some("search"));
        content_stack.set_visible_child_name("home");
        surface.append(&content_stack);

        let user_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        user_box.append(
            &gtk::Label::builder()
                .label(user_display_name())
                .xalign(0.0)
                .css_classes(["start-menu-user"])
                .build(),
        );
        user_box.append(
            &gtk::Label::builder()
                .label("IoExplorer Start")
                .xalign(0.0)
                .css_classes(["start-menu-user-subtitle", "dim-label"])
                .build(),
        );

        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["start-menu-footer"])
            .build();
        footer.append(&user_box);
        surface.append(&footer);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("IoExplorer Start")
            .child(&surface)
            .default_width(PANEL_WIDTH)
            .default_height(PANEL_HEIGHT)
            .build();
        window.set_decorated(false);
        window.set_resizable(false);
        window.add_css_class("start-menu-window");
        configure_start_menu_window(&window, placement);

        let this = Rc::new(Self {
            app: app.clone(),
            window,
            surface,
            search_entry,
            content_stack,
            results_box,
            empty_label,
            all_entries,
            server_mode,
            placement: Cell::new(placement),
            power_menu_visible: Rc::new(Cell::new(false)),
        });

        for entry in pinned_entries {
            pinned_grid.insert(&this.launcher_button(entry, false), -1);
        }
        for entry in recommended_entries {
            recommended_box.append(&this.launcher_button(entry, true));
        }
        footer.append(&this.power_menu_button());

        this.install_callbacks();
        this
    }

    fn install_callbacks(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.search_entry.connect_search_changed(move |_| {
            this.update_search_results();
        });

        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                this.close_menu();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(controller);

        let this = Rc::clone(self);
        self.window.connect_is_active_notify(move |window| {
            if window.is_visible() && !window.is_active() && !this.power_menu_visible.get() {
                this.close_menu();
            }
        });

        let this = Rc::clone(self);
        self.window.connect_close_request(move |_| {
            if this.server_mode {
                this.hide_menu();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    fn install_server_listener(self: &Rc<Self>, receiver: mpsc::Receiver<ToggleRequest>) {
        let this = Rc::clone(self);
        glib::timeout_add_local(Duration::from_millis(24), move || {
            while let Ok(request) = receiver.try_recv() {
                this.toggle_menu(request.placement);
            }
            glib::ControlFlow::Continue
        });
    }

    fn toggle_menu(&self, placement: StartPlacement) {
        if self.window.is_visible() {
            self.hide_menu();
        } else {
            self.show_menu(placement);
        }
    }

    fn show_menu(&self, placement: StartPlacement) {
        self.apply_placement(placement);
        self.search_entry.set_text("");
        self.update_search_results();
        self.window.present();

        let search_entry = self.search_entry.clone();
        glib::idle_add_local_once(move || {
            search_entry.grab_focus();
        });
    }

    fn hide_menu(&self) {
        self.search_entry.set_text("");
        self.update_search_results();
        self.window.set_visible(false);
    }

    fn close_menu(&self) {
        if self.server_mode {
            self.hide_menu();
        } else {
            self.app.quit();
        }
    }

    fn apply_placement(&self, placement: StartPlacement) {
        self.placement.set(placement);
        self.surface.set_halign(placement.halign());
        self.surface.set_valign(placement.valign());
        apply_layer_shell_placement(&self.window, placement);
    }

    fn update_search_results(&self) {
        let query = self.search_entry.text().trim().to_lowercase();
        if query.is_empty() {
            self.content_stack.set_visible_child_name("home");
            return;
        }

        clear_box_children(&self.results_box);
        let matches = self
            .all_entries
            .iter()
            .filter(|entry| entry.matches(&query))
            .take(SEARCH_RESULT_LIMIT)
            .cloned()
            .collect::<Vec<_>>();

        if matches.is_empty() {
            self.results_box.append(&self.empty_label);
        } else {
            for entry in matches {
                self.results_box.append(&self.launcher_button(entry, true));
            }
        }

        self.content_stack.set_visible_child_name("search");
    }

    fn launcher_button(&self, entry: StartEntry, compact: bool) -> gtk::Button {
        let button = launcher_button_content(&entry, compact);
        let app = self.app.clone();
        let window = self.window.clone();
        let server_mode = self.server_mode;
        button.connect_clicked(move |_| {
            if let Err(error) = entry.action.launch() {
                tracing::warn!(%error, "failed to launch start-menu entry");
            }
            if server_mode {
                window.set_visible(false);
            } else {
                app.quit();
            }
        });
        button
    }

    fn power_menu_button(&self) -> gtk::MenuButton {
        let menu_button = gtk::MenuButton::builder()
            .icon_name("system-shutdown-symbolic")
            .tooltip_text("Power options")
            .css_classes(["start-menu-power"])
            .build();
        menu_button.set_focusable(false);

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
            .position(gtk::PositionType::Top)
            .child(&menu)
            .css_classes(["context-menu"])
            .build();
        let power_menu_visible = Rc::clone(&self.power_menu_visible);
        popover.connect_visible_notify(move |popover| {
            power_menu_visible.set(popover.is_visible());
        });

        let power_menu_visible = Rc::clone(&self.power_menu_visible);
        let app = self.app.clone();
        let window = self.window.clone();
        let server_mode = self.server_mode;
        popover.connect_closed(move |_| {
            power_menu_visible.set(false);
            if window.is_visible() && !window.is_active() {
                if server_mode {
                    window.set_visible(false);
                } else {
                    app.quit();
                }
            }
        });

        for action in [
            PowerAction {
                label: "Sleep",
                icon_name: "weather-clear-night-symbolic",
                systemctl_verb: "suspend",
            },
            PowerAction {
                label: "Reboot",
                icon_name: "system-reboot-symbolic",
                systemctl_verb: "reboot",
            },
            PowerAction {
                label: "Shutdown",
                icon_name: "system-shutdown-symbolic",
                systemctl_verb: "poweroff",
            },
        ] {
            menu.append(&self.power_action_button(&popover, action));
        }

        menu_button.set_popover(Some(&popover));
        menu_button
    }

    fn power_action_button(&self, popover: &gtk::Popover, action: PowerAction) -> gtk::Button {
        let button = gtk::Button::builder()
            .halign(gtk::Align::Fill)
            .css_classes(["context-menu-item"])
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .halign(gtk::Align::Fill)
            .build();
        content.append(
            &gtk::Image::builder()
                .icon_name(action.icon_name)
                .pixel_size(16)
                .build(),
        );
        content.append(
            &gtk::Label::builder()
                .label(action.label)
                .xalign(0.0)
                .hexpand(true)
                .build(),
        );
        button.set_child(Some(&content));

        let popover = popover.clone();
        let app = self.app.clone();
        let window = self.window.clone();
        let server_mode = self.server_mode;
        button.connect_clicked(move |_| {
            popover.popdown();
            match launch_system_power_action(action.systemctl_verb) {
                Ok(()) => {
                    if server_mode {
                        window.set_visible(false);
                    } else {
                        app.quit();
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        verb = action.systemctl_verb,
                        %error,
                        "failed to launch start-menu power action"
                    );
                }
            }
        });

        button
    }
}

#[derive(Clone, Copy)]
struct PowerAction {
    label: &'static str,
    icon_name: &'static str,
    systemctl_verb: &'static str,
}

fn configure_start_menu_window(window: &gtk::ApplicationWindow, placement: StartPlacement) {
    if gtk4_layer_shell::is_supported() {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.set_namespace(Some("ioexplorer-start"));
        apply_layer_shell_placement(window, placement);
        window.set_exclusive_zone(0);
    } else {
        tracing::warn!("gtk4-layer-shell unsupported, falling back to popup window");
    }
}

fn apply_layer_shell_placement(window: &gtk::ApplicationWindow, placement: StartPlacement) {
    const SCREEN_MARGIN: i32 = 24;

    if !gtk4_layer_shell::is_supported() {
        return;
    }

    window.set_anchor(Edge::Top, placement.top_anchor());
    window.set_anchor(Edge::Bottom, placement.bottom_anchor());
    window.set_anchor(Edge::Left, placement.left_anchor());
    window.set_anchor(Edge::Right, placement.right_anchor());

    window.set_margin(Edge::Top, SCREEN_MARGIN);
    window.set_margin(Edge::Bottom, SCREEN_MARGIN);
    window.set_margin(Edge::Left, SCREEN_MARGIN);
    window.set_margin(Edge::Right, SCREEN_MARGIN);
}

fn section_header(title: &str, subtitle: &str) -> gtk::Box {
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["start-menu-section-header"])
        .build();
    header.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["start-menu-section-title"])
            .build(),
    );
    header.append(
        &gtk::Label::builder()
            .label(subtitle)
            .xalign(0.0)
            .css_classes(["start-menu-section-subtitle", "dim-label"])
            .build(),
    );
    header
}

fn launcher_button_content(entry: &StartEntry, compact: bool) -> gtk::Button {
    let button = gtk::Button::builder()
        .focusable(false)
        .css_classes([if compact {
            "start-menu-result"
        } else {
            "start-menu-launcher"
        }])
        .build();

    let content = gtk::Box::builder()
        .orientation(if compact {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        })
        .spacing(if compact { 12 } else { 8 })
        .hexpand(true)
        .build();
    let icon = image_for_entry(&entry.icon, if compact { 28 } else { 32 });
    icon.add_css_class("start-menu-launcher-icon");
    content.append(&icon);

    let text_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    text_box.append(
        &gtk::Label::builder()
            .label(&entry.title)
            .xalign(if compact { 0.0 } else { 0.5 })
            .justify(if compact {
                gtk::Justification::Left
            } else {
                gtk::Justification::Center
            })
            .wrap(!compact)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(if compact { 32 } else { 14 })
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["start-menu-launcher-title"])
            .build(),
    );
    text_box.append(
        &gtk::Label::builder()
            .label(&entry.subtitle)
            .xalign(if compact { 0.0 } else { 0.5 })
            .justify(if compact {
                gtk::Justification::Left
            } else {
                gtk::Justification::Center
            })
            .wrap(!compact)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["start-menu-launcher-subtitle", "dim-label"])
            .build(),
    );
    content.append(&text_box);
    button.set_child(Some(&content));
    button
}

fn image_for_entry(icon: &StartIcon, pixel_size: i32) -> gtk::Image {
    let image = match icon {
        StartIcon::GIcon(icon) => gtk::Image::from_gicon(icon),
        StartIcon::IconName(icon_name) => gtk::Image::from_icon_name(icon_name),
    };
    image.set_pixel_size(pixel_size);
    image
}

fn all_start_entries() -> Vec<StartEntry> {
    let mut seen = HashSet::new();
    let mut entries = gio::AppInfo::all()
        .into_iter()
        .filter(|app| app.should_show())
        .filter_map(|app| {
            let title = app.display_name().to_string();
            if title.trim().is_empty() {
                return None;
            }

            let id = app.id().map(|id| id.to_string());
            let key = id
                .clone()
                .unwrap_or_else(|| title.to_lowercase().replace(' ', "-"));
            seen.insert(key).then_some(())?;

            let subtitle = "Installed app".to_string();
            let search_text = format!(
                "{} {} {}",
                title.to_lowercase(),
                subtitle.to_lowercase(),
                id.as_deref().unwrap_or_default().to_lowercase()
            );

            Some(StartEntry {
                id,
                title,
                subtitle,
                search_text,
                icon: app.icon().map(StartIcon::GIcon).unwrap_or_else(|| {
                    StartIcon::IconName("application-x-executable-symbolic".to_string())
                }),
                action: StartAction::LaunchApp(app),
            })
        })
        .collect::<Vec<_>>();

    entries.sort_by_key(|entry| entry.title.to_lowercase());
    entries.extend(recommended_entries());
    entries
}

fn pinned_entries(all_entries: &[StartEntry]) -> Vec<StartEntry> {
    const PREFERRED_IDS: &[&str] = &[
        "io.github.ionix.IoExplorer",
        "org.gnome.Nautilus.desktop",
        "org.gnome.Terminal.desktop",
        "org.gnome.TextEditor.desktop",
        "org.gnome.Settings.desktop",
        "firefox.desktop",
        "org.mozilla.firefox.desktop",
        "code.desktop",
        "codium.desktop",
        "kitty.desktop",
        "org.gnome.Calculator.desktop",
        "org.gnome.Console.desktop",
    ];

    let mut pinned = Vec::new();
    for preferred in PREFERRED_IDS {
        if let Some(entry) = all_entries
            .iter()
            .find(|entry| entry.id.as_deref() == Some(*preferred))
        {
            pinned.push(entry.clone());
        }
        if pinned.len() >= PINNED_APP_LIMIT {
            return pinned;
        }
    }

    for entry in all_entries {
        if matches!(entry.action, StartAction::OpenPath(_)) {
            continue;
        }
        if pinned
            .iter()
            .any(|pinned_entry| pinned_entry.title == entry.title)
        {
            continue;
        }
        pinned.push(entry.clone());
        if pinned.len() >= PINNED_APP_LIMIT {
            break;
        }
    }
    pinned
}

fn recommended_entries() -> Vec<StartEntry> {
    let Some(user_dirs) = UserDirs::new() else {
        return Vec::new();
    };

    let mut entries = vec![folder_entry(
        "Home",
        user_dirs.home_dir().to_path_buf(),
        "user-home-symbolic",
    )];

    push_folder_entry(
        &mut entries,
        "Documents",
        user_dirs.document_dir(),
        "folder-documents-symbolic",
    );
    push_folder_entry(
        &mut entries,
        "Downloads",
        user_dirs.download_dir(),
        "folder-download-symbolic",
    );
    push_folder_entry(
        &mut entries,
        "Pictures",
        user_dirs.picture_dir(),
        "folder-pictures-symbolic",
    );
    push_folder_entry(
        &mut entries,
        "Music",
        user_dirs.audio_dir(),
        "folder-music-symbolic",
    );
    push_folder_entry(
        &mut entries,
        "Videos",
        user_dirs.video_dir(),
        "folder-videos-symbolic",
    );

    entries
}

fn push_folder_entry(
    entries: &mut Vec<StartEntry>,
    title: &str,
    path: Option<&Path>,
    icon_name: &str,
) {
    if let Some(path) = path {
        entries.push(folder_entry(title, path.to_path_buf(), icon_name));
    }
}

fn folder_entry(title: &str, path: PathBuf, icon_name: &str) -> StartEntry {
    StartEntry {
        id: Some(format!("folder:{}", path.display())),
        title: title.to_string(),
        subtitle: path.display().to_string(),
        search_text: format!("{} {}", title.to_lowercase(), path.display()),
        icon: StartIcon::IconName(icon_name.to_string()),
        action: StartAction::OpenPath(path),
    }
}

fn launch_in_ioexplorer(path: &Path) -> io::Result<()> {
    let mut child = Command::new(ioexplorer_binary()).arg(path).spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn launch_system_power_action(verb: &str) -> io::Result<()> {
    let mut child = Command::new("systemctl").arg(verb).spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn ioexplorer_binary() -> PathBuf {
    if let Some(path) = env::var_os("IOEXPLORER_APP") {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join("ioexplorer");
        if sibling.exists() {
            return sibling;
        }
    }

    PathBuf::from("ioexplorer")
}

fn user_display_name() -> String {
    env::var("USER")
        .ok()
        .filter(|user| !user.trim().is_empty())
        .unwrap_or_else(|| "User".to_string())
}

fn clear_box_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        child.unparent();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HorizontalPlacement, StartMenuArgs, StartPlacement, ToggleRequest, VerticalPlacement,
    };

    #[test]
    fn parses_default_start_menu_args() {
        let args = StartMenuArgs::parse(Vec::<String>::new()).expect("default args");

        assert!(!args.server);
        assert_eq!(args.placement, StartPlacement::default());
    }

    #[test]
    fn parses_center_top_start_menu_args() {
        let args = StartMenuArgs::parse(["--center".to_string(), "--top".to_string()])
            .expect("center top args");

        assert_eq!(
            args.placement,
            StartPlacement {
                horizontal: HorizontalPlacement::Center,
                vertical: VerticalPlacement::Top,
            }
        );
    }

    #[test]
    fn parses_center_as_both_axes_when_unpaired() {
        let args = StartMenuArgs::parse(["--center".to_string()]).expect("center args");

        assert_eq!(
            args.placement,
            StartPlacement {
                horizontal: HorizontalPlacement::Center,
                vertical: VerticalPlacement::Center,
            }
        );
    }

    #[test]
    fn round_trips_toggle_request() {
        let request = ToggleRequest {
            placement: StartPlacement {
                horizontal: HorizontalPlacement::Left,
                vertical: VerticalPlacement::Top,
            },
        };

        assert_eq!(ToggleRequest::parse(&request.serialize()), Some(request));
    }
}
