use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use directories::UserDirs;
use gtk::prelude::*;

use crate::{
    providers::ProviderUri,
    ui::{
        context_menu::ContextMenu,
        views::format_bytes,
        volumes::{VolumeAction, VolumeEntry, Volumes},
    },
};

const DEVICE_ROW_CLASS: &str = "sidebar-device-row";
const BOOKMARK_ROW_CLASS: &str = "sidebar-bookmark-row";

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
    /// Retained so the width can follow a config reload; the root alone is not
    /// enough, since the scroller carries the minimum content width.
    scroll: gtk::ScrolledWindow,
    places: Rc<RefCell<Vec<SidebarPlace>>>,
    /// Drives listed under Devices, whose rows follow the places in `list`.
    devices: Rc<RefCell<Vec<VolumeEntry>>>,
    volumes: Rc<Volumes>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidebarSection {
    Files,
    Computer,
    Settings,
}

impl Sidebar {
    pub fn new(width: i32, bookmarks: &[PathBuf], volumes: Rc<Volumes>) -> Self {
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
            .css_classes(["sidebar-nav-button", "sidebar-settings-button"])
            .build();
        settings_button.set_focusable(false);
        settings_button.set_child(Some(&place_content(
            "Settings",
            "preferences-system-symbolic",
            18,
        )));

        let scroll = gtk::ScrolledWindow::builder()
            .min_content_width(width)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();

        // Settings is pinned under the scrolling list, so a long run of
        // bookmarks and drives never pushes it out of reach.
        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["sidebar-footer"])
            .build();
        footer.append(&settings_button);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["sidebar"])
            .build();
        root.set_size_request(width, -1);
        root.append(&computer_button);
        root.append(&scroll);
        root.append(&footer);

        let sidebar = Self {
            root,
            computer_button,
            settings_button,
            list,
            scroll,
            places: Rc::new(RefCell::new(Vec::new())),
            devices: Rc::new(RefCell::new(Vec::new())),
            volumes,
        };
        sidebar.install_device_handlers();
        sidebar.set_bookmarks(bookmarks);
        sidebar
    }

    fn install_device_handlers(&self) {
        // A list box drops headers set outside its header func, so the section
        // title is placed here: above the first device row.
        self.list.set_header_func(|row, before| {
            let section = [
                (BOOKMARK_ROW_CLASS, "Bookmarks"),
                (DEVICE_ROW_CLASS, "Devices"),
            ]
            .into_iter()
            .find(|(class, _)| {
                row.has_css_class(class)
                    && !before.is_some_and(|before| before.has_css_class(class))
            });
            match section {
                None => row.set_header(None::<&gtk::Widget>),
                Some((_, title)) => {
                    let current = row
                        .header()
                        .and_downcast::<gtk::Label>()
                        .map(|label| label.label());
                    if current.as_deref() != Some(title) {
                        row.set_header(Some(&section_header(title)));
                    }
                }
            }
        });

        let places = Rc::clone(&self.places);
        let devices = Rc::clone(&self.devices);
        let volumes = Rc::clone(&self.volumes);
        self.list.connect_row_activated(move |list, row| {
            let entry = device_at(&places, &devices, row.index());
            if let Some(entry) = entry {
                volumes.run(VolumeAction::Open, &entry, list.upcast_ref());
            }
        });

        let right_click = gtk::GestureClick::new();
        right_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let list = self.list.clone();
        let places = Rc::clone(&self.places);
        let devices = Rc::clone(&self.devices);
        let volumes = Rc::clone(&self.volumes);
        right_click.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = list.row_at_y(y as i32) else {
                return;
            };
            let Some(entry) = device_at(&places, &devices, row.index()) else {
                return;
            };

            gesture.set_state(gtk::EventSequenceState::Claimed);
            let context = volumes.context_menu(&entry, list.upcast_ref());
            ContextMenu::popup_at(&list, x, y, &context);
        });
        self.list.add_controller(right_click);
    }

    pub fn set_width(&self, width: i32) {
        self.scroll.set_min_content_width(width);
        self.root.set_size_request(width, -1);
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

    /// Highlights the place that is exactly `uri`, so the sidebar follows
    /// navigation from anywhere (address bar, back, a folder double-click).
    /// A folder that is no place clears the highlight.
    pub fn select_place_for(&self, uri: &ProviderUri) {
        let index = self
            .places
            .borrow()
            .iter()
            .position(|place| &place.uri == uri);
        let row = index.and_then(|index| self.list.row_at_index(index as i32));
        if row != self.list.selected_row() {
            match row {
                Some(row) => self.list.select_row(Some(&row)),
                None => self.list.unselect_all(),
            }
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
        self.refresh_devices();
    }

    /// Re-list the Devices section, leaving the place rows (and whichever of
    /// them is selected) untouched.
    pub fn refresh_devices(&self) {
        let place_count = self.places.borrow().len() as i32;
        let selected = self.list.selected_row().map(|row| row.index());
        let selected_device = selected
            .and_then(|index| device_at(&self.places, &self.devices, index))
            .map(|entry| entry.name().to_string());

        while let Some(row) = self.list.row_at_index(place_count) {
            self.list.remove(&row);
        }

        let devices: Vec<VolumeEntry> = self
            .volumes
            .entries()
            .into_iter()
            .filter(is_sidebar_device)
            .collect();
        for entry in &devices {
            self.list.append(&device_row(entry, &self.volumes));
        }

        // Removing the selected row moves the list's cursor, and with it the
        // selection, onto another row (usually Home), so put it back.
        let reselect = match selected {
            Some(index) if index < place_count => self.list.row_at_index(index),
            _ => selected_device
                .and_then(|name| devices.iter().position(|entry| entry.name() == name))
                .and_then(|position| self.list.row_at_index(place_count + position as i32)),
        };
        match reselect {
            Some(row) => self.list.select_row(Some(&row)),
            None => self.list.unselect_all(),
        }
        *self.devices.borrow_mut() = devices;
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

/// Removable media and user-mountable partitions, mounted or not. System
/// mounts like `/` stay out, since Filesystem already covers them.
fn is_sidebar_device(entry: &VolumeEntry) -> bool {
    match entry {
        VolumeEntry::Mounted(volume) => volume.mount.is_some(),
        VolumeEntry::Unmounted(_) => true,
    }
}

/// The device behind the sidebar row at `index`, if it is past the places.
fn device_at(
    places: &RefCell<Vec<SidebarPlace>>,
    devices: &RefCell<Vec<VolumeEntry>>,
    index: i32,
) -> Option<VolumeEntry> {
    let index = usize::try_from(index).ok()?;
    let device_index = index.checked_sub(places.borrow().len())?;
    devices.borrow().get(device_index).cloned()
}

fn device_row(entry: &VolumeEntry, volumes: &Rc<Volumes>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .css_classes([DEVICE_ROW_CLASS])
        .build();

    let item = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .build();
    let (icon, tooltip) = match entry {
        VolumeEntry::Mounted(volume) => (
            &volume.icon,
            format!("{} - {}", volume.name, volume.mount_path.display()),
        ),
        VolumeEntry::Unmounted(volume) => {
            row.add_css_class("sidebar-device-unmounted");
            (&volume.icon, format!("{} - not mounted", volume.name))
        }
    };
    row.set_tooltip_text(Some(&tooltip));

    let icon = gtk::Image::from_gicon(icon);
    icon.set_pixel_size(18);
    icon.add_css_class("sidebar-device-icon");
    icon.add_css_class("sidebar-row-icon");
    let label = gtk::Label::builder()
        .label(entry.name())
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(&label);
    if let VolumeEntry::Mounted(volume) = entry
        && let (Some(size), Some(used)) = (volume.size, volume.used)
        && size > 0
    {
        let fraction = used as f64 / size as f64;
        let usage = gtk::ProgressBar::builder()
            .fraction(fraction)
            .css_classes(["sidebar-device-usage"])
            .build();
        if fraction >= 0.9 {
            usage.add_css_class("usage-critical");
        }
        text.append(&usage);
        row.set_tooltip_text(Some(&format!(
            "{} - {} free of {}",
            volume.mount_path.display(),
            format_bytes(size.saturating_sub(used)),
            format_bytes(size)
        )));
    }
    item.append(&icon);
    item.append(&text);

    // Eject takes the whole drive away safely, so it wins over a plain unmount
    // whenever the drive supports it.
    let mounted = matches!(entry, VolumeEntry::Mounted(_));
    let eject_action = if mounted && entry.can_eject() {
        Some((VolumeAction::Eject, "Eject"))
    } else if entry.can_unmount() {
        Some((VolumeAction::Unmount, "Unmount"))
    } else {
        None
    };
    if let Some((action, tooltip)) = eject_action {
        let button = gtk::Button::builder()
            .icon_name("media-eject-symbolic")
            .tooltip_text(tooltip)
            .valign(gtk::Align::Center)
            .css_classes(["flat", "sidebar-eject-button"])
            .build();
        button.set_focusable(false);
        let entry = entry.clone();
        let volumes = Rc::clone(volumes);
        button.connect_clicked(move |button| volumes.run(action, &entry, button.upcast_ref()));
        item.append(&button);
    }

    row.set_child(Some(&item));
    row
}

fn section_header(title: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["sidebar-section-title"])
        .build()
}

fn place_row(place: &SidebarPlace) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .tooltip_text(place.uri.display_path())
        .build();
    if place.is_bookmark {
        row.add_css_class(BOOKMARK_ROW_CLASS);
    }
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
        .css_classes(["sidebar-row-icon"])
        .build();
    let label = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
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
