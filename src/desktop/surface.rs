//! One layer-shell surface: the desktop as it appears on a single output.

use std::{
    cell::{Cell as CellFlag, RefCell},
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    rc::Rc,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::{
    config::{CustomActionConfig, DesktopConfig},
    custom_actions::ActionTarget,
    file_ops::FileClipboardOperation,
    providers::{FileItem, FileKind, local::LocalProvider},
    ui::{
        context_menu::{self, MenuAction},
        views::thumbnail::ThumbnailCache,
    },
};

use super::{
    entries,
    layout::{Cell, GridMetrics, first_free_cells},
    menu::DesktopContext,
    positions::{PositionStore, StoredPosition},
    tiles::{self, Tile},
};

/// Whether an output should render a given file, and whether it may record
/// where it put it.
///
/// A file exists once, so it must appear on exactly one desktop even though
/// every output lists the same folder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Claim {
    /// This output owns it. Moves are saved against this output.
    Owned,
    /// Shown here only because the output that owns it is unplugged. Placed,
    /// but never written back — plugging the monitor in again must restore the
    /// layout the user made on it.
    Orphan,
    /// Another live output owns it.
    NotOurs,
}

/// Cancels a pending one-shot timer, if it has not already fired.
///
/// The slot is cleared by the timer's own closure as it runs, so anything still
/// in here is genuinely pending. Removing an already-fired source raises a GLib
/// error that `SourceId::remove` unwraps — inside a C callback, which cannot
/// unwind, so it takes the whole process down rather than panicking.
fn cancel(slot: &Rc<RefCell<Option<glib::SourceId>>>) {
    if let Some(timer) = slot.borrow_mut().take() {
        timer.remove();
    }
}

/// Normalises a drag into a top-left origin plus a size, so a rubber band
/// dragged up or left is the same rectangle as one dragged down or right.
fn rect(start_x: f64, start_y: f64, offset_x: f64, offset_y: f64) -> (f64, f64, f64, f64) {
    (
        start_x.min(start_x + offset_x),
        start_y.min(start_y + offset_y),
        offset_x.abs(),
        offset_y.abs(),
    )
}

/// Configures the window as a full-output desktop backdrop.
///
/// Three choices differ from the spotlight overlay next door, all deliberately:
///
/// * `Layer::Bottom`, not `Background`. Wallpaper daemons (`swaybg`,
///   `hyprpaper`, `swww`) sit on `Background`, and the relative order of two
///   surfaces on the *same* layer is compositor-defined — roughly creation
///   order. `Bottom` puts the icons above the wallpaper whichever starts first,
///   while still staying below panels and every ordinary window.
/// * `KeyboardMode::OnDemand`. `Exclusive` would hold focus forever and make the
///   session unusable, so this is the only sane request. Be aware it buys
///   nothing on some compositors: measured on Hyprland 0.56.2, an identical
///   surface receives key events on `Layer::Top` but *none* on `Layer::Bottom`
///   — on-demand focus is only granted to the upper layers. Correct stacking
///   matters more to a desktop than bare shortcuts do, so the desktop stays on
///   `Bottom` and everything is reachable by mouse and context menu instead;
///   anything that needs typing (rename) opens a short-lived `Layer::Overlay`
///   surface with `KeyboardMode::Exclusive`, which can hold focus. The request
///   is left in place because compositors that do honour it cost us nothing.
/// * `exclusive_zone(0)`, not `-1`. `-1` means "ignore everyone else's
///   reservations", which lands icons *underneath* a panel where they can never
///   be clicked. `0` reserves nothing but respects what others reserved, so the
///   compositor hands us the usable area and the grid re-flows when a bar
///   appears or resizes.
///
/// Returns false when the compositor has no layer-shell support.
pub fn configure_layer_shell(
    window: &gtk::ApplicationWindow,
    monitor: &gdk::Monitor,
    respect_panels: bool,
) -> bool {
    if !gtk4_layer_shell::is_supported() {
        tracing::warn!("gtk4-layer-shell unsupported, falling back to a plain window");
        return false;
    }

    window.init_layer_shell();
    window.set_namespace(Some("ioexplorer-desktop"));
    window.set_monitor(Some(monitor));
    window.set_layer(Layer::Bottom);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    // All four edges, so the compositor stretches the surface to the output.
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
        window.set_margin(edge, 0);
    }
    window.set_exclusive_zone(if respect_panels { 0 } else { -1 });

    true
}

pub struct DesktopSurface {
    pub window: gtk::ApplicationWindow,
    monitor_key: String,
    folder: PathBuf,
    provider: LocalProvider,
    thumbnails: ThumbnailCache,
    positions: Rc<RefCell<PositionStore>>,
    config: RefCell<DesktopConfig>,

    fixed: gtk::Fixed,
    /// Absent until the compositor has configured the surface — see
    /// [`Self::set_viewport`].
    metrics: CellFlag<Option<GridMetrics>>,

    /// Everything on screen, keyed by file name rather than listing index so a
    /// reload cannot re-point a tile at a different file.
    tiles: RefCell<HashMap<String, Tile>>,
    items: RefCell<HashMap<String, FileItem>>,
    /// Listing order, for placing a batch of new icons predictably.
    order: RefCell<Vec<String>>,
    occupied: RefCell<BTreeSet<Cell>>,

    /// Selected file names. Keyed by name for the same reason the tiles are:
    /// an index-based selection silently re-points when the folder changes.
    selection: RefCell<BTreeSet<String>>,
    /// Where a Shift-click range starts from.
    anchor: RefCell<Option<String>>,

    overlay: gtk::Overlay,
    rubberband: gtk::Box,
    rubberband_start: CellFlag<Option<(f64, f64)>>,
    /// Suppresses the click that ends a rubber-band drag, which would otherwise
    /// immediately clear the selection the drag just made.
    suppress_clear: CellFlag<bool>,
    /// Where inside a tile the current drag was grabbed, so the icon does not
    /// jump to put its top-left under the cursor.
    ///
    /// Shared between surfaces: a drag that crosses to another screen is
    /// completed by *that* screen's surface, which never saw the press and
    /// would otherwise place the icon with its corner under the cursor.
    grab_offset: Rc<CellFlag<(f64, f64)>>,

    toast: gtk::Label,
    /// Shared with the timeout closure so it can forget itself as it fires.
    /// `SourceId::remove` aborts the process when the source has already run,
    /// and these are one-shot timers, so a stale id must never be kept.
    toast_timer: Rc<RefCell<Option<glib::SourceId>>>,

    /// User-defined actions, from `AppConfig::actions` rather than the desktop
    /// section — they are shared with the file manager's menus.
    custom_action_configs: RefCell<Vec<CustomActionConfig>>,

