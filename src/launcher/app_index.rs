//! Installed-application index shared by the start menu and spotlight.
//!
//! [`AppEntry`] is deliberately plain data rather than a `gio::AppInfo`: GObject
//! types are `!Send`, so keeping them out of the index is what lets matching and
//! ranking be pure, testable, and free to move between threads later.

use std::{cell::Cell, cell::RefCell, collections::HashSet, rc::Rc, time::Duration};

use gio::prelude::*;

/// How long to wait after a `changed` signal before rescanning. A package
/// install touches `applications/` many times in a row; without this the index
/// would be rebuilt dozens of times for one operation.
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(400);

const FALLBACK_ICON: &str = "application-x-executable-symbolic";

/// A serialized `gio::Icon`, or a bare icon-theme name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IconRef(pub String);

impl IconRef {
    pub fn fallback() -> Self {
        Self(FALLBACK_ICON.to_string())
    }

    pub fn from_icon_name(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Serializes a `gio::Icon` for storage in plain data.
    pub fn from_gicon(icon: &gio::Icon) -> Self {
        // `IconExt::to_string` collides with `ToString::to_string`; qualifying it
        // is required or this silently stores unrelated text.
        match gio::prelude::IconExt::to_string(icon) {
            Some(serialized) => Self(serialized.to_string()),
            None => Self::fallback(),
        }
    }
}

/// One installed application, flattened out of its `.desktop` entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppEntry {
    pub desktop_id: String,
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub exec_name: Option<String>,
    pub icon: IconRef,
}

/// A snapshot of the installed applications, sorted by name.
#[derive(Debug, Default)]
pub struct AppIndex {
    entries: Vec<AppEntry>,
}

impl AppIndex {
    /// Scans every visible `.desktop` entry. Must run on the main thread.
    pub fn scan() -> Self {
        let mut seen = HashSet::new();
        let mut entries = gio::AppInfo::all()
            .into_iter()
            .filter(gio::prelude::AppInfoExt::should_show)
            .filter_map(|app| entry_from_app_info(&app, &mut seen))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.desktop_id.cmp(&right.desktop_id))
        });

        Self { entries }
    }

    /// Builds an index from entries that were not scanned.
    ///
    /// Test-only: the real one comes from [`AppIndex::scan`], which reads the
    /// desktop database and has to run on the main thread.
    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<AppEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[AppEntry] {
        &self.entries
    }

    /// Finds the installed application a window's app id belongs to.
    ///
    /// A Wayland app id — or an X11 class under XWayland — is only conventionally
    /// related to a desktop entry, so this tries the conventions in descending
    /// order of confidence and stops at the first that matches. Everything is
    /// compared case-insensitively because the two spellings routinely disagree
    /// on case alone (`Steam` against `steam.desktop`).
    pub fn find_by_app_id(&self, app_id: &str) -> Option<&AppEntry> {
        let app_id = app_id.trim();
        if app_id.is_empty() {
            return None;
        }
        let wanted = app_id.to_lowercase();

        // `discord` → `discord.desktop`. Covers the overwhelming majority.
        let by_id = |entry: &&AppEntry| stem(&entry.desktop_id).to_lowercase() == wanted;
        // `org.gnome.Nautilus` reported as plain `nautilus`. Checked after the
        // full id so an app whose id really is the last segment still wins.
        let by_tail = |entry: &&AppEntry| {
            stem(&entry.desktop_id)
                .rsplit('.')
                .next()
                .is_some_and(|tail| tail.to_lowercase() == wanted)
        };
        let by_exec = |entry: &&AppEntry| {
            entry
                .exec_name
                .as_deref()
                .is_some_and(|exec| exec.to_lowercase() == wanted)
        };
        let by_name = |entry: &&AppEntry| entry.name.to_lowercase() == wanted;

        self.entries
            .iter()
            .find(by_id)
            .or_else(|| self.entries.iter().find(by_tail))
            .or_else(|| self.entries.iter().find(by_exec))
            .or_else(|| self.entries.iter().find(by_name))
    }
}

/// Strips the `.desktop` suffix, leaving anything else alone.
fn stem(desktop_id: &str) -> &str {
    desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id)
}

fn entry_from_app_info(app: &gio::AppInfo, seen: &mut HashSet<String>) -> Option<AppEntry> {
    let name = app.display_name().to_string();
    if name.trim().is_empty() {
        return None;
    }

    let desktop_id = app
        .id()
        .map(|id| id.to_string())
        .unwrap_or_else(|| name.to_lowercase().replace(' ', "-"));
    seen.insert(desktop_id.clone()).then_some(())?;

    let desktop = app.downcast_ref::<gio::DesktopAppInfo>();
    let keywords = desktop
        .map(|desktop| {
            desktop
                .keywords()
                .into_iter()
                .map(|keyword| keyword.to_string())
                .collect()
        })
        .unwrap_or_default();
    let categories = desktop
        .and_then(gio::DesktopAppInfo::categories)
        .map(|categories| split_categories(&categories))
        .unwrap_or_default();
    let generic_name = desktop
        .and_then(gio::DesktopAppInfo::generic_name)
        .map(|value| value.to_string());

    Some(AppEntry {
        desktop_id,
        name,
        generic_name,
        comment: app.description().map(|value| value.to_string()),
        keywords,
        categories,
        exec_name: app
            .executable()
            .file_name()
            .map(|name| name.to_string_lossy().to_string()),
        icon: app
            .icon()
            .map(|icon| IconRef::from_gicon(&icon))
            .unwrap_or_else(IconRef::fallback),
    })
}

