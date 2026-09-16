//! The interactive overlay: one layer surface per output showing the frozen
//! frame, a toolbar, and the active tool's marks and highlight.
//!
//! State lives in one [`Session`] shared by every surface, because an action
//! on one screen routinely has to show up on another — a band dragged across
//! two monitors, a hover moving from one to the next. Every surface draws the
//! same global scene, clipped to its own output.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    time::Duration,
};

use gtk::{gdk, glib, graphene, gsk, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use super::{
    canvas::Canvas,
    capture::FrozenOutput,
    compositor::Scene,
    geometry::{Point, Rect},
    tools::{self, Annotation, Highlight, Outcome, Style, Tool, ToolContext},
};

/// Keys the overlay binds itself, which no tool may take as its shortcut.
pub const RESERVED_KEYS: &[char] = &['w', 's', 'a'];

type FinishCallback = Box<dyn FnOnce(Finish)>;

/// How the overlay ended.
pub enum Finish {
    Capture {
        area: Rect,
        annotations: Vec<Box<dyn Annotation>>,
    },
    Cancel,
}

/// Everything outside the emphasised region is darkened by this much.
const DIM: gdk::RGBA = gdk::RGBA::new(0.0, 0.0, 0.0, 0.45);
/// A hairline just inside the accent border, so the outline still reads over
/// content that happens to be the accent colour.
const OUTLINE: gdk::RGBA = gdk::RGBA::new(0.0, 0.0, 0.0, 0.55);
const BORDER_WIDTH: f64 = 2.0;
/// Strength of the accent wash over a hovered window or screen.
const TINT_ALPHA: f32 = 0.12;
const LABEL_BACKGROUND: gdk::RGBA = gdk::RGBA::new(0.05, 0.06, 0.08, 0.88);
const LABEL_TEXT: gdk::RGBA = gdk::RGBA::new(0.96, 0.97, 0.99, 1.0);

struct Session {
    outputs: Vec<FrozenOutput>,
    scene: Scene,
    tools: Vec<Box<dyn Tool>>,
    active: usize,
    annotations: Vec<Box<dyn Annotation>>,
    /// Undone marks, most recent last. Cleared by any new mark.
    undone: Vec<Box<dyn Annotation>>,
    style: Style,
    pointer: Option<Point>,
    modifiers: gdk::ModifierType,
    /// What was last drawn as the highlight, so a hover that stays inside the
    /// same window does not redraw every screen.
    highlight: Option<Highlight>,
}

impl Session {
    /// Runs `f` against the active tool with a context borrowed from the rest
    /// of the session.
    fn with_tool<R>(&mut self, f: impl FnOnce(&mut dyn Tool, &ToolContext) -> R) -> R {
        let Session {
            outputs,
            scene,
            tools,
            active,
            style,
            modifiers,
            ..
        } = self;
        let ctx = ToolContext {
            outputs,
            scene,
            style: *style,
            modifiers: *modifiers,
        };
        f(tools[*active].as_mut(), &ctx)
    }

    fn tool(&self) -> &dyn Tool {
        self.tools[self.active].as_ref()
    }

    fn context(&self) -> ToolContext<'_> {
        ToolContext {
            outputs: &self.outputs,
            scene: &self.scene,
            style: self.style,
            modifiers: self.modifiers,
        }
    }

    fn current_highlight(&self) -> Option<Highlight> {
        self.tool().highlight(&self.context(), self.pointer)
    }

    fn output_at(&self, point: Point) -> Option<&FrozenOutput> {
        self.outputs
            .iter()
            .find(|output| output.rect.contains(point))
    }

    fn all_screens(&self) -> Option<Rect> {
        Rect::bounding(self.outputs.iter().map(|output| &output.rect))
    }
}

struct Surface {
    window: gtk::ApplicationWindow,
    canvas: Canvas,
}

struct Toolbar {
    root: gtk::Box,
    tool_buttons: Vec<gtk::ToggleButton>,
    style_controls: gtk::Box,
    /// The custom colour picker. While it is open, typed keys belong to its
    /// hex entry rather than to the overlay's shortcuts.
    color_popover: gtk::Popover,
    undo: gtk::Button,
    redo: gtk::Button,
}

pub struct Overlay {
    session: RefCell<Session>,
    surfaces: RefCell<Vec<Surface>>,
    toolbar: RefCell<Option<Toolbar>>,
    accent: gdk::RGBA,
    on_finish: RefCell<Option<FinishCallback>>,
    finished: Cell<bool>,
}

