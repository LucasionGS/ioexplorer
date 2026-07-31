//! The topbar's sort menu.
//!
//! A menu button over a popover of radio rows: one group picks the field, one
//! picks the direction, and a check pins folders above files.
//!
//! The menu reports a pick and nothing more — it does not re-style itself off
//! its own toggles. The window owns the sort, and reflects every change back
//! through [`SortMenu::set_order`], which suppresses the callback so this menu
//! and the settings page cannot ping-pong against each other.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;

use crate::sorting::{SortKey, SortOrder};

type SortChangedHandler = Box<dyn Fn(SortOrder)>;

struct Notifier {
    order: Cell<SortOrder>,
    handlers: RefCell<Vec<SortChangedHandler>>,
    /// Set while [`SortMenu::set_order`] drives the toggles, so the syncing
    /// itself is not mistaken for a user pick.
    updating: Cell<bool>,
}

impl Notifier {
    fn update(&self, apply: impl FnOnce(&mut SortOrder)) {
        if self.updating.get() {
            return;
        }

        let mut order = self.order.get();
        apply(&mut order);
        if order == self.order.get() {
            return;
        }

        self.order.set(order);
        for handler in self.handlers.borrow().iter() {
            handler(order);
        }
    }
}

pub struct SortMenu {
    pub button: gtk::MenuButton,
    key_buttons: Vec<(SortKey, gtk::CheckButton)>,
    ascending_button: gtk::CheckButton,
    descending_button: gtk::CheckButton,
    folders_first_check: gtk::CheckButton,
    notifier: Rc<Notifier>,
}

impl SortMenu {
    pub fn new(order: SortOrder) -> Self {
        let notifier = Rc::new(Notifier {
            order: Cell::new(order),
            handlers: RefCell::new(Vec::new()),
            updating: Cell::new(false),
        });

        let menu = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();

        menu.append(&section_label("Sort By"));

        let mut key_buttons = Vec::new();
        let mut key_group: Option<gtk::CheckButton> = None;
        for key in SortKey::ALL {
            let check = menu_check(key.label());
            if let Some(group) = &key_group {
                check.set_group(Some(group));
            } else {
                key_group = Some(check.clone());
            }
            // After grouping: joining a group clears the button.
            check.set_active(key == order.key);

            let notifier = Rc::clone(&notifier);
            check.connect_toggled(move |check| {
                if check.is_active() {
                    notifier.update(|order| order.key = key);
                }
            });

            menu.append(&check);
            key_buttons.push((key, check));
        }

        menu.append(&separator());
        menu.append(&section_label("Order"));

        let ascending_button = menu_check("Ascending");
        let descending_button = menu_check("Descending");
        descending_button.set_group(Some(&ascending_button));
        ascending_button.set_active(!order.descending);
        descending_button.set_active(order.descending);

        let notifier_for_direction = Rc::clone(&notifier);
        descending_button.connect_toggled(move |check| {
            let descending = check.is_active();
            notifier_for_direction.update(|order| order.descending = descending);
        });

        menu.append(&ascending_button);
        menu.append(&descending_button);

        menu.append(&separator());

        let folders_first_check = menu_check("Folders First");
        folders_first_check.set_active(order.folders_first);
        let notifier_for_folders = Rc::clone(&notifier);
        folders_first_check.connect_toggled(move |check| {
            let folders_first = check.is_active();
            notifier_for_folders.update(|order| order.folders_first = folders_first);
        });
        menu.append(&folders_first_check);

        let popover = gtk::Popover::builder()
            .child(&menu)
            .has_arrow(false)
            .css_classes(["context-menu", "sort-menu"])
            .build();

        let button = gtk::MenuButton::builder()
            .icon_name(order.icon_name())
            .tooltip_text(order.summary())
            .popover(&popover)
            .css_classes(["toolbar-button"])
            .build();
        button.set_focusable(false);

        Self {
            button,
            key_buttons,
            ascending_button,
            descending_button,
            folders_first_check,
            notifier,
        }
    }

    /// Fires when the user picks something. Not fired by [`Self::set_order`].
    pub fn connect_changed(&self, handler: impl Fn(SortOrder) + 'static) {
        self.notifier.handlers.borrow_mut().push(Box::new(handler));
    }

    pub fn set_order(&self, order: SortOrder) {
        self.notifier.order.set(order);
        self.notifier.updating.set(true);

        for (key, check) in &self.key_buttons {
            check.set_active(*key == order.key);
        }
        self.ascending_button.set_active(!order.descending);
        self.descending_button.set_active(order.descending);
        self.folders_first_check.set_active(order.folders_first);

        self.notifier.updating.set(false);

        self.button.set_icon_name(order.icon_name());
        self.button.set_tooltip_text(Some(&order.summary()));
    }
}

fn menu_check(label: &str) -> gtk::CheckButton {
    let check = gtk::CheckButton::builder()
        .label(label)
        .css_classes(["sort-menu-item"])
        .build();
    check.set_focusable(false);
    check
}

fn section_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["dim-label", "sort-menu-heading"])
        .build()
}

fn separator() -> gtk::Separator {
    gtk::Separator::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["sort-menu-separator"])
        .build()
}
