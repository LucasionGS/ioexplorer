//! `ioexplorer-quick`: a quick menu at the pointer for symbols, emoji, saved
//! GIFs and the clipboard history, in the spirit of Windows' Win+. panel.
//!
//! The menu opens beside the pointer on the Symbols tab (or the configured
//! one). Picking a character closes it and types the character into the window
//! that had focus, puts it on the clipboard, or both. A saved GIF goes in the
//! same way as the link it was saved from, or else is pasted as the image.
//! Running the command again while the menu is open closes it, so one key both
//! opens and dismisses it.
//!
//! `--server` keeps it running: GTK, the stylesheets and the character tables
//! stay loaded, so the menu opens at once, and the clipboard is recorded for
//! the Clipboard tab. The command then only asks the server over its socket,
//! and falls back to opening the menu itself when no server is listening.

mod data;
mod gifs;
mod history;
mod insert;
mod placement;
mod state;
mod window;

use std::{
    cell::{Cell, RefCell},
    fs,
    path::PathBuf,
    rc::Rc,
    time::{Duration, SystemTime},
};

use gtk::{gdk, gio, glib, prelude::*};

use crate::{
    config::{AppConfig, QuickInsert, QuickTab},
    launcher::toggle::{self, ToggleMessage},
    shot::deliver::wl_copy,
    theme,
};

use window::{Picked, QuickMenu};

const APP_ID: &str = "io.github.ionix.IoExplorer.Quick";
const QUICK_SOCKET: &str = "quick.sock";
const QUICK_SOCKET_FALLBACK: &str = "ioexplorer-quick.sock";

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// How long to wait after the menu closes before typing, so the compositor
/// has handed keyboard focus back to the window the text is for.
const FOCUS_RETURN_DELAY: Duration = Duration::from_millis(150);

/// How long a picked image is served in every format, from this process,
/// before it is handed to `wl-copy` as a PNG and the process exits. Long
/// enough for the paste that follows the pick to have read it.
const IMAGE_SERVE_TIME: Duration = Duration::from_secs(3);

const USAGE: &str = "\
Usage: ioexplorer-quick [OPTIONS]

Opens a quick menu at the pointer to pick a symbol, an emoji, a saved GIF or
something copied earlier. Running it again while the menu is open closes it.

Options:
  -t, --tab TAB        Open on TAB: symbols, emoji, gif or clipboard
                       (default: the config's default-tab, or symbols)
  -i, --insert MODE    What picking does: type, copy, or both (default: the
                       config's insert, or both)
      --server         Stay running, so the menu opens at once, and record
                       every copy for the Clipboard tab; start it with the
                       session. The command then hands over to it.
  -h, --help           Show this help

In the menu:
  Type                 Search the tab, by name or keyword
  Arrows               Move through the grid
  Enter, click         Insert and close
  Shift+Enter/click    Queue and keep the menu open; the queue is inserted
                       when it closes, and Backspace removes from it
  Ctrl+D, right-click  Add to favourites, or remove; favourites come first
  Tab, Shift+Tab       Next / previous tab
  Page Up / Page Down  Previous / next category
  Esc                  Clear the search, then close

In the GIF tab:
  Type                 Search by tag or file name
  Drag                 Drop the GIF's file into another window
  F2                   Edit the selected GIF's tags
  Delete               Move the selected GIF to the trash
  A GIF or image on the clipboard is offered for saving, with tags:
  Ctrl+S               Go to the offer's tags; Enter or Ctrl+S there saves

In the Clipboard tab, which needs --server running:
  Type                 Search copied text
  Ctrl+D, right-click  Pin, or unpin; pinned copies stay and come first
  Delete               Forget the selected copy

Exit status is 0 when something was inserted, 1 when the menu was closed
without a pick, and 2 for invalid arguments. With a server running, the
command hands over and exits with 0 at once.";

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct QuickArgs {
    tab: Option<QuickTab>,
    insert: Option<QuickInsert>,
    help: bool,
    server: bool,
    record_clipboard: bool,
}

impl QuickArgs {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => {
                    (flag.to_string(), Some(value.to_string()))
                }
                _ => (arg.clone(), None),
            };
            let mut value = |name: &str| {
                inline
                    .clone()
                    .or_else(|| args.next())
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match flag.as_str() {
                "-h" | "--help" => parsed.help = true,
                "--server" => parsed.server = true,
                "--record-clipboard" => parsed.record_clipboard = true,
                "-t" | "--tab" => parsed.tab = Some(parse_tab(&value(&flag)?)?),
                "-i" | "--insert" => parsed.insert = Some(parse_insert(&value(&flag)?)?),
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(parsed)
    }
}

/// What a server is asked for: a menu, on these terms, or closing the open
/// one. One line: `open TAB INSERT`, with `-` for the config's choice.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct OpenRequest {
    tab: Option<QuickTab>,
    insert: Option<QuickInsert>,
}

impl ToggleMessage for OpenRequest {
    fn serialize(&self) -> String {
        let tab = self.tab.map_or("-", |tab| match tab {
            QuickTab::Symbols => "symbols",
            QuickTab::Emoji => "emoji",
            QuickTab::Gif => "gif",
            QuickTab::Clipboard => "clipboard",
        });
        let insert = self.insert.map_or("-", |insert| match insert {
            QuickInsert::Type => "type",
            QuickInsert::Copy => "copy",
            QuickInsert::Both => "both",
        });
        format!("open {tab} {insert}\n")
    }

    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        (parts.next()? == "open").then_some(())?;
        let tab = match parts.next()? {
            "-" => None,
            tab => Some(parse_tab(tab).ok()?),
        };
        let insert = match parts.next()? {
            "-" => None,
            insert => Some(parse_insert(insert).ok()?),
        };
        parts.next().is_none().then_some(Self { tab, insert })
    }
}

