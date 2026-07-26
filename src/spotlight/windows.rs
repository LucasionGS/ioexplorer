//! Open windows: listing the apps that are already running, and switching to
//! one instead of launching a second copy.
//!
//! Enumeration and activation both go through the compositor's own control
//! interface. That is not a shortcut: Wayland deliberately gives an ordinary
//! client no way to see or focus another client's toplevels. Doing it without a
//! compositor interface means speaking `wlr-foreign-toplevel-management-v1` or
//! `ext-foreign-toplevel-list-v1` as a raw Wayland client, which is a much
//! larger dependency than shelling out to the tool the compositor ships.
//!
//! Every query runs on a worker thread. They are only a few milliseconds
//! against a local socket, but they are still *subprocesses*, and
//! `std::process::Command` cannot be given a deadline — a wedged compositor
//! would block its caller indefinitely. On a `KeyboardMode::Exclusive` layer
//! surface a blocked main loop cannot even be escaped from, so a thread is
//! cheap insurance. Staleness is the same monotonic generation counter the
//! filesystem walker uses.

use std::{
    env,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use serde::Deserialize;

/// Cap on a compositor's reply, so a runaway `hyprctl` cannot be read into
/// memory without bound. A hundred windows of JSON is well under this.
const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// Which compositor control interface this session exposes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compositor {
    Hyprland,
    Sway,
    /// Nothing this module knows how to drive. The provider stays silent rather
    /// than showing rows that could not be acted on.
    Unsupported,
}

/// Picks a backend from the environment.
///
/// Both variables are set by the compositor itself for its own clients, so this
/// is a statement about which compositor we are running under, not a guess.
pub fn detect() -> Compositor {
    if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        Compositor::Hyprland
    } else if env::var_os("SWAYSOCK").is_some() {
        Compositor::Sway
    } else {
        Compositor::Unsupported
    }
}

/// One open window, flattened out of whatever the compositor reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenWindow {
    /// Opaque token the backend uses to focus this window. Never parsed for
    /// meaning and never shown — only handed back to [`activate`].
    pub handle: String,
    pub title: String,
    /// The Wayland app id, or the X11 class when running under XWayland.
    pub app_id: String,
    /// Workspace name as the compositor spells it, e.g. `2` or `special:magic`.
    pub workspace: String,
    /// Output name, e.g. `DP-1`. Empty when the compositor did not say.
    pub monitor: String,
    pub xwayland: bool,
    pub focused: bool,
}

impl OpenWindow {
    /// What to put in a row's heading. A window with no title of its own — some
    /// splash and preference windows never set one — falls back to its app id
    /// rather than rendering as a blank row.
    pub fn heading(&self) -> &str {
        match self.title.trim().is_empty() {
            true => self.app_id.as_str(),
            false => self.title.as_str(),
        }
    }

    /// Where the window lives, as one human-readable phrase.
    pub fn location(&self) -> String {
        let mut parts = Vec::new();
        if !self.workspace.is_empty() {
            parts.push(format!("Workspace {}", self.workspace));
        }
        if !self.monitor.is_empty() {
            parts.push(self.monitor.clone());
        }
        parts.join(" · ")
    }
}

// -- listing ---------------------------------------------------------------

#[derive(Debug)]
pub enum WindowsEvent {
    Ready {
        generation: u64,
        windows: Vec<OpenWindow>,
    },
    Failed {
        generation: u64,
        error: String,
    },
}

impl WindowsEvent {
    fn generation(&self) -> u64 {
        match self {
            Self::Ready { generation, .. } | Self::Failed { generation, .. } => *generation,
        }
    }
}

/// Background source of the open-window list.
pub struct WindowSource {
    compositor: Compositor,
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<WindowsEvent>,
    receiver: mpsc::Receiver<WindowsEvent>,
}

impl WindowSource {
    pub fn new(compositor: Compositor) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            compositor,
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    pub fn compositor(&self) -> Compositor {
        self.compositor
    }

    /// Whether listing windows can work at all here.
    pub fn available(&self) -> bool {
        self.compositor != Compositor::Unsupported
    }

    /// Asks the compositor for the current window list, superseding any
    /// in-flight request.
    pub fn refresh(&self) {
        if !self.available() {
            return;
        }

        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let compositor = self.compositor;
        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);

