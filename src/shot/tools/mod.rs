//! Tools: what a press, a drag and a release mean on the frozen screen.
//!
//! Every tool — selecting a region, drawing with a pen — is one [`Tool`], and
//! the overlay knows nothing about any of them in particular. It forwards the
//! pointer, draws whatever the active tool previews or highlights, and acts on
//! the [`Outcome`] a release returns. Adding a tool is therefore:
//!
//! 1. a new module here with a type implementing [`Tool`], plus an
//!    [`Annotation`] if it leaves marks on the image;
//! 2. one line in [`all`], which is also what puts it on the toolbar and binds
//!    its shortcut.
//!
//! All coordinates are global logical layout coordinates, the same space the
//! outputs and windows are described in, so a mark that crosses two screens is
//! still one mark.

mod pen;
mod select;

use gtk::{cairo, gdk};

use super::{
    capture::FrozenOutput,
    compositor::Scene,
    geometry::{Point, Rect},
};

pub use pen::PenTool;
pub use select::{SelectTool, target_at};

/// A finished mark on the image. Drawn onto the overlay while editing and onto
/// the final shot, through the same code, so what is seen is what is saved.
pub trait Annotation {
    /// Paints the mark. The context is in global logical coordinates.
    fn draw(&self, cr: &cairo::Context);

    /// Everything the mark can touch, including stroke width, so the overlay
    /// can skip screens it is not on and clip the rest.
    fn bounds(&self) -> Rect;
}

/// The colour and weight shared by every drawing tool, so switching from one
/// to another keeps the pen the user already picked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub color: gdk::RGBA,
    pub width: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: PALETTE[0],
            width: WIDTHS[1],
        }
    }
}

/// The swatches offered on the toolbar.
pub const PALETTE: [gdk::RGBA; 7] = [
    gdk::RGBA::new(0.937, 0.267, 0.267, 1.0),
    gdk::RGBA::new(0.976, 0.588, 0.157, 1.0),
    gdk::RGBA::new(0.984, 0.827, 0.180, 1.0),
    gdk::RGBA::new(0.290, 0.816, 0.412, 1.0),
    gdk::RGBA::new(0.263, 0.588, 0.980, 1.0),
    gdk::RGBA::new(1.0, 1.0, 1.0, 1.0),
    gdk::RGBA::new(0.08, 0.08, 0.1, 1.0),
];

/// Stroke weights offered on the toolbar, in logical pixels.
pub const WIDTHS: [f64; 3] = [2.0, 4.0, 8.0];

/// What a tool can see while handling input.
pub struct ToolContext<'a> {
    pub outputs: &'a [FrozenOutput],
    pub scene: &'a Scene,
    pub style: Style,
    /// Held modifiers, for constraints such as Shift-to-square.
    pub modifiers: gdk::ModifierType,
}

impl ToolContext<'_> {
    pub fn shift(&self) -> bool {
        self.modifiers.contains(gdk::ModifierType::SHIFT_MASK)
    }

    /// The size `area` will have in the saved image, in device pixels. Shown on
    /// the selection label, where logical pixels would lie on a HiDPI screen.
    pub fn pixel_size(&self, area: Rect) -> (i64, i64) {
        let area = area.snapped_out();
        let scale = self
            .outputs
            .iter()
            .filter(|output| output.rect.intersects(&area))
            .map(FrozenOutput::pixel_scale)
            .fold(1.0_f64, f64::max);
        (
            (area.width * scale).round() as i64,
            (area.height * scale).round() as i64,
        )
    }
}

/// What a release asks the overlay to do.
pub enum Outcome {
    /// Nothing further; the tool may still be showing something.
    None,
    /// Keep this mark on the image.
    Annotate(Box<dyn Annotation>),
    /// Take the shot of this area.
    Capture(Rect),
}

/// A region the overlay should emphasise: everything else is dimmed and this
/// is outlined and labelled.
#[derive(Clone, Debug, PartialEq)]
pub struct Highlight {
    pub rect: Rect,
    pub label: String,
    /// Wash the region in the accent colour. For a target picked by hovering,
    /// where the question is "what would a click take"; not for a band being
    /// dragged, where the user is judging the exact pixels inside it.
    pub tint: bool,
}

pub trait Tool {
    /// Toolbar label.
    fn label(&self) -> &'static str;

    /// One-line description for the toolbar tooltip.
    fn description(&self) -> &'static str;

    /// The key that selects this tool, lower case.
    fn shortcut(&self) -> char;

    /// Cursor name while the tool is active.
    fn cursor(&self) -> &'static str {
        "crosshair"
    }

    /// Whether the colour and width controls apply to this tool.
    fn uses_style(&self) -> bool {
        false
    }

    /// Primary button went down.
    fn press(&mut self, ctx: &ToolContext, at: Point);

    /// The pointer moved with the button held — also called again with the
    /// last position when a modifier changes, so constraints apply immediately.
    fn drag(&mut self, ctx: &ToolContext, at: Point);

    /// Primary button came up.
    fn release(&mut self, ctx: &ToolContext, at: Point) -> Outcome;

    /// Abandons whatever is in progress. Returns whether there was anything.
    fn cancel(&mut self) -> bool;

    /// Whether a press is currently being handled.
    fn in_progress(&self) -> bool;

    /// The mark being drawn, before it is committed.
    fn preview(&self) -> Option<&dyn Annotation> {
        None
    }

    /// What to emphasise given where the pointer is.
    fn highlight(&self, _ctx: &ToolContext, _pointer: Option<Point>) -> Option<Highlight> {
        None
    }
}

/// Every tool, in toolbar order. The first is active when the overlay opens.
pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SelectTool::default()),
        Box::new(PenTool::default()),
    ]
}

/// The tools offered when picking an area to record: selection only, kept to
/// one screen. Marks would be meaningless — they are not drawn into a live
/// recording.
pub fn for_recording() -> Vec<Box<dyn Tool>> {
    vec![Box::new(SelectTool::confined())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_distinct_shortcut() {
        let tools = all();
        let mut keys: Vec<char> = tools.iter().map(|tool| tool.shortcut()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), tools.len());
    }

    /// Shortcuts that would collide with the overlay's own capture keys.
    #[test]
    fn no_tool_shadows_a_capture_shortcut() {
        for tool in all() {
            assert!(
                !super::super::overlay::RESERVED_KEYS.contains(&tool.shortcut()),
                "{} uses a reserved key",
                tool.label()
            );
        }
    }
}
