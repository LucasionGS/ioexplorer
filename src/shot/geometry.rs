//! Rectangles in the compositor's global layout space.
//!
//! Everything the screenshot tool reasons about — outputs, windows, the rubber
//! band, pen strokes — lives in one coordinate system: logical pixels, with the
//! origin wherever the compositor put the top-left of its layout. Converting to
//! device pixels happens exactly once, when the final image is rendered, so a
//! selection that spans a 1× and a 2× screen is still a single rectangle.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Point) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The rectangle spanned by two corners, in either order — a band dragged
    /// up and to the left is the same rectangle as one dragged down and right.
    pub fn from_corners(a: Point, b: Point) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (a.x - b.x).abs(),
            height: (a.y - b.y).abs(),
        }
    }

    /// Like [`from_corners`](Self::from_corners), but forced to a 1:1 ratio.
    ///
    /// The square takes the *longer* of the two sides and grows away from the
    /// anchor in whichever direction the pointer went, so the corner under the
    /// cursor stays on the cursor's side of the anchor rather than flipping.
    pub fn square_from_corners(anchor: Point, pointer: Point) -> Self {
        let dx = pointer.x - anchor.x;
        let dy = pointer.y - anchor.y;
        let side = dx.abs().max(dy.abs());
        let corner = Point::new(
            anchor.x + side.copysign(if dx == 0.0 { 1.0 } else { dx }),
            anchor.y + side.copysign(if dy == 0.0 { 1.0 } else { dy }),
        );
        Self::from_corners(anchor, corner)
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Half-open, so a point on the shared edge of two adjacent outputs belongs
    /// to exactly one of them.
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.intersection(other).is_some()
    }

    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    /// The bounding box of every rectangle, or `None` for an empty set.
    pub fn bounding<'a>(rects: impl IntoIterator<Item = &'a Rect>) -> Option<Rect> {
        rects
            .into_iter()
            .filter(|rect| !rect.is_empty())
            .fold(None, |acc: Option<Rect>, rect| {
                Some(acc.map_or(*rect, |acc| acc.union(rect)))
            })
    }

    /// Snaps outwards to whole logical pixels, so a capture never cuts a pixel
    /// row in half and blurs its edge.
    pub fn snapped_out(&self) -> Rect {
        let x = self.x.floor();
        let y = self.y.floor();
        Rect::new(x, y, self.right().ceil() - x, self.bottom().ceil() - y)
    }

    pub fn inflate(&self, by: f64) -> Rect {
        Rect::new(
            self.x - by,
            self.y - by,
            self.width + by * 2.0,
            self.height + by * 2.0,
        )
    }

    pub fn translate(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_normalise_whichever_way_the_band_was_dragged() {
        let down_right = Rect::from_corners(Point::new(10.0, 20.0), Point::new(110.0, 70.0));
        let up_left = Rect::from_corners(Point::new(110.0, 70.0), Point::new(10.0, 20.0));

        assert_eq!(down_right, Rect::new(10.0, 20.0, 100.0, 50.0));
        assert_eq!(up_left, down_right);
    }

    #[test]
    fn a_square_takes_the_longer_side() {
        let square = Rect::square_from_corners(Point::new(0.0, 0.0), Point::new(100.0, 40.0));
        assert_eq!(square, Rect::new(0.0, 0.0, 100.0, 100.0));
    }

    /// Dragging up and to the left must grow the square up and to the left,
    /// not snap it back below the anchor.
    #[test]
    fn a_square_grows_towards_the_pointer() {
        let square = Rect::square_from_corners(Point::new(200.0, 200.0), Point::new(150.0, 120.0));
        assert_eq!(square, Rect::new(120.0, 120.0, 80.0, 80.0));

        let mixed = Rect::square_from_corners(Point::new(200.0, 200.0), Point::new(260.0, 190.0));
        assert_eq!(mixed, Rect::new(200.0, 140.0, 60.0, 60.0));
    }

    #[test]
    fn a_square_along_one_axis_still_has_both_sides() {
        let square = Rect::square_from_corners(Point::new(0.0, 0.0), Point::new(30.0, 0.0));
        assert_eq!(square, Rect::new(0.0, 0.0, 30.0, 30.0));
    }

    #[test]
    fn containment_is_half_open_so_shared_edges_have_one_owner() {
        let left = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let right = Rect::new(1920.0, 0.0, 1920.0, 1080.0);
        let edge = Point::new(1920.0, 500.0);

        assert!(!left.contains(edge));
        assert!(right.contains(edge));
    }

    #[test]
    fn intersection_of_disjoint_or_touching_rects_is_none() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(a.intersection(&Rect::new(10.0, 0.0, 10.0, 10.0)).is_none());
        assert!(a.intersection(&Rect::new(50.0, 50.0, 10.0, 10.0)).is_none());
        assert_eq!(
            a.intersection(&Rect::new(5.0, 5.0, 10.0, 10.0)),
            Some(Rect::new(5.0, 5.0, 5.0, 5.0))
        );
    }

    #[test]
    fn bounding_box_of_an_irregular_layout() {
        // A portrait screen to the left of two stacked landscape ones.
        let outputs = [
            Rect::new(0.0, 630.0, 1080.0, 1920.0),
            Rect::new(1080.0, 0.0, 1920.0, 1080.0),
            Rect::new(1080.0, 1080.0, 1920.0, 1080.0),
        ];
        assert_eq!(
            Rect::bounding(&outputs),
            Some(Rect::new(0.0, 0.0, 3000.0, 2550.0))
        );
        assert_eq!(Rect::bounding(&[]), None);
    }

    #[test]
    fn snapping_never_shrinks_the_selection() {
        let snapped = Rect::new(10.4, 20.6, 99.2, 10.1).snapped_out();
        assert_eq!(snapped, Rect::new(10.0, 20.0, 100.0, 11.0));
    }
}
