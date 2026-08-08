//! The `ioexplorer-desktop` surface: `~/Desktop` as a layer-shell backdrop of
//! movable icons, drawn over whatever wallpaper daemon the user runs.

pub mod entries;
pub mod layout;
mod menu;
pub mod positions;
mod prompt;
mod surface;
mod tiles;

use std::{cell::RefCell, collections::BTreeMap, path::PathBuf, rc::Rc};

use gtk::{gdk, gio::prelude::*, glib, prelude::*};

use crate::{
    config::{AppConfig, CustomActionConfig, DesktopConfig},
    selector,
    ui::views::thumbnail::{self, ThumbnailCache},
};

use positions::PositionStore;
use surface::{DesktopSurface, SurfaceContext};

const APP_ID: &str = "io.github.ionix.IoExplorer.Desktop";

#[derive(Debug, Default)]
struct DesktopArgs {
    /// Forces a plain window instead of a layer surface, for running nested or
    /// under X11 during development.
    windowed: bool,
}

impl DesktopArgs {
    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut parsed = Self::default();
        for arg in args {
            match arg.as_str() {
                "--windowed" => parsed.windowed = true,
                other => tracing::warn!(arg = %other, "ignoring unknown argument"),
            }
        }
        parsed
    }
}

pub fn run() -> glib::ExitCode {
    init_logging();

    let args = DesktopArgs::parse(std::env::args().skip(1));

    // Uniqueness left on, unlike every other surface in this project: they want
    // several instances, a desktop wants exactly one. A second invocation
    // activates the running instance over D-Bus and exits.
    let app = gtk::Application::builder().application_id(APP_ID).build();

    let live_config = selector::install_live_config(&app);
    let desktop: Rc<RefCell<Option<Rc<DesktopApp>>>> = Rc::new(RefCell::new(None));

    app.connect_activate({
        let desktop = Rc::clone(&desktop);
        let live_config = Rc::clone(&live_config);
        move |app| {
            // Activation is re-entrant: a second process activates us rather
            // than starting its own, and that must not build a second desktop.
            if desktop.borrow().is_some() {
                return;
            }

            // `LiveConfig` installs the stylesheet itself; without one (no
            // display in tests, or a failed watcher) the bundled CSS still has
            // to load, or the desktop renders with the default GTK background
            // over the wallpaper.
            let config = match live_config.borrow().as_ref() {
                Some(live) => live.config(),
                None => {
                    crate::theme::install_bundled();
                    Rc::new(AppConfig::load())
                }
            };

            // After the theme, and above it: the desktop must not be tinted by
            // whatever `window` background the active theme happens to set.
            crate::theme::install_desktop_transparency();

            let this = DesktopApp::new(
                config.desktop.clone(),
                config.actions.clone(),
                args.windowed,
            );
            this.start(app);
            *desktop.borrow_mut() = Some(Rc::clone(&this));

            if let Some(live) = live_config.borrow().as_ref() {
                let weak = Rc::downgrade(&this);
                live.connect_changed(move |change| {
                    if change.desktop_changed()
                        && let Some(this) = weak.upgrade()
                    {
                        this.apply_config(change.config.desktop.clone());
                    }
                });
            }
        }
    });

    // A layout change on the way out would otherwise be lost with the pending
    // save still queued.
    app.connect_shutdown({
        let desktop = Rc::clone(&desktop);
        move |_| {
            if let Some(this) = desktop.borrow().as_ref()
                && let Err(error) = this.positions.borrow_mut().flush()
            {
                tracing::warn!(%error, "failed to save desktop positions on shutdown");
            }
        }
    });

    // argv is parsed above, so GTK must not try to parse it again.
    let argv0 = std::env::args().next().unwrap_or_default();
    app.run_with_args(&[argv0])
}

/// Owns everything shared between outputs: the position table, the thumbnail
/// cache, and the config.
struct DesktopApp {
    config: RefCell<DesktopConfig>,
    custom_actions: RefCell<Vec<CustomActionConfig>>,
    folder: PathBuf,
    positions: Rc<RefCell<PositionStore>>,
    thumbnails: ThumbnailCache,
    surfaces: RefCell<BTreeMap<String, Rc<DesktopSurface>>>,
    /// Live outputs in enumeration order, shared with every surface so they
    /// agree on who owns an unclaimed file.
    monitor_order: Rc<RefCell<Vec<String>>>,
    /// Where the in-flight drag was grabbed inside its tile. Shared, because a
    /// drag that ends on another screen is completed by that screen's surface.
    grab_offset: Rc<std::cell::Cell<(f64, f64)>>,
    windowed: bool,
}

impl DesktopApp {
    fn new(
        config: DesktopConfig,
        custom_actions: Vec<CustomActionConfig>,
        windowed: bool,
    ) -> Rc<Self> {
        let folder = config.folder_path().unwrap_or_else(|| PathBuf::from("."));

        Rc::new(Self {
            config: RefCell::new(config),
            custom_actions: RefCell::new(custom_actions),
            folder,
            positions: Rc::new(RefCell::new(PositionStore::load())),
            thumbnails: thumbnail::new_cache(),
            surfaces: RefCell::new(BTreeMap::new()),
            monitor_order: Rc::new(RefCell::new(Vec::new())),
            grab_offset: Rc::new(std::cell::Cell::new((0.0, 0.0))),
            windowed,
        })
    }

