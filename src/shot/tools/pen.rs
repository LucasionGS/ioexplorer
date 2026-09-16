//! Freehand pen.

use gtk::{cairo, prelude::GdkCairoContextExt};

use super::{Annotation, Outcome, Style, Tool, ToolContext};
use crate::shot::geometry::{Point, Rect};

/// Points closer than this to the previous one are dropped. The pointer
/// reports far more often than a stroke changes shape, and every kept point is
/// redrawn on every frame of the stroke.
const MIN_SEGMENT: f64 = 0.75;

#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    points: Vec<Point>,
    style: Style,
}

impl Stroke {
    pub fn new(start: Point, style: Style) -> Self {
        Self {
            points: vec![start],
            style,
        }
    }

    pub fn push(&mut self, point: Point) {
        if self
            .points
            .last()
            .is_none_or(|last| last.distance(point) >= MIN_SEGMENT)
        {
            self.points.push(point);
        }
    }

    #[cfg(test)]
    pub fn points(&self) -> &[Point] {
        &self.points
    }
}

impl Annotation for Stroke {
    /// Smooths the polyline by running a quadratic curve through the midpoint
    /// of every segment, with the recorded points as control points. Raw
    /// pointer samples drawn as straight segments show visible corners on any
    /// quick curve.
    fn draw(&self, cr: &cairo::Context) {
        let Some(first) = self.points.first() else {
            return;
        };

        cr.set_source_color(&self.style.color);
        cr.set_line_width(self.style.width);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);

        if self.points.len() == 1 {
            cr.arc(
                first.x,
                first.y,
                self.style.width / 2.0,
                0.0,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
            return;
        }

        cr.move_to(first.x, first.y);
        let mut current = *first;
        for pair in self.points.windows(2).skip(1) {
            let control = pair[0];
            let end = Point::new((pair[0].x + pair[1].x) / 2.0, (pair[0].y + pair[1].y) / 2.0);
            // Cairo has only cubic curves; this is the exact cubic equivalent
            // of the quadratic `current → control → end`.
            cr.curve_to(
                current.x + (control.x - current.x) * 2.0 / 3.0,
                current.y + (control.y - current.y) * 2.0 / 3.0,
                end.x + (control.x - end.x) * 2.0 / 3.0,
                end.y + (control.y - end.y) * 2.0 / 3.0,
                end.x,
                end.y,
            );
            current = end;
        }
        let last = self.points[self.points.len() - 1];
        cr.line_to(last.x, last.y);
        let _ = cr.stroke();
    }

    fn bounds(&self) -> Rect {
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for point in &self.points {
            min_x = min_x.min(point.x);
            min_y = min_y.min(point.y);
            max_x = max_x.max(point.x);
            max_y = max_y.max(point.y);
        }
        if self.points.is_empty() {
            return Rect::default();
        }
        // Half the width either side, plus a pixel for antialiasing.
        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y).inflate(self.style.width / 2.0 + 1.0)
    }
}

#[derive(Default)]
pub struct PenTool {
    stroke: Option<Stroke>,
}

impl Tool for PenTool {
    fn label(&self) -> &'static str {
        "Pen"
    }

    fn description(&self) -> &'static str {
        "Draw freehand. Hold Shift for a straight line"
    }

    fn shortcut(&self) -> char {
        'p'
    }

    fn uses_style(&self) -> bool {
        true
    }

    fn press(&mut self, ctx: &ToolContext, at: Point) {
        self.stroke = Some(Stroke::new(at, ctx.style));
    }

    fn drag(&mut self, ctx: &ToolContext, at: Point) {
        let Some(stroke) = &mut self.stroke else {
            return;
        };
        if ctx.shift() {
            // A straight line keeps only its ends; releasing Shift resumes
            // freehand from wherever the line reached.
            let start = stroke.points[0];
            stroke.points.clear();
            stroke.points.push(start);
            stroke.points.push(at);
        } else {
            stroke.push(at);
        }
    }

    fn release(&mut self, ctx: &ToolContext, at: Point) -> Outcome {
        self.drag(ctx, at);
        match self.stroke.take() {
            Some(stroke) => Outcome::Annotate(Box::new(stroke)),
            None => Outcome::None,
        }
    }

    fn cancel(&mut self) -> bool {
        self.stroke.take().is_some()
    }

    fn in_progress(&self) -> bool {
        self.stroke.is_some()
    }

    fn preview(&self) -> Option<&dyn Annotation> {
        self.stroke.as_ref().map(|stroke| stroke as &dyn Annotation)
    }
}