        thread::spawn(move || {
            let event = match list(compositor) {
                Ok(windows) => WindowsEvent::Ready {
                    generation,
                    windows,
                },
                // Reported rather than swallowed: a compositor query that keeps
                // failing would otherwise leave the prefix saying it was still
                // asking, forever, with the reason only in the log.
                Err(error) => {
                    tracing::warn!(%error, "failed to list open windows");
                    WindowsEvent::Failed { generation, error }
                }
            };
            // A reply the user has already moved past is worth nothing, and
            // sending it would make the UI flicker back to stale rows.
            if counter.load(Ordering::Relaxed) == generation {
                let _ = sender.send(event);
            }
        });
    }

    /// The newest reply that is still current, if one arrived.
    ///
    /// Only the last event is returned: an older one describes a state the
    /// compositor has already left, so there is nothing to merge.
    pub fn drain(&self) -> Option<Result<Vec<OpenWindow>, String>> {
        let current = self.generation.load(Ordering::Relaxed);
        let mut latest = None;

        while let Ok(event) = self.receiver.try_recv() {
            if event.generation() == current {
                latest = Some(match event {
                    WindowsEvent::Ready { windows, .. } => Ok(windows),
                    WindowsEvent::Failed { error, .. } => Err(error),
                });
            }
        }

        latest
    }
}

/// Queries the compositor. Blocking — worker threads only.
fn list(compositor: Compositor) -> Result<Vec<OpenWindow>, String> {
    match compositor {
        Compositor::Hyprland => {
            let clients = capture("hyprctl", &["-j", "clients"])?;
            // Window records name their monitor by index, so the names have to
            // be looked up separately. A failure here is not fatal: a window
            // list without output names is still perfectly usable.
            let monitors = capture("hyprctl", &["-j", "monitors"]).unwrap_or_default();
            Ok(parse_hyprland(&clients, &monitors))
        }
        Compositor::Sway => {
            let tree = capture("swaymsg", &["-t", "get_tree", "-r"])?;
            Ok(parse_sway(&tree))
        }
        Compositor::Unsupported => Ok(Vec::new()),
    }
}

/// Runs a compositor query and returns its stdout.
fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout.len() > MAX_REPLY_BYTES {
        return Err(format!("{program} returned an implausibly large reply"));
    }

    String::from_utf8(output.stdout).map_err(|_| format!("{program} returned invalid UTF-8"))
}

// -- Hyprland --------------------------------------------------------------

#[derive(Deserialize)]
struct HyprClient {
    address: String,
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    workspace: HyprWorkspace,
    /// Index into the monitor list, or `-1` for a window on no output.
    #[serde(default = "no_monitor")]
    monitor: i64,
    #[serde(default)]
    xwayland: bool,
    /// `0` is the focused window, ascending away from it — a ready-made
    /// most-recently-used ordering.
    #[serde(default, rename = "focusHistoryID")]
    focus_history_id: i64,
}

fn no_monitor() -> i64 {
    -1
}

