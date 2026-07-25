//! Window placement for the spotlight overlay.
//!
//! The layer surface is anchored to all four edges, which per the layer-shell
//! protocol stretches it to exactly the output size. That removes any need to
//! detect which monitor we are on for the common case: the card is a child with
//! `halign: Center` / `valign: Start`, so the "slightly above centre" position
//! is just a top margin, and results grow downward while the entry stays put.

use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

/// Smallest gap between the top of the screen and the card.
const MIN_TOP_MARGIN: i32 = 24;
/// Used only until the window has a real allocation.
const FALLBACK_SCREEN_HEIGHT: i32 = 1080;

/// Configures the window as a full-output overlay. Returns false when the
/// compositor has no layer-shell support.
pub fn configure_layer_shell(window: &gtk::ApplicationWindow) -> bool {
    if !gtk4_layer_shell::is_supported() {
        tracing::warn!("gtk4-layer-shell unsupported, falling back to a plain window");
        return false;
    }

    window.init_layer_shell();
    window.set_namespace(Some("ioexplorer-spotlight"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
        window.set_margin(edge, 0);
    }
    window.set_exclusive_zone(-1);

    true
}

/// Positions the card `ratio` of the way down the screen.
pub fn apply_top_offset(window: &gtk::ApplicationWindow, card: &gtk::Box, ratio: f64) {
    let height = match window.height() {
        height if height > 0 => height,
        _ => monitor_height_hint().unwrap_or(FALLBACK_SCREEN_HEIGHT),
    };

    let margin = (f64::from(height) * ratio).round() as i32;
    card.set_margin_top(margin.max(MIN_TOP_MARGIN));
}

/// Tallest monitor height, in logical pixels — the same units layer-shell
/// margins and `WidgetExt::height` use, so no scale-factor maths is needed.
fn monitor_height_hint() -> Option<i32> {
    let display = gtk::gdk::Display::default()?;
    display
        .monitors()
        .iter::<gtk::gdk::Monitor>()
        .flatten()
        .map(|monitor| monitor.geometry().height())
        .max()
}