    /// Live outputs in enumeration order, shared with every other surface.
    /// Ownership of an unclaimed file goes to the first entry, so which desktop
    /// a new file lands on is stable rather than a race between surfaces.
    monitor_order: Rc<RefCell<Vec<String>>>,
    /// Names shown on loan from an absent output — rendered, never saved.
    orphans: RefCell<BTreeSet<String>>,
    /// Brings every surface back in line with the shared state — the position
    /// table and the global hidden flag. Set by the owning app once all of them
    /// exist, since a surface has no other way to reach its siblings.
    broadcast: RefCell<Option<Rc<dyn Fn()>>>,

    folder_monitor: RefCell<Option<gio::FileMonitor>>,
    pending_reload: CellFlag<bool>,
    /// Shared with its closure for the same reason as `toast_timer`.
    save_timer: Rc<RefCell<Option<glib::SourceId>>>,
}

/// How long the folder monitor coalesces events before re-listing. Matches the
/// file manager's, and is applied twice over — gio's own rate limit plus this —
/// because an unpack or a bulk copy produces a burst per file.
const RELOAD_DEBOUNCE_MS: u32 = 250;
/// How long a drag settles before the layout is written. Long enough that
/// dragging several icons in a row is one write, short enough to survive a
/// crash moments later.
const SAVE_DEBOUNCE_MS: u32 = 750;
/// How long a toast stays up.
const TOAST_MS: u32 = 3000;
/// Pointer travel before a press counts as a rubber band rather than a click.
const RUBBERBAND_THRESHOLD: f64 = 3.0;

/// Everything a surface needs that is the same for every output.
///
/// Grouped rather than passed one by one: only the monitor and its key differ
/// between surfaces, and threading eight identical arguments through each call
/// obscured which two were the interesting ones.
pub struct SurfaceContext {
    pub folder: PathBuf,
    pub config: DesktopConfig,
    pub custom_action_configs: Vec<CustomActionConfig>,
    pub monitor_order: Rc<RefCell<Vec<String>>>,
    pub grab_offset: Rc<CellFlag<(f64, f64)>>,
    pub positions: Rc<RefCell<PositionStore>>,
    pub thumbnails: ThumbnailCache,
    pub windowed: bool,
}

impl DesktopSurface {
    pub fn new(
        app: &gtk::Application,
        monitor: &gdk::Monitor,
        monitor_key: String,
        context: SurfaceContext,
    ) -> Rc<Self> {
        let SurfaceContext {
            folder,
            config,
            custom_action_configs,
            monitor_order,
            grab_offset,
            positions,
            thumbnails,
            windowed,
        } = context;
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["desktop-window"])
            .build();

        if windowed || !configure_layer_shell(&window, monitor, config.respect_panels) {
            // Development fallback: a plain window, so the surface can be run
            // nested or under X11 without a layer-shell compositor.
            window.set_title(Some("Desktop"));
            window.set_default_size(900, 700);
        }

        // Give the window a real size request of its own, from the output it
        // is bound to.
        //
        // Without this a screen holding no icons asks for nothing: the Overlay's
        // main child is a bare DrawingArea at 0x0, and the Fixed only has a
        // natural size once it has tiles in it. The compositor then maps a
        // surface with no substance, which never becomes visible and never
        // receives a pointer — so an empty screen had no context menu and could
        // not be dropped onto, while the one screen that happened to own the
        // icons worked fine.
        let geometry = monitor.geometry();
        if geometry.width() > 0 && geometry.height() > 0 {
            window.set_default_size(geometry.width(), geometry.height());
        }