#[derive(Default, Deserialize)]
struct HyprWorkspace {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct HyprMonitor {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
}

/// Turns `hyprctl -j clients` into window rows, most-recently-used first.
///
/// Pure over the two JSON payloads so the whole mapping is testable without a
/// compositor running.
pub fn parse_hyprland(clients_json: &str, monitors_json: &str) -> Vec<OpenWindow> {
    let monitors: Vec<HyprMonitor> = serde_json::from_str(monitors_json).unwrap_or_default();
    let monitor_name = |id: i64| {
        monitors
            .iter()
            .find(|monitor| monitor.id == id)
            .map(|monitor| monitor.name.clone())
            .unwrap_or_default()
    };

    let mut clients: Vec<HyprClient> = match serde_json::from_str(clients_json) {
        Ok(clients) => clients,
        Err(error) => {
            tracing::warn!(%error, "cannot parse the Hyprland window list");
            return Vec::new();
        }
    };

    // Hyprland reports windows in creation order; focus history is what the
    // user actually thinks of as "the last thing I was in".
    clients.sort_by_key(|client| client.focus_history_id);

    clients
        .into_iter()
        // An unmapped client has no surface on screen yet, so there is nothing
        // to switch to. A classless, titleless one is not a window the user
        // would recognise either.
        .filter(|client| client.mapped)
        .filter(|client| !(client.class.trim().is_empty() && client.title.trim().is_empty()))
        .map(|client| OpenWindow {
            handle: client.address.clone(),
            title: client.title,
            app_id: client.class,
            workspace: client.workspace.name,
            monitor: monitor_name(client.monitor),
            xwayland: client.xwayland,
            focused: client.focus_history_id == 0,
        })
        .collect()
}

/// Whether `handle` is a Hyprland window address and nothing else.
///
/// This is a security boundary rather than a tidiness check. The handle is
/// interpolated into a Lua expression below, so a value carrying a quote could
/// otherwise close the string and run code inside the compositor. Anything that
/// is not `0x` followed by hex digits is refused outright, not escaped.
fn is_hyprland_address(handle: &str) -> bool {
    let Some(hex) = handle.strip_prefix("0x") else {
        return false;
    };
    !hex.is_empty() && hex.len() <= 16 && hex.chars().all(|byte| byte.is_ascii_hexdigit())
}

// -- Sway ------------------------------------------------------------------

#[derive(Deserialize)]
struct SwayNode {
    #[serde(default)]
    id: i64,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    window_properties: Option<SwayWindowProperties>,
    #[serde(default)]
    nodes: Vec<SwayNode>,
    #[serde(default)]
    floating_nodes: Vec<SwayNode>,
}

#[derive(Deserialize)]
struct SwayWindowProperties {
    #[serde(default)]
    class: Option<String>,
}

/// Turns `swaymsg -t get_tree` into window rows.
///
/// Untested against a live sway — this project runs under Hyprland — but the
/// walk deliberately keys off structure rather than exact field spellings: a
/// leaf carrying a `pid` is a window, whatever else the node happens to say.
pub fn parse_sway(tree_json: &str) -> Vec<OpenWindow> {
    let Ok(root) = serde_json::from_str::<SwayNode>(tree_json) else {
        tracing::warn!("cannot parse the sway window tree");
        return Vec::new();
    };

    let mut windows = Vec::new();
    collect_sway(&root, "", "", &mut windows);
    windows
}

fn collect_sway(node: &SwayNode, workspace: &str, output: &str, windows: &mut Vec<OpenWindow>) {
    // The enclosing workspace and output are only knowable on the way down, so
    // they are threaded through the recursion rather than looked up after.
    let output = match node.kind.as_str() {
        "output" => node.name.as_deref().unwrap_or_default(),
        _ => output,
    };
    let workspace = match node.kind.as_str() {
        "workspace" => node.name.as_deref().unwrap_or_default(),
        _ => workspace,
    };

    // A `pid` marks a real client surface; containers and workspaces have none.
    if let Some(_pid) = node.pid {
        let class = node
            .window_properties
            .as_ref()
            .and_then(|properties| properties.class.clone());
        // No `app_id` means it arrived through Xwayland, which reports a class.
        let xwayland = node.app_id.is_none() && class.is_some();

        windows.push(OpenWindow {
            handle: node.id.to_string(),
            title: node.name.clone().unwrap_or_default(),
            app_id: node.app_id.clone().or(class).unwrap_or_default(),
            workspace: workspace.to_string(),
            monitor: output.to_string(),
            xwayland,
            focused: node.focused,
        });
    }

    for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
        collect_sway(child, workspace, output, windows);
    }
}

/// Whether `handle` is a sway container id and nothing else. Same reasoning as
/// [`is_hyprland_address`]: it reaches a compositor as part of a criteria
/// expression, so its shape is checked rather than escaped.
fn is_sway_id(handle: &str) -> bool {
    !handle.is_empty() && handle.len() <= 19 && handle.chars().all(|byte| byte.is_ascii_digit())
}

// -- activation ------------------------------------------------------------

/// Switches to a window, moving to whichever workspace it is on.
pub fn activate(compositor: Compositor, handle: &str) -> Result<(), String> {
    match compositor {
        Compositor::Hyprland => activate_hyprland(handle),
        Compositor::Sway => activate_sway(handle),
        Compositor::Unsupported => Err("no supported compositor interface".to_string()),
    }
}

fn activate_hyprland(handle: &str) -> Result<(), String> {
    if !is_hyprland_address(handle) {
        return Err(format!("refusing to focus a malformed handle: {handle}"));
    }

    // Hyprland 0.56 replaced the flat dispatcher names with a Lua API, and which
    // form works depends on the compositor that happens to be running — nothing
    // visible from here says which. Trying the current form and then the legacy
    // one covers both without having to parse a version string, whose format is
    // itself not guaranteed.
    let lua = format!("hl.dsp.focus{{window=\"address:{handle}\"}}");
    let legacy = format!("address:{handle}");

    let mut last = None;
    for args in [
        vec!["dispatch", lua.as_str()],
        vec!["dispatch", "focuswindow", legacy.as_str()],
    ] {
        match dispatch("hyprctl", &args) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }

    Err(last.unwrap_or_else(|| "hyprctl refused to focus the window".to_string()))
}

