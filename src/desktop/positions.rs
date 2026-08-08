//! Which icon sits where, across restarts.
//!
//! Kept in `~/.local/state/ioexplorer/desktop-positions.toml` rather than in
//! `config.toml`: a drag rewrites this file, and the config file is watched by
//! every running ioexplorer process, so positions there would fire a full
//! config reload in all of them every time an icon moves.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
};

use serde::{Deserialize, Serialize};

use super::layout::{Cell, GridMetrics};

/// Bumped only if the schema changes incompatibly. Present from the start so a
/// future reader can tell an old file from a new one.
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct StoredPositions {
    #[serde(default)]
    pub version: u32,
    // A table, so it has to be written after every scalar above it.
    #[serde(default)]
    pub monitors: BTreeMap<String, MonitorPositions>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct MonitorPositions {
    /// What the output measured when these positions were written. Recorded for
    /// a human reading the file; the layout does not depend on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    /// The output's own snap preference. `None` means "follow the config".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap_to_grid: Option<bool>,
    // A table, so it has to be written after every scalar above it.
    #[serde(default)]
    pub icons: BTreeMap<String, StoredPosition>,
}

/// Where one icon sits, recorded twice over.
///
/// The cell is resolution-independent, so it survives a monitor swap or a panel
/// appearing. The pixel pair records a freeform placement exactly, which a cell
/// cannot. Keeping both is what makes a resolution change non-destructive: the
/// cell is re-derived against the new grid, and the pixels are only consulted
/// when the user turned snapping off.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct StoredPosition {
    pub col: i32,
    pub row: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
}

impl StoredPosition {
    pub fn snapped(cell: Cell) -> Self {
        Self {
            col: cell.col,
            row: cell.row,
            x: None,
            y: None,
        }
    }

    pub fn free(cell: Cell, x: i32, y: i32) -> Self {
        Self {
            col: cell.col,
            row: cell.row,
            x: Some(x),
            y: Some(y),
        }
    }

    pub fn cell(&self) -> Cell {
        Cell::new(self.col, self.row)
    }

    /// The pixel this position resolves to on `metrics`.
    ///
    /// A freeform position is used verbatim when snapping is off; with snapping
    /// on, or with no pixels recorded, the cell wins. Either way the result is
    /// forced inside the viewport, so a position stored on a wider output stays
    /// reachable on a narrower one.
    pub fn resolve(&self, metrics: GridMetrics, snap_to_grid: bool) -> (i32, i32) {
        match (self.x, self.y) {
            (Some(x), Some(y)) if !snap_to_grid => metrics.clamp_point(x, y),
            _ => {
                let (x, y) = metrics.cell_origin(metrics.clamp_cell(self.cell()));
                metrics.clamp_point(x, y)
            }
        }
    }
}

/// The in-memory position table, shared by every surface.
///
/// Shared rather than per-surface because the same file name can appear under
/// two outputs and must only render once: `owner_of` arbitrates, and one owner
/// means one coalesced save.
#[derive(Debug, Default)]
pub struct PositionStore {
    stored: StoredPositions,
    dirty: bool,
}