fn parse_tab(value: &str) -> Result<QuickTab, String> {
    match value {
        "symbols" | "symbol" => Ok(QuickTab::Symbols),
        "emoji" | "emojis" => Ok(QuickTab::Emoji),
        "gif" | "gifs" => Ok(QuickTab::Gif),
        "clipboard" | "clip" => Ok(QuickTab::Clipboard),
        other => Err(format!("unknown tab: {other}")),
    }
}

fn parse_insert(value: &str) -> Result<QuickInsert, String> {
    match value {
        "type" => Ok(QuickInsert::Type),
        "copy" => Ok(QuickInsert::Copy),
        "both" => Ok(QuickInsert::Both),
        other => Err(format!("unknown insert mode: {other}")),
    }
}

pub fn run() -> glib::ExitCode {
    init_logging();

    let args = match QuickArgs::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("ioexplorer-quick: {error}\n\n{USAGE}");
            return glib::ExitCode::from(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }
    if args.record_clipboard {
        let limit = AppConfig::load().quick.clipboard_limit;
        return match history::record(limit) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("ioexplorer-quick: cannot record the copy: {error}");
                glib::ExitCode::FAILURE
            }
        };
    }

    if args.server {
        return run_server();
    }

    let request = OpenRequest {
        tab: args.tab,
        insert: args.insert,
    };
    if toggle::send(QUICK_SOCKET, QUICK_SOCKET_FALLBACK, &request).is_ok() {
        return glib::ExitCode::SUCCESS;
    }

    // No server: this process opens the menu. Unique: a second launch reaches
    // this one's `activate`, which closes the menu, so the hotkey toggles it.
    // When the process is only still here to serve the clipboard, the second
    // launch opens a new menu in it instead.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::empty())
        .build();

    let exit_code = Rc::new(Cell::new(1_u8));
    let menu: Rc<RefCell<Option<Rc<QuickMenu>>>> = Rc::new(RefCell::new(None));

    app.connect_activate({
        let exit_code = Rc::clone(&exit_code);
        let menu = Rc::clone(&menu);
        move |app| {
            let open = menu.borrow().clone();
            match open {
                Some(open) => open.close(),
                None => start(app, request, &exit_code, &menu, None),
            }
        }
    });

    let argv0 = std::env::args().next().unwrap_or_default();
    let status = app.run_with_args(&[argv0]);
    match exit_code.get() {
        0 => status,
        code => glib::ExitCode::from(code),
    }
}

/// What a server keeps between menus.
struct Server {
    user_css: Option<theme::UserCss>,
    /// The user's stylesheet and when it last changed, as last loaded.
    css_loaded: RefCell<Option<(PathBuf, Option<SystemTime>)>>,
    watcher: RefCell<Option<history::Watcher>>,
}

impl Server {
    /// Rereads the user's stylesheet when it changed since the last menu, so
    /// a theme edit shows without restarting the server.
    fn refresh_css(&self, config: &AppConfig) {
        let Some(user_css) = &self.user_css else {
            return;
        };
        let current = css_stamp(config);
        if *self.css_loaded.borrow() == current {
            return;
        }
        match current.as_ref().map(|(path, _)| fs::read_to_string(path)) {
            Some(Ok(css)) => user_css.load(&css),
            _ => user_css.clear(),
        }
        *self.css_loaded.borrow_mut() = current;
    }
}