#[cfg(test)]
mod tests {
    use gtk::gdk;

    use super::*;
    use crate::shot::compositor::Scene;

    fn ctx<'a>(scene: &'a Scene, modifiers: gdk::ModifierType) -> ToolContext<'a> {
        ToolContext {
            outputs: &[],
            scene,
            style: Style {
                color: gdk::RGBA::RED,
                width: 4.0,
            },
            modifiers,
        }
    }

    #[test]
    fn a_stroke_collects_points_and_skips_jitter() {
        let mut stroke = Stroke::new(Point::new(0.0, 0.0), Style::default());
        stroke.push(Point::new(0.1, 0.1));
        stroke.push(Point::new(5.0, 0.0));
        stroke.push(Point::new(10.0, 5.0));

        assert_eq!(stroke.points().len(), 3);
    }

    #[test]
    fn bounds_include_the_stroke_width() {
        let style = Style {
            color: gdk::RGBA::BLACK,
            width: 8.0,
        };
        let mut stroke = Stroke::new(Point::new(10.0, 10.0), style);
        stroke.push(Point::new(30.0, 20.0));

        assert_eq!(stroke.bounds(), Rect::new(5.0, 5.0, 30.0, 20.0));
    }

    #[test]
    fn a_release_commits_the_stroke() {
        let scene = Scene::default();
        let ctx = ctx(&scene, gdk::ModifierType::empty());
        let mut pen = PenTool::default();

        pen.press(&ctx, Point::new(0.0, 0.0));
        pen.drag(&ctx, Point::new(10.0, 10.0));
        assert!(pen.preview().is_some());

        assert!(matches!(
            pen.release(&ctx, Point::new(20.0, 10.0)),
            Outcome::Annotate(_)
        ));
        assert!(!pen.in_progress());
        assert!(pen.preview().is_none());
    }

    #[test]
    fn shift_draws_a_straight_line() {
        let scene = Scene::default();
        let plain = ctx(&scene, gdk::ModifierType::empty());
        let shifted = ctx(&scene, gdk::ModifierType::SHIFT_MASK);
        let mut pen = PenTool::default();

        pen.press(&plain, Point::new(0.0, 0.0));
        pen.drag(&plain, Point::new(5.0, 9.0));
        pen.drag(&shifted, Point::new(40.0, 0.0));

        let stroke = pen.stroke.as_ref().unwrap();
        assert_eq!(
            stroke.points(),
            &[Point::new(0.0, 0.0), Point::new(40.0, 0.0)]
        );
    }

    /// Drawing must actually put paint down, including a single-click dot.
    #[test]
    fn strokes_and_dots_paint_pixels() {
        for points in [
            vec![Point::new(10.0, 10.0)],
            vec![Point::new(2.0, 2.0), Point::new(18.0, 18.0)],
        ] {
            let mut stroke = Stroke::new(
                points[0],
                Style {
                    color: gdk::RGBA::RED,
                    width: 6.0,
                },
            );
            for point in &points[1..] {
                stroke.push(*point);
            }

            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 20, 20).unwrap();
            {
                let cr = cairo::Context::new(&surface).unwrap();
                stroke.draw(&cr);
            }
            let data = surface.data().unwrap();
            assert!(
                data.chunks(4).any(|pixel| pixel[3] > 0),
                "nothing was drawn"
            );
        }
    }
}
