use std::fs;

use crate::config::AppConfig;

const BUNDLED_CSS: &str = include_str!("../data/styles/ioexplorer.css");

pub fn install(config: &AppConfig) {
    let Some(display) = gtk::gdk::Display::default() else {
        tracing::warn!("no display available for CSS provider");
        return;
    };

    let provider = gtk::CssProvider::new();
    provider.load_from_string(BUNDLED_CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    if let Some(path) = &config.custom_css {
        match fs::read_to_string(path) {
            Ok(css) => {
                let custom = gtk::CssProvider::new();
                custom.load_from_string(&css);
                gtk::style_context_add_provider_for_display(
                    &display,
                    &custom,
                    gtk::STYLE_PROVIDER_PRIORITY_USER,
                );
            }
            Err(error) => tracing::warn!(?path, %error, "failed to load custom CSS"),
        }
    }
}