impl Overlay {
    /// Maps the overlay on every output and returns it. `on_finish` runs once,
    /// after the surfaces have been hidden.
    pub fn open(
        app: &gtk::Application,
        display: &gdk::Display,
        outputs: Vec<FrozenOutput>,
        scene: Scene,
        accent: gdk::RGBA,
        on_finish: impl FnOnce(Finish) + 'static,
    ) -> Rc<Self> {
        let toolbar_output = scene
            .cursor
            .and_then(|cursor| {
                outputs
                    .iter()
                    .position(|output| output.rect.contains(cursor))
            })
            .or_else(|| {
                let focused = scene.focused_output.as_deref()?;
                outputs.iter().position(|output| output.name == focused)
            })
            .unwrap_or(0);

        let this = Rc::new(Self {
            session: RefCell::new(Session {
                pointer: scene.cursor,
                outputs,
                scene,
                tools: tools::all(),
                active: 0,
                annotations: Vec::new(),
                undone: Vec::new(),
                style: Style::default(),
                modifiers: gdk::ModifierType::empty(),
                highlight: None,
            }),
            surfaces: RefCell::new(Vec::new()),
            toolbar: RefCell::new(None),
            accent,
            on_finish: RefCell::new(Some(Box::new(on_finish))),
            finished: Cell::new(false),
        });

        let monitors: Vec<gdk::Monitor> = display
            .monitors()
            .iter::<gdk::Monitor>()
            .flatten()
            .collect();
        let outputs = this.session.borrow().outputs.clone();

        for (index, output) in outputs.iter().enumerate() {
            let monitor = monitors
                .iter()
                .find(|monitor| monitor.connector().as_deref() == Some(output.name.as_str()))
                .or_else(|| monitors.get(index));
            let surface = this.build_surface(app, monitor, output, index == toolbar_output);
            this.surfaces.borrow_mut().push(surface);
        }

        this.sync_toolbar();
        {
            let mut session = this.session.borrow_mut();
            session.highlight = session.current_highlight();
        }
        for surface in this.surfaces.borrow().iter() {
            surface.window.present();
        }

        this
    }

