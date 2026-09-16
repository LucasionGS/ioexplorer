//! Region selection: drag a rectangle, or click a window or screen to take it
//! whole.

use super::{Highlight, Outcome, Tool, ToolContext};
use crate::shot::geometry::{Point, Rect};

/// Pointer travel, in logical pixels, before a press becomes a drag. Below it,
/// a release is a click and picks whatever is under the pointer — a hand that
/// wobbles by a pixel on click must not produce a 1×1 screenshot.
const DRAG_THRESHOLD: f64 = 4.0;

#[derive(Default)]
pub struct SelectTool {
    anchor: Option<Point>,
    band: Option<Rect>,
}

impl SelectTool {
    fn band_for(ctx: &ToolContext, anchor: Point, at: Point) -> Rect {
        if ctx.shift() {
            Rect::square_from_corners(anchor, at)
        } else {
            Rect::from_corners(anchor, at)
        }
    }
}

/// What a click at `point` would capture: the topmost window there, or else
/// the screen, with a label naming it.
pub fn target_at(ctx: &ToolContext, point: Point) -> Option<(Rect, String)> {
    if let Some(window) = ctx.scene.window_at(point) {
        return Some((window.rect, window.name().to_string()));
    }
    ctx.outputs
        .iter()
        .find(|output| output.rect.contains(point))
        .map(|output| (output.rect, output.name.clone()))
}

