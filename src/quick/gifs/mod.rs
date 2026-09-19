//! The GIF tab's collection: saved GIFs and images, their tags, finding new
//! ones on the clipboard, and handing a picked one over.

pub mod clipboard;
pub mod library;
pub mod thumbs;

use std::io::Cursor;

use std::path::Path;

use gtk::{gdk, glib, prelude::*};

pub use library::{Gif, ImageKind, Library, parse_tags};

use super::history;

/// Puts `gif` on the clipboard in every form an application might take: the
/// image in its own format, a PNG of it for applications that only take PNG
/// (Chromium's, which is also why a GIF pasted into those arrives still), and
/// the file itself for those that take files.
///
/// Must run while the menu still has keyboard focus: a Wayland compositor
/// only lets a client set the clipboard in response to its own input.
pub fn offer(gif: &Gif) -> Result<(), String> {
    offer_image(&gif.path, gif.kind)
}

/// [`offer`] for any image file.
pub fn offer_image(path: &Path, kind: ImageKind) -> Result<(), String> {
    let display = gdk::Display::default().ok_or("no display")?;
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;

    let uri = gtk::gio::File::for_path(path).uri();
    let mut providers = vec![gdk::ContentProvider::for_bytes(
        kind.mime_type(),
        &glib::Bytes::from(&bytes),
    )];
    if kind != ImageKind::Png {
        match png_of(&bytes) {
            Ok(png) => providers.push(gdk::ContentProvider::for_bytes(
                "image/png",
                &glib::Bytes::from_owned(png),
            )),
            Err(error) => tracing::info!(%error, "cannot offer a PNG of the image"),
        }
    }
    providers.push(gdk::ContentProvider::for_bytes(
        "text/uri-list",
        &glib::Bytes::from_owned(format!("{uri}\r\n").into_bytes()),
    ));
    providers.push(gdk::ContentProvider::for_bytes(
        "x-special/gnome-copied-files",
        &glib::Bytes::from_owned(format!("copy\n{uri}").into_bytes()),
    ));

    history::mark_own_copy();
    display
        .clipboard()
        .set_content(Some(&gdk::ContentProvider::new_union(&providers)))
        .map_err(|error| error.to_string())
}

/// The image at `path` as PNG — the one format every application that takes
/// pasted images accepts. The first frame of an animation.
pub fn png_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    match ImageKind::sniff(&bytes) {
        Some(ImageKind::Png) => Ok(bytes),
        _ => png_of(&bytes),
    }
}

/// The first frame, as PNG.
fn png_of(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(png)
}
