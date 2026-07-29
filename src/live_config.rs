//! Watches `config.toml` and the user's `theme.css` so external edits apply
//! without restarting.
//!
//! Every GUI binary builds one [`LiveConfig`] during `startup` and hands it to
//! whatever it opens. Windows register listeners; the CSS needs none, since the
//! provider lives here and reloading it is enough to repaint.
//!
//! Directories are watched rather than the files themselves. Editors — and our
//! own [`config::write_atomic`] saves — replace the file rather than rewriting
//! it in place, and a monitor bound to the old inode goes deaf after the first
//! write.

use std::{
    cell::{Cell, RefCell},
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gio::prelude::*;

use crate::{config::AppConfig, theme};

/// How long to wait after a change before re-reading. One save is several
/// events — a temporary created, renamed, then attributes set — and reloading
/// per event would rebuild the spotlight runtime several times over.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);

/// `SIGHUP`. Hardcoded rather than pulling in `libc` for a single constant; it
/// is 1 on every platform this ships to.
const SIGHUP: i32 = 1;

/// A config reload that actually changed something.
///
/// The `*_changed` predicates exist so listeners can re-apply only the parts
/// that moved: rebuilding a file listing or re-resolving the spotlight runtime
/// on every unrelated edit would be visible.
pub struct ConfigChange {
    pub config: Rc<AppConfig>,
    pub previous: Rc<AppConfig>,
}

impl ConfigChange {
    pub fn list_columns_changed(&self) -> bool {
        self.config.list_columns != self.previous.list_columns
    }

    pub fn actions_changed(&self) -> bool {
        self.config.actions != self.previous.actions
    }

    pub fn sidebar_width_changed(&self) -> bool {
        self.config.sidebar_width != self.previous.sidebar_width
    }

    pub fn custom_css_changed(&self) -> bool {
        self.config.custom_css != self.previous.custom_css
    }

    pub fn spotlight_changed(&self) -> bool {
        self.config.spotlight != self.previous.spotlight
    }
}

type ConfigListener = Box<dyn Fn(&ConfigChange)>;
type CssListener = Box<dyn Fn(Option<&str>)>;

pub struct LiveConfig {
    config: RefCell<Rc<AppConfig>>,
    /// `None` when there is no display, which is the case in tests.
    user_css: Option<theme::UserCss>,
    /// The CSS file currently being watched, which follows `config.custom_css`.
    css_path: RefCell<Option<PathBuf>>,
    /// What was last pushed into the provider, so re-reading an unchanged file
    /// — including one we just wrote ourselves — fires nothing.
    css_text: RefCell<Option<String>>,
    // The handlers disconnect when a monitor drops, so these must outlive the
    // callbacks even though nothing ever reads them.
    monitors: RefCell<Vec<gio::FileMonitor>>,
    pending: Cell<Option<glib::SourceId>>,
    config_listeners: RefCell<Vec<ConfigListener>>,
    css_listeners: RefCell<Vec<CssListener>>,
}

impl LiveConfig {
    /// Loads the config, installs the stylesheets, and starts watching.
    ///
    /// Call this from `connect_startup`: installing a CSS provider needs a
    /// display, which does not exist before then.
    pub fn new() -> Rc<Self> {
        let config = AppConfig::load();
        let user_css = theme::install(&config);
        let css_path = theme::effective_custom_css_path(&config);
        let css_text = css_path.as_deref().and_then(read_css);

        let this = Rc::new(Self {
            config: RefCell::new(Rc::new(config)),
            user_css,
            css_path: RefCell::new(css_path),
            css_text: RefCell::new(css_text),
            monitors: RefCell::new(Vec::new()),
            pending: Cell::new(None),
            config_listeners: RefCell::new(Vec::new()),
            css_listeners: RefCell::new(Vec::new()),
        });

        this.rewatch();
        this
    }

    pub fn config(&self) -> Rc<AppConfig> {
        Rc::clone(&self.config.borrow())
    }

    /// Registers a callback fired after each config reload that changed something.
    pub fn connect_changed(&self, listener: impl Fn(&ConfigChange) + 'static) {
        self.config_listeners.borrow_mut().push(Box::new(listener));
    }

    /// Registers a callback fired after the user's CSS is reloaded. The CSS is
    /// already applied by then; this is for anything mirroring it, such as the
    /// settings editor's colour pickers.
    pub fn connect_css_changed(&self, listener: impl Fn(Option<&str>) + 'static) {
        self.css_listeners.borrow_mut().push(Box::new(listener));
    }

    /// Records a config we just wrote, so the watcher's own event is a no-op.
    pub fn note_config_written(&self, config: &AppConfig) {
        *self.config.borrow_mut() = Rc::new(config.clone());
    }

    /// Applies CSS we just wrote and records it, so the resulting file event
    /// does not bounce back into whatever produced it.
    pub fn apply_css_now(&self, css: &str) {
        if let Some(user_css) = &self.user_css {
            user_css.load(css);
        }
        *self.css_text.borrow_mut() = Some(css.to_string());
    }

    /// Re-reads both files now, skipping the debounce.
    pub fn reload_now(self: &Rc<Self>) {
        self.cancel_pending();
        self.reload();
    }

    /// Makes `systemctl --user reload` mean "re-read the config".
    ///
    /// The shipped unit's `ExecReload` sends `SIGHUP`, whose default
    /// disposition would otherwise kill the daemon.
    pub fn install_sighup_handler(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::unix_signal_add_local(SIGHUP, move || {
            if let Some(this) = weak.upgrade() {
                tracing::info!("SIGHUP received, reloading config");
                this.reload_now();
            }
            glib::ControlFlow::Continue
        });
    }

    /// Rebuilds the monitors for wherever the two files currently live.
    fn rewatch(self: &Rc<Self>) {
        for monitor in self.monitors.borrow_mut().drain(..) {
            monitor.cancel();
        }

        let dirs = watch_dirs(
            AppConfig::config_path().as_deref(),
            self.css_path.borrow().as_deref(),
        );

        let mut monitors = Vec::with_capacity(dirs.len());
        for dir in dirs {
            // Watching needs the directory to exist, and on a first run nothing
            // has written it yet. `AppConfig::save` creates it anyway, so this
            // is only earlier, not new.
            let _ = fs::create_dir_all(&dir);

            let file = gio::File::for_path(&dir);
            let monitor = match file.monitor_directory(
                gio::FileMonitorFlags::WATCH_MOVES,
                None::<&gio::Cancellable>,
            ) {
                Ok(monitor) => monitor,
                Err(error) => {
                    tracing::warn!(dir = %dir.display(), %error, "failed to watch config directory");
                    continue;
                }
            };

            monitor.set_rate_limit(RELOAD_DEBOUNCE.as_millis() as i32);
            let weak = Rc::downgrade(self);
            monitor.connect_changed(move |_, file, other_file, event| {
                if !event_affects_config(event) {
                    return;
                }
                let Some(this) = weak.upgrade() else {
                    return;
                };
                // A rename reports its destination in `other_file`, so a
                // temporary moved over the config only matches on that one.
                if this.is_watched(file) || other_file.is_some_and(|file| this.is_watched(file)) {
                    this.schedule_reload();
                }
            });

            monitors.push(monitor);
        }

        *self.monitors.borrow_mut() = monitors;
    }

    fn is_watched(&self, file: &gio::File) -> bool {
        let Some(path) = file.path() else {
            return false;
        };

        AppConfig::config_path().is_some_and(|config| config == path)
            || self.css_path.borrow().as_ref() == Some(&path)
    }

    fn schedule_reload(self: &Rc<Self>) {
        self.cancel_pending();

        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(RELOAD_DEBOUNCE, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.pending.set(None);
            this.reload();
        });
        self.pending.set(Some(source));
    }

    fn cancel_pending(&self) {
        if let Some(pending) = self.pending.take() {
            pending.remove();
        }
    }

    fn reload(self: &Rc<Self>) {
        self.reload_config();
        self.reload_css();
    }

    fn reload_config(self: &Rc<Self>) {
        let Some(path) = AppConfig::config_path() else {
            return;
        };

        let next = match AppConfig::try_load_from(&path) {
            Ok(Some(config)) => config,
            // A deleted config means defaults, which is what a restart would do.
            Ok(None) => AppConfig::default(),
            // Keep the last good config: a malformed or half-written file must
            // not wipe live settings out from under whoever is editing it.
            Err(error) => {
                tracing::warn!(%error, "keeping the previous config");
                return;
            }
        };

        let previous = self.config();
        if *previous == next {
            return;
        }

        tracing::info!(path = %path.display(), "config changed on disk, reloading");
        let config = Rc::new(next);
        *self.config.borrow_mut() = Rc::clone(&config);

        // Re-point the watch before listeners run, or the first edit to the new
        // CSS file would be missed.
        if config.custom_css != previous.custom_css {
            *self.css_path.borrow_mut() = theme::effective_custom_css_path(&config);
            self.rewatch();
        }

        let change = ConfigChange { config, previous };
        // Listeners may reach back into the config, so never hold a borrow here.
        for listener in self.config_listeners.borrow().iter() {
            listener(&change);
        }
    }

    fn reload_css(&self) {
        let css = self.css_path.borrow().as_deref().and_then(read_css);
        if *self.css_text.borrow() == css {
            return;
        }

        tracing::info!("theme CSS changed on disk, reloading");
        match (&self.user_css, &css) {
            (Some(user_css), Some(css)) => user_css.load(css),
            (Some(user_css), None) => user_css.clear(),
            (None, _) => {}
        }
        *self.css_text.borrow_mut() = css.clone();

        for listener in self.css_listeners.borrow().iter() {
            listener(css.as_deref());
        }
    }
}