    fn build_surface(
        self: &Rc<Self>,
        app: &gtk::Application,
        monitor: Option<&gdk::Monitor>,
        output: &FrozenOutput,
        with_toolbar: bool,
    ) -> Surface {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .css_classes(["shot-window"])
            .decorated(false)
            .build();

        if !configure_layer_shell(&window, monitor) {
            window.set_title(Some("Screenshot"));
            match monitor {
                Some(monitor) => window.fullscreen_on_monitor(monitor),
                None => window.fullscreen(),
            }
        }

        let canvas = Canvas::new();
        canvas.set_cursor_from_name(Some(self.session.borrow().tool().cursor()));
        let output_rect = output.rect;
        let image = output.image.clone();
        let weak = Rc::downgrade(self);
        canvas.set_draw_func(move |canvas, snapshot, width, height| {
            if let Some(this) = weak.upgrade() {
                this.draw(canvas, snapshot, output_rect, &image, width, height);
            }
        });

        self.attach_input(&canvas, output_rect);

        let stack = gtk::Overlay::new();
        stack.set_child(Some(&canvas));
        if with_toolbar {
            let toolbar = self.build_toolbar();
            stack.add_overlay(&toolbar.root);
            *self.toolbar.borrow_mut() = Some(toolbar);
        }
        window.set_child(Some(&stack));

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let weak = Rc::downgrade(self);
            move |_, key, _, state| match weak.upgrade() {
                Some(this) => this.key_pressed(key, state),
                None => glib::Propagation::Proceed,
            }
        });
        keys.connect_key_released({
            let weak = Rc::downgrade(self);
            move |_, key, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.key_released(key);
                }
            }
        });
        window.add_controller(keys);

        Surface { window, canvas }
    }

    // -- input -------------------------------------------------------------

    fn attach_input(self: &Rc<Self>, canvas: &Canvas, output_rect: Rect) {
        let to_global = move |x: f64, y: f64| Point::new(output_rect.x + x, output_rect.y + y);

        let motion = gtk::EventControllerMotion::new();
        let on_motion = {
            let weak = Rc::downgrade(self);
            move |x: f64, y: f64| {
                if let Some(this) = weak.upgrade() {
                    this.pointer_moved(to_global(x, y));
                }
            }
        };
        motion.connect_enter({
            let on_motion = on_motion.clone();
            move |_, x, y| on_motion(x, y)
        });
        motion.connect_motion(move |_, x, y| on_motion(x, y));
        canvas.add_controller(motion);

        // The drag's start point is kept per surface: the pointer is grabbed by
        // whichever surface the press landed on, and every offset that follows
        // is relative to it — even once the pointer is over another screen.
        let start = Rc::new(Cell::new(Point::default()));
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        drag.connect_drag_begin({
            let weak = Rc::downgrade(self);
            let start = Rc::clone(&start);
            move |gesture, x, y| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let at = to_global(x, y);
                start.set(at);
                this.press(at, gesture.current_event_state());
            }
        });
        drag.connect_drag_update({
            let weak = Rc::downgrade(self);
            let start = Rc::clone(&start);
            move |gesture, dx, dy| {
                if let Some(this) = weak.upgrade() {
                    let origin = start.get();
                    this.drag(
                        Point::new(origin.x + dx, origin.y + dy),
                        gesture.current_event_state(),
                    );
                }
            }
        });
        drag.connect_drag_end({
            let weak = Rc::downgrade(self);
            move |gesture, dx, dy| {
                if let Some(this) = weak.upgrade() {
                    let origin = start.get();
                    this.release(
                        Point::new(origin.x + dx, origin.y + dy),
                        gesture.current_event_state(),
                    );
                }
            }
        });
        canvas.add_controller(drag);

        let secondary = gtk::GestureClick::new();
        secondary.set_button(gdk::BUTTON_SECONDARY);
        secondary.connect_pressed({
            let weak = Rc::downgrade(self);
            move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.cancel_or_close();
                }
            }
        });
        canvas.add_controller(secondary);
    }

    fn pointer_moved(&self, at: Point) {
        let changed = {
            let mut session = self.session.borrow_mut();
            session.pointer = Some(at);
            if session.tool().in_progress() {
                // The drag gesture owns the pointer while a press is held.
                false
            } else {
                let highlight = session.current_highlight();
                let changed = highlight != session.highlight;
                session.highlight = highlight;
                changed
            }
        };
        if changed {
            self.redraw();
        }
    }

    fn press(&self, at: Point, state: gdk::ModifierType) {
        {
            let mut session = self.session.borrow_mut();
            session.pointer = Some(at);
            session.modifiers = state;
            session.with_tool(|tool, ctx| tool.press(ctx, at));
        }
        // Out of the way while working, so a band can be dragged through it.
        self.set_toolbar_visible(false);
        self.redraw();
    }

    fn drag(&self, at: Point, state: gdk::ModifierType) {
        {
            let mut session = self.session.borrow_mut();
            if !session.tool().in_progress() {
                return;
            }
            session.pointer = Some(at);
            session.modifiers = state;
            session.with_tool(|tool, ctx| tool.drag(ctx, at));
            session.highlight = session.current_highlight();
        }
        self.redraw();
    }

    fn release(&self, at: Point, state: gdk::ModifierType) {
        let outcome = {
            let mut session = self.session.borrow_mut();
            if !session.tool().in_progress() {
                // Cancelled mid-drag; the gesture still reports its end.
                return;
            }
            session.pointer = Some(at);
            session.modifiers = state;
            let outcome = session.with_tool(|tool, ctx| tool.release(ctx, at));
            session.highlight = session.current_highlight();
            outcome
        };

        self.set_toolbar_visible(true);
        match outcome {
            Outcome::None => {}
            Outcome::Annotate(annotation) => {
                let mut session = self.session.borrow_mut();
                session.annotations.push(annotation);
                session.undone.clear();
            }
            Outcome::Capture(area) => {
                self.capture(area);
                return;
            }
        }
        self.sync_toolbar();
        self.redraw();
    }

    fn cancel_or_close(&self) {
        let cancelled = self.session.borrow_mut().with_tool(|tool, _| tool.cancel());
        if cancelled {
            {
                let mut session = self.session.borrow_mut();
                session.highlight = session.current_highlight();
            }
            self.set_toolbar_visible(true);
            self.redraw();
        } else {
            self.finish(Finish::Cancel);
        }
    }

    /// Re-applies the current drag, for when a modifier changed without the
    /// pointer moving — pressing Shift must square the band at once.
    fn modifiers_changed(&self, modifiers: gdk::ModifierType) {
        let in_progress = {
            let mut session = self.session.borrow_mut();
            session.modifiers = modifiers;
            match session.pointer {
                Some(at) if session.tool().in_progress() => {
                    session.with_tool(|tool, ctx| tool.drag(ctx, at));
                    session.highlight = session.current_highlight();
                    true
                }
                _ => false,
            }
        };
        if in_progress {
            self.redraw();
        }
    }

    fn key_pressed(&self, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        // The overlay captures keys before any widget sees them, so without
        // this a hex colour typed into the picker would fire `A`, `S`, `W`…
        let picking_color = self
            .toolbar
            .borrow()
            .as_ref()
            .is_some_and(|toolbar| toolbar.color_popover.is_visible());
        if picking_color {
            return glib::Propagation::Proceed;
        }

        let control = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

        match key {
            gdk::Key::Shift_L | gdk::Key::Shift_R => {
                self.modifiers_changed(state | gdk::ModifierType::SHIFT_MASK);
                return glib::Propagation::Stop;
            }
            gdk::Key::Escape => {
                self.cancel_or_close();
                return glib::Propagation::Stop;
            }
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => {
                self.capture_under_pointer();
                return glib::Propagation::Stop;
            }
            _ => {}
        }

        let Some(character) = key.to_lower().to_unicode() else {
            return glib::Propagation::Proceed;
        };

        if control {
            match (character, shift) {
                ('z', false) => self.undo(),
                ('z', true) | ('y', _) => self.redo(),
                _ => return glib::Propagation::Proceed,
            }
            return glib::Propagation::Stop;
        }

        match character {
            'w' => self.capture_focused_window(),
            's' => self.capture_screen(),
            'a' => self.capture_all(),
            other if !RESERVED_KEYS.contains(&other) => {
                let index = self
                    .session
                    .borrow()
                    .tools
                    .iter()
                    .position(|tool| tool.shortcut() == other);
                match index {
                    Some(index) => self.select_tool(index),
                    None => return glib::Propagation::Proceed,
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn key_released(&self, key: gdk::Key) {
        if matches!(key, gdk::Key::Shift_L | gdk::Key::Shift_R) {
            let modifiers = self.session.borrow().modifiers - gdk::ModifierType::SHIFT_MASK;
            self.modifiers_changed(modifiers);
        }
    }

    // -- actions -------------------------------------------------------------

    fn select_tool(&self, index: usize) {
        {
            let mut session = self.session.borrow_mut();
            if session.active == index || index >= session.tools.len() {
                return;
            }
            session.with_tool(|tool, _| tool.cancel());
            session.active = index;
            session.highlight = session.current_highlight();
        }
        self.set_toolbar_visible(true);
        let cursor = self.session.borrow().tool().cursor();
        for surface in self.surfaces.borrow().iter() {
            surface.canvas.set_cursor_from_name(Some(cursor));
        }
        self.sync_toolbar();
        self.redraw();
    }

    fn set_style(&self, update: impl FnOnce(&mut Style)) {
        update(&mut self.session.borrow_mut().style);
    }

    fn undo(&self) {
        {
            let mut session = self.session.borrow_mut();
            let Some(annotation) = session.annotations.pop() else {
                return;
            };
            session.undone.push(annotation);
        }
        self.sync_toolbar();
        self.redraw();
    }

    fn redo(&self) {
        {
            let mut session = self.session.borrow_mut();
            let Some(annotation) = session.undone.pop() else {
                return;
            };
            session.annotations.push(annotation);
        }
        self.sync_toolbar();
        self.redraw();
    }

    fn capture_under_pointer(&self) {
        let target = {
            let session = self.session.borrow();
            session
                .pointer
                .and_then(|pointer| tools::target_at(&session.context(), pointer))
        };
        if let Some((area, _)) = target {
            self.capture(area);
        }
    }

    fn capture_focused_window(&self) {
        let area = self
            .session
            .borrow()
            .scene
            .focused_window()
            .map(|window| window.rect);
        match area {
            Some(area) => self.capture(area),
            None => tracing::info!("no focused window to capture"),
        }
    }

    /// The screen under the pointer, which in the overlay is the one the user
    /// is looking at; the compositor's focused output is only the fallback.
    fn capture_screen(&self) {
        let area = {
            let session = self.session.borrow();
            session
                .pointer
                .and_then(|pointer| session.output_at(pointer))
                .or_else(|| {
                    let focused = session.scene.focused_output.as_deref()?;
                    session.outputs.iter().find(|output| output.name == focused)
                })
                .or_else(|| session.outputs.first())
                .map(|output| output.rect)
        };
        if let Some(area) = area {
            self.capture(area);
        }
    }

    fn capture_all(&self) {
        let area = self.session.borrow().all_screens();
        if let Some(area) = area {
            self.capture(area);
        }
    }

    fn capture(&self, area: Rect) {
        let annotations = std::mem::take(&mut self.session.borrow_mut().annotations);
        self.finish(Finish::Capture { area, annotations });
    }

    fn finish(&self, finish: Finish) {
        if self.finished.replace(true) {
            return;
        }
        for surface in self.surfaces.borrow().iter() {
            surface.window.set_visible(false);
        }

        // Handed over after a short pause, so the compositor has unmapped the
        // overlay before encoding and saving hold the main loop.
        let Some(on_finish) = self.on_finish.borrow_mut().take() else {
            return;
        };
        let windows: Vec<gtk::ApplicationWindow> = self
            .surfaces
            .borrow()
            .iter()
            .map(|surface| surface.window.clone())
            .collect();
        glib::timeout_add_local_once(Duration::from_millis(40), move || {
            on_finish(finish);
            for window in windows {
                window.destroy();
            }
        });
    }

    fn redraw(&self) {
        for surface in self.surfaces.borrow().iter() {
            surface.canvas.queue_draw();
        }
    }

    // -- drawing -------------------------------------------------------------

    fn draw(
        &self,
        canvas: &Canvas,
        snapshot: &gtk::Snapshot,
        output: Rect,
        image: &gdk::Texture,
        width: f64,
        height: f64,
    ) {
        let session = self.session.borrow();

        snapshot.append_texture(image, &graphene_rect(Rect::new(0.0, 0.0, width, height)));

        let marks = session
            .annotations
            .iter()
            .map(|annotation| annotation.as_ref())
            .chain(session.tool().preview());
        for mark in marks {
            let Some(visible) = mark.bounds().intersection(&output) else {
                continue;
            };
            let cr = snapshot.append_cairo(&graphene_rect(visible.translate(-output.x, -output.y)));
            cr.translate(-output.x, -output.y);
            mark.draw(&cr);
        }

        match &session.highlight {
            Some(highlight) => {
                self.draw_highlight(canvas, snapshot, &session, output, highlight);
            }
            None => {
                // No region in play, as while drawing: a thin frame is all that
                // says the screen is frozen, and it keeps the image undimmed.
                let local = Rect::new(0.0, 0.0, output.width, output.height);
                frame(snapshot, local, BORDER_WIDTH, &self.accent);
            }
        }
    }

    fn draw_highlight(
        &self,
        canvas: &Canvas,
        snapshot: &gtk::Snapshot,
        session: &Session,
        output: Rect,
        highlight: &Highlight,
    ) {
        let local = |rect: Rect| rect.translate(-output.x, -output.y);

        let Some(inside) = highlight.rect.intersection(&output) else {
            snapshot.append_color(&DIM, &graphene_rect(local(output)));
            return;
        };

        for shade in [
            Rect::new(output.x, output.y, output.width, inside.y - output.y),
            Rect::new(
                output.x,
                inside.bottom(),
                output.width,
                output.bottom() - inside.bottom(),
            ),
            Rect::new(output.x, inside.y, inside.x - output.x, inside.height),
            Rect::new(
                inside.right(),
                inside.y,
                output.right() - inside.right(),
                inside.height,
            ),
        ] {
            if !shade.is_empty() {
                snapshot.append_color(&DIM, &graphene_rect(local(shade)));
            }
        }

        // Drawn inside the region's edge, so a window or screen that fills the
        // output is still visibly outlined.
        let outline = local(highlight.rect);
        if highlight.tint {
            let mut wash = self.accent;
            wash.set_alpha(TINT_ALPHA);
            snapshot.append_color(&wash, &graphene_rect(local(inside)));
        }
        frame(snapshot, outline, BORDER_WIDTH, &self.accent);
        frame(snapshot, outline.inflate(-BORDER_WIDTH), 1.0, &OUTLINE);

        if label_output(session, highlight.rect) == Some(output) {
            draw_label(canvas, snapshot, output, highlight);
        }
    }

    // -- toolbar -------------------------------------------------------------

    fn build_toolbar(self: &Rc<Self>) -> Toolbar {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .margin_top(28)
            .css_classes(["shot-toolbar"])
            .build();

        let mut tool_buttons: Vec<gtk::ToggleButton> = Vec::new();
        let tools: Vec<(&'static str, &'static str, char)> = self
            .session
            .borrow()
            .tools
            .iter()
            .map(|tool| (tool.label(), tool.description(), tool.shortcut()))
            .collect();
        for (index, (label, description, shortcut)) in tools.into_iter().enumerate() {
            let button = gtk::ToggleButton::builder()
                .label(label)
                .tooltip_text(format!(
                    "{label} ({})\n{description}",
                    shortcut.to_ascii_uppercase()
                ))
                .focus_on_click(false)
                .build();
            if let Some(first) = tool_buttons.first() {
                button.set_group(Some(first));
            }
            button.connect_toggled({
                let weak = Rc::downgrade(self);
                move |button| {
                    if button.is_active()
                        && let Some(this) = weak.upgrade()
                    {
                        this.select_tool(index);
                    }
                }
            });
            root.append(&button);
            tool_buttons.push(button);
        }

        let style_controls = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        style_controls.append(&separator());
        let color_popover = self.append_swatches(&style_controls);
        style_controls.append(&separator());
        self.append_widths(&style_controls);
        root.append(&style_controls);

        root.append(&separator());
        let undo = action_button("Undo", "Undo the last mark (Ctrl+Z)", self, Self::undo);
        let redo = action_button("Redo", "Redo (Ctrl+Shift+Z)", self, Self::redo);
        root.append(&undo);
        root.append(&redo);

        root.append(&separator());
        root.append(&action_button(
            "Window",
            "Capture the focused window (W)",
            self,
            Self::capture_focused_window,
        ));
        root.append(&action_button(
            "Screen",
            "Capture this screen (S)",
            self,
            Self::capture_screen,
        ));
        root.append(&action_button(
            "All screens",
            "Capture every screen, as they are arranged (A)",
            self,
            Self::capture_all,
        ));

        root.append(&separator());
        let close = action_button("✕", "Cancel (Esc)", self, |this| {
            this.finish(Finish::Cancel)
        });
        close.add_css_class("shot-close");
        root.append(&close);

        // The pointer passing over the toolbar must not leave a stale window
        // highlight behind it, drawn as though a click would take that window.
        let motion = gtk::EventControllerMotion::new();
        motion.connect_enter({
            let weak = Rc::downgrade(self);
            move |_, _, _| {
                if let Some(this) = weak.upgrade() {
                    let mut session = this.session.borrow_mut();
                    if session.highlight.take().is_some() {
                        drop(session);
                        this.redraw();
                    }
                }
            }
        });
        root.add_controller(motion);

        Toolbar {
            root,
            tool_buttons,
            style_controls,
            color_popover,
            undo,
            redo,
        }
    }

    /// The preset swatches, then a custom colour that opens a full picker.
    /// All of them are one radio group, so exactly one reads as selected.
    fn append_swatches(self: &Rc<Self>, container: &gtk::Box) -> gtk::Popover {
        let current = self.session.borrow().style.color;
        let mut first: Option<gtk::ToggleButton> = None;

        for color in tools::PALETTE {
            let swatch = gtk::DrawingArea::builder()
                .content_width(16)
                .content_height(16)
                .build();
            swatch.set_draw_func(move |_, cr, width, height| {
                draw_swatch(cr, width, height, &color);
            });

            let button = gtk::ToggleButton::builder()
                .child(&swatch)
                .tooltip_text("Pen colour")
                .focus_on_click(false)
                .active(color == current)
                .css_classes(["shot-swatch"])
                .build();
            match &first {
                Some(first) => button.set_group(Some(first)),
                None => first = Some(button.clone()),
            }
            button.connect_toggled({
                let weak = Rc::downgrade(self);
                move |button| {
                    if button.is_active()
                        && let Some(this) = weak.upgrade()
                    {
                        this.set_style(|style| style.color = color);
                    }
                }
            });
            container.append(&button);
        }

        let (button, popover) = self.custom_color_button();
        button.set_group(first.as_ref());
        container.append(&button);
        popover
    }

    /// A swatch showing the custom colour inside a hue ring. Every click
    /// selects the custom colour and opens the picker, including a click on
    /// the swatch that is already selected — that is how the colour is changed.
    #[allow(deprecated)] // `GtkColorChooserWidget`; see below.
    fn custom_color_button(self: &Rc<Self>) -> (gtk::ToggleButton, gtk::Popover) {
        let custom = Rc::new(Cell::new(CUSTOM_COLOR_DEFAULT));

        let swatch = gtk::DrawingArea::builder()
            .content_width(16)
            .content_height(16)
            .build();
        swatch.set_draw_func({
            let custom = Rc::clone(&custom);
            move |_, cr, width, height| draw_custom_swatch(cr, width, height, &custom.get())
        });

        let button = gtk::ToggleButton::builder()
            .child(&swatch)
            .tooltip_text("Custom colour")
            .focus_on_click(false)
            .css_classes(["shot-swatch"])
            .build();

        // `GtkColorChooserWidget` is deprecated in favour of `GtkColorDialog`,
        // but a dialog is a new toplevel window, and ordinary windows stack
        // *below* an overlay-layer surface — it would open invisibly behind the
        // frozen screen. A popover is a popup of this very surface, so it
        // appears above it, and the widget has no replacement that can live in
        // one.
        let chooser = gtk::ColorChooserWidget::builder()
            .show_editor(true)
            .use_alpha(true)
            .rgba(&custom.get())
            .build();
        let popover = gtk::Popover::builder()
            .child(&chooser)
            .position(gtk::PositionType::Bottom)
            .css_classes(["shot-color-popover"])
            .build();
        popover.set_parent(&button);
        // A popover is not a regular child, so it has to be detached by hand
        // or GTK warns about a leaked child when the toolbar is destroyed.
        button.connect_destroy({
            let popover = popover.clone();
            move |_| popover.unparent()
        });

        chooser.connect_rgba_notify({
            let weak = Rc::downgrade(self);
            let custom = Rc::clone(&custom);
            let swatch = swatch.clone();
            let button = button.clone();
            move |chooser| {
                let color = chooser.rgba();
                custom.set(color);
                swatch.queue_draw();
                button.set_active(true);
                if let Some(this) = weak.upgrade() {
                    this.set_style(|style| style.color = color);
                }
            }
        });

        button.connect_clicked({
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.set_style(|style| style.color = custom.get());
                }
                popover.popup();
            }
        });

        (button, popover)
    }

    fn append_widths(self: &Rc<Self>, container: &gtk::Box) {
        let current = self.session.borrow().style.width;
        let mut first: Option<gtk::ToggleButton> = None;

        for width in tools::WIDTHS {
            let dot = gtk::DrawingArea::builder()
                .content_width(16)
                .content_height(16)
                .build();
            dot.set_draw_func(move |_, cr, area_width, area_height| {
                cr.arc(
                    f64::from(area_width) / 2.0,
                    f64::from(area_height) / 2.0,
                    (width / 2.0 + 1.0).min(7.0),
                    0.0,
                    std::f64::consts::TAU,
                );
                cr.set_source_rgba(0.95, 0.96, 0.98, 0.9);
                let _ = cr.fill();
            });

            let button = gtk::ToggleButton::builder()
                .child(&dot)
                .tooltip_text(format!("Pen width {width}px"))
                .focus_on_click(false)
                .active(width == current)
                .css_classes(["shot-swatch"])
                .build();
            match &first {
                Some(first) => button.set_group(Some(first)),
                None => first = Some(button.clone()),
            }
            button.connect_toggled({
                let weak = Rc::downgrade(self);
                move |button| {
                    if button.is_active()
                        && let Some(this) = weak.upgrade()
                    {
                        this.set_style(|style| style.width = width);
                    }
                }
            });
            container.append(&button);
        }
    }

    /// Brings the toolbar in line with the session. Called after anything
    /// that could change what it shows, including keyboard shortcuts.
    fn sync_toolbar(&self) {
        let toolbar = self.toolbar.borrow();
        let Some(toolbar) = toolbar.as_ref() else {
            return;
        };
        let (active, uses_style, can_undo, can_redo) = {
            let session = self.session.borrow();
            (
                session.active,
                session.tool().uses_style(),
                !session.annotations.is_empty(),
                !session.undone.is_empty(),
            )
        };

        // Setting a button active re-enters `select_tool`, which returns early
        // for the already-active index, so this cannot loop.
        if let Some(button) = toolbar.tool_buttons.get(active)
            && !button.is_active()
        {
            button.set_active(true);
        }
        toolbar.style_controls.set_visible(uses_style);
        toolbar.undo.set_sensitive(can_undo);
        toolbar.redo.set_sensitive(can_redo);
    }

    fn set_toolbar_visible(&self, visible: bool) {
        if let Some(toolbar) = self.toolbar.borrow().as_ref() {
            toolbar.root.set_opacity(if visible { 1.0 } else { 0.0 });
            toolbar.root.set_can_target(visible);
        }
    }
}

/// Configures an overlay surface covering one output.
///
/// `Layer::Overlay` so it covers panels too, since they are part of the frozen
/// frame, and `KeyboardMode::Exclusive` so Escape and the shortcuts work
/// without a click first. `exclusive_zone(-1)` ignores other surfaces'
/// reservations, so the frozen frame lines up with the screen pixel for pixel
/// rather than being pushed out from under a bar.
fn configure_layer_shell(window: &gtk::ApplicationWindow, monitor: Option<&gdk::Monitor>) -> bool {
    if !gtk4_layer_shell::is_supported() {
        tracing::warn!("gtk4-layer-shell unsupported, falling back to fullscreen windows");
        return false;
    }

    window.init_layer_shell();
    window.set_namespace(Some("ioexplorer-shot"));
    window.set_monitor(monitor);
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
        window.set_margin(edge, 0);
    }
    window.set_exclusive_zone(-1);
    true
}

