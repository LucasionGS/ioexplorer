use std::{env, path::PathBuf};

use gtk::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use crate::{config::AppConfig, selector, theme, ui::window::AppWindow};

pub const APP_ID: &str = "io.github.ionix.IoExplorer";

pub fn run() -> glib::ExitCode {
    init_logging();

    let args = env::args().skip(1).collect::<Vec<_>>();
    if selector::is_chooser_invocation(&args) {
        return selector::run_from_args(&args);
    }

    if let Some(paths) = select_paths_from_args(&args) {
        return run_select(paths);
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_startup(|_| {
        let config = AppConfig::load();
        theme::install(&config);
    });

    app.connect_activate(|app| {
        let config = AppConfig::load();
        let window = AppWindow::new(app, config);
        window.present();
    });

    app.connect_open(|app, files, _hint| {
        let config = AppConfig::load();
        let window = AppWindow::new(app, config);
        let paths = files.iter().filter_map(gio::File::path).collect::<Vec<_>>();
        window.open_paths(paths);
        window.present();
    });

    app.run()
}

fn run_select(paths: Vec<PathBuf>) -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_startup(|_| {
        let config = AppConfig::load();
        theme::install(&config);
    });

    app.connect_activate(move |app| {
        let config = AppConfig::load();
        let window = AppWindow::new(app, config);
        window.reveal_paths(paths.clone());
        window.present();
    });

    app.run_with_args(&["ioexplorer"])
}

fn select_paths_from_args(args: &[String]) -> Option<Vec<PathBuf>> {
    let select_index = args.iter().position(|arg| arg == "--select")?;
    let paths = args[select_index + 1..]
        .iter()
        .filter_map(|arg| gio::File::for_commandline_arg(arg).path())
        .collect();
    Some(paths)
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}
