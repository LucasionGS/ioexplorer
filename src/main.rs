mod app;
mod config;
mod providers;
mod theme;
mod ui;

fn main() -> glib::ExitCode {
    app::run()
}
