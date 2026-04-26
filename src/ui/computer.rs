use std::{cell::RefCell, path::PathBuf};

use gio::prelude::FileExt;
use gtk::prelude::*;

const FILESYSTEM_USAGE_ATTRIBUTES: &str = "filesystem::size,filesystem::free,filesystem::used";

#[derive(Clone)]
pub struct MountVolume {
    pub name: String,
    pub device: String,
    pub mount_path: PathBuf,
    pub fs_type: String,
    pub size: Option<u64>,
    pub used: Option<u64>,
    pub readonly: bool,
    icon: gio::Icon,
}

pub struct ComputerPage {
    pub root: gtk::ScrolledWindow,
    pub list: gtk::ListBox,
    volumes: RefCell<Vec<MountVolume>>,
}

impl ComputerPage {
    pub fn new() -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(false)
            .css_classes(["computer-volume-list"])
            .build();

        let header_icon = gtk::Image::builder()
            .icon_name("computer-symbolic")
            .pixel_size(28)
            .build();
        let header_label = gtk::Label::builder()
            .label("This PC")
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["computer-page-title"])
            .build();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .build();
        header.append(&header_icon);
        header.append(&header_label);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .css_classes(["computer-page"])
            .build();
        content.append(&header);
        content.append(&list);

        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .css_classes(["content-scroll", "computer-page-scroll"])
            .build();

        Self {
            root,
            list,
            volumes: RefCell::new(Vec::new()),
        }
    }

    pub fn refresh(&self) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }

        let volumes = mounted_volumes();
        if volumes.is_empty() {
            self.list.append(&empty_row());
        } else {
            for volume in &volumes {
                self.list.append(&volume_row(volume));
            }
        }
        *self.volumes.borrow_mut() = volumes;
    }

    pub fn volume_count(&self) -> usize {
        self.volumes.borrow().len()
    }

    pub fn mount_path_at(&self, index: usize) -> Option<PathBuf> {
        self.volumes
            .borrow()
            .get(index)
            .map(|volume| volume.mount_path.clone())
    }
}

pub fn mounted_volumes() -> Vec<MountVolume> {
    let (mounts, _) = gio::UnixMountEntry::mounts();
    let mut volumes: Vec<MountVolume> = mounts
        .into_iter()
        .filter(should_show_mount_entry)
        .map(|mount| mount_volume(&mount))
        .collect();

    volumes.sort_by_key(mount_sort_key);
    volumes.dedup_by(|left, right| left.mount_path == right.mount_path);
    volumes
}

fn should_show_mount_entry(mount: &gio::UnixMountEntry) -> bool {
    should_show_mount(
        &mount.mount_path(),
        &mount.device_path().to_string_lossy(),
        mount.fs_type().as_str(),
        mount.guess_should_display(),
    )
}

fn should_show_mount(
    mount_path: &std::path::Path,
    device_path: &str,
    fs_type: &str,
    should_display: bool,
) -> bool {
    if mount_path == std::path::Path::new("/") {
        return true;
    }

    if pseudo_filesystem(fs_type) {
        return false;
    }

    should_display || device_backed_partition(device_path)
}

fn device_backed_partition(device_path: &str) -> bool {
    device_path.starts_with("/dev/") && !device_path.starts_with("/dev/loop")
}

fn pseudo_filesystem(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "autofs"
            | "binfmt_misc"
            | "bpf"
            | "cgroup"
            | "cgroup2"
            | "configfs"
            | "debugfs"
            | "devpts"
            | "devtmpfs"
            | "fusectl"
            | "hugetlbfs"
            | "mqueue"
            | "nsfs"
            | "proc"
            | "pstore"
            | "securityfs"
            | "sysfs"
            | "tracefs"
            | "tmpfs"
    )
}

fn mount_volume(mount: &gio::UnixMountEntry) -> MountVolume {
    let mount_path = mount.mount_path();
    let fs_type = mount.fs_type().to_string();
    let device = mount.device_path().display().to_string();
    let (size, free, queried_used) = filesystem_usage(&mount_path);
    let used = queried_used.or_else(|| match (size, free) {
        (Some(size), Some(free)) => Some(size.saturating_sub(free)),
        _ => None,
    });

    MountVolume {
        name: mount_display_name(mount, &mount_path),
        device,
        mount_path,
        fs_type,
        size,
        used,
        readonly: mount.is_readonly(),
        icon: mount.guess_symbolic_icon(),
    }
}

