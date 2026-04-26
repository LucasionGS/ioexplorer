mod app;
mod bookmarks;
mod config;
mod providers;
mod state;
mod theme;
mod ui;

fn main() -> glib::ExitCode {
    app::run()
}