fn activate_sway(handle: &str) -> Result<(), String> {
    if !is_sway_id(handle) {
        return Err(format!("refusing to focus a malformed handle: {handle}"));
    }

    dispatch("swaymsg", &[&format!("[con_id={handle}] focus")])
}

/// Runs a control command, treating a reported error as a failure even when the
/// process exits cleanly.
///
/// `hyprctl` prints `error: …` on stdout for a rejected dispatch. It does also
/// exit non-zero today, but the fallback chain above depends on recognising a
/// refusal precisely, so both signals are checked rather than trusting one.
fn dispatch(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reply = stdout.trim();

    if !output.status.success() || reply.to_ascii_lowercase().starts_with("error") {
        let detail = match reply.is_empty() {
            true => String::from_utf8_lossy(&output.stderr).trim().to_string(),
            false => reply.to_string(),
        };
        return Err(format!("{program} rejected the request: {detail}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a live `hyprctl -j clients`, keeping every field the parser
    /// reads plus an unmapped client and a titleless one to be filtered out.
    const CLIENTS: &str = r#"[
      {
        "address": "0x111",
        "mapped": true,
        "class": "steam",
        "title": "Steam",
        "workspace": { "id": 7, "name": "7" },
        "monitor": 3,
        "xwayland": true,
        "focusHistoryID": 4
      },
      {
        "address": "0x222",
        "mapped": true,
        "class": "discord",
        "title": "general - Discord",
        "workspace": { "id": 2, "name": "2" },
        "monitor": 1,
        "xwayland": false,
        "focusHistoryID": 0
      },
      {
        "address": "0x333",
        "mapped": false,
        "class": "kitty",
        "title": "not yet on screen",
        "workspace": { "id": 1, "name": "1" },
        "monitor": 0,
        "xwayland": false,
        "focusHistoryID": 9
      },
      {
        "address": "0x444",
        "mapped": true,
        "class": "",
        "title": "",
        "workspace": { "id": 1, "name": "1" },
        "monitor": 0,
        "xwayland": false,
        "focusHistoryID": 7
      }
    ]"#;

    const MONITORS: &str = r#"[
      { "id": 0, "name": "HDMI-A-1" },
      { "id": 1, "name": "DP-1" },
      { "id": 3, "name": "DP-3" }
    ]"#;

    #[test]
    fn hyprland_windows_are_ordered_most_recently_used_first() {
        let windows = parse_hyprland(CLIENTS, MONITORS);

        let handles = windows
            .iter()
            .map(|window| window.handle.as_str())
            .collect::<Vec<_>>();
        assert_eq!(handles, vec!["0x222", "0x111"]);
        assert!(windows[0].focused, "focusHistoryID 0 is the focused window");
        assert!(!windows[1].focused);
    }

    #[test]
    fn hyprland_unmapped_and_anonymous_clients_are_dropped() {
        let windows = parse_hyprland(CLIENTS, MONITORS);

        assert_eq!(windows.len(), 2, "the unmapped and blank clients are gone");
        assert!(!windows.iter().any(|window| window.handle == "0x333"));
        assert!(!windows.iter().any(|window| window.handle == "0x444"));
    }

    #[test]
    fn hyprland_monitor_indices_become_output_names() {
        let windows = parse_hyprland(CLIENTS, MONITORS);

        let steam = windows
            .iter()
            .find(|window| window.app_id == "steam")
            .expect("steam window");
        assert_eq!(steam.monitor, "DP-3");
        assert_eq!(steam.workspace, "7");
        assert!(steam.xwayland);
        assert_eq!(steam.location(), "Workspace 7 · DP-3");
    }

    /// The monitor list is fetched separately and is allowed to fail; losing it
    /// must cost the output name and nothing else.
    #[test]
    fn a_missing_monitor_list_still_yields_windows() {
        let windows = parse_hyprland(CLIENTS, "");

        assert_eq!(windows.len(), 2);
        assert!(windows.iter().all(|window| window.monitor.is_empty()));
        assert_eq!(windows[0].location(), "Workspace 2");
    }

    #[test]
    fn malformed_json_yields_no_windows_rather_than_panicking() {
        assert!(parse_hyprland("not json", MONITORS).is_empty());
        assert!(parse_hyprland("", "").is_empty());
        assert!(parse_hyprland("{}", "").is_empty());
        assert!(parse_sway("not json").is_empty());
    }

    /// A window that never set a title would otherwise render as a blank row.
    #[test]
    fn a_titleless_window_falls_back_to_its_app_id() {
        let window = OpenWindow {
            handle: "0x1".to_string(),
            title: "   ".to_string(),
            app_id: "org.example.Thing".to_string(),
            workspace: "1".to_string(),
            monitor: String::new(),
            xwayland: false,
            focused: false,
        };

        assert_eq!(window.heading(), "org.example.Thing");
    }

    #[test]
    fn sway_leaves_carry_their_workspace_and_output() {
        let tree = r#"{
          "id": 1, "type": "root", "nodes": [
            { "id": 2, "type": "output", "name": "DP-1", "nodes": [
              { "id": 3, "type": "workspace", "name": "3", "nodes": [
                { "id": 40, "type": "con", "name": "vim", "app_id": "foot", "pid": 900, "focused": true }
              ], "floating_nodes": [
                { "id": 41, "type": "floating_con", "name": "Steam", "pid": 901,
                  "window_properties": { "class": "steam" } }
              ] }
            ] }
          ] }"#;

        let windows = parse_sway(tree);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].handle, "40");
        assert_eq!(windows[0].app_id, "foot");
        assert_eq!(windows[0].workspace, "3");
        assert_eq!(windows[0].monitor, "DP-1");
        assert!(!windows[0].xwayland);
        assert!(windows[0].focused);

        // Floating windows are children too, and an X11 client reports a class
        // instead of an app id.
        assert_eq!(windows[1].handle, "41");
        assert_eq!(windows[1].app_id, "steam");
        assert!(windows[1].xwayland);
        assert_eq!(windows[1].workspace, "3");
    }

    #[test]
    fn only_addresses_reach_the_hyprland_dispatcher() {
        assert!(is_hyprland_address("0x55fff20193c0"));
        assert!(is_hyprland_address("0xAB"));

        assert!(!is_hyprland_address(""));
        assert!(!is_hyprland_address("0x"));
        assert!(!is_hyprland_address("55fff20193c0"));
        // The handle lands inside a Lua string literal, so a quote that could
        // close it early must never be treated as an address.
        assert!(!is_hyprland_address(r#"0x1"} os.execute("id") --"#));
        assert!(!is_hyprland_address("0x1 or true"));
        assert!(!is_hyprland_address("0xzz"));
        // Longer than any real pointer, so it is a mistake or an attack.
        assert!(!is_hyprland_address("0x00000000000000001"));
    }

    #[test]
    fn only_digits_reach_the_sway_dispatcher() {
        assert!(is_sway_id("40"));

        assert!(!is_sway_id(""));
        assert!(!is_sway_id("40] focus; [con_id=41"));
        assert!(!is_sway_id("0x40"));
        assert!(!is_sway_id("-1"));
    }

    #[test]
    fn a_malformed_handle_is_refused_before_any_process_runs() {
        let error = activate(Compositor::Hyprland, "0x1\" } --").expect_err("must refuse");
        assert!(error.contains("malformed"), "{error}");

        let error = activate(Compositor::Sway, "1; reboot").expect_err("must refuse");
        assert!(error.contains("malformed"), "{error}");
    }

    /// Every query must come back with an outcome, success or failure. Silence
    /// is indistinguishable from a reply still in flight, which is what left the
    /// prefix able to claim it was still asking indefinitely.
    ///
    /// Deliberately not asserting *which* outcome: whether `swaymsg` runs here
    /// depends on the machine, and a test that passes only off sway is worse
    /// than one that pins the invariant that actually matters.
    #[test]
    fn a_query_always_reports_an_outcome() {
        let source = WindowSource::new(Compositor::Sway);
        source.refresh();

        let reply = (0..200).find_map(|_| {
            thread::sleep(std::time::Duration::from_millis(10));
            source.drain()
        });

        assert!(reply.is_some(), "the worker reported nothing at all");
    }

    #[test]
    fn an_unsupported_compositor_lists_nothing_and_refuses_to_activate() {
        let source = WindowSource::new(Compositor::Unsupported);

        assert!(!source.available());
        source.refresh();
        assert!(source.drain().is_none());
        assert!(activate(Compositor::Unsupported, "0x1").is_err());
    }
}
