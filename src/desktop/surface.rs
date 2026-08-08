//! One layer-shell surface: the desktop as it appears on a single output.

use std::{
    cell::{Cell as CellFlag, RefCell},
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    rc::Rc,
};

use gtk::{gdk, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::{
    config::DesktopConfig,
    providers::{FileItem, local::LocalProvider},
    ui::views::thumbnail::ThumbnailCache,
};

use super::{
    entries,
    layout::{Cell, GridMetrics, first_free_cells},
    positions::{PositionStore, StoredPosition},
    tiles::{self, Tile},
};

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
}

impl DesktopSurface {
    pub fn new(
        app: &gtk::Application,
        monitor: &gdk::Monitor,
        monitor_key: String,
        folder: PathBuf,
        config: DesktopConfig,
        positions: Rc<RefCell<PositionStore>>,
        thumbnails: ThumbnailCache,
        windowed: bool,
    ) -> Rc<Self> {
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

        // `gtk::Widget` has no resize signal, and a layer surface's size is only
        // known once the compositor configures it. A DrawingArea's `resize` is
        // the one reliable hook, so it exists purely to report the viewport.
        let sizer = gtk::DrawingArea::builder().can_target(false).build();
        let fixed = gtk::Fixed::builder()
            .css_classes(["desktop-surface"])
            .build();

        let overlay = gtk::Overlay::builder().child(&sizer).build();
        overlay.add_overlay(&fixed);
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

    pub fn present(&self) {
        self.window.present();
    }

    pub fn close(&self) {
        self.window.close();
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
                self.positions
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
    }

    fn place_new_tile(
        &self,
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
