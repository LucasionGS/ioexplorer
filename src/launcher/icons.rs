//! Icon and container helpers shared by the launcher surfaces.

use gtk::prelude::*;

use crate::launcher::app_index::IconRef;

/// Builds an image for an [`IconRef`], falling back to treating it as an icon name.
pub fn image_for(icon: &IconRef, pixel_size: i32) -> gtk::Image {
    let image = match gio::Icon::for_string(&icon.0) {
        Ok(gicon) => gtk::Image::from_gicon(&gicon),
        Err(_) => gtk::Image::from_icon_name(&icon.0),
    };
    image.set_pixel_size(pixel_size);
    image
}

/// Removes every child of a box, which GTK4 has no single call for.
pub fn clear_box_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// Removes every row of a list box.
pub fn clear_list_rows(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}
