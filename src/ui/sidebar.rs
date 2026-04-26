use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

use directories::UserDirs;
use gtk::prelude::*;

use crate::providers::ProviderUri;

#[derive(Debug, Clone)]
pub struct SidebarPlace {
    pub label: String,
    pub icon_name: &'static str,
    pub uri: ProviderUri,
    pub is_bookmark: bool,
}

pub struct Sidebar {
    pub root: gtk::Box,
    pub computer_button: gtk::ToggleButton,
    pub settings_button: gtk::ToggleButton,
    pub list: gtk::ListBox,
    places: RefCell<Vec<SidebarPlace>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidebarSection {
    Files,
    Computer,
    Settings,
}

impl Sidebar {
    pub fn new(width: i32, bookmarks: &[PathBuf]) -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .vexpand(true)
            .css_classes(["sidebar-list"])
            .build();

        let computer_button = gtk::ToggleButton::builder()
            .tooltip_text("This PC")
            .css_classes(["sidebar-nav-button"])
            .build();
        computer_button.set_focusable(false);
        computer_button.set_child(Some(&place_content("This PC", "computer-symbolic", 18)));

        let settings_button = gtk::ToggleButton::builder()
            .tooltip_text("Settings")
            .css_classes(["sidebar-nav-button"])
            .build();
        settings_button.set_focusable(false);
        settings_button.set_child(Some(&place_content(
            "Settings",
            "preferences-system-symbolic",
            18,
        )));

        let separator = gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["sidebar-separator"])
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .min_content_width(width)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["sidebar"])
            .build();
        root.set_size_request(width, -1);
        root.append(&computer_button);
        root.append(&settings_button);
        root.append(&separator);
        root.append(&scroll);

        let sidebar = Self {
            root,
            computer_button,
            settings_button,
            list,
            places: RefCell::new(Vec::new()),
        };
        sidebar.set_bookmarks(bookmarks);
        sidebar
    }

    pub fn place_at(&self, index: usize) -> Option<SidebarPlace> {
        self.places.borrow().get(index).cloned()
    }

    pub fn set_active_section(&self, section: SidebarSection) {
        self.computer_button
            .set_active(section == SidebarSection::Computer);
        self.settings_button
            .set_active(section == SidebarSection::Settings);
        if section != SidebarSection::Files {
            self.list.unselect_all();
        }
    }

    pub fn set_bookmarks(&self, bookmarks: &[PathBuf]) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }

        let places = places_with_bookmarks(bookmarks);
        for place in &places {
            self.list.append(&place_row(place));
        }
        *self.places.borrow_mut() = places;
    }
}

fn places_with_bookmarks(bookmarks: &[PathBuf]) -> Vec<SidebarPlace> {
    let mut places = default_places();
    for path in bookmarks {
        places.push(SidebarPlace {
            label: bookmark_label(path),
            icon_name: "user-bookmarks-symbolic",
            uri: ProviderUri::local(path),
            is_bookmark: true,
        });
    }
    places
}

fn bookmark_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn default_places() -> Vec<SidebarPlace> {
    let mut places = Vec::new();

    if let Some(user_dirs) = UserDirs::new() {
        let home = user_dirs.home_dir().to_path_buf();
        push_place(&mut places, "Home", "user-home-symbolic", &home);
        push_place(
            &mut places,
            "Desktop",
            "user-desktop-symbolic",
            user_place_path(user_dirs.desktop_dir(), &home, "Desktop"),
        );
        push_place(
            &mut places,
            "Documents",
            "folder-documents-symbolic",
            user_place_path(user_dirs.document_dir(), &home, "Documents"),
        );
        push_place(
            &mut places,
            "Downloads",
            "folder-download-symbolic",
            user_place_path(user_dirs.download_dir(), &home, "Downloads"),
        );
        push_place(
            &mut places,
            "Pictures",
            "folder-pictures-symbolic",
            user_place_path(user_dirs.picture_dir(), &home, "Pictures"),
        );
        push_place(
            &mut places,
            "Music",
            "folder-music-symbolic",
            user_place_path(user_dirs.audio_dir(), &home, "Music"),
        );
        push_place(
            &mut places,
            "Videos",
            "folder-videos-symbolic",
            user_place_path(user_dirs.video_dir(), &home, "Videos"),
        );
    }

    places.push(SidebarPlace {
        label: "Filesystem".to_string(),
        icon_name: "drive-harddisk-symbolic",
        uri: ProviderUri::local(PathBuf::from("/")),
        is_bookmark: false,
    });

    places
}

fn user_place_path(configured: Option<&Path>, home: &Path, fallback_name: &str) -> PathBuf {
    configured
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(fallback_name))
}

fn push_place(
    places: &mut Vec<SidebarPlace>,
    label: &str,
    icon_name: &'static str,
    path: impl AsRef<std::path::Path>,
) {
    let path = path.as_ref();
    if path.exists() && !place_exists(places, path) {
        places.push(SidebarPlace {
            label: label.to_string(),
            icon_name,
            uri: ProviderUri::local(path),
            is_bookmark: false,
        });
    }
}

fn place_exists(places: &[SidebarPlace], path: &Path) -> bool {
    places.iter().any(|place| {
        place
            .uri
            .local_path()
            .is_ok_and(|uri_path| uri_path == path)
    })
}

fn place_row(place: &SidebarPlace) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .build();
    row.set_child(Some(&place_content(&place.label, place.icon_name, 18)));
    row
}

fn place_content(label: &str, icon_name: &str, pixel_size: i32) -> gtk::Box {
    let item = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .build();
    let icon = gtk::Image::builder()
        .icon_name(icon_name)
        .pixel_size(pixel_size)
        .build();
    let label = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .build();

    item.append(&icon);
    item.append(&label);
    item
}

#[cfg(test)]
mod tests {
    use super::{SidebarPlace, place_exists, user_place_path};
    use crate::providers::ProviderUri;

    #[test]
    fn user_place_path_falls_back_to_home_folder() {
        assert_eq!(
            user_place_path(None, std::path::Path::new("/home/test"), "Documents"),
            std::path::PathBuf::from("/home/test/Documents")
        );
    }

    #[test]
    fn user_place_path_prefers_configured_folder() {
        assert_eq!(
            user_place_path(
                Some(std::path::Path::new("/data/docs")),
                std::path::Path::new("/home/test"),
                "Documents",
            ),
            std::path::PathBuf::from("/data/docs")
        );
    }

    #[test]
    fn place_exists_matches_local_paths() {
        let places = vec![SidebarPlace {
            label: "Home".to_string(),
            icon_name: "user-home-symbolic",
            uri: ProviderUri::local("/home/test"),
            is_bookmark: false,
        }];

        assert!(place_exists(&places, std::path::Path::new("/home/test")));
        assert!(!place_exists(
            &places,
            std::path::Path::new("/home/test/Documents")
        ));
    }
}