        // `gtk::Widget` has no resize signal, and a layer surface's size is only
        // known once the compositor configures it. A DrawingArea's `resize` is
        // the one reliable hook, so it exists purely to report the viewport.
        let sizer = gtk::DrawingArea::builder()
            .can_target(false)
            .hexpand(true)
            .vexpand(true)
            .build();
        // Fills the surface rather than shrinking to its tiles, so the drop
        // target and hit-testing cover the whole screen even when it is empty.
        let fixed = gtk::Fixed::builder()
            .css_classes(["desktop-surface"])
            .hexpand(true)
            .vexpand(true)
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Fill)
            .build();

        let rubberband = gtk::Box::builder()
            .css_classes(["rubberband-selection"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .visible(false)
            .can_target(false)
            .build();

        let toast = gtk::Label::builder()
            .css_classes(["desktop-toast"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .visible(false)
            .can_target(false)
            .build();

        let overlay = gtk::Overlay::builder().child(&sizer).build();
        overlay.add_overlay(&fixed);
        overlay.add_overlay(&rubberband);
        overlay.add_overlay(&toast);
        window.set_child(Some(&overlay));

        let surface = Rc::new(Self {
            window,
            monitor_key,
            folder,
            provider: LocalProvider::new(),
            thumbnails,
            positions,
            config: RefCell::new(config),
            fixed,
            metrics: CellFlag::new(None),
            tiles: RefCell::new(HashMap::new()),
            items: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            occupied: RefCell::new(BTreeSet::new()),
            selection: RefCell::new(BTreeSet::new()),
            anchor: RefCell::new(None),
            overlay,
            rubberband,
            rubberband_start: CellFlag::new(None),
            suppress_clear: CellFlag::new(false),
            grab_offset,
            toast,
            toast_timer: Rc::new(RefCell::new(None)),
            custom_action_configs: RefCell::new(custom_action_configs),
            monitor_order,
            orphans: RefCell::new(BTreeSet::new()),
            broadcast: RefCell::new(None),
            folder_monitor: RefCell::new(None),
            pending_reload: CellFlag::new(false),
            save_timer: Rc::new(RefCell::new(None)),
        });

        sizer.connect_resize({
            let surface = Rc::downgrade(&surface);
            move |_, width, height| {
                if let Some(surface) = surface.upgrade() {
                    surface.set_viewport(width, height);
                }
            }
        });

        surface.install_keyboard();
        surface.install_rubberband();
        surface.install_empty_space_menu();
        surface.install_surface_drop_target();
        surface.watch_folder();
        surface
    }

    /// Best-effort keyboard handling.
    ///
    /// Nothing here fires on Hyprland, which does not grant keyboard focus to a
    /// `Bottom`-layer surface (see [`configure_layer_shell`]); no feature is
    /// allowed to depend on it, and every operation has a menu route. It stays
    /// because compositors that *do* honour on-demand focus get F5 for free,
    /// and because the debug line tells you which situation you are in: run
    /// with `RUST_LOG=ioexplorer=debug`, click the desktop, press a key, and
    /// see whether anything arrives.
    fn install_keyboard(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let surface = Rc::downgrade(self);
            move |_, key, _, state| {
                tracing::debug!(?key, ?state, "desktop received a key press");
                let Some(surface) = surface.upgrade() else {
                    return glib::Propagation::Proceed;
                };

                match key {
                    gdk::Key::F5 => {
                        surface.reload();
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            }
        });
        self.window.add_controller(keys);
    }

    pub fn set_broadcast(&self, broadcast: Rc<dyn Fn()>) {
        *self.broadcast.borrow_mut() = Some(broadcast);
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn close(&self) {
        self.window.close();
    }

    /// Whether this output renders `name`, and whether it may save where.
    ///
    /// Every surface lists the same folder, so without this each output would
    /// draw the whole desktop and the user would see four copies of every icon.
    fn claim(&self, name: &str) -> Claim {
        let order = self.monitor_order.borrow();
        let positions = self.positions.borrow();

        if let Some(owner) = positions.owner_of(name, &order) {
            return if owner == self.monitor_key {
                Claim::Owned
            } else {
                Claim::NotOurs
            };
        }

        // Unclaimed by any live output, so it goes to the default screen. Which
        // screen that is has to be decided the same way by every surface, or
        // whichever reloaded first would win the race.
        if self.default_output(&order).as_deref() != Some(self.monitor_key.as_str()) {
            return Claim::NotOurs;
        }

        if positions.is_stored_anywhere(name) {
            // Its own output is unplugged; show it, but do not take it over.
            Claim::Orphan
        } else {
            Claim::Owned
        }
    }

    /// The screen a file with no recorded home appears on.
    ///
    /// The configured output when it is actually connected, else the first one.
    /// Falling back matters: naming a screen that is currently unplugged would
    /// otherwise leave every unplaced icon with nowhere to be drawn at all.
    fn default_output(&self, order: &[String]) -> Option<String> {
        self.config
            .borrow()
            .output
            .clone()
            .filter(|name| order.contains(name))
            .or_else(|| order.first().cloned())
    }

    /// Whether icons snap on this output: its own stored preference if it has
    /// one, else whatever the config says.
    fn snap_to_grid(&self) -> bool {
        self.positions
            .borrow()
            .snap_to_grid(&self.monitor_key)
            .unwrap_or(self.config.borrow().snap_to_grid)
    }

    /// The compositor told us how big we are. This is the entry point for all
    /// layout: nothing may be placed before it fires, or every icon lands in
    /// column 0 against a viewport that is still zero-sized.
    fn set_viewport(self: &Rc<Self>, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }

        let config = self.config.borrow();
        let metrics = GridMetrics::new(
            (width, height),
            config.clamped_icon_size(),
            config.clamped_grid_spacing(),
        );
        drop(config);

        tracing::debug!(monitor = %self.monitor_key, width, height, "surface configured");

        let first_layout = self.metrics.get().is_none();
        if self.metrics.get() == Some(metrics) {
            return;
        }
        self.metrics.set(Some(metrics));

        self.positions
            .borrow_mut()
            .set_geometry(&self.monitor_key, format!("{width}x{height}"));

        if first_layout {
            self.reload();
        } else {
            // A resolution change re-derives every pixel from the stored cells.
            // Deliberately not saved: plugging in a projector must not rewrite
            // the user's layout.
            self.replace_all_tiles();
        }
    }

    /// Re-lists the folder and applies the difference in place.
    pub fn reload(self: &Rc<Self>) {
        let Some(metrics) = self.metrics.get() else {
            // Not configured yet; the first `set_viewport` will do this.
            return;
        };

        let config = self.config.borrow().clone();
        let items = match entries::list(&self.provider, &self.folder, &config) {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(%error, folder = %self.folder.display(), "failed to list the desktop folder");
                return;
            }
        };

        // Every output lists the same folder, so each keeps only what it owns.
        // Orphans — files whose own output is unplugged — are shown here on
        // loan and recorded so their positions are never written back.
        let mut orphans = BTreeSet::new();
        let items: Vec<FileItem> = items
            .into_iter()
            .filter(|item| match self.claim(&item.name) {
                Claim::Owned => true,
                Claim::Orphan => {
                    orphans.insert(item.name.clone());
                    true
                }
                Claim::NotOurs => false,
            })
            .collect();
        *self.orphans.borrow_mut() = orphans;

        let next_order = entries::names(&items);
        let mut next_items = entries::by_name(items);
        let diff = entries::reconcile(&self.order.borrow(), &next_order);

        for name in &diff.removed {
            if let Some(tile) = self.tiles.borrow_mut().remove(name) {
                self.fixed.remove(&tile.root);
            }
            self.items.borrow_mut().remove(name);
        }

        let icon_size = config.clamped_icon_size();
        for name in &diff.kept {
            if let (Some(tile), Some(item)) = (self.tiles.borrow().get(name), next_items.get(name))
            {
                tile.update(item, icon_size);
            }
        }

        // Cells are recomputed from what survived, so a removal frees its slot.
        let mut occupied = BTreeSet::new();
        for name in &diff.kept {
            if let Some(position) = self.positions.borrow().get(&self.monitor_key, name) {
                occupied.insert(metrics.clamp_cell(position.cell()));
            }
        }

        let unplaced: Vec<&String> = diff
            .added
            .iter()
            .filter(|name| {
                // An orphan already has a position, just on an absent output;
                // giving it one here would steal it from the monitor it
                // belongs to the moment that gets plugged back in.
                !self.orphans.borrow().contains(*name)
                    && self
                        .positions
                        .borrow()
                        .get(&self.monitor_key, name)
                        .is_none()
            })
            .collect();
        let fresh = first_free_cells(metrics, &occupied, unplaced.len());
        for (name, cell) in unplaced.iter().zip(fresh) {
            self.positions.borrow_mut().set(
                &self.monitor_key,
                name,
                StoredPosition::snapped(metrics.clamp_cell(cell)),
            );
        }

        for name in &diff.added {
            let Some(item) = next_items.remove(name) else {
                continue;
            };
            self.place_new_tile(name, &item, metrics, icon_size, &mut occupied);
            self.items.borrow_mut().insert(name.clone(), item);
        }

        for (name, item) in next_items {
            self.items.borrow_mut().insert(name, item);
        }

        *self.occupied.borrow_mut() = occupied;
        *self.order.borrow_mut() = next_order;

        // New icons were just given cells, and a removal may have pruned some.
        // Persist so the layout is the same on the next start.
        if self.positions.borrow().is_dirty() {
            self.schedule_save();
        }

        self.apply_icon_visibility();

        // A file that vanished takes its selection with it.
        let present: BTreeSet<String> = self.tiles.borrow().keys().cloned().collect();
        let stale = {
            let mut selection = self.selection.borrow_mut();
            let before = selection.len();
            selection.retain(|name| present.contains(name));
            selection.len() != before
        };
        if stale {
            self.sync_selection();
        }
    }

    fn place_new_tile(
        self: &Rc<Self>,
        name: &str,
        item: &FileItem,
        metrics: GridMetrics,
        icon_size: i32,
        occupied: &mut BTreeSet<Cell>,
    ) {
        let position = self
            .positions
            .borrow()
            .get(&self.monitor_key, name)
            .unwrap_or_default();
        let (x, y) = position.resolve(metrics, self.snap_to_grid());

        let tile = tiles::build(
            item,
            icon_size,
            metrics,
            self.config.borrow().label_backdrop,
            &self.thumbnails,
        );
        self.install_tile_handlers(name, &tile);
        self.fixed.put(&tile.root, f64::from(x), f64::from(y));
        // A desktop holds tens of entries, not thousands, so every tile asks
        // for its preview up front — the icon view's viewport culling would be
        // machinery with nothing to do.
        tile.request_thumbnail(item, icon_size, metrics, &self.thumbnails);

        occupied.insert(metrics.clamp_cell(position.cell()));
        self.tiles.borrow_mut().insert(name.to_string(), tile);
    }

    /// Moves every existing tile to where the current metrics put it.
    fn replace_all_tiles(self: &Rc<Self>) {
        let Some(metrics) = self.metrics.get() else {
            return;
        };
        let snap = self.snap_to_grid();
        let icon_size = self.config.borrow().clamped_icon_size();

        let mut occupied = BTreeSet::new();
        for (name, tile) in self.tiles.borrow().iter() {
            let position = self
                .positions
                .borrow()
                .get(&self.monitor_key, name)
                .unwrap_or_default();
            let (x, y) = position.resolve(metrics, snap);

            tile.root.set_width_request(metrics.tile_width);
            tile.root.set_height_request(metrics.tile_height);
            self.fixed.move_(&tile.root, f64::from(x), f64::from(y));
            if let Some(item) = self.items.borrow().get(name) {
                tile.update(item, icon_size);
                tile.request_thumbnail(item, icon_size, metrics, &self.thumbnails);
            }

            occupied.insert(metrics.clamp_cell(position.cell()));
        }
        *self.occupied.borrow_mut() = occupied;
    }

    /// Re-applies edited settings without restarting.
    ///
    /// A changed folder or filter means the listing is wrong, so it re-lists; a
    /// changed icon size or spacing means the *grid* is wrong, so it rebuilds
    /// the metrics and moves every tile.
    pub fn apply_config(self: &Rc<Self>, config: DesktopConfig) {
        let previous = self.config.replace(config.clone());

        let relist = previous.show_hidden != config.show_hidden || previous.sort != config.sort;
        let regrid = previous.clamped_icon_size() != config.clamped_icon_size()
            || previous.clamped_grid_spacing() != config.clamped_grid_spacing();

        if previous.respect_panels != config.respect_panels && gtk4_layer_shell::is_supported() {
            self.window
                .set_exclusive_zone(if config.respect_panels { 0 } else { -1 });
        }

        if regrid && let Some(old) = self.metrics.get() {
            // Force `set_viewport` to see a change: the surface size did not
            // move, but the grid it implies did.
            self.metrics.set(None);
            self.set_viewport(old.viewport.0, old.viewport.1);
            return;
        }

        if relist {
            self.reload();
        } else {
            self.replace_all_tiles();
        }
    }

    /// Click, double-click, right-click and drag, for one tile.
    fn install_tile_handlers(self: &Rc<Self>, name: &str, tile: &Tile) {
        let primary = gtk::GestureClick::new();
        primary.set_button(gdk::BUTTON_PRIMARY);
        primary.connect_released({
            let surface = Rc::downgrade(self);
            let name = name.to_string();
            move |click, n_press, _, _| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                match n_press {
                    1 => surface.select(&name, click.current_event_state()),
                    2 => surface.activate(&name),
                    _ => {}
                }
            }
        });
        tile.root.add_controller(primary);

        let secondary = gtk::GestureClick::new();
        secondary.set_button(gdk::BUTTON_SECONDARY);
        secondary.connect_pressed({
            let surface = Rc::downgrade(self);
            let name = name.to_string();
            let root = tile.root.clone();
            move |_, _, x, y| {
                if let Some(surface) = surface.upgrade() {
                    surface.show_entry_menu(&name, root.clone().upcast(), x, y);
                }
            }
        });
        tile.root.add_controller(secondary);

        // Dragging a tile drags the whole selection, and records where inside
        // the tile it was grabbed so the drop can place it without jumping.
        let surface = Rc::downgrade(self);
        let name = name.to_string();
        crate::ui::dnd::install_drag_source(&tile.root, move |x, y| {
            let Some(surface) = surface.upgrade() else {
                return Vec::new();
            };
            tracing::debug!(monitor = %surface.monitor_key, %name, "drag started");
            surface.grab_offset.set((x, y));

            if !surface.selection.borrow().contains(&name) {
                surface.select(&name, gdk::ModifierType::empty());
            }
            // The grabbed tile leads, so the drop knows which one lands under
            // the cursor and moves the rest relative to it.
            let mut paths = vec![surface.folder.join(&name)];
            paths.extend(
                surface
                    .selected_paths()
                    .into_iter()
                    .filter(|path| path != &surface.folder.join(&name)),
            );
            paths
        });
    }

    // -----------------------------------------------------------------------
    // Selection
    // -----------------------------------------------------------------------

    /// Applies a click to the selection, honouring Ctrl and Shift.
    fn select(self: &Rc<Self>, name: &str, state: gdk::ModifierType) {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

        {
            let mut selection = self.selection.borrow_mut();
            if shift {
                // Range over the listing order, from the anchor to here.
                let order = self.order.borrow();
                let anchor = self.anchor.borrow().clone();
                let anchor_index = anchor
                    .and_then(|anchor| order.iter().position(|entry| *entry == anchor))
                    .unwrap_or(0);
                if let Some(index) = order.iter().position(|entry| entry == name) {
                    let (low, high) = (anchor_index.min(index), anchor_index.max(index));
                    if !ctrl {
                        selection.clear();
                    }
                    selection.extend(order[low..=high].iter().cloned());
                }
            } else if ctrl {
                if !selection.remove(name) {
                    selection.insert(name.to_string());
                }
                *self.anchor.borrow_mut() = Some(name.to_string());
            } else {
                selection.clear();
                selection.insert(name.to_string());
                *self.anchor.borrow_mut() = Some(name.to_string());
            }
        }

        self.sync_selection();
    }

    fn clear_selection(self: &Rc<Self>) {
        self.selection.borrow_mut().clear();
        *self.anchor.borrow_mut() = None;
        self.sync_selection();
    }

    fn sync_selection(&self) {
        let selection = self.selection.borrow();
        for (name, tile) in self.tiles.borrow().iter() {
            tile.set_selected(selection.contains(name));
        }
    }

    fn selected_names(&self) -> Vec<String> {
        self.selection.borrow().iter().cloned().collect()
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected_names()
            .into_iter()
            .map(|name| self.folder.join(name))
            .collect()
    }

    fn selected_items(&self) -> Vec<FileItem> {
        let items = self.items.borrow();
        self.selection
            .borrow()
            .iter()
            .filter_map(|name| items.get(name).cloned())
            .collect()
    }

    // -----------------------------------------------------------------------
    // Activation
    // -----------------------------------------------------------------------

    /// Opens an entry: folders in the file manager, everything else in whatever
    /// the desktop database says owns it. Mirrors `AppWindow::activate_entry`.
    fn activate(self: &Rc<Self>, name: &str) {
        let Some(item) = self.items.borrow().get(name).cloned() else {
            return;
        };
        let path = self.folder.join(name);

        if item.kind == FileKind::Directory {
            if let Err(error) = crate::launcher::spawn::launch_in_ioexplorer(&path) {
                self.show_toast(&format!("Failed to open {name}: {error}"));
            }
            return;
        }

        if crate::file_ops::is_desktop_entry_file(&item) {
            match gio::DesktopAppInfo::from_filename(&path) {
                Some(app_info) => {
                    if let Err(error) = app_info.launch(&[], gio::AppLaunchContext::NONE) {
                        self.show_toast(&format!("Failed to launch {name}: {error}"));
                    }
                }
                None => self.show_toast(&format!("{name} is not a valid desktop entry")),
            }
            return;
        }

        let Some(uri) = item.uri.to_file_uri() else {
            return;
        };
        if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
        {
            self.show_toast(&format!("Failed to open {name}: {error}"));
        }
    }

    // -----------------------------------------------------------------------
    // Toast
    // -----------------------------------------------------------------------

    /// The desktop's status line. There is no status bar to write into, and a
    /// silent failure on a delete is worse than an ugly one.
    pub fn show_toast(&self, message: &str) {
        if message.is_empty() {
            return;
        }

        self.toast.set_label(message);
        self.toast.set_visible(true);

        cancel(&self.toast_timer);

        let toast = self.toast.clone();
        let slot = Rc::clone(&self.toast_timer);
        let timer = glib::timeout_add_local_once(
            std::time::Duration::from_millis(u64::from(TOAST_MS)),
            move || {
                // Drop the id before doing anything: this source is finished
                // the moment the closure returns, and removing it later aborts.
                slot.borrow_mut().take();
                toast.set_visible(false);
            },
        );
        *self.toast_timer.borrow_mut() = Some(timer);
    }

    fn apply_outcome(self: &Rc<Self>, outcome: crate::file_ops::Outcome) {
        if outcome.is_silent() {
            return;
        }
        self.show_toast(&outcome.message);
        // A changed folder is picked up by the monitor, so no explicit reload.
    }

    // -----------------------------------------------------------------------
    // Rubber band and empty-space clicks
    // -----------------------------------------------------------------------

    /// Marquee selection, plus click-on-nothing to clear.
    ///
    /// Deliberately not the file manager's implementation: that one is wired to
    /// a `ScrolledWindow`'s adjustments and picks items by `glib::Type`, neither
    /// of which applies to a `gtk::Fixed` whose children are plain boxes.
    fn install_rubberband(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        // Capture, so the press is seen before a tile's own drag source claims
        // it; the handler stands down when the press landed on a tile.
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);

        drag.connect_drag_begin({
            let surface = Rc::downgrade(self);
            move |gesture, x, y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                if surface.tile_at(x, y).is_some() {
                    // A tile owns this press: let it drag itself instead.
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                surface.rubberband_start.set(Some((x, y)));
            }
        });

        drag.connect_drag_update({
            let surface = Rc::downgrade(self);
            move |_, offset_x, offset_y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                let Some((start_x, start_y)) = surface.rubberband_start.get() else {
                    return;
                };
                if offset_x.abs() < RUBBERBAND_THRESHOLD && offset_y.abs() < RUBBERBAND_THRESHOLD {
                    return;
                }

                let (x, y, width, height) = rect(start_x, start_y, offset_x, offset_y);
                surface.rubberband.set_margin_start(x as i32);
                surface.rubberband.set_margin_top(y as i32);
                surface
                    .rubberband
                    .set_size_request(width as i32, height as i32);
                surface.rubberband.set_visible(true);
                surface.select_in_rect(x, y, width, height);
            }
        });

        drag.connect_drag_end({
            let surface = Rc::downgrade(self);
            move |_, offset_x, offset_y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                let dragged = surface.rubberband.is_visible();
                surface.rubberband.set_visible(false);
                surface.rubberband_start.set(None);

                if dragged
                    || offset_x.abs() >= RUBBERBAND_THRESHOLD
                    || offset_y.abs() >= RUBBERBAND_THRESHOLD
                {
                    // The click that ends the drag must not clear what it selected.
                    surface.suppress_clear.set(true);
                }
            }
        });

        self.overlay.add_controller(drag);

        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_released({
            let surface = Rc::downgrade(self);
            move |_, _, x, y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                if surface.suppress_clear.replace(false) {
                    return;
                }
                if surface.tile_at(x, y).is_none() {
                    surface.clear_selection();
                }
            }
        });
        self.overlay.add_controller(click);
    }

    /// The name of the tile under a point in overlay coordinates, if any.
    ///
    /// Walks up from the picked widget to whichever direct child of the `Fixed`
    /// contains it — desktop tiles are plain boxes with no distinctive type, so
    /// there is nothing to match on but ancestry.
    fn tile_at(&self, x: f64, y: f64) -> Option<String> {
        let picked = self.overlay.pick(x, y, gtk::PickFlags::DEFAULT)?;
        let fixed: gtk::Widget = self.fixed.clone().upcast();

        let mut widget = picked;
        loop {
            let parent = widget.parent()?;
            if parent == fixed {
                break;
            }
            widget = parent;
        }

        self.tiles
            .borrow()
            .iter()
            .find(|(_, tile)| tile.root.clone().upcast::<gtk::Widget>() == widget)
            .map(|(name, _)| name.clone())
    }

    fn select_in_rect(self: &Rc<Self>, x: f64, y: f64, width: f64, height: f64) {
        let rect = gtk::graphene::Rect::new(x as f32, y as f32, width as f32, height as f32);

        let mut selected = BTreeSet::new();
        for (name, tile) in self.tiles.borrow().iter() {
            let hit = tile
                .root
                .compute_bounds(&self.overlay)
                .and_then(|bounds| bounds.intersection(&rect))
                .is_some();
            if hit {
                selected.insert(name.clone());
            }
        }

        *self.selection.borrow_mut() = selected;
        self.sync_selection();
    }

    // -----------------------------------------------------------------------
    // Folder watching
    // -----------------------------------------------------------------------

    /// Watches the desktop folder, coalescing the burst a bulk copy produces.
    fn watch_folder(self: &Rc<Self>) {
        let file = gio::File::for_path(&self.folder);
        let monitor = match file
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => monitor,
            Err(error) => {
                tracing::warn!(%error, folder = %self.folder.display(), "cannot watch the desktop folder");
                return;
            }
        };
        monitor.set_rate_limit(RELOAD_DEBOUNCE_MS as i32);

        monitor.connect_changed({
            let surface = Rc::downgrade(self);
            move |_, file, other_file, event| {
                if !crate::file_ops::folder_monitor_event_affects_listing(event) {
                    return;
                }
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                surface.note_monitor_event(event, file, other_file);
                surface.queue_reload();
            }
        });

        *self.folder_monitor.borrow_mut() = Some(monitor);
    }

    /// Keeps the position table honest about renames and deletions *before* the
    /// coalesced reload lands, so a renamed file keeps its slot instead of
    /// being treated as a brand new one.
    fn note_monitor_event(
        &self,
        event: gio::FileMonitorEvent,
        file: &gio::File,
        other_file: Option<&gio::File>,
    ) {
        let name_of = |file: &gio::File| {
            file.path().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        };

        match event {
            gio::FileMonitorEvent::Renamed | gio::FileMonitorEvent::Moved => {
                if let (Some(from), Some(to)) = (name_of(file), other_file.and_then(name_of)) {
                    self.positions
                        .borrow_mut()
                        .rename(&self.monitor_key, &from, &to);
                    self.schedule_save();
                }
            }
            gio::FileMonitorEvent::Deleted | gio::FileMonitorEvent::MovedOut => {
                if let Some(name) = name_of(file) {
                    self.positions.borrow_mut().prune(&self.monitor_key, &name);
                    self.schedule_save();
                }
            }
            _ => {}
        }
    }

    /// Coalesces a burst of filesystem events into one re-list.
    fn queue_reload(self: &Rc<Self>) {
        if self.pending_reload.replace(true) {
            return;
        }

        let surface = Rc::downgrade(self);
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(u64::from(RELOAD_DEBOUNCE_MS)),
            move || {
                if let Some(surface) = surface.upgrade() {
                    surface.pending_reload.set(false);
                    surface.reload();
                }
            },
        );
    }

    /// Writes the layout out after things settle. Cancel-and-reschedule, so
    /// dragging several icons in a row costs one write rather than one each.
    fn schedule_save(&self) {
        cancel(&self.save_timer);

        let positions = Rc::clone(&self.positions);
        let slot = Rc::clone(&self.save_timer);
        let timer = glib::timeout_add_local_once(
            std::time::Duration::from_millis(u64::from(SAVE_DEBOUNCE_MS)),
            move || {
                slot.borrow_mut().take();
                if let Err(error) = positions.borrow_mut().flush() {
                    tracing::warn!(%error, "failed to save desktop positions");
                }
            },
        );
        *self.save_timer.borrow_mut() = Some(timer);
    }

    // -----------------------------------------------------------------------
    // Dragging
    // -----------------------------------------------------------------------

    /// The drop target covering the whole desktop.
    ///
    /// Intra-desktop moves are a real drag-and-drop rather than a `GestureDrag`
    /// on each tile. That is forced: `dnd::install_drag_source` runs in the
    /// capture phase, so a bubble-phase gesture on the same tile would never
    /// see the sequence and a capture-phase one would race it. Making the move
    /// a drop sidesteps the conflict entirely and gets the compositor-rendered
    /// drag icon for free.
    fn install_surface_drop_target(self: &Rc<Self>) {
        let surface = Rc::downgrade(self);
        crate::ui::dnd::install_drop_target_at(&self.fixed, move |payload, x, y| {
            if let Some(surface) = surface.upgrade() {
                surface.handle_drop(payload, x, y);
            }
        });
    }

    fn handle_drop(self: &Rc<Self>, payload: crate::ui::dnd::DropPayload, x: f64, y: f64) {
        tracing::debug!(monitor = %self.monitor_key, x, y, "received a drop");
        let crate::ui::dnd::DropPayload::LocalPaths { paths, .. } = &payload else {
            // Bytes, a texture or a URL: nothing the desktop imports itself yet.
            return;
        };

        let from_here = paths
            .iter()
            .all(|path| path.parent() == Some(self.folder.as_path()));

        if from_here {
            self.reposition_dropped(paths, x, y);
            return;
        }

        let operation = match crate::ui::dnd::internal_drag_paths() {
            Some(internal) if internal == *paths => crate::ui::dnd::DropOperation::Move,
            _ => crate::ui::dnd::DropOperation::Copy,
        };
        self.apply_outcome(crate::file_ops::transfer_paths_into_target(
            operation,
            paths,
            &self.folder,
        ));
    }

    /// Moves icons the user dragged within their own desktop.
    ///
    /// The dragged tile lands under the cursor minus wherever it was grabbed;
    /// everything else selected moves by the same delta, but each snaps on its
    /// own so a multi-selection lands in distinct cells rather than stacking.
    fn reposition_dropped(self: &Rc<Self>, paths: &[PathBuf], x: f64, y: f64) {
        let Some(metrics) = self.metrics.get() else {
            return;
        };

        let names: Vec<String> = paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let Some(anchor) = names.first() else {
            return;
        };

        let (grab_x, grab_y) = self.grab_offset.get();
        let target_x = (x - grab_x).round() as i32;
        let target_y = (y - grab_y).round() as i32;

        let origin = self
            .tiles
            .borrow()
            .get(anchor)
            .and_then(|tile| tile.root.compute_bounds(&self.fixed))
            .map(|bounds| (bounds.x() as i32, bounds.y() as i32))
            .unwrap_or((target_x, target_y));
        let (delta_x, delta_y) = (target_x - origin.0, target_y - origin.1);

        let snap = self.snap_to_grid();
        let mut arrived_from_elsewhere = false;

        for name in &names {
            let current = self
                .tiles
                .borrow()
                .get(name)
                .and_then(|tile| tile.root.compute_bounds(&self.fixed))
                .map(|bounds| (bounds.x() as i32, bounds.y() as i32));

            // No tile here means the icon came from another screen's surface.
            // Everything below is the same either way; only the bookkeeping at
            // the end differs, because this output has to take it over.
            let moved = match current {
                Some(_) if name == anchor => (target_x, target_y),
                Some((current_x, current_y)) => (current_x + delta_x, current_y + delta_y),
                None => {
                    arrived_from_elsewhere = true;
                    (target_x, target_y)
                }
            };

            let position = if snap {
                StoredPosition::snapped(metrics.cell_for_point(moved.0, moved.1))
            } else {
                let (cx, cy) = metrics.clamp_point(moved.0, moved.1);
                StoredPosition::free(metrics.cell_for_point(cx, cy), cx, cy)
            };

            let (px, py) = position.resolve(metrics, snap);
            if let Some(tile) = self.tiles.borrow().get(name) {
                self.fixed.move_(&tile.root, f64::from(px), f64::from(py));
            }

            match current {
                // An orphan is only on loan from an unplugged output; moving it
                // around here must not quietly transfer it to this one.
                Some(_) if self.orphans.borrow().contains(name) => {}
                Some(_) => self
                    .positions
                    .borrow_mut()
                    .set(&self.monitor_key, name, position),
                // Dragged in from another screen: this output owns it now, and
                // the one it came from must let go.
                None => self
                    .positions
                    .borrow_mut()
                    .claim_for(&self.monitor_key, name, position),
            }
        }

        self.refresh_occupied(metrics);
        self.schedule_save();

        if arrived_from_elsewhere {
            // Both surfaces are now out of date: this one is missing a tile it
            // owns, and the other is still drawing one it does not.
            self.broadcast();
        }
    }

    /// Re-syncs every surface, not just this one.
    ///
    /// Needed whenever shared state changes: ownership moving between outputs
    /// (a surface only ever lists on its own behalf and has no idea another one
    /// just took a file off it), or the global hidden flag being toggled.
    fn broadcast(self: &Rc<Self>) {
        if let Some(broadcast) = self.broadcast.borrow().clone() {
            broadcast();
        } else {
            self.reload();
            self.apply_icon_visibility();
        }
    }

    fn refresh_occupied(&self, metrics: GridMetrics) {
        let positions = self.positions.borrow();
        let mut occupied = BTreeSet::new();
        for name in self.tiles.borrow().keys() {
            if let Some(position) = positions.get(&self.monitor_key, name) {
                occupied.insert(metrics.clamp_cell(position.cell()));
            }
        }
        *self.occupied.borrow_mut() = occupied;
    }

    /// Re-flows every icon into the grid, in the current sort order.
    fn arrange_icons(self: &Rc<Self>) {
        let Some(metrics) = self.metrics.get() else {
            return;
        };

        let order = self.order.borrow().clone();
        let cells = first_free_cells(metrics, &BTreeSet::new(), order.len());
        for (name, cell) in order.iter().zip(cells) {
            let position = StoredPosition::snapped(metrics.clamp_cell(cell));
            let (x, y) = position.resolve(metrics, true);
            if let Some(tile) = self.tiles.borrow().get(name) {
                self.fixed.move_(&tile.root, f64::from(x), f64::from(y));
            }
            self.positions
                .borrow_mut()
                .set(&self.monitor_key, name, position);
        }

        self.refresh_occupied(metrics);
        self.schedule_save();
        self.show_toast("Arranged icons");
    }

    /// Hides or shows every icon, on every screen.
    ///
    /// Deliberately hides the `Fixed` rather than the window: the surface has to
    /// stay live and clickable while hidden, or there would be no way to get the
    /// context menu back to unhide them.
    fn set_icons_hidden(self: &Rc<Self>, hidden: bool) {
        self.positions.borrow_mut().set_icons_hidden(hidden);
        self.schedule_save();
        // Every other screen has to follow, whichever one the menu was used on.
        self.broadcast();
        self.show_toast(if hidden {
            "Desktop icons hidden"
        } else {
            "Desktop icons shown"
        });
    }

    /// Matches this surface to the shared hidden state.
    pub fn apply_icon_visibility(&self) {
        let hidden = self.positions.borrow().icons_hidden();
        self.fixed.set_visible(!hidden);
    }

    fn set_snap_to_grid(self: &Rc<Self>, snap: bool) {
        self.positions
            .borrow_mut()
            .set_snap_to_grid(&self.monitor_key, snap);

        if snap && let Some(metrics) = self.metrics.get() {
            // Turning snapping on pulls every freely-placed icon onto its cell.
            for name in self.tiles.borrow().keys() {
                let cell = self
                    .positions
                    .borrow()
                    .get(&self.monitor_key, name)
                    .map(|position| position.cell())
                    .unwrap_or_default();
                self.positions.borrow_mut().set(
                    &self.monitor_key,
                    name,
                    StoredPosition::snapped(metrics.clamp_cell(cell)),
                );
            }
        }

        self.replace_all_tiles();
        self.schedule_save();
        self.show_toast(if snap {
            "Snapping to grid"
        } else {
            "Free placement"
        });
    }

    // -----------------------------------------------------------------------
    // Context menus
    // -----------------------------------------------------------------------

    fn install_empty_space_menu(self: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        click.connect_pressed({
            let surface = Rc::downgrade(self);
            move |_, _, x, y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                if surface.tile_at(x, y).is_some() {
                    // The tile's own handler covers this press.
                    return;
                }
                surface.show_empty_space_menu(x, y);
            }
        });
        self.overlay.add_controller(click);
    }

    fn show_empty_space_menu(self: &Rc<Self>, x: f64, y: f64) {
        let paste: MenuAction = {
            let surface = Rc::clone(self);
            Rc::new(move || surface.paste())
        };
        let new_folder: MenuAction = {
            let surface = Rc::clone(self);
            Rc::new(move || surface.create_folder())
        };
        let bookmark = self.bookmark_action(self.folder.clone());
        let custom = self.custom_actions(vec![ActionTarget::current_folder(self.folder.clone())]);

        let inner = context_menu::EmptySpaceContext::new(paste, new_folder, bookmark, custom);
        let snap = self.snap_to_grid();
        let hidden = self.positions.borrow().icons_hidden();

        let context = DesktopContext::new(inner)
            .before("Open In IoExplorer", Some("folder-open-symbolic"), {
                let surface = Rc::clone(self);
                Rc::new(move || surface.open_folder_in_ioexplorer())
            })
            .after("Arrange Icons", Some("view-grid-symbolic"), {
                let surface = Rc::clone(self);
                Rc::new(move || surface.arrange_icons())
            })
            .after(
                if snap {
                    "Snap To Grid: On"
                } else {
                    "Snap To Grid: Off"
                },
                Some("view-grid-symbolic"),
                {
                    let surface = Rc::clone(self);
                    Rc::new(move || surface.set_snap_to_grid(!snap))
                },
            )
            .after(
                if hidden { "Show Icons" } else { "Hide Icons" },
                Some(if hidden {
                    "view-reveal-symbolic"
                } else {
                    "view-conceal-symbolic"
                }),
                {
                    let surface = Rc::clone(self);
                    Rc::new(move || surface.set_icons_hidden(!hidden))
                },
            )
            .after("Refresh", Some("view-refresh-symbolic"), {
                let surface = Rc::clone(self);
                Rc::new(move || surface.reload())
            });

        context_menu::ContextMenu::popup_at(&self.overlay, x, y, &context);
    }

    fn show_entry_menu(self: &Rc<Self>, name: &str, parent: gtk::Widget, x: f64, y: f64) {
        // Right-clicking outside the selection acts on what was clicked, which
        // is what every file manager does and what the user means.
        if !self.selection.borrow().contains(name) {
            self.select(name, gdk::ModifierType::empty());
        }

        let paths = self.selected_paths();
        let items = self.selected_items();
        if paths.is_empty() {
            return;
        }

        let extract = crate::file_ops::archive_paths(&items).map(|archives| {
            let surface = Rc::clone(self);
            Rc::new(move || surface.extract_archives(archives.clone())) as MenuAction
        });

        let bookmark = (items.len() == 1 && items[0].kind == FileKind::Directory)
            .then(|| self.bookmark_action(paths[0].clone()));

        let actions = context_menu::FileEntryActions {
            // The image viewer is welded to the file manager's window; "Open"
            // covers the same need here without dragging it across.
            view: None,
            bookmark,
            extract,
            copy: {
                let surface = Rc::clone(self);
                Rc::new(move |paths| surface.copy_to_clipboard(paths, FileClipboardOperation::Copy))
            },
            cut: {
                let surface = Rc::clone(self);
                Rc::new(move |paths| surface.copy_to_clipboard(paths, FileClipboardOperation::Cut))
            },
            rename: {
                let surface = Rc::clone(self);
                Rc::new(move |path| surface.rename(path))
            },
            delete: {
                let surface = Rc::clone(self);
                Rc::new(move |paths| surface.delete(paths))
            },
            custom_actions: self
                .custom_actions(items.iter().filter_map(ActionTarget::from_item).collect()),
        };

        let Some(inner) = context_menu::FileEntryContext::for_paths(paths, actions) else {
            return;
        };

        let name = name.to_string();
        let context = DesktopContext::new(inner)
            .before("Open", Some("document-open-symbolic"), {
                let surface = Rc::clone(self);
                let name = name.clone();
                Rc::new(move || surface.activate(&name))
            })
            .before("Open In IoExplorer", Some("folder-open-symbolic"), {
                let surface = Rc::clone(self);
                Rc::new(move || surface.open_folder_in_ioexplorer())
            });

        context_menu::ContextMenu::popup_at(&parent, x, y, &context);
    }

    fn bookmark_action(&self, path: PathBuf) -> context_menu::BookmarkAction {
        let bookmarked = crate::bookmarks::load().contains(&path);
        let label = if bookmarked {
            "Remove Bookmark"
        } else {
            "Add Bookmark"
        };

        context_menu::BookmarkAction::new(
            label,
            Rc::new(move || {
                let mut bookmarks = crate::bookmarks::load();
                if bookmarked {
                    bookmarks.retain(|entry| *entry != path);
                } else {
                    bookmarks.push(path.clone());
                }
                if let Err(error) = crate::bookmarks::save(&bookmarks) {
                    tracing::warn!(%error, "failed to save bookmarks");
                }
            }),
        )
    }

    fn custom_actions(&self, targets: Vec<ActionTarget>) -> Vec<context_menu::CustomAction> {
        let configs = self.custom_action_configs.borrow().clone();
        crate::custom_actions::matching_actions(&configs, &targets)
            .into_iter()
            .map(|config| {
                let folder = self.folder.clone();
                let targets = targets.clone();
                let label = config.label.clone();
                context_menu::CustomAction::new(
                    label,
                    Rc::new(move || {
                        crate::file_ops::run_custom_action(&config, &targets, Some(&folder));
                    }),
                )
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Menu operations
    // -----------------------------------------------------------------------

    fn copy_to_clipboard(self: &Rc<Self>, paths: Vec<PathBuf>, operation: FileClipboardOperation) {
        match crate::file_ops::copy_paths_to_clipboard(&self.window.clipboard(), &paths, operation)
        {
            Ok(message) => self.show_toast(&message),
            Err(message) => self.show_toast(&message),
        }
    }

    fn paste(self: &Rc<Self>) {
        let clipboard = self.window.clipboard();
        let surface = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let value = match clipboard
                .read_value_future(gtk::gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    surface.show_toast(&format!("Clipboard does not contain files: {error}"));
                    return;
                }
            };

            let Ok(file_list) = value.get::<gtk::gdk::FileList>() else {
                surface.show_toast("Clipboard does not contain files");
                return;
            };
            let paths: Vec<PathBuf> = file_list
                .files()
                .into_iter()
                .filter_map(|file| file.path())
                .collect();
            if paths.is_empty() {
                surface.show_toast("Clipboard does not contain files");
                return;
            }

            surface.apply_outcome(crate::file_ops::transfer_paths_into_target(
                crate::ui::dnd::DropOperation::Copy,
                &paths,
                &surface.folder,
            ));
        });
    }

    fn delete(self: &Rc<Self>, paths: Vec<PathBuf>) {
        self.apply_outcome(crate::file_ops::delete_paths(&paths));
    }

    /// Renames through the focused prompt surface, because a `Bottom`-layer
    /// surface cannot take the keyboard an inline entry would need.
    fn rename(self: &Rc<Self>, path: PathBuf) {
        let Some(app) = self.window.application() else {
            return;
        };
        let current = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        let surface = Rc::clone(self);
        super::prompt::ask(&app, "Rename", &current, "Rename", move |new_name| {
            surface.apply_outcome(crate::file_ops::rename_path(&path, &new_name));
        });
    }

    fn create_folder(self: &Rc<Self>) {
        let Some(app) = self.window.application() else {
            return;
        };
        let surface = Rc::clone(self);
        let folder = self.folder.clone();

        super::prompt::ask(&app, "New Folder", "New Folder", "Create", move |name| {
            match crate::file_ops::create_folder_named(&folder, &name) {
                Ok(target) => surface.show_toast(&format!("Created {}", target.display())),
                Err(message) => surface.show_toast(&message),
            }
        });
    }

    fn extract_archives(self: &Rc<Self>, archives: Vec<crate::file_ops::ArchivePath>) {
        if archives.is_empty() {
            return;
        }

        self.show_toast(&match archives.as_slice() {
            [only] => format!("Extracting {}...", only.display_name()),
            archives => format!("Extracting {} archives...", archives.len()),
        });

        let surface = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let total = archives.len();
            let mut extracted = 0;
            let mut last_error = None;

            for archive in archives {
                // Resolved one at a time so extracting `photos.zip` twice gives
                // `photos` then `photos 2`, which needs the first to exist.
                let destination = crate::file_ops::next_available_path(&archive.destination);
                match crate::archive::extract(archive.format, &archive.path, &destination).await {
                    Ok(()) => extracted += 1,
                    Err(error) => {
                        last_error = Some(format!(
                            "Failed to extract {}: {error}",
                            archive.display_name()
                        ));
                    }
                }
            }

            surface.show_toast(&match last_error {
                Some(error) => error,
                None if total == 1 => "Extracted 1 archive".to_string(),
                None => format!("Extracted {extracted} archives"),
            });
        });
    }

    fn open_folder_in_ioexplorer(self: &Rc<Self>) {
        if let Err(error) = crate::launcher::spawn::launch_in_ioexplorer(&self.folder) {
            self.show_toast(&format!("Failed to open the desktop folder: {error}"));
        }
    }

    /// Drops stored positions for files that are no longer there.
    ///
    /// Startup only, and only against a listing that succeeded — running it on
    /// a routine reload would let one transient read failure wipe the layout.
    pub fn prune_stale_positions(&self) {
        let config = self.config.borrow().clone();
        let Ok(items) = entries::list(&self.provider, &self.folder, &config) else {
            return;
        };

        let present: BTreeSet<String> = items.into_iter().map(|item| item.name).collect();
        self.positions
            .borrow_mut()
            .prune_missing(&self.monitor_key, &present);
    }
}