fn filesystem_usage(mount_path: &std::path::Path) -> (Option<u64>, Option<u64>, Option<u64>) {
    let file = gio::File::for_path(mount_path);
    match file.query_filesystem_info(FILESYSTEM_USAGE_ATTRIBUTES, None::<&gio::Cancellable>) {
        Ok(info) => (
            optional_uint64(&info, "filesystem::size"),
            optional_uint64(&info, "filesystem::free"),
            optional_uint64(&info, "filesystem::used"),
        ),
        Err(error) => {
            tracing::warn!(path = %mount_path.display(), %error, "failed to query filesystem usage");
            (None, None, None)
        }
    }
}

fn optional_uint64(info: &gio::FileInfo, attribute: &str) -> Option<u64> {
    info.has_attribute(attribute)
        .then(|| info.attribute_uint64(attribute))
}

fn mount_display_name(mount: &gio::UnixMountEntry, mount_path: &std::path::Path) -> String {
    if mount_path == std::path::Path::new("/") {
        return "Root".to_string();
    }

    let name = mount.guess_name().to_string();
    if name.trim().is_empty() {
        mount_path.display().to_string()
    } else {
        name
    }
}

fn mount_sort_key(volume: &MountVolume) -> (u8, String) {
    let priority = if volume.mount_path == std::path::Path::new("/") {
        0
    } else {
        1
    };
    (priority, volume.mount_path.display().to_string())
}

fn volume_row(volume: &MountVolume) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .css_classes(["computer-volume-row"])
        .build();

    let icon = gtk::Image::from_gicon(&volume.icon);
    icon.set_pixel_size(32);
    icon.set_valign(gtk::Align::Start);

    let name_label = gtk::Label::builder()
        .label(&volume.name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["computer-volume-name"])
        .build();
    let device_label = gtk::Label::builder()
        .label(device_summary(volume))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label"])
        .build();
    let mount_label = gtk::Label::builder()
        .label(format!("Mounted at {}", volume.mount_path.display()))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["dim-label"])
        .build();

    let progress = gtk::ProgressBar::builder()
        .hexpand(true)
        .valign(gtk::Align::Center)
        .css_classes(["computer-volume-progress"])
        .build();
    if let Some(fraction) = usage_fraction(volume.used, volume.size) {
        progress.set_fraction(fraction);
    }

    let usage_label = gtk::Label::builder()
        .label(usage_summary(volume.used, volume.size))
        .xalign(1.0)
        .width_chars(25)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label", "computer-volume-usage"])
        .build();

    let usage = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    usage.append(&progress);
    usage.append(&usage_label);

    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(5)
        .hexpand(true)
        .build();
    details.append(&name_label);
    details.append(&device_label);
    details.append(&mount_label);
    details.append(&usage);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&icon);
    content.append(&details);
    row.set_child(Some(&content));

    row
}

fn empty_row() -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .build();
    let label = gtk::Label::builder()
        .label("No mounted volumes found")
        .xalign(0.0)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build();
    row.set_child(Some(&label));
    row
}

fn device_summary(volume: &MountVolume) -> String {
    let mut parts = vec![volume.device.clone(), volume.fs_type.clone()];
    if volume.readonly {
        parts.push("read-only".to_string());
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" - ")
}

fn usage_summary(used: Option<u64>, total: Option<u64>) -> String {
    match (used, total) {
        (Some(used), Some(total)) if total > 0 => {
            format!("{} used of {}", format_bytes(used), format_bytes(total))
        }
        (_, Some(total)) if total > 0 => format!("{} total", format_bytes(total)),
        _ => "Usage unavailable".to_string(),
    }
}

fn usage_fraction(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    match (used, total) {
        (Some(used), Some(total)) if total > 0 => {
            Some((used as f64 / total as f64).clamp(0.0, 1.0))
        }
        _ => None,
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{format_bytes, should_show_mount, usage_fraction, usage_summary};

    #[test]
    fn always_shows_root_mount() {
        assert!(should_show_mount(
            Path::new("/"),
            "overlay",
            "overlay",
            false
        ));
    }

    #[test]
    fn shows_device_backed_non_internal_mounts() {
        assert!(should_show_mount(
            Path::new("/home"),
            "/dev/nvme0n1p2",
            "ext4",
            false
        ));
    }

    #[test]
    fn shows_device_backed_system_mounts() {
        assert!(should_show_mount(
            Path::new("/boot"),
            "/dev/nvme0n1p1",
            "vfat",
            false
        ));
    }

    #[test]
    fn hides_loopback_devices() {
        assert!(!should_show_mount(
            Path::new("/snap/example"),
            "/dev/loop3",
            "squashfs",
            false
        ));
    }

    #[test]
    fn hides_pseudo_filesystems() {
        assert!(!should_show_mount(Path::new("/proc"), "proc", "proc", true));
    }

    #[test]
    fn formats_volume_usage() {
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(usage_summary(Some(512), Some(1024)), "512 B used of 1.0 KB");
        assert_eq!(usage_fraction(Some(256), Some(1024)), Some(0.25));
    }
}
