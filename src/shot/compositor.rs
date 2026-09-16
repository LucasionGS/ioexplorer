//! What is on screen at the moment of the freeze: which windows are visible,
//! where they are, which one is on top, and what has focus.
//!
//! Wayland gives an ordinary client no way to see another client's windows, so
//! this goes through the compositor's own control interface, the same way the
//! spotlight window switcher does. The answer is only needed once per shot, so
//! the queries are plain blocking calls made while the capture runs.

use std::process::Command;

use serde::Deserialize;

use super::geometry::{Point, Rect};
use crate::spotlight::windows::{self, Compositor};

/// Cap on a compositor's reply, so a runaway control tool cannot be read into
/// memory without bound.
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// A window that can be seen at the moment of the freeze.
#[derive(Clone, Debug, PartialEq)]
pub struct VisibleWindow {
    pub title: String,
    pub app_id: String,
    /// Global logical coordinates, excluding the compositor's border.
    pub rect: Rect,
    pub focused: bool,
}

impl VisibleWindow {
    /// A short human name, for the hover label.
    pub fn name(&self) -> &str {
        match self.app_id.trim().is_empty() {
            true => self.title.as_str(),
            false => self.app_id.as_str(),
        }
    }
}

/// Everything the tool needs to know about the desktop's arrangement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// Visible windows, **topmost first**, so the first hit is the one the
    /// pointer is actually over.
    pub windows: Vec<VisibleWindow>,
    /// Connector name of the output the compositor considers focused.
    pub focused_output: Option<String>,
    /// Where the pointer was when the shot was triggered.
    pub cursor: Option<Point>,
}

impl Scene {
    pub fn window_at(&self, point: Point) -> Option<&VisibleWindow> {
        self.windows
            .iter()
            .find(|window| window.rect.contains(point))
    }

    pub fn focused_window(&self) -> Option<&VisibleWindow> {
        self.windows.iter().find(|window| window.focused)
    }
}

/// Asks the running compositor for the scene. Never fails: a compositor this
/// does not know, or one that refuses, yields an empty scene, which still
/// supports region, screen and all-screen captures.
pub fn query() -> Scene {
    let compositor = windows::detect();
    let result = match compositor {
        Compositor::Hyprland => query_hyprland(),
        Compositor::Sway => query_sway(),
        Compositor::Unsupported => Ok(Scene::default()),
    };

    result.unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot read the window layout; window picking is unavailable");
        Scene::default()
    })
}

fn query_hyprland() -> Result<Scene, String> {
    let clients = capture("hyprctl", &["-j", "clients"])?;
    let monitors = capture("hyprctl", &["-j", "monitors"])?;
    let cursor = capture("hyprctl", &["-j", "cursorpos"]).unwrap_or_default();
    Ok(parse_hyprland(&clients, &monitors, &cursor))
}

fn query_sway() -> Result<Scene, String> {
    let tree = capture("swaymsg", &["-t", "get_tree", "-r"])?;
    let outputs = capture("swaymsg", &["-t", "get_outputs", "-r"]).unwrap_or_default();
    Ok(parse_sway(&tree, &outputs))
}

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
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    at: [f64; 2],
    #[serde(default)]
    size: [f64; 2],
    #[serde(default)]
    workspace: HyprWorkspaceRef,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    pinned: bool,
    /// `0` none, `1` maximised, `2` fullscreen. Anything non-zero covers the
    /// tiled layer.
    #[serde(default)]
    fullscreen: i64,
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    /// `0` is the focused window, ascending away from it.
    #[serde(default = "never_focused", rename = "focusHistoryID")]
    focus_history_id: i64,
}

fn never_focused() -> i64 {
    i64::MAX
}

#[derive(Default, Deserialize)]
struct HyprWorkspaceRef {
    #[serde(default)]
    id: i64,
}

#[derive(Deserialize)]
struct HyprMonitor {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "activeWorkspace")]
    active_workspace: HyprWorkspaceRef,
    #[serde(default, rename = "specialWorkspace")]
    special_workspace: HyprWorkspaceRef,
    #[serde(default)]
    focused: bool,
}