fn css_stamp(config: &AppConfig) -> Option<(PathBuf, Option<SystemTime>)> {
    let path = theme::effective_custom_css_path(config)?;
    let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
    Some((path, modified))
}

fn run_server() -> glib::ExitCode {
    let (listener, _socket) = match toggle::bind(QUICK_SOCKET, QUICK_SOCKET_FALLBACK) {
        Ok(Some(bound)) => bound,
        Ok(None) => {
            eprintln!("ioexplorer-quick: a server is running already");
            return glib::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("ioexplorer-quick: cannot start the server: {error}");
            return glib::ExitCode::FAILURE;
        }
    };
    let receiver = RefCell::new(Some(toggle::spawn_listener::<OpenRequest>(listener)));

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let Some(receiver) = receiver.borrow_mut().take() else {
            return;
        };
        let config = AppConfig::load();
        let server = Rc::new(Server {
            user_css: theme::install(&config),
            css_loaded: RefCell::new(css_stamp(&config)),
            watcher: RefCell::new(match history::Watcher::start() {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    tracing::warn!(%error, "the clipboard is not recorded");
                    None
                }
            }),
        });
        window::warm_up();
        tracing::info!("the quick menu server is ready");

        let menu: Rc<RefCell<Option<Rc<QuickMenu>>>> = Rc::new(RefCell::new(None));
        let exit_code = Rc::new(Cell::new(0));
        let requests = Rc::clone(&server);
        let weak_app = app.downgrade();
        toggle::install_receiver(receiver, move |request: OpenRequest| {
            let Some(app) = weak_app.upgrade() else {
                return;
            };
            let open = menu.borrow().clone();
            match open {
                Some(open) => open.close(),
                None => start(&app, request, &exit_code, &menu, Some(&requests)),
            }
        });

        for signal in [SIGINT, SIGTERM] {
            let app = app.downgrade();
            glib::unix_signal_add_local(signal as _, move || {
                if let Some(app) = app.upgrade() {
                    app.quit();
                }
                glib::ControlFlow::Break
            });
        }

        // Held until it is told to stop, which also stops the watcher.
        let hold = RefCell::new(Some(app.hold()));
        app.connect_shutdown(move |_| {
            server.watcher.borrow_mut().take();
            hold.borrow_mut().take();
        });
    });

    let argv0 = std::env::args().next().unwrap_or_default();
    app.run_with_args(&[argv0])
}

fn start(
    app: &gtk::Application,
    request: OpenRequest,
    exit_code: &Rc<Cell<u8>>,
    slot: &Rc<RefCell<Option<Rc<QuickMenu>>>>,
    server: Option<&Rc<Server>>,
) {
    let Some(display) = gdk::Display::default() else {
        eprintln!("ioexplorer-quick: no display");
        return;
    };

    let config = AppConfig::load();
    let _user_css = match server {
        Some(server) => {
            server.refresh_css(&config);
            None
        }
        None => theme::install(&config),
    };
    // A server stays anyway; only a one-shot hands an image to wl-copy.
    let handoff = server.is_none();
    let mut quick = config.quick;
    if let Some(insert) = request.insert {
        quick.insert = insert;
    }
    let mode = quick.insert;
    let tab = request.tab.unwrap_or(quick.default_tab);

    let monitors: Vec<gdk::Monitor> = display
        .monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .collect();
    let placement = placement::place(
        &window::screens(&monitors),
        placement::pointer(),
        window::CARD_SIZE,
    );

    let hold = app.hold();
    let menu = QuickMenu::open(app, quick, tab, placement, &monitors, {
        let exit_code = Rc::clone(exit_code);
        let slot = Rc::clone(slot);
        move |picked| {
            // Deferred: this runs inside the menu's own handlers.
            glib::idle_add_local_once(move || {
                slot.borrow_mut().take();
            });
            let Some(picked) = picked else {
                drop(hold);
                return;
            };
            exit_code.set(0);
            glib::timeout_add_local_once(FOCUS_RETURN_DELAY, move || {
                if deliver(&picked, mode) {
                    serve_clipboard(hold, picked.image, handoff);
                } else {
                    drop(hold);
                }
            });
        }
    });
    *slot.borrow_mut() = Some(menu);
}

