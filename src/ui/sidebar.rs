use std::path::PathBuf;

use directories::UserDirs;
use gtk::prelude::*;

use crate::providers::ProviderUri;

#[derive(Debug, Clone)]
pub struct SidebarPlace {
    pub label: String,
    pub icon_name: &'static str,
    pub uri: ProviderUri,
}

pub struct Sidebar {
    pub root: gtk::ScrolledWindow,
    pub list: gtk::ListBox,
    pub places: Vec<SidebarPlace>,
}

impl Sidebar {
    pub fn new(width: i32) -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["sidebar-list"])
            .build();

        let places = default_places();
        for place in &places {
            list.append(&place_row(place));
        }

        let root = gtk::ScrolledWindow::builder()
            .min_content_width(width)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .css_classes(["sidebar"])
            .build();

        Self { root, list, places }
    }
}

fn default_places() -> Vec<SidebarPlace> {
    let mut places = Vec::new();

    if let Some(user_dirs) = UserDirs::new() {
        push_place(
            &mut places,
            "Home",
            "user-home-symbolic",
            user_dirs.home_dir(),
        );
        if let Some(path) = user_dirs.desktop_dir() {
            push_place(&mut places, "Desktop", "user-desktop-symbolic", path);
        }
        if let Some(path) = user_dirs.document_dir() {
            push_place(&mut places, "Documents", "folder-documents-symbolic", path);
        }
        if let Some(path) = user_dirs.download_dir() {
            push_place(&mut places, "Downloads", "folder-download-symbolic", path);
        }
        if let Some(path) = user_dirs.picture_dir() {
            push_place(&mut places, "Pictures", "folder-pictures-symbolic", path);
        }
        if let Some(path) = user_dirs.audio_dir() {
            push_place(&mut places, "Music", "folder-music-symbolic", path);
        }
        if let Some(path) = user_dirs.video_dir() {
            push_place(&mut places, "Videos", "folder-videos-symbolic", path);
        }
    }

    places.push(SidebarPlace {
        label: "Filesystem".to_string(),
        icon_name: "drive-harddisk-symbolic",
        uri: ProviderUri::local(PathBuf::from("/")),
    });

    places
}

fn push_place(
    places: &mut Vec<SidebarPlace>,
    label: &str,
    icon_name: &'static str,
    path: impl AsRef<std::path::Path>,
) {
    let path = path.as_ref();
    if path.exists() {
        places.push(SidebarPlace {
            label: label.to_string(),
            icon_name,
            uri: ProviderUri::local(path),
        });
    }
}

fn place_row(place: &SidebarPlace) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .build();
    let item = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .build();
    let icon = gtk::Image::builder()
        .icon_name(place.icon_name)
        .pixel_size(18)
        .build();
    let label = gtk::Label::builder()
        .label(&place.label)
        .xalign(0.0)
        .hexpand(true)
        .build();

    item.append(&icon);
    item.append(&label);
    row.set_child(Some(&item));
    row
}