#[derive(Deserialize)]
struct HyprCursor {
    x: f64,
    y: f64,
}

/// Turns `hyprctl -j clients`, `monitors` and `cursorpos` into a [`Scene`].
///
/// Hyprland does not report stacking order, so it is reconstructed from what
/// does determine it: a special workspace draws over the regular one, a
/// fullscreen window over everything on its workspace, floating windows over
/// tiled ones, and among floating windows the more recently focused is raised.
/// That matches what is on screen in every case short of a floating window
/// that was raised without being focused, which Hyprland does not do on its own.
pub fn parse_hyprland(clients_json: &str, monitors_json: &str, cursor_json: &str) -> Scene {
    let monitors: Vec<HyprMonitor> = serde_json::from_str(monitors_json).unwrap_or_default();
    let clients: Vec<HyprClient> = match serde_json::from_str(clients_json) {
        Ok(clients) => clients,
        Err(error) => {
            tracing::warn!(%error, "cannot parse the Hyprland window list");
            Vec::new()
        }
    };

    let is_special = |id: i64| {
        monitors
            .iter()
            .any(|monitor| monitor.special_workspace.id != 0 && monitor.special_workspace.id == id)
    };
    let is_visible = |id: i64| {
        is_special(id)
            || monitors
                .iter()
                .any(|monitor| monitor.active_workspace.id == id)
    };

    let mut visible: Vec<(HyprClient, (u8, u8, i64))> = clients
        .into_iter()
        .filter(|client| client.mapped && !client.hidden)
        .filter(|client| client.size[0] > 0.0 && client.size[1] > 0.0)
        .filter(|client| client.pinned || is_visible(client.workspace.id))
        .map(|client| {
            // Lower sorts first, which is topmost.
            let workspace_layer = if is_special(client.workspace.id) {
                0
            } else {
                1
            };
            let window_layer = match (client.fullscreen != 0, client.pinned, client.floating) {
                (true, _, _) => 0,
                (false, true, _) => 1,
                (false, false, true) => 2,
                (false, false, false) => 3,
            };
            let key = (workspace_layer, window_layer, client.focus_history_id);
            (client, key)
        })
        .collect();
    visible.sort_by_key(|(_, key)| *key);

    let windows = visible
        .into_iter()
        .map(|(client, _)| VisibleWindow {
            rect: Rect::new(client.at[0], client.at[1], client.size[0], client.size[1]),
            focused: client.focus_history_id == 0,
            title: client.title,
            app_id: client.class,
        })
        .collect();

    let cursor = serde_json::from_str::<HyprCursor>(cursor_json)
        .ok()
        .map(|cursor| Point::new(cursor.x, cursor.y));

    Scene {
        windows,
        focused_output: monitors
            .iter()
            .find(|monitor| monitor.focused)
            .map(|monitor| monitor.name.clone()),
        cursor,
    }
}

// -- sway ------------------------------------------------------------------

#[derive(Deserialize)]
struct SwayNode {
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
    visible: Option<bool>,
    #[serde(default)]
    fullscreen_mode: i64,
    #[serde(default)]
    rect: SwayRect,
    #[serde(default)]
    window_properties: Option<SwayWindowProperties>,
    #[serde(default)]
    nodes: Vec<SwayNode>,
    #[serde(default)]
    floating_nodes: Vec<SwayNode>,
}

#[derive(Default, Deserialize)]
struct SwayRect {
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    #[serde(default)]
    width: f64,
    #[serde(default)]
    height: f64,
}

#[derive(Deserialize)]
struct SwayWindowProperties {
    #[serde(default)]
    class: Option<String>,
}

#[derive(Deserialize)]
struct SwayOutput {
    #[serde(default)]
    name: String,
    #[serde(default)]
    focused: bool,
}