impl Drop for LiveConfig {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.remove();
        }
        for monitor in self.monitors.borrow_mut().drain(..) {
            monitor.cancel();
        }
    }
}

fn read_css(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(css) => Some(css),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read custom CSS");
            None
        }
    }
}

/// The directories holding the files worth watching, deduplicated.
///
/// `custom_css` can point anywhere, so this is not always just the config
/// directory.
fn watch_dirs(config_path: Option<&Path>, css_path: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    for parent in [config_path, css_path]
        .into_iter()
        .flatten()
        .filter_map(Path::parent)
    {
        if !dirs.iter().any(|dir| dir == parent) {
            dirs.push(parent.to_path_buf());
        }
    }

    dirs
}

fn event_affects_config(event: gio::FileMonitorEvent) -> bool {
    matches!(
        event,
        gio::FileMonitorEvent::Changed
            | gio::FileMonitorEvent::ChangesDoneHint
            | gio::FileMonitorEvent::Created
            | gio::FileMonitorEvent::Deleted
            | gio::FileMonitorEvent::AttributeChanged
            | gio::FileMonitorEvent::Moved
            | gio::FileMonitorEvent::Renamed
            | gio::FileMonitorEvent::MovedIn
            | gio::FileMonitorEvent::MovedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ListColumns, ViewMode};

    fn change(mutate: impl FnOnce(&mut AppConfig)) -> ConfigChange {
        let previous = AppConfig::default();
        let mut config = previous.clone();
        mutate(&mut config);

        ConfigChange {
            config: Rc::new(config),
            previous: Rc::new(previous),
        }
    }

    #[test]
    fn session_state_is_not_a_config_change() {
        // The running window owns the view mode, hidden-file toggle and icon
        // size; a file changing on disk must not reset them mid-session.
        let change = change(|config| {
            config.default_view = ViewMode::List;
            config.show_hidden = true;
            config.icon_size = 64;
        });

        assert!(!change.list_columns_changed());
        assert!(!change.actions_changed());
        assert!(!change.sidebar_width_changed());
        assert!(!change.custom_css_changed());
        assert!(!change.spotlight_changed());
    }

    #[test]
    fn each_section_reports_only_itself() {
        let columns = change(|config| {
            config.list_columns = ListColumns {
                size: false,
                kind: false,
                modified: false,
            }
        });
        assert!(columns.list_columns_changed());
        assert!(!columns.sidebar_width_changed());
        assert!(!columns.spotlight_changed());

        let width = change(|config| config.sidebar_width = 400);
        assert!(width.sidebar_width_changed());
        assert!(!width.list_columns_changed());

        let css = change(|config| config.custom_css = Some(PathBuf::from("/tmp/theme.css")));
        assert!(css.custom_css_changed());
        assert!(!css.actions_changed());

        let spotlight = change(|config| config.spotlight.width = 900);
        assert!(spotlight.spotlight_changed());
        assert!(!spotlight.sidebar_width_changed());
    }

    #[test]
    fn watch_dirs_dedupes_a_shared_directory() {
        let dirs = watch_dirs(
            Some(Path::new("/home/u/.config/ioexplorer/config.toml")),
            Some(Path::new("/home/u/.config/ioexplorer/theme.css")),
        );

        assert_eq!(dirs, vec![PathBuf::from("/home/u/.config/ioexplorer")]);
    }

    #[test]
    fn watch_dirs_follows_css_kept_elsewhere() {
        let dirs = watch_dirs(
            Some(Path::new("/home/u/.config/ioexplorer/config.toml")),
            Some(Path::new("/home/u/themes/dark.css")),
        );

        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/.config/ioexplorer"),
                PathBuf::from("/home/u/themes"),
            ]
        );
    }

    #[test]
    fn watch_dirs_tolerates_missing_paths() {
        assert!(watch_dirs(None, None).is_empty());
        assert_eq!(
            watch_dirs(None, Some(Path::new("/home/u/themes/dark.css"))),
            vec![PathBuf::from("/home/u/themes")]
        );
    }

    #[test]
    fn replacement_events_count_as_changes() {
        // Editors and our own saves rename a temporary over the file rather
        // than rewriting it, so these are the events that actually matter.
        assert!(event_affects_config(gio::FileMonitorEvent::Created));
        assert!(event_affects_config(gio::FileMonitorEvent::Renamed));
        assert!(event_affects_config(gio::FileMonitorEvent::MovedIn));
        assert!(event_affects_config(gio::FileMonitorEvent::Changed));
        assert!(event_affects_config(gio::FileMonitorEvent::Deleted));

        assert!(!event_affects_config(gio::FileMonitorEvent::PreUnmount));
    }
}
