//! Where the menu opens: beside the pointer, on the screen the pointer is on,
//! and never hanging off an edge.
//!
//! The menu is a card inside a layer surface that covers the whole output, so
//! placing it is only a matter of the card's margins within that output.

use std::process::Command;

use serde::Deserialize;

use crate::spotlight::windows::{self, Compositor};

/// Gap between the pointer and the card, so the pointer does not sit on a cell
/// and select it the moment the menu appears.
const POINTER_GAP: i32 = 14;
/// Smallest gap between the card and a screen edge.
const EDGE_GAP: i32 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// An output's area in global logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Screen {
    fn contains(&self, point: Point) -> bool {
        point.x >= f64::from(self.x)
            && point.y >= f64::from(self.y)
            && point.x < f64::from(self.x + self.width)
            && point.y < f64::from(self.y + self.height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Placement {
    /// On `screen`, with the card's top-left corner at `left`, `top` within it.
    At { screen: usize, left: i32, top: i32 },
    /// The pointer is unknown: centred on whichever output the compositor
    /// gives the surface.
    Centered,
}

/// Places a `card`-sized menu down and to the right of the pointer, flipping
/// to the other side of it where that would run off the screen.
pub fn place(screens: &[Screen], pointer: Option<Point>, card: (i32, i32)) -> Placement {
    let Some(pointer) = pointer else {
        return Placement::Centered;
    };
    let Some((index, screen)) = screens
        .iter()
        .enumerate()
        .find(|(_, screen)| screen.contains(pointer))
    else {
        return Placement::Centered;
    };

    let x = pointer.x.round() as i32 - screen.x;
    let y = pointer.y.round() as i32 - screen.y;
    let (width, height) = card;

    Placement::At {
        screen: index,
        left: beside(x, width, screen.width),
        top: beside(y, height, screen.height),
    }
}

/// One axis: after the pointer if it fits, else before it, else as close as
/// the screen allows.
fn beside(pointer: i32, size: i32, extent: i32) -> i32 {
    let after = pointer + POINTER_GAP;
    let before = pointer - POINTER_GAP - size;
    let start = if after + size + EDGE_GAP <= extent {
        after
    } else if before >= EDGE_GAP {
        before
    } else {
        extent - size - EDGE_GAP
    };
    start.max(EDGE_GAP.min(extent - size).max(0))
}

/// Where the pointer is, in global logical coordinates, when the compositor
/// will say. Wayland keeps this from ordinary clients.
pub fn pointer() -> Option<Point> {
    match windows::detect() {
        Compositor::Hyprland => hyprland_pointer(),
        // sway has no pointer query; the menu opens centred there.
        Compositor::Sway | Compositor::Unsupported => None,
    }
}

fn hyprland_pointer() -> Option<Point> {
    #[derive(Deserialize)]
    struct Cursor {
        x: f64,
        y: f64,
    }

    let output = Command::new("hyprctl")
        .args(["-j", "cursorpos"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let cursor: Cursor = serde_json::from_slice(&output.stdout).ok()?;
    Some(Point {
        x: cursor.x,
        y: cursor.y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: Screen = Screen {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const RIGHT: Screen = Screen {
        x: 1920,
        y: 0,
        width: 2560,
        height: 1440,
    };
    const CARD: (i32, i32) = (380, 440);

    fn at(x: f64, y: f64) -> Option<Point> {
        Some(Point { x, y })
    }

    #[test]
    fn opens_below_and_right_of_the_pointer() {
        assert_eq!(
            place(&[LEFT, RIGHT], at(100.0, 200.0), CARD),
            Placement::At {
                screen: 0,
                left: 114,
                top: 214
            }
        );
    }

    #[test]
    fn coordinates_are_relative_to_the_pointer_screen() {
        assert_eq!(
            place(&[LEFT, RIGHT], at(2000.0, 100.0), CARD),
            Placement::At {
                screen: 1,
                left: 94,
                top: 114
            }
        );
    }

    #[test]
    fn flips_away_from_the_far_edges() {
        let Placement::At { left, top, .. } = place(&[LEFT], at(1800.0, 1000.0), CARD) else {
            panic!("placed");
        };
        assert_eq!(left, 1800 - 14 - 380);
        assert_eq!(top, 1000 - 14 - 440);
    }

    #[test]
    fn a_screen_too_small_either_way_still_keeps_it_on_screen() {
        let small = Screen {
            x: 0,
            y: 0,
            width: 1920,
            height: 600,
        };
        let Placement::At { top, .. } = place(&[small], at(10.0, 300.0), CARD) else {
            panic!("placed");
        };
        assert_eq!(top, 600 - 440 - 8);
    }

    #[test]
    fn an_unknown_pointer_centres() {
        assert_eq!(place(&[LEFT], None, CARD), Placement::Centered);
        assert_eq!(place(&[LEFT], at(-50.0, 10.0), CARD), Placement::Centered);
    }
}