impl Tool for SelectTool {
    fn label(&self) -> &'static str {
        "Region"
    }

    fn description(&self) -> &'static str {
        "Drag to select an area, or click a window or screen. Hold Shift for a square"
    }

    fn shortcut(&self) -> char {
        'r'
    }

    fn press(&mut self, _ctx: &ToolContext, at: Point) {
        self.anchor = Some(at);
        self.band = None;
    }

    fn drag(&mut self, ctx: &ToolContext, at: Point) {
        let Some(anchor) = self.anchor else {
            return;
        };
        // Once a band exists it stays a band, even if dragged back to a
        // sliver: shrinking a selection is not changing one's mind about it.
        if self.band.is_some() || anchor.distance(at) >= DRAG_THRESHOLD {
            self.band = Some(Self::band_for(ctx, anchor, at));
        }
    }

    fn release(&mut self, ctx: &ToolContext, at: Point) -> Outcome {
        let Some(anchor) = self.anchor.take() else {
            return Outcome::None;
        };
        let band = self.band.take();

        match band {
            Some(_) => {
                let band = Self::band_for(ctx, anchor, at).snapped_out();
                // A band collapsed to a line has nothing in it to capture.
                if band.width < 1.0 || band.height < 1.0 {
                    Outcome::None
                } else {
                    Outcome::Capture(band)
                }
            }
            None => match target_at(ctx, anchor) {
                Some((rect, _)) => Outcome::Capture(rect),
                None => Outcome::None,
            },
        }
    }

    fn cancel(&mut self) -> bool {
        let had = self.anchor.is_some();
        self.anchor = None;
        self.band = None;
        had
    }

    fn in_progress(&self) -> bool {
        self.anchor.is_some()
    }

    fn highlight(&self, ctx: &ToolContext, pointer: Option<Point>) -> Option<Highlight> {
        if let Some(band) = self.band {
            let (width, height) = ctx.pixel_size(band);
            return Some(Highlight {
                rect: band,
                label: format!("{width} × {height}"),
                tint: false,
            });
        }

        let (rect, name) = target_at(ctx, pointer?)?;
        let (width, height) = ctx.pixel_size(rect);
        Some(Highlight {
            rect,
            label: format!("{name} · {width} × {height}"),
            tint: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use gtk::{gdk, glib, prelude::*};

    use super::*;
    use crate::shot::{
        capture::FrozenOutput,
        compositor::{Scene, VisibleWindow},
        tools::Style,
    };

    fn outputs() -> Vec<FrozenOutput> {
        let image = gdk::MemoryTexture::new(
            200,
            100,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(vec![0_u8; 200 * 100 * 4]),
            800,
        )
        .upcast();
        vec![FrozenOutput {
            name: "DP-1".to_string(),
            rect: Rect::new(0.0, 0.0, 200.0, 100.0),
            image,
        }]
    }

    fn scene() -> Scene {
        Scene {
            windows: vec![VisibleWindow {
                title: "Terminal".to_string(),
                app_id: "kitty".to_string(),
                rect: Rect::new(10.0, 10.0, 50.0, 40.0),
                focused: true,
            }],
            ..Scene::default()
        }
    }

    fn ctx<'a>(
        outputs: &'a [FrozenOutput],
        scene: &'a Scene,
        modifiers: gdk::ModifierType,
    ) -> ToolContext<'a> {
        ToolContext {
            outputs,
            scene,
            style: Style::default(),
            modifiers,
        }
    }

    fn capture_area(outcome: Outcome) -> Option<Rect> {
        match outcome {
            Outcome::Capture(rect) => Some(rect),
            _ => None,
        }
    }

    #[test]
    fn a_click_on_a_window_captures_that_window() {
        let (outputs, scene) = (outputs(), scene());
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::empty());
        let mut tool = SelectTool::default();

        tool.press(&ctx, Point::new(20.0, 20.0));
        tool.drag(&ctx, Point::new(21.0, 21.0));
        let area = capture_area(tool.release(&ctx, Point::new(21.0, 21.0)));

        assert_eq!(area, Some(Rect::new(10.0, 10.0, 50.0, 40.0)));
    }

    #[test]
    fn a_click_on_bare_desktop_captures_the_screen() {
        let (outputs, scene) = (outputs(), scene());
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::empty());
        let mut tool = SelectTool::default();

        tool.press(&ctx, Point::new(150.0, 80.0));
        let area = capture_area(tool.release(&ctx, Point::new(150.0, 80.0)));

        assert_eq!(area, Some(Rect::new(0.0, 0.0, 200.0, 100.0)));
    }

    #[test]
    fn a_drag_captures_the_band_even_over_a_window() {
        let (outputs, scene) = (outputs(), scene());
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::empty());
        let mut tool = SelectTool::default();

        tool.press(&ctx, Point::new(20.0, 20.0));
        tool.drag(&ctx, Point::new(90.0, 60.0));
        let area = capture_area(tool.release(&ctx, Point::new(90.0, 60.0)));

        assert_eq!(area, Some(Rect::new(20.0, 20.0, 70.0, 40.0)));
    }

    #[test]
    fn shift_makes_the_band_square() {
        let (outputs, scene) = (outputs(), scene());
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::SHIFT_MASK);
        let mut tool = SelectTool::default();

        tool.press(&ctx, Point::new(20.0, 20.0));
        tool.drag(&ctx, Point::new(90.0, 40.0));
        let highlight = tool.highlight(&ctx, None).expect("a band");
        assert_eq!(highlight.rect, Rect::new(20.0, 20.0, 70.0, 70.0));
        assert_eq!(highlight.label, "70 × 70");
        assert!(!highlight.tint, "a dragged band shows its pixels untouched");

        let area = capture_area(tool.release(&ctx, Point::new(90.0, 40.0)));
        assert_eq!(area, Some(Rect::new(20.0, 20.0, 70.0, 70.0)));
    }

    #[test]
    fn hovering_highlights_the_window_under_the_pointer() {
        let (outputs, scene) = (outputs(), scene());
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::empty());
        let tool = SelectTool::default();

        let over_window = tool.highlight(&ctx, Some(Point::new(30.0, 30.0))).unwrap();
        assert_eq!(over_window.rect, Rect::new(10.0, 10.0, 50.0, 40.0));
        assert_eq!(over_window.label, "kitty · 50 × 40");
        assert!(over_window.tint, "a hover target is tinted");

        let over_desktop = tool.highlight(&ctx, Some(Point::new(150.0, 90.0))).unwrap();
        assert_eq!(over_desktop.rect, Rect::new(0.0, 0.0, 200.0, 100.0));
    }

    /// The label reports the saved image's size, which on a 2× screen is twice
    /// the logical size.
    #[test]
    fn the_label_counts_device_pixels() {
        let mut outputs = outputs();
        outputs[0].rect = Rect::new(0.0, 0.0, 100.0, 50.0);
        let scene = Scene::default();
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::empty());

        assert_eq!(ctx.pixel_size(Rect::new(0.0, 0.0, 10.0, 10.0)), (20, 20));
    }

    #[test]
    fn cancel_drops_the_band() {
        let (outputs, scene) = (outputs(), scene());
        let ctx = ctx(&outputs, &scene, gdk::ModifierType::empty());
        let mut tool = SelectTool::default();

        tool.press(&ctx, Point::new(0.0, 0.0));
        tool.drag(&ctx, Point::new(50.0, 50.0));
        assert!(tool.cancel());
        assert!(!tool.in_progress());
        assert!(matches!(
            tool.release(&ctx, Point::new(50.0, 50.0)),
            Outcome::None
        ));
    }
}