    fn start(self: &Rc<Self>, app: &gtk::Application) {
        tracing::info!(folder = %self.folder.display(), "starting desktop");

        self.sync_monitors(app);

        if let Some(display) = gdk::Display::default() {
            display.monitors().connect_items_changed({
                let this = Rc::downgrade(self);
                let app = app.clone();
                move |_, _, _, _| {
                    if let Some(this) = this.upgrade() {
                        this.sync_monitors(&app);
                    }
                }
            });
        }
    }

    /// Brings the set of surfaces in line with the set of live outputs.
    fn sync_monitors(self: &Rc<Self>, app: &gtk::Application) {
        let Some(display) = gdk::Display::default() else {
            tracing::warn!("no display; cannot place the desktop");
            return;
        };

        let live: Vec<(String, gdk::Monitor)> = display
            .monitors()
            .iter::<gdk::Monitor>()
            .flatten()
            .map(|monitor| (monitor_key(&monitor), monitor))
            .collect();
        let live_keys: Vec<String> = live.iter().map(|(key, _)| key.clone()).collect();

        // Close surfaces whose output vanished. Their stored positions stay on
        // disk untouched — unplugging a monitor must not cost its layout.
        self.surfaces.borrow_mut().retain(|key, existing| {
            let alive = live_keys.contains(key);
            if !alive {
                tracing::info!(monitor = %key, "output went away");
                existing.close();
            }
            alive
        });

        // One claim per file name, so a name stored under two outputs renders
        // once. Order is enumeration order, so the result is stable.
        *self.monitor_order.borrow_mut() = live_keys.clone();
        self.positions.borrow_mut().dedupe(&live_keys);

        for (key, monitor) in live {
            if self.surfaces.borrow().contains_key(&key) {
                continue;
            }

            tracing::info!(monitor = %key, "adding a desktop surface");
            let created = DesktopSurface::new(
                app,
                &monitor,
                key.clone(),
                SurfaceContext {
                    folder: self.folder.clone(),
                    config: self.config.borrow().clone(),
                    custom_action_configs: self.custom_actions.borrow().clone(),
                    monitor_order: Rc::clone(&self.monitor_order),
                    grab_offset: Rc::clone(&self.grab_offset),
                    positions: Rc::clone(&self.positions),
                    thumbnails: Rc::clone(&self.thumbnails),
                    windowed: self.windowed,
                },
            );
            created.prune_stale_positions();
            created.present();

            // A resolution change re-derives pixels from the stored cells.
            monitor.connect_geometry_notify({
                let created = Rc::downgrade(&created);
                move |_| {
                    if let Some(created) = created.upgrade() {
                        created.reload();
                    }
                }
            });

            self.surfaces.borrow_mut().insert(key, created);
        }

        // Every surface can now reach the others, which it needs when an icon
        // is dragged from one screen to another and ownership changes hands.
        let broadcast: Rc<dyn Fn()> = {
            let this = Rc::downgrade(self);
            Rc::new(move || {
                if let Some(this) = this.upgrade() {
                    for surface in this.surfaces.borrow().values() {
                        surface.reload();
                        surface.apply_icon_visibility();
                    }
                }
            })
        };
        for surface in self.surfaces.borrow().values() {
            surface.set_broadcast(Rc::clone(&broadcast));
        }

        // Ownership just moved: a new output may take files the default screen
        // was showing on loan, and a departed one leaves orphans behind.
        broadcast();

        if let Err(error) = self.positions.borrow_mut().flush() {
            tracing::warn!(%error, "failed to save desktop positions");
        }
    }

    fn apply_config(self: &Rc<Self>, config: DesktopConfig) {
        *self.config.borrow_mut() = config.clone();
        thumbnail::retain_sizes(&self.thumbnails, &[config.clamped_icon_size()]);

        for surface in self.surfaces.borrow().values() {
            surface.apply_config(config.clone());
        }
    }
}

/// A stable name for an output.
///
/// The connector ("DP-1", "eDP-1") is per physical port, which is the right
/// granularity: two identical monitors are told apart, and a layout survives a
/// reboot. `description()` was rejected precisely because two of the same model
/// collide under it. Moving a display to a different port does yield a new key
/// and re-flows that output's icons once — worth documenting, not worth a
/// heuristic.
fn monitor_key(monitor: &gdk::Monitor) -> String {
    if let Some(connector) = monitor.connector() {
        return connector.to_string();
    }

    match (monitor.manufacturer(), monitor.model()) {
        (Some(make), Some(model)) => format!("{make} {model}"),
        (Some(make), None) => make.to_string(),
        (None, Some(model)) => model.to_string(),
        (None, None) => "default".to_string(),
    }
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::DesktopArgs;

    #[test]
    fn windowed_mode_is_opt_in() {
        assert!(!DesktopArgs::parse(std::iter::empty()).windowed);
        assert!(DesktopArgs::parse(["--windowed".to_string()].into_iter()).windowed);
    }

    /// An unrecognised flag is logged and skipped rather than fatal: this runs
    /// as a session daemon, and refusing to start over a stray argument would
    /// leave the user with no desktop at all.
    #[test]
    fn an_unknown_argument_does_not_stop_the_surface() {
        let parsed =
            DesktopArgs::parse(["--nonsense".to_string(), "--windowed".to_string()].into_iter());

        assert!(parsed.windowed);
    }
}