/// Types or copies what was picked, once focus is back where it belongs.
/// Returns whether this process now serves the clipboard, and so has to stay
/// until something else is copied.
fn deliver(picked: &Picked, mode: QuickInsert) -> bool {
    if picked.image.is_none() {
        return !picked.text.is_empty() && insert::insert(&picked.text, mode).serve_clipboard;
    }
    // The image is on the clipboard already, served from here. Queued text
    // can only be typed: copying it would replace the image.
    if !picked.text.is_empty() && mode != QuickInsert::Copy {
        insert::insert(&picked.text, QuickInsert::Type);
    }
    if mode != QuickInsert::Copy
        && let Err(error) = insert::paste_shortcut()
    {
        tracing::warn!(%error, "cannot paste the image; it is on the clipboard");
    }
    true
}

/// Keeps the process alive while it owns the clipboard, since exiting would
/// take the copied contents with it.
///
/// With `handoff`, a picked image is served in every format only for
/// [`IMAGE_SERVE_TIME`], which covers the paste; then it goes to `wl-copy` as
/// a PNG, which keeps it on the clipboard after the process exits. `wl-copy`
/// offers one format only, so it has to be the one everything accepts. A
/// server stays running, so it keeps serving every format. Anything else, or
/// an image without `wl-copy` to hand it to, is served until something else
/// is copied.
///
/// Only this hold is released: a menu opened in the meantime keeps the
/// process running.
fn serve_clipboard(hold: gio::ApplicationHoldGuard, image: Option<PathBuf>, handoff: bool) {
    let Some(display) = gdk::Display::default() else {
        drop(hold);
        return;
    };
    let clipboard = display.clipboard();
    let hold = Rc::new(RefCell::new(Some(hold)));

    let released = Rc::clone(&hold);
    let handler = Rc::new(RefCell::new(None));
    let id = clipboard.connect_changed({
        let handler = Rc::clone(&handler);
        move |clipboard| {
            if !clipboard.is_local() {
                released.borrow_mut().take();
                if let Some(id) = handler.borrow_mut().take() {
                    clipboard.disconnect(id);
                }
            }
        }
    });
    *handler.borrow_mut() = Some(id);

    let Some(image) = image.filter(|_| handoff) else {
        return;
    };
    glib::timeout_add_local_once(IMAGE_SERVE_TIME, move || {
        if hold.borrow().is_none() || !clipboard.is_local() {
            return;
        }
        let handed = gifs::png_file(&image).and_then(|png| {
            history::mark_own_copy();
            wl_copy("image/png", &png)
        });
        match handed {
            // wl-copy taking the clipboard over fires `changed`, which
            // releases the hold.
            Ok(()) => tracing::debug!("the image is handed to wl-copy"),
            Err(error) => {
                tracing::info!(%error, "cannot hand the image to wl-copy; serving it until replaced");
            }
        }
    });
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<QuickArgs, String> {
        QuickArgs::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn nothing_given_leaves_it_to_the_config() {
        assert_eq!(parse(&[]).unwrap(), QuickArgs::default());
    }

    #[test]
    fn tab_and_insert_parse_in_both_spellings() {
        let args = parse(&["--tab", "emoji", "-i", "copy"]).unwrap();
        assert_eq!(args.tab, Some(QuickTab::Emoji));
        assert_eq!(args.insert, Some(QuickInsert::Copy));

        assert_eq!(parse(&["-t", "gif"]).unwrap().tab, Some(QuickTab::Gif));

        let joined = parse(&["--tab=symbols", "--insert=type"]).unwrap();
        assert_eq!(joined.tab, Some(QuickTab::Symbols));
        assert_eq!(joined.insert, Some(QuickInsert::Type));
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            OpenRequest::default(),
            OpenRequest {
                tab: Some(QuickTab::Clipboard),
                insert: Some(QuickInsert::Copy),
            },
            OpenRequest {
                tab: Some(QuickTab::Emoji),
                insert: None,
            },
        ] {
            assert_eq!(OpenRequest::parse(&request.serialize()), Some(request));
        }
        assert_eq!(OpenRequest::parse("open stickers -"), None);
        assert_eq!(OpenRequest::parse("toggle"), None);
        assert_eq!(OpenRequest::parse("open - - extra"), None);
        assert!(parse(&["--server"]).unwrap().server);
    }

    #[test]
    fn mistakes_are_reported() {
        assert!(parse(&["--tab"]).is_err());
        assert!(parse(&["--tab", "stickers"]).is_err());
        assert!(parse(&["--insert", "paste"]).is_err());
        assert!(parse(&["emoji"]).is_err());
    }
}
