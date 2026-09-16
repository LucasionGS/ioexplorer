//! A widget that renders through a closure into a `GtkSnapshot`.
//!
//! `GtkDrawingArea` would be the obvious choice, but it rasterises everything
//! through cairo into a full-surface image and re-uploads that on every frame.
//! The overlay redraws on every pointer motion across a full-output frozen
//! frame, so that is a 4K upload per mouse movement on a large screen. Drawing
//! into a snapshot instead keeps the frozen frame as a GPU texture node and the
//! dimming as colour nodes; only the pen strokes go through cairo, and only
//! within their own bounds.

use std::cell::RefCell;

use gtk::{glib, prelude::*, subclass::prelude::*};

type DrawFunc = Box<dyn Fn(&Canvas, &gtk::Snapshot, f64, f64)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Canvas {
        pub draw: RefCell<Option<DrawFunc>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "IoExplorerShotCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Canvas {}

    impl WidgetImpl for Canvas {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(draw) = self.draw.borrow().as_ref() {
                draw(
                    &widget,
                    snapshot,
                    f64::from(widget.width()),
                    f64::from(widget.height()),
                );
            }
        }
    }
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Canvas {
    pub fn new() -> Self {
        let canvas: Self = glib::Object::new();
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        canvas
    }

    /// Sets what the widget paints, given itself and its current size. The
    /// widget is passed in rather than captured, since a closure stored on the
    /// widget holding a strong reference to it would never be freed.
    pub fn set_draw_func(&self, draw: impl Fn(&Canvas, &gtk::Snapshot, f64, f64) + 'static) {
        *self.imp().draw.borrow_mut() = Some(Box::new(draw));
        self.queue_draw();
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}
