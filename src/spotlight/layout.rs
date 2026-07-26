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
/// Smallest gap left below a tall card, so it never runs off the output.
const MIN_BOTTOM_MARGIN: i32 = 24;
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

/// Positions `stage` — the card and anything flanking it — `ratio` of the way
/// down the screen, leaving room for a *fully grown* card rather than the
/// current one.
///
/// `max_card_height` is deliberately a fixed budget, not a measurement. The
/// margin controls how much space the card gets, which controls how tall it is
/// allocated — so clamping against the card's live height makes the offset
/// depend on a value it is itself an input to, and the two oscillate. Budgeting
/// for the worst case makes the result a pure function of the output size: the
/// entry sits still while the reply grows downward beneath it, which is the
/// behaviour we want anyway.
///
/// Returns the margin applied so callers can detect a real change.
pub fn apply_top_offset(
    window: &gtk::ApplicationWindow,
    stage: &gtk::Box,
    ratio: f64,
    max_card_height: i32,
) -> i32 {
    let height = match window.height() {
        height if height > 0 => height,
        _ => monitor_height_hint().unwrap_or(FALLBACK_SCREEN_HEIGHT),
    };

    let ideal = (f64::from(height) * ratio).round() as i32;
    let highest = (height - max_card_height - MIN_BOTTOM_MARGIN).max(MIN_TOP_MARGIN);
    let margin = ideal.clamp(MIN_TOP_MARGIN, highest);

    if stage.margin_top() != margin {
        stage.set_margin_top(margin);
    }
    margin
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
