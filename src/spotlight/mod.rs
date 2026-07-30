//! A spotlight-style launcher: search applications, folders and bookmarks, or
//! use a prefix to run commands, browse paths, calculate, and search files.
//!
//! Like the start menu, running the command a second time toggles a `--server`
//! instance closed rather than opening a second window.

mod ai;
mod calc;
mod custom_results;
mod file_search;
mod image_cache;
mod keys;
mod layout;
mod passwords;
mod paths;
mod prefixes;
mod preview;
mod query;
mod results;
mod runtime;
mod software;
mod ssh;
mod vpn;
mod window;
mod windows;

use std::{cell::RefCell, env, rc::Rc, sync::mpsc};

use gtk::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use crate::{
    launcher::toggle::{self, ToggleMessage},
    selector,
};

use runtime::SpotlightRuntime;
use window::SpotlightWindow;

const SPOTLIGHT_APP_ID: &str = "io.github.ionix.IoExplorer.Spotlight";
const SPOTLIGHT_SOCKET: &str = "spotlight.sock";
const SPOTLIGHT_SOCKET_FALLBACK: &str = "ioexplorer-spotlight.sock";

pub fn run() -> glib::ExitCode {
    init_logging();

    let args = match SpotlightArgs::parse(env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return glib::ExitCode::FAILURE;
        }
    };

    if args.server {
        return run_server();
    }

    // A running server toggles; otherwise fall back to a one-shot window.
    if toggle::send(SPOTLIGHT_SOCKET, SPOTLIGHT_SOCKET_FALLBACK, &ToggleRequest).is_ok() {
        return glib::ExitCode::SUCCESS;
    }

    run_application(LaunchMode::OneShot)
}

fn run_server() -> glib::ExitCode {
    let (listener, _socket_guard) = match toggle::bind(SPOTLIGHT_SOCKET, SPOTLIGHT_SOCKET_FALLBACK)
    {
        Ok(Some(bound)) => bound,
        Ok(None) => {
            tracing::info!("ioexplorer-spotlight server already running");
            return glib::ExitCode::SUCCESS;
        }
        Err(error) => {
            tracing::error!(%error, "failed to start ioexplorer-spotlight server");
            return glib::ExitCode::FAILURE;
        }
    };

    let receiver = toggle::spawn_listener::<ToggleRequest>(listener);
    run_application(LaunchMode::Server(Rc::new(RefCell::new(Some(receiver)))))
}

fn run_application(mode: LaunchMode) -> glib::ExitCode {
    let argv0 = env::args()
        .next()
        .unwrap_or_else(|| "ioexplorer-spotlight".to_string());
    let app = gtk::Application::builder()
        .application_id(SPOTLIGHT_APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let live_config = selector::install_live_config(&app);

    app.connect_activate(move |app| {
        let live = live_config.borrow().clone();
        let config = live
            .as_ref()
            .map(|live| live.config().spotlight.clone())
            .unwrap_or_default();

        let window = SpotlightWindow::new(app, SpotlightRuntime::resolve(config), mode.is_server());

        // The server outlives any number of config edits, so it re-resolves
        // rather than making the user restart the daemon.
        if let Some(live) = &live {
            let weak_window = Rc::downgrade(&window);
            live.connect_changed(move |change| {
                if change.spotlight_changed()
                    && let Some(window) = weak_window.upgrade()
                {
                    window.apply_config(change.config.spotlight.clone());
                }
            });
        }

        match &mode {
            LaunchMode::Server(receiver) => {
                window.install_tick(receiver.borrow_mut().take());
                window.hide();
            }
            LaunchMode::OneShot => {
                window.install_tick(None);
                window.show();
            }
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

/// The spotlight window has no placement options, so a toggle carries no payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToggleRequest;

impl ToggleMessage for ToggleRequest {
    fn serialize(&self) -> String {
        "toggle\n".to_string()
    }

    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        (parts.next()? == "toggle").then_some(())?;
        parts.next().is_none().then_some(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SpotlightArgs {
    server: bool,
}

impl SpotlightArgs {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut server = false;

        for arg in args {
            match arg.as_str() {
                "--server" => server = true,
                "--help" => {
                    return Err("Usage: ioexplorer-spotlight [--server]".to_string());
                }
                _ => return Err(format!("Unknown argument: {arg}")),
            }
        }

        Ok(Self { server })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Result<SpotlightArgs, String> {
        SpotlightArgs::parse(values.iter().map(|value| (*value).to_string()))
    }

    #[test]
    fn parses_the_server_flag() {
        assert_eq!(args(&[]), Ok(SpotlightArgs { server: false }));
        assert_eq!(args(&["--server"]), Ok(SpotlightArgs { server: true }));
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(args(&["--nope"]).is_err());
        assert!(args(&["--help"]).is_err());
    }

    #[test]
    fn toggle_requests_round_trip() {
        let parsed = ToggleRequest::parse(&ToggleRequest.serialize());

        assert_eq!(parsed, Some(ToggleRequest));
    }

    #[test]
    fn rejects_malformed_toggle_requests() {
        assert!(ToggleRequest::parse("").is_none());
        assert!(ToggleRequest::parse("open\n").is_none());
        assert!(ToggleRequest::parse("toggle extra\n").is_none());
    }
}
