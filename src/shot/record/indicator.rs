//! The on-screen recording indicator: a blinking dot, the elapsed time, and a
//! Stop button, in a corner of a screen that is not being recorded.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::shot::{capture::OutputInfo, geometry::Rect};

/// Room the indicator is assumed to take, top right, when deciding whether it
/// would land inside the recorded area.
const FOOTPRINT: (f64, f64) = (260.0, 72.0);
const MARGIN: i32 = 14;

pub struct Indicator {
    window: gtk::ApplicationWindow,
    timer: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Indicator {
    pub fn show(
        app: &gtk::Application,
        monitor: &gdk::Monitor,
        started: Instant,
        microphone: bool,
        on_stop: impl Fn() + 'static,
    ) -> Self {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["shot-indicator-window"])
            .decorated(false)
            .resizable(false)
            .build();

        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_namespace(Some("ioexplorer-shot-indicator"));
            window.set_monitor(Some(monitor));
            window.set_layer(Layer::Overlay);
            // Never takes focus: typing must keep going to whatever is being
            // recorded.
            window.set_keyboard_mode(KeyboardMode::None);
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Right, true);
            window.set_margin(Edge::Top, MARGIN);
            window.set_margin(Edge::Right, MARGIN);
            window.set_exclusive_zone(0);
        } else {
            window.set_title(Some("Recording"));
        }

        let pill = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["shot-indicator"])
            .build();
        let dot = gtk::Label::builder()
            .label("●")
            .css_classes(["shot-indicator-dot"])
            .build();
        let time = gtk::Label::builder()
            .label(format_elapsed(Duration::ZERO))
            .css_classes(["shot-indicator-time"])
            .build();
        pill.append(&dot);
        pill.append(&time);
        if microphone {
            let mic = gtk::Image::builder()
                .icon_name("audio-input-microphone-symbolic")
                .tooltip_text("The microphone is being recorded")
                .build();
            pill.append(&mic);
        }
        let stop = gtk::Button::builder()
            .label("Stop")
            .tooltip_text("Stop recording and save it")
            .focus_on_click(false)
            .build();
        stop.connect_clicked(move |_| on_stop());
        pill.append(&stop);
        window.set_child(Some(&pill));

        let timer = Rc::new(RefCell::new(Some(glib::timeout_add_local(
            Duration::from_millis(500),
            move || {
                time.set_label(&format_elapsed(started.elapsed()));
                if dot.has_css_class("dim") {
                    dot.remove_css_class("dim");
                } else {
                    dot.add_css_class("dim");
                }
                glib::ControlFlow::Continue
            },
        ))));

        window.present();
        Self { window, timer }
    }

    pub fn close(&self) {
        // Taken, so a second close cannot remove an already-removed source,
        // which GLib turns into a process abort.
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        self.window.destroy();
    }
}

/// `0:07`, `12:34`, `1:02:03`.
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Picks the output to show the indicator on, so it is never in the video.
///
/// Any screen other than the recorded one will do, preferring the one the
/// pointer is on, where it will be seen. With a single screen it can still go
/// in the corner when that corner is outside the recorded area; otherwise
/// there is nowhere to put it, and there is no indicator.
pub fn choose_output<'a>(
    outputs: &'a [OutputInfo],
    recorded: &OutputInfo,
    area: Rect,
    preferred: Option<&str>,
) -> Option<&'a OutputInfo> {
    let others: Vec<&OutputInfo> = outputs
        .iter()
        .filter(|output| output.name != recorded.name)
        .collect();
    if !others.is_empty() {
        return others
            .iter()
            .find(|output| Some(output.name.as_str()) == preferred)
            .or_else(|| others.first())
            .copied();
    }

    let corner = Rect::new(
        recorded.rect.right() - FOOTPRINT.0,
        recorded.rect.y,
        FOOTPRINT.0,
        FOOTPRINT.1,
    );
    outputs
        .iter()
        .find(|output| output.name == recorded.name)
        .filter(|_| !corner.intersects(&area))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, x: f64) -> OutputInfo {
        OutputInfo {
            name: name.to_string(),
            rect: Rect::new(x, 0.0, 1920.0, 1080.0),
        }
    }

    #[test]
    fn elapsed_time_formats() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "0:07");
        assert_eq!(format_elapsed(Duration::from_secs(754)), "12:34");
        assert_eq!(format_elapsed(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn another_screen_is_preferred_and_the_pointer_screen_first() {
        let outputs = vec![
            output("DP-1", 0.0),
            output("DP-2", 1920.0),
            output("DP-3", 3840.0),
        ];
        let recorded = &outputs[0];

        let chosen = choose_output(&outputs, recorded, recorded.rect, Some("DP-3"));
        assert_eq!(chosen.map(|output| output.name.as_str()), Some("DP-3"));

        let fallback = choose_output(&outputs, recorded, recorded.rect, Some("DP-1"));
        assert_eq!(fallback.map(|output| output.name.as_str()), Some("DP-2"));
    }

    #[test]
    fn a_single_screen_uses_the_corner_only_when_it_is_not_recorded() {
        let outputs = vec![output("eDP-1", 0.0)];
        let recorded = &outputs[0];

        let small = Rect::new(100.0, 300.0, 600.0, 400.0);
        assert!(choose_output(&outputs, recorded, small, None).is_some());

        assert!(choose_output(&outputs, recorded, recorded.rect, None).is_none());
        let under_corner = Rect::new(1500.0, 0.0, 400.0, 300.0);
        assert!(choose_output(&outputs, recorded, under_corner, None).is_none());
    }
}
