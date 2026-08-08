//! A small focused surface for the one thing the desktop cannot do itself:
//! accept typing.
//!
//! The desktop sits on `Layer::Bottom`, which Hyprland never gives keyboard
//! focus to, so an inline `gtk::Entry` on a tile would be uneditable. A plain
//! toplevel dialog is not an option either — a `zwlr_layer_surface_v1` is not
//! an `xdg_toplevel`, so there is nothing valid to be transient for, and the
//! modal hint would simply be dropped.
//!
//! What does work is a second *layer* surface on `Overlay` with
//! `KeyboardMode::Exclusive`: the upper layers are exactly the ones compositors
//! grant focus to. It lives for the length of one prompt and takes the whole
//! keyboard while it does, which is what a modal wants anyway.

use std::rc::Rc;

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};

/// Asks for a single line of text. `on_accept` runs with the trimmed value
/// unless the user cancels or leaves it unchanged.
pub fn ask(
    app: &gtk::Application,
    title: &str,
    initial: &str,
    accept_label: &str,
    on_accept: impl Fn(String) + 'static,
) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .css_classes(["desktop-prompt-window"])
        .build();

    if gtk4_layer_shell::is_supported() {
        window.init_layer_shell();
        window.set_namespace(Some("ioexplorer-desktop-prompt"));
        window.set_layer(Layer::Overlay);
        // The whole point of this surface: unlike the desktop beneath it, this
        // one is allowed to hold the keyboard.
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        // No anchors, so the compositor centres it.
        window.set_exclusive_zone(0);
    } else {
        window.set_title(Some(title));
        window.set_modal(true);
    }
    window.set_default_width(420);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .css_classes(["desktop-prompt"])
        .build();

    content.append(&gtk::Label::builder().label(title).xalign(0.0).build());

    let entry = gtk::Entry::builder().text(initial).hexpand(true).build();
    content.append(&entry);

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let cancel = gtk::Button::builder().label("Cancel").build();
    let accept = gtk::Button::builder()
        .label(accept_label)
        .css_classes(["suggested-action"])
        .build();
    buttons.append(&cancel);
    buttons.append(&accept);
    content.append(&buttons);

    window.set_child(Some(&content));

    let submit = Rc::new({
        let window = window.clone();
        let entry = entry.clone();
        let on_accept = Rc::new(on_accept);
        move || {
            let value = entry.text().trim().to_string();
            window.close();
            if !value.is_empty() {
                on_accept(value);
            }
        }
    });

    accept.connect_clicked({
        let submit = Rc::clone(&submit);
        move |_| submit()
    });
    entry.connect_activate({
        let submit = Rc::clone(&submit);
        move |_| submit()
    });
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });

    let escape = gtk::EventControllerKey::new();
    escape.connect_key_pressed({
        let window = window.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    window.add_controller(escape);

    window.present();
    // Selects the stem rather than the extension where there is one, so
    // renaming `notes.txt` does not make the user re-type `.txt`.
    let text = entry.text();
    let stem_end = text
        .rfind('.')
        .filter(|index| *index > 0)
        .map_or(-1, |index| index as i32);
    entry.select_region(0, stem_end);
    entry.grab_focus();
}

#[cfg(test)]
mod tests {
    /// The stem/extension split the prompt selects on open.
    fn stem_end(text: &str) -> i32 {
        text.rfind('.')
            .filter(|index| *index > 0)
            .map_or(-1, |index| index as i32)
    }

    #[test]
    fn renaming_preselects_the_stem_not_the_extension() {
        assert_eq!(stem_end("notes.txt"), 5);
        assert_eq!(stem_end("archive.tar.gz"), 11);
    }

    /// A name with no extension selects everything, and a dotfile is all stem —
    /// selecting nothing of `.bashrc` would be worse than selecting all of it.
    #[test]
    fn names_without_an_extension_select_wholly() {
        assert_eq!(stem_end("Documents"), -1);
        assert_eq!(stem_end(".bashrc"), -1);
    }
}