impl PositionStore {
    /// Reads the file, tolerating both absence and corruption.
    ///
    /// A malformed file yields an empty store rather than an error: refusing to
    /// start because a layout file got truncated would be a worse failure than
    /// re-flowing the icons.
    pub fn load() -> Self {
        let Some(path) = crate::state::desktop_positions_path() else {
            return Self::default();
        };

        let stored = match fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str::<StoredPositions>(&contents) {
                Ok(stored) => stored,
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to parse desktop positions, starting empty");
                    StoredPositions::default()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => StoredPositions::default(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to read desktop positions");
                StoredPositions::default()
            }
        };

        Self {
            stored,
            dirty: false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Writes the table out, atomically. A no-op when nothing changed.
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let Some(path) = crate::state::desktop_positions_path() else {
            return Ok(());
        };

        self.stored.version = SCHEMA_VERSION;
        let contents = toml::to_string_pretty(&self.stored).map_err(io::Error::other)?;
        // Atomic-by-rename, so a reader never sees a half-written layout.
        crate::config::write_atomic(&path, &contents)?;
        self.dirty = false;
        Ok(())
    }

    pub fn get(&self, monitor: &str, name: &str) -> Option<StoredPosition> {
        self.stored.monitors.get(monitor)?.icons.get(name).copied()
    }

    pub fn set(&mut self, monitor: &str, name: &str, position: StoredPosition) {
        let icons = &mut self.monitor_mut(monitor).icons;
        if icons.get(name) == Some(&position) {
            return;
        }
        icons.insert(name.to_string(), position);
        self.dirty = true;
    }

    pub fn snap_to_grid(&self, monitor: &str) -> Option<bool> {
        self.stored.monitors.get(monitor)?.snap_to_grid
    }

    pub fn set_snap_to_grid(&mut self, monitor: &str, snap: bool) {
        let entry = self.monitor_mut(monitor);
        if entry.snap_to_grid == Some(snap) {
            return;
        }
        entry.snap_to_grid = Some(snap);
        self.dirty = true;
    }

    pub fn set_geometry(&mut self, monitor: &str, geometry: String) {
        let entry = self.monitor_mut(monitor);
        if entry.geometry.as_deref() == Some(geometry.as_str()) {
            return;
        }
        entry.geometry = Some(geometry);
        self.dirty = true;
    }

    /// Forgets one icon, after its file was observed to go away.
    pub fn prune(&mut self, monitor: &str, name: &str) {
        if let Some(entry) = self.stored.monitors.get_mut(monitor)
            && entry.icons.remove(name).is_some()
        {
            self.dirty = true;
        }
    }

    /// Drops stored positions for names not in `present`.
    ///
    /// Only safe at startup against a listing that actually succeeded. Never
    /// call it on a routine reconciliation: one transient read failure would
    /// wipe the whole layout.
    pub fn prune_missing(&mut self, monitor: &str, present: &BTreeSet<String>) {
        let Some(entry) = self.stored.monitors.get_mut(monitor) else {
            return;
        };

        let before = entry.icons.len();
        entry.icons.retain(|name, _| present.contains(name));
        if entry.icons.len() != before {
            self.dirty = true;
        }
    }

    /// Carries a position across a rename, so `mv a b` — or an inline rename —
    /// leaves the icon exactly where it was.
    pub fn rename(&mut self, monitor: &str, from: &str, to: &str) {
        let Some(entry) = self.stored.monitors.get_mut(monitor) else {
            return;
        };
        let Some(position) = entry.icons.remove(from) else {
            return;
        };

        entry.icons.insert(to.to_string(), position);
        self.dirty = true;
    }

    /// Which output should render `name`.
    ///
    /// A name can legitimately be stored under two outputs — the user dragged
    /// it across, or a monitor was unplugged and its table kept — but the file
    /// exists once and must show once. First monitor in `order` wins.
    pub fn owner_of(&self, name: &str, order: &[String]) -> Option<String> {
        order
            .iter()
            .find(|monitor| {
                self.stored
                    .monitors
                    .get(*monitor)
                    .is_some_and(|entry| entry.icons.contains_key(name))
            })
            .cloned()
    }

    /// Whether any output at all — live or absent — has a position for `name`.
    ///
    /// Distinguishes "this file is new" from "this file belongs to a monitor
    /// that is currently unplugged", which want different treatment: the first
    /// gets a cell of its own, the second is only shown on loan.
    pub fn is_stored_anywhere(&self, name: &str) -> bool {
        self.stored
            .monitors
            .values()
            .any(|entry| entry.icons.contains_key(name))
    }

    /// Drops duplicate entries so every name is claimed by exactly one live
    /// output. Run at startup and whenever the monitor set changes.
    ///
    /// Outputs absent from `order` are left untouched: an unplugged monitor's
    /// layout must survive being unplugged.
    pub fn dedupe(&mut self, order: &[String]) {
        let mut claimed: BTreeSet<String> = BTreeSet::new();

        for monitor in order {
            let Some(entry) = self.stored.monitors.get_mut(monitor) else {
                continue;
            };

            let before = entry.icons.len();
            entry.icons.retain(|name, _| claimed.insert(name.clone()));
            if entry.icons.len() != before {
                self.dirty = true;
            }
        }
    }

    fn monitor_mut(&mut self, monitor: &str) -> &mut MonitorPositions {
        self.stored.monitors.entry(monitor.to_string()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::layout::GridMetrics;

    fn grid() -> GridMetrics {
        GridMetrics::new((1920, 1080), 72, 12)
    }

    fn store_with(monitor: &str, icons: &[(&str, StoredPosition)]) -> PositionStore {
        let mut store = PositionStore::default();
        for (name, position) in icons {
            store.set(monitor, name, *position);
        }
        store
    }

    /// `monitors` is a table and `icons` is a table, so each has to serialize
    /// after every scalar beside it or the write fails outright.
    #[test]
    fn a_saved_layout_round_trips() {
        let mut stored = StoredPositions {
            version: SCHEMA_VERSION,
            monitors: BTreeMap::new(),
        };
        stored.monitors.insert(
            "DP-1".to_string(),
            MonitorPositions {
                geometry: Some("3440x1440".to_string()),
                snap_to_grid: Some(false),
                icons: BTreeMap::from([
                    (
                        "Documents".to_string(),
                        StoredPosition::snapped(Cell::new(0, 0)),
                    ),
                    (
                        "notes.txt".to_string(),
                        StoredPosition::free(Cell::new(1, 3), 412, 733),
                    ),
                ]),
            },
        );

        let contents = toml::to_string_pretty(&stored).expect("serialize");
        let parsed: StoredPositions = toml::from_str(&contents).expect("parse");

        assert_eq!(parsed, stored);
    }

    #[test]
    fn a_missing_file_is_an_empty_layout_not_an_error() {
        let parsed: StoredPositions = toml::from_str("").expect("empty parses");

        assert_eq!(parsed, StoredPositions::default());
        assert!(parsed.monitors.is_empty());
    }

    #[test]
    fn a_position_without_pixels_falls_back_to_its_cell() {
        let grid = grid();
        let position = StoredPosition::snapped(Cell::new(2, 1));

        assert_eq!(
            position.resolve(grid, false),
            grid.cell_origin(Cell::new(2, 1))
        );
    }

    #[test]
    fn a_free_position_uses_its_pixels_when_snapping_is_off() {
        let grid = grid();
        let position = StoredPosition::free(Cell::new(1, 1), 400, 300);

        assert_eq!(position.resolve(grid, false), (400, 300));
    }

    /// Turning snapping back on must pull a freely-placed icon onto its cell,
    /// not leave it hovering between two.
    #[test]
    fn snapping_on_ignores_the_stored_pixels() {
        let grid = grid();
        let position = StoredPosition::free(Cell::new(1, 1), 400, 300);

        assert_eq!(
            position.resolve(grid, true),
            grid.cell_origin(Cell::new(1, 1))
        );
    }

    /// A layout made on an ultrawide has to stay reachable on a laptop panel.
    #[test]
    fn a_position_from_a_larger_output_resolves_inside_a_smaller_one() {
        let small = GridMetrics::new((1280, 720), 72, 12);
        let far_out = StoredPosition::free(Cell::new(20, 12), 3000, 1300);

        let (x, y) = far_out.resolve(small, false);

        assert!(x + small.tile_width <= 1280);
        assert!(y + small.tile_height <= 720);
    }

    #[test]
    fn setting_a_position_marks_the_store_dirty() {
        let mut store = PositionStore::default();
        assert!(!store.is_dirty());

        store.set(
            "DP-1",
            "notes.txt",
            StoredPosition::snapped(Cell::new(0, 0)),
        );

        assert!(store.is_dirty());
        assert_eq!(
            store.get("DP-1", "notes.txt"),
            Some(StoredPosition::snapped(Cell::new(0, 0)))
        );
    }

    /// Re-setting the same position must not schedule a write. The drop handler
    /// runs on every drag, including ones that end where they started.
    #[test]
    fn setting_an_unchanged_position_is_not_a_write() {
        let position = StoredPosition::snapped(Cell::new(0, 0));
        let mut store = store_with("DP-1", &[("notes.txt", position)]);
        store.flush().ok();
        store.dirty = false;

        store.set("DP-1", "notes.txt", position);

        assert!(!store.is_dirty());
    }

    #[test]
    fn a_rename_carries_the_position_across() {
        let position = StoredPosition::free(Cell::new(2, 2), 500, 400);
        let mut store = store_with("DP-1", &[("old.txt", position)]);

        store.rename("DP-1", "old.txt", "new.txt");

        assert_eq!(store.get("DP-1", "new.txt"), Some(position));
        assert_eq!(store.get("DP-1", "old.txt"), None);
    }

    #[test]
    fn renaming_something_unknown_changes_nothing() {
        let mut store = store_with(
            "DP-1",
            &[("notes.txt", StoredPosition::snapped(Cell::new(0, 0)))],
        );
        store.dirty = false;

        store.rename("DP-1", "absent.txt", "new.txt");

        assert!(!store.is_dirty());
        assert_eq!(store.get("DP-1", "new.txt"), None);
    }

    #[test]
    fn pruning_forgets_a_deleted_icon() {
        let mut store = store_with(
            "DP-1",
            &[("gone.txt", StoredPosition::snapped(Cell::new(0, 0)))],
        );

        store.prune("DP-1", "gone.txt");

        assert_eq!(store.get("DP-1", "gone.txt"), None);
    }

    #[test]
    fn a_startup_prune_drops_only_what_is_absent() {
        let mut store = store_with(
            "DP-1",
            &[
                ("kept.txt", StoredPosition::snapped(Cell::new(0, 0))),
                ("gone.txt", StoredPosition::snapped(Cell::new(0, 1))),
            ],
        );

        store.prune_missing("DP-1", &BTreeSet::from(["kept.txt".to_string()]));

        assert!(store.get("DP-1", "kept.txt").is_some());
        assert_eq!(store.get("DP-1", "gone.txt"), None);
    }

    #[test]
    fn the_first_monitor_in_order_owns_a_duplicated_name() {
        let mut store = PositionStore::default();
        let position = StoredPosition::snapped(Cell::new(0, 0));
        store.set("DP-1", "shared.txt", position);
        store.set("HDMI-A-1", "shared.txt", position);

        let order = vec!["DP-1".to_string(), "HDMI-A-1".to_string()];

        assert_eq!(
            store.owner_of("shared.txt", &order),
            Some("DP-1".to_string())
        );
    }

    #[test]
    fn dedupe_leaves_one_claim_per_name() {
        let mut store = PositionStore::default();
        let position = StoredPosition::snapped(Cell::new(0, 0));
        store.set("DP-1", "shared.txt", position);
        store.set("HDMI-A-1", "shared.txt", position);
        store.set("HDMI-A-1", "own.txt", position);

        store.dedupe(&["DP-1".to_string(), "HDMI-A-1".to_string()]);

        assert!(store.get("DP-1", "shared.txt").is_some());
        assert_eq!(store.get("HDMI-A-1", "shared.txt"), None);
        assert!(store.get("HDMI-A-1", "own.txt").is_some());
    }

    /// Unplugging a monitor must not cost the user its layout.
    #[test]
    fn dedupe_leaves_an_absent_output_untouched() {
        let mut store = PositionStore::default();
        let position = StoredPosition::snapped(Cell::new(0, 0));
        store.set("DP-1", "shared.txt", position);
        store.set("UNPLUGGED-1", "shared.txt", position);

        store.dedupe(&["DP-1".to_string()]);

        assert!(store.get("UNPLUGGED-1", "shared.txt").is_some());
    }

    #[test]
    fn an_output_snap_preference_overrides_the_config_default() {
        let mut store = PositionStore::default();
        assert_eq!(store.snap_to_grid("DP-1"), None);

        store.set_snap_to_grid("DP-1", false);

        assert_eq!(store.snap_to_grid("DP-1"), Some(false));
        assert!(store.is_dirty());
    }
}