fn action_button(
    label: &str,
    tooltip: &str,
    overlay: &Rc<Overlay>,
    action: impl Fn(&Overlay) + 'static,
) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .tooltip_text(tooltip)
        .focus_on_click(false)
        .build();
    let weak: Weak<Overlay> = Rc::downgrade(overlay);
    button.connect_clicked(move |_| {
        if let Some(this) = weak.upgrade() {
            action(&this);
        }
    });
    button
}

fn separator() -> gtk::Separator {
    gtk::Separator::new(gtk::Orientation::Vertical)
}

fn graphene_rect(rect: Rect) -> graphene::Rect {
    graphene::Rect::new(
        rect.x as f32,
        rect.y as f32,
        rect.width as f32,
        rect.height as f32,
    )
}

/// A border of `width` drawn inside `rect`.
fn frame(snapshot: &gtk::Snapshot, rect: Rect, width: f64, color: &gdk::RGBA) {
    if rect.width <= width * 2.0 || rect.height <= width * 2.0 {
        snapshot.append_color(color, &graphene_rect(rect));
        return;
    }
    for edge in [
        Rect::new(rect.x, rect.y, rect.width, width),
        Rect::new(rect.x, rect.bottom() - width, rect.width, width),
        Rect::new(rect.x, rect.y + width, width, rect.height - width * 2.0),
        Rect::new(
            rect.right() - width,
            rect.y + width,
            width,
            rect.height - width * 2.0,
        ),
    ] {
        snapshot.append_color(color, &graphene_rect(edge));
    }
}