fn split_categories(categories: &str) -> Vec<String> {
    categories
        .split(';')
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .map(str::to_string)
        .collect()
}

/// Launches an application by its desktop id.
pub fn launch_desktop_id(desktop_id: &str) -> Result<(), String> {
    let app = gio::DesktopAppInfo::new(desktop_id)
        .ok_or_else(|| format!("no application found for {desktop_id}"))?;
    app.launch(&[], None::<&gio::AppLaunchContext>)
        .map_err(|error| format!("failed to launch {desktop_id}: {error}"))
}

type IndexListener = Box<dyn Fn(&Rc<AppIndex>)>;

/// An [`AppIndex`] that rebuilds itself when applications are installed or removed.
pub struct LiveAppIndex {
    // The signal handler is disconnected when the monitor drops, so this field
    // must outlive the callbacks even though nothing ever reads it.
    _monitor: gio::AppInfoMonitor,
    index: RefCell<Rc<AppIndex>>,
    pending: Cell<Option<glib::SourceId>>,
    listeners: RefCell<Vec<IndexListener>>,
}

impl LiveAppIndex {
    pub fn new() -> Rc<Self> {
        let monitor = gio::AppInfoMonitor::get();
        let this = Rc::new(Self {
            _monitor: monitor.clone(),
            index: RefCell::new(Rc::new(AppIndex::scan())),
            pending: Cell::new(None),
            listeners: RefCell::new(Vec::new()),
        });

        let weak = Rc::downgrade(&this);
        monitor.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.schedule_rescan();
            }
        });

        this
    }

    pub fn snapshot(&self) -> Rc<AppIndex> {
        Rc::clone(&self.index.borrow())
    }

    /// Registers a callback fired after each rebuild.
    pub fn connect_changed(&self, listener: impl Fn(&Rc<AppIndex>) + 'static) {
        self.listeners.borrow_mut().push(Box::new(listener));
    }

    fn schedule_rescan(self: &Rc<Self>) {
        if let Some(pending) = self.pending.take() {
            pending.remove();
        }

        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(RESCAN_DEBOUNCE, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.pending.set(None);
            this.rescan();
        });
        self.pending.set(Some(source));
    }

    fn rescan(&self) {
        let index = Rc::new(AppIndex::scan());
        *self.index.borrow_mut() = Rc::clone(&index);

        // Listeners may reach back into the index, so never hold a borrow here.
        for listener in self.listeners.borrow().iter() {
            listener(&index);
        }
    }
}

impl Drop for LiveAppIndex {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_semicolon_separated_categories() {
        assert_eq!(
            split_categories("System;FileTools; FileManager ;;"),
            vec![
                "System".to_string(),
                "FileTools".to_string(),
                "FileManager".to_string()
            ]
        );
    }

    #[test]
    fn empty_categories_produce_no_entries() {
        assert!(split_categories("").is_empty());
        assert!(split_categories(";;").is_empty());
    }

    #[test]
    fn fallback_icon_is_a_plain_icon_name() {
        assert_eq!(IconRef::fallback(), IconRef(FALLBACK_ICON.to_string()));
    }

    fn entry(desktop_id: &str, name: &str, exec_name: Option<&str>) -> AppEntry {
        AppEntry {
            desktop_id: desktop_id.to_string(),
            name: name.to_string(),
            generic_name: None,
            comment: None,
            keywords: Vec::new(),
            categories: Vec::new(),
            exec_name: exec_name.map(str::to_string),
            icon: IconRef::from_icon_name(format!("{name}-icon")),
        }
    }

    fn index() -> AppIndex {
        AppIndex {
            entries: vec![
                entry("discord.desktop", "Discord", Some("discord")),
                entry("org.gnome.Nautilus.desktop", "Files", Some("nautilus")),
                entry("code.desktop", "Visual Studio Code", Some("code")),
                entry("spotify-adblock.desktop", "Spotify", Some("spotify")),
            ],
        }
    }

    #[test]
    fn an_app_id_matches_its_desktop_entry() {
        let index = index();

        assert_eq!(
            index.find_by_app_id("discord").map(|e| e.name.as_str()),
            Some("Discord")
        );
        // Compositors and desktop entries disagree on case all the time.
        assert_eq!(
            index.find_by_app_id("Discord").map(|e| e.name.as_str()),
            Some("Discord")
        );
    }

    /// GTK apps report a reverse-DNS app id; X11 clients report only the tail.
    #[test]
    fn a_reverse_dns_entry_is_found_by_its_last_segment() {
        let index = index();

        assert_eq!(
            index
                .find_by_app_id("org.gnome.Nautilus")
                .map(|e| e.name.as_str()),
            Some("Files")
        );
        assert_eq!(
            index.find_by_app_id("nautilus").map(|e| e.name.as_str()),
            Some("Files")
        );
    }

    /// The entry id bears no relation to the class, but the executable does.
    #[test]
    fn an_app_id_falls_back_to_the_executable_then_the_name() {
        let index = index();

        assert_eq!(
            index.find_by_app_id("spotify").map(|e| e.name.as_str()),
            Some("Spotify")
        );
        assert_eq!(
            index
                .find_by_app_id("visual studio code")
                .map(|e| e.name.as_str()),
            Some("Visual Studio Code")
        );
    }

    #[test]
    fn an_unknown_app_id_matches_nothing() {
        let index = index();

        assert!(index.find_by_app_id("no-such-app").is_none());
        assert!(index.find_by_app_id("").is_none());
        assert!(index.find_by_app_id("   ").is_none());
    }
}