/// Turns `swaymsg -t get_tree` and `get_outputs` into a [`Scene`].
///
/// Untested against a live sway — this project runs under Hyprland — but it
/// relies only on the documented `visible` flag and `rect`, both of which are
/// already in global layout coordinates.
pub fn parse_sway(tree_json: &str, outputs_json: &str) -> Scene {
    let Ok(root) = serde_json::from_str::<SwayNode>(tree_json) else {
        tracing::warn!("cannot parse the sway window tree");
        return Scene::default();
    };

    // (window, layer) — layer 0 fullscreen, 1 floating, 2 tiled.
    let mut found = Vec::new();
    collect_sway(&root, false, &mut found);
    found.sort_by_key(|(_, layer)| *layer);

    let outputs: Vec<SwayOutput> = serde_json::from_str(outputs_json).unwrap_or_default();

    Scene {
        windows: found.into_iter().map(|(window, _)| window).collect(),
        focused_output: outputs
            .iter()
            .find(|output| output.focused)
            .map(|output| output.name.clone()),
        cursor: None,
    }
}

fn collect_sway(node: &SwayNode, floating: bool, found: &mut Vec<(VisibleWindow, u8)>) {
    if node.pid.is_some()
        && node.visible.unwrap_or(false)
        && node.rect.width > 0.0
        && node.rect.height > 0.0
    {
        let class = node
            .window_properties
            .as_ref()
            .and_then(|properties| properties.class.clone());
        let layer = match (node.fullscreen_mode != 0, floating) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => 2,
        };
        found.push((
            VisibleWindow {
                title: node.name.clone().unwrap_or_default(),
                app_id: node.app_id.clone().or(class).unwrap_or_default(),
                rect: Rect::new(node.rect.x, node.rect.y, node.rect.width, node.rect.height),
                focused: node.focused,
            },
            layer,
        ));
    }

    let floating = floating || node.kind == "floating_con";
    for child in &node.nodes {
        collect_sway(child, floating, found);
    }
    for child in &node.floating_nodes {
        collect_sway(child, true, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITORS: &str = r#"[
      { "name": "DP-1", "activeWorkspace": { "id": 1 }, "specialWorkspace": { "id": 0 }, "focused": false },
      { "name": "DP-2", "activeWorkspace": { "id": 2 }, "specialWorkspace": { "id": -98 }, "focused": true }
    ]"#;

    fn client(
        title: &str,
        workspace: i64,
        at: [i32; 2],
        size: [i32; 2],
        floating: bool,
        focus: i64,
    ) -> String {
        format!(
            r#"{{ "mapped": true, "hidden": false, "at": [{}, {}], "size": [{}, {}],
                 "workspace": {{ "id": {workspace} }}, "floating": {floating}, "pinned": false,
                 "fullscreen": 0, "class": "app", "title": "{title}", "focusHistoryID": {focus} }}"#,
            at[0], at[1], size[0], size[1]
        )
    }

    fn clients(entries: &[String]) -> String {
        format!("[{}]", entries.join(","))
    }

    fn titles(scene: &Scene) -> Vec<&str> {
        scene
            .windows
            .iter()
            .map(|window| window.title.as_str())
            .collect()
    }

    #[test]
    fn windows_on_hidden_workspaces_are_not_pickable() {
        let json = clients(&[
            client("shown", 1, [0, 0], [100, 100], false, 1),
            client("elsewhere", 5, [0, 0], [100, 100], false, 0),
        ]);
        let scene = parse_hyprland(&json, MONITORS, "");

        assert_eq!(titles(&scene), vec!["shown"]);
    }

    #[test]
    fn floating_windows_stack_above_tiled_ones() {
        let json = clients(&[
            client("tiled", 1, [0, 0], [800, 600], false, 0),
            client("floating", 1, [100, 100], [200, 200], true, 3),
        ]);
        let scene = parse_hyprland(&json, MONITORS, "");

        assert_eq!(titles(&scene), vec!["floating", "tiled"]);
        let hit = scene.window_at(Point::new(150.0, 150.0)).expect("a window");
        assert_eq!(hit.title, "floating");
        let tiled = scene.window_at(Point::new(700.0, 500.0)).expect("a window");
        assert_eq!(tiled.title, "tiled");
    }

    #[test]
    fn the_more_recently_focused_floating_window_is_on_top() {
        let json = clients(&[
            client("older", 1, [0, 0], [300, 300], true, 4),
            client("newer", 1, [100, 100], [300, 300], true, 2),
        ]);
        let scene = parse_hyprland(&json, MONITORS, "");

        assert_eq!(titles(&scene), vec!["newer", "older"]);
    }

    #[test]
    fn an_open_special_workspace_covers_the_regular_one() {
        let json = clients(&[
            client("regular", 2, [0, 0], [800, 600], true, 0),
            client("scratchpad", -98, [0, 0], [800, 600], false, 1),
        ]);
        let scene = parse_hyprland(&json, MONITORS, "");

        assert_eq!(titles(&scene), vec!["scratchpad", "regular"]);
    }

    #[test]
    fn fullscreen_beats_floating() {
        let full = r#"{ "mapped": true, "at": [0, 0], "size": [1920, 1080], "workspace": { "id": 1 },
                       "floating": false, "fullscreen": 2, "class": "mpv", "title": "video", "focusHistoryID": 5 }"#;
        let json = clients(&[
            client("floating", 1, [10, 10], [200, 200], true, 0),
            full.to_string(),
        ]);
        let scene = parse_hyprland(&json, MONITORS, "");

        assert_eq!(titles(&scene), vec!["video", "floating"]);
    }

    #[test]
    fn focus_output_and_cursor_are_read() {
        let json = clients(&[client("focused", 1, [0, 0], [10, 10], false, 0)]);
        let scene = parse_hyprland(&json, MONITORS, r#"{ "x": 1500, "y": 20 }"#);

        assert_eq!(scene.focused_output.as_deref(), Some("DP-2"));
        assert_eq!(scene.cursor, Some(Point::new(1500.0, 20.0)));
        assert_eq!(
            scene.focused_window().map(|window| window.title.as_str()),
            Some("focused")
        );
    }

    #[test]
    fn unmapped_hidden_and_zero_sized_clients_are_dropped() {
        let json = r#"[
          { "mapped": false, "at": [0,0], "size": [10,10], "workspace": { "id": 1 }, "title": "a" },
          { "mapped": true, "hidden": true, "at": [0,0], "size": [10,10], "workspace": { "id": 1 }, "title": "b" },
          { "mapped": true, "at": [0,0], "size": [0,0], "workspace": { "id": 1 }, "title": "c" }
        ]"#;

        assert!(parse_hyprland(json, MONITORS, "").windows.is_empty());
    }

    #[test]
    fn malformed_replies_yield_an_empty_scene() {
        let scene = parse_hyprland("nope", "nope", "nope");
        assert_eq!(scene, Scene::default());
        assert_eq!(parse_sway("nope", "nope"), Scene::default());
    }

    #[test]
    fn sway_visible_leaves_are_collected_floating_first() {
        let tree = r#"{
          "type": "root", "nodes": [
            { "type": "output", "name": "DP-1", "nodes": [
              { "type": "workspace", "name": "1", "nodes": [
                { "type": "con", "name": "editor", "app_id": "foot", "pid": 1, "visible": true,
                  "focused": true, "rect": { "x": 0, "y": 0, "width": 960, "height": 1080 } },
                { "type": "con", "name": "hidden tab", "app_id": "foot", "pid": 2, "visible": false,
                  "rect": { "x": 0, "y": 0, "width": 960, "height": 1080 } }
              ], "floating_nodes": [
                { "type": "floating_con", "name": "Steam", "pid": 3, "visible": true,
                  "window_properties": { "class": "steam" },
                  "rect": { "x": 100, "y": 100, "width": 400, "height": 300 } }
              ] }
            ] }
          ] }"#;
        let outputs = r#"[{ "name": "DP-1", "focused": true }]"#;

        let scene = parse_sway(tree, outputs);

        assert_eq!(titles(&scene), vec!["Steam", "editor"]);
        assert_eq!(scene.windows[0].app_id, "steam");
        assert!(scene.windows[1].focused);
        assert_eq!(scene.focused_output.as_deref(), Some("DP-1"));
    }
}