/// Which output draws the label: the one holding the region's top-left
/// corner, or — for a band whose corner is in a gap between screens — the
/// first output the region touches. Exactly one, so it is never drawn twice.
fn label_output(session: &Session, region: Rect) -> Option<Rect> {
    let corner = Point::new(region.x, region.y);
    session
        .output_at(corner)
        .or_else(|| {
            session
                .outputs
                .iter()
                .find(|output| output.rect.intersects(&region))
        })
        .map(|output| output.rect)
}

fn draw_label(canvas: &Canvas, snapshot: &gtk::Snapshot, output: Rect, highlight: &Highlight) {
    const PADDING_X: f64 = 8.0;
    const PADDING_Y: f64 = 4.0;
    const GAP: f64 = 8.0;

    let layout = canvas.create_pango_layout(Some(&highlight.label));
    let mut font = gtk::pango::FontDescription::new();
    font.set_weight(gtk::pango::Weight::Bold);
    layout.set_font_description(Some(&font));
    let (_, extents) = layout.pixel_extents();
    let pill_width = f64::from(extents.width()) + PADDING_X * 2.0;
    let pill_height = f64::from(extents.height()) + PADDING_Y * 2.0;

    let region = highlight.rect.intersection(&output).unwrap_or(output);
    // Above the region when there is room, otherwise tucked inside its corner.
    let mut x = region.x + BORDER_WIDTH;
    let mut y = region.y - pill_height - GAP;
    if y < output.y {
        y = region.y + BORDER_WIDTH + GAP;
        x = region.x + BORDER_WIDTH + GAP;
    }
    x = x.min(output.right() - pill_width - GAP).max(output.x + GAP);
    y = y
        .min(output.bottom() - pill_height - GAP)
        .max(output.y + GAP);

    let pill = Rect::new(x - output.x, y - output.y, pill_width, pill_height);
    let rounded = gsk::RoundedRect::from_rect(graphene_rect(pill), 6.0);
    snapshot.push_rounded_clip(&rounded);
    snapshot.append_color(&LABEL_BACKGROUND, &graphene_rect(pill));
    snapshot.pop();

    snapshot.save();
    snapshot.translate(&graphene::Point::new(
        (pill.x + PADDING_X - f64::from(extents.x())) as f32,
        (pill.y + PADDING_Y - f64::from(extents.y())) as f32,
    ));
    snapshot.append_layout(&layout, &LABEL_TEXT);
    snapshot.restore();
}

