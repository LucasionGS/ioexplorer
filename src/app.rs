use std::env;

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
        if let Some(path) = files.first().and_then(gio::File::path) {
            window.navigate_to_path(path);
        }
        window.present();
    });

    app.run()
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}