/// The custom swatch's colour until one is picked.
const CUSTOM_COLOR_DEFAULT: gdk::RGBA = gdk::RGBA::new(0.66, 0.36, 0.98, 1.0);

fn draw_swatch(cr: &gtk::cairo::Context, width: i32, height: i32, color: &gdk::RGBA) {
    let radius = f64::from(width.min(height)) / 2.0 - 1.0;
    cr.arc(
        f64::from(width) / 2.0,
        f64::from(height) / 2.0,
        radius,
        0.0,
        std::f64::consts::TAU,
    );
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        1.0,
    );
    let _ = cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

/// A hue wheel ring around a dot of the current custom colour, so the button
/// reads as "any colour" while still showing which one is set.
fn draw_custom_swatch(cr: &gtk::cairo::Context, width: i32, height: i32, color: &gdk::RGBA) {
    const SEGMENTS: usize = 24;
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let outer = f64::from(width.min(height)) / 2.0 - 0.5;
    let ring = 3.0;

    cr.set_line_width(ring);
    for segment in 0..SEGMENTS {
        let start = segment as f64 / SEGMENTS as f64;
        let end = (segment + 1) as f64 / SEGMENTS as f64;
        let (red, green, blue) = hue_to_rgb(start);
        cr.set_source_rgb(red, green, blue);
        // A hair of overlap, so no seams show between segments.
        cr.arc(
            cx,
            cy,
            outer - ring / 2.0,
            start * std::f64::consts::TAU - 0.02,
            end * std::f64::consts::TAU + 0.02,
        );
        let _ = cr.stroke();
    }

    cr.arc(cx, cy, outer - ring - 1.5, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()),
    );
    let _ = cr.fill();
}

/// Fully saturated, full-value RGB for a hue in `0.0..1.0`.
fn hue_to_rgb(hue: f64) -> (f64, f64, f64) {
    let sector = (hue.rem_euclid(1.0)) * 6.0;
    let rising = sector.fract();
    let falling = 1.0 - rising;
    match sector as u8 {
        0 => (1.0, rising, 0.0),
        1 => (falling, 1.0, 0.0),
        2 => (0.0, 1.0, rising),
        3 => (0.0, falling, 1.0),
        4 => (rising, 0.0, 1.0),
        _ => (1.0, 0.0, falling),
    }
}

#[cfg(test)]
mod tests {
    use super::hue_to_rgb;

    #[test]
    fn hue_wheel_primaries() {
        assert_eq!(hue_to_rgb(0.0), (1.0, 0.0, 0.0));
        assert_eq!(hue_to_rgb(1.0 / 3.0).1, 1.0);
        assert_eq!(hue_to_rgb(2.0 / 3.0).2, 1.0);
        assert_eq!(hue_to_rgb(1.0), (1.0, 0.0, 0.0), "the wheel wraps");
    }
}
