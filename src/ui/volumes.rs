use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use gio::prelude::{DriveExt, FileExt, MountExt, VolumeExt, VolumeMonitorExt};
use gtk::prelude::*;

use crate::ui::context_menu;

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
    /// The volume monitor's handle for this mount, present for drives it
    /// manages (removable media, user-mountable partitions). Only these can be
    /// unmounted or ejected; system mounts like `/` have none.
    pub mount: Option<gio::Mount>,
    pub icon: gio::Icon,
}

/// A volume the volume monitor knows about that is not mounted yet, such as a
/// freshly plugged USB stick or a spare internal partition.
#[derive(Clone)]
pub struct UnmountedVolume {
    pub name: String,
    pub device: String,
    pub fs_type: String,
    pub size: Option<u64>,
    pub volume: gio::Volume,
    pub icon: gio::Icon,
}

/// A volume shown on the This PC page and in the sidebar's Devices section.
#[derive(Clone)]
pub enum VolumeEntry {
    Mounted(MountVolume),
    Unmounted(UnmountedVolume),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolumeAction {
    /// Browse the volume, mounting it first when needed.
    Open,
    /// Mount without navigating to it.
    Mount,
    Unmount,
    /// Eject the whole drive, or power it off when it has no eject mechanism.
    Eject,
}

/// What [`VolumeAction::Eject`] acts on, picked from the most to the least
/// specific object that supports it.
enum EjectTarget {
    Drive(gio::Drive),
    /// Drives without removable media (USB hard disks) are powered off instead.
    StopDrive(gio::Drive),
    Mount(gio::Mount),
    Volume(gio::Volume),
}

type OpenCallback = Rc<dyn Fn(PathBuf)>;
type StatusCallback = Rc<dyn Fn(String)>;

/// The drives, volumes and mounts GIO knows about, shared by the This PC page
/// and the sidebar so both list the same volumes and run the same operations.
pub struct Volumes {
    /// Retained for its signals; GIO drops the monitor's state once the last
    /// reference goes away.
    monitor: gio::VolumeMonitor,
    open: RefCell<Option<OpenCallback>>,
    status: RefCell<Option<StatusCallback>>,
}

impl Volumes {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            monitor: gio::VolumeMonitor::get(),
            open: RefCell::new(None),
            status: RefCell::new(None),
        })
    }

    /// Every volume worth showing: mounted ones first (root on top), then the
    /// ones that can be mounted.
    pub fn entries(&self) -> Vec<VolumeEntry> {
        let mounted = mounted_volumes(&self.monitor);
        let unmounted = unmounted_volumes(&self.monitor, &mounted);
        mounted
            .into_iter()
            .map(VolumeEntry::Mounted)
            .chain(unmounted.into_iter().map(VolumeEntry::Unmounted))
            .collect()
    }

    /// Navigate to `path` once a volume is opened, mounting it first if needed.
    pub fn connect_open(&self, open: impl Fn(PathBuf) + 'static) {
        *self.open.borrow_mut() = Some(Rc::new(open));
    }

    /// Report progress and failures of mount, unmount and eject operations.
    pub fn connect_status(&self, status: impl Fn(String) + 'static) {
        *self.status.borrow_mut() = Some(Rc::new(status));
    }

    /// Run `changed` whenever a drive, volume or mount appears, disappears or
    /// changes state, which [`gio::UnixMountMonitor`] alone misses for drives
    /// that are plugged in but not mounted.
    pub fn connect_changed(&self, changed: impl Fn() + 'static) {
        let changed = Rc::new(changed);
        let monitor = &self.monitor;

        let notify = Rc::clone(&changed);
        monitor.connect_drive_connected(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_drive_disconnected(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_drive_changed(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_volume_added(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_volume_removed(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_volume_changed(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_mount_added(move |_, _| notify());
        let notify = Rc::clone(&changed);
        monitor.connect_mount_removed(move |_, _| notify());
    }

    /// Run `action` on `entry`. `parent` is any widget in the window that
    /// password prompts and "device is busy" dialogs should appear over.
    pub fn run(self: &Rc<Self>, action: VolumeAction, entry: &VolumeEntry, parent: &gtk::Widget) {
        run_volume_action(action, entry, parent, self);
    }

    /// The right-click menu for `entry`, offering only the actions it supports.
    pub fn context_menu(
        self: &Rc<Self>,
        entry: &VolumeEntry,
        parent: &gtk::Widget,
    ) -> context_menu::VolumeContext {
        volume_context(entry, parent, self)
    }

    fn open(&self, path: PathBuf) {
        let open = self.open.borrow().clone();
        if let Some(open) = open {
            open(path);
        }
    }

    fn status(&self, message: String) {
        let status = self.status.borrow().clone();
        if let Some(status) = status {
            status(message);
        }
    }

    fn failed(&self, verb: &str, name: &str, error: &glib::Error) {
        // GIO already showed the user a dialog (or they dismissed the password
        // prompt), so repeating the error would only be noise.
        if error.matches(gio::IOErrorEnum::FailedHandled) {
            self.status(format!("Did not {verb} {name}"));
        } else {
            self.status(format!("Failed to {verb} {name}: {}", error.message()));
        }
    }
}

impl VolumeEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Mounted(volume) => &volume.name,
            Self::Unmounted(volume) => &volume.name,
        }
    }

    pub fn can_mount(&self) -> bool {
        matches!(self, Self::Unmounted(volume) if volume.volume.can_mount())
    }

    pub fn can_unmount(&self) -> bool {
        matches!(self, Self::Mounted(MountVolume { mount: Some(mount), .. }) if mount.can_unmount())
    }

    pub fn can_eject(&self) -> bool {
        self.eject_target().is_some()
    }

    fn eject_target(&self) -> Option<EjectTarget> {
        let (drive, volume, mount) = match self {
            Self::Mounted(volume) => {
                let mount = volume.mount.clone()?;
                (mount.drive(), mount.volume(), Some(mount))
            }
            Self::Unmounted(volume) => (volume.volume.drive(), Some(volume.volume.clone()), None),
        };

        if let Some(drive) = drive {
            if drive.can_eject() {
                return Some(EjectTarget::Drive(drive));
            }
            if drive.can_stop() {
                return Some(EjectTarget::StopDrive(drive));
            }
        }
        if let Some(mount) = mount.filter(|mount| mount.can_eject()) {
            return Some(EjectTarget::Mount(mount));
        }
        volume
            .filter(|volume| volume.can_eject())
            .map(EjectTarget::Volume)
    }
}

fn mounted_volumes(monitor: &gio::VolumeMonitor) -> Vec<MountVolume> {
    let managed_mounts: HashMap<PathBuf, gio::Mount> = monitor
        .mounts()
        .into_iter()
        .filter_map(|mount| Some((mount.root().path()?, mount)))
        .collect();

    let (mounts, _) = gio::UnixMountEntry::mounts();
    let mut volumes: Vec<MountVolume> = mounts
        .into_iter()
        .filter(should_show_mount_entry)
        .map(|mount| mount_volume(&mount, &managed_mounts))
        .collect();

    volumes.sort_by_key(mount_sort_key);
    volumes.dedup_by(|left, right| left.mount_path == right.mount_path);
    volumes
}

/// Volumes that can be mounted but are not, skipping any whose device is
/// already mounted somewhere the volume monitor does not track.
fn unmounted_volumes(
    monitor: &gio::VolumeMonitor,
    mounted: &[MountVolume],
) -> Vec<UnmountedVolume> {
    let mounted_devices: BTreeSet<&str> = mounted
        .iter()
        .map(|volume| volume.device.as_str())
        .collect();

    let mut volumes: Vec<(String, UnmountedVolume)> = monitor
        .volumes()
        .into_iter()
        .filter(|volume| volume.get_mount().is_none() && volume.can_mount())
        .map(|volume| {
            let device = volume
                .identifier(gio::VOLUME_IDENTIFIER_KIND_UNIX_DEVICE)
                .map(|device| device.to_string())
                .unwrap_or_default();
            let (size, fs_type) = block_device_info(&device);
            let sort_key = volume
                .sort_key()
                .map(|key| key.to_string())
                .unwrap_or_default();
            let unmounted = UnmountedVolume {
                name: volume.name().to_string(),
                device,
                fs_type: fs_type.unwrap_or_default(),
                size,
                icon: volume.symbolic_icon(),
                volume,
            };
            (sort_key, unmounted)
        })
        .filter(|(_, volume)| {
            volume.device.is_empty() || !mounted_devices.contains(volume.device.as_str())
        })
        .collect();

    volumes.sort_by(|(left_key, left), (right_key, right)| {
        left_key
            .cmp(right_key)
            .then_with(|| left.name.cmp(&right.name))
    });
    volumes.into_iter().map(|(_, volume)| volume).collect()
}

/// The size in bytes and filesystem type of a block device like `/dev/sda1`,
/// read from sysfs and the udev database since GIO exposes neither for a
/// volume that is not mounted.
fn block_device_info(device: &str) -> (Option<u64>, Option<String>) {
    let Some(name) = device
        .strip_prefix("/dev/")
        .filter(|name| !name.contains('/'))
    else {
        return (None, None);
    };
    let block_dir = Path::new("/sys/class/block").join(name);

    let size = fs::read_to_string(block_dir.join("size"))
        .ok()
        .and_then(|sectors| sectors.trim().parse::<u64>().ok())
        // sysfs always counts in 512-byte sectors, whatever the device's own sector size.
        .map(|sectors| sectors.saturating_mul(512));
    let fs_type = fs::read_to_string(block_dir.join("dev"))
        .ok()
        .and_then(|dev| fs::read_to_string(format!("/run/udev/data/b{}", dev.trim())).ok())
        .and_then(|contents| udev_property(&contents, "ID_FS_TYPE"));

    (size, fs_type)
}

// Example:
// E:ID_FS_TYPE=exfat
fn udev_property(contents: &str, name: &str) -> Option<String> {
    contents
        .lines()
        .filter_map(|line| line.strip_prefix("E:"))
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
    mount_path: &Path,
    device_path: &str,
    fs_type: &str,
    should_display: bool,
) -> bool {
    if mount_path == Path::new("/") {
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

fn mount_volume(
    mount: &gio::UnixMountEntry,
    managed_mounts: &HashMap<PathBuf, gio::Mount>,
) -> MountVolume {
    let mount_path = mount.mount_path();
    let managed = managed_mounts.get(&mount_path).cloned();
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
        // The volume monitor knows the drive type, so it picks a USB or card
        // icon where the mount table only knows "a disk".
        icon: managed
            .as_ref()
            .map(|managed| managed.symbolic_icon())
            .unwrap_or_else(|| mount.guess_symbolic_icon()),
        mount: managed,
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
    if mount_path == Path::new("/") {
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

fn volume_context(
    entry: &VolumeEntry,
    parent: &gtk::Widget,
    volumes: &Rc<Volumes>,
) -> context_menu::VolumeContext {
    let action = |action: VolumeAction| -> context_menu::MenuAction {
        let entry = entry.clone();
        let parent = parent.clone();
        let volumes = Rc::clone(volumes);
        Rc::new(move || run_volume_action(action, &entry, &parent, &volumes))
    };

    context_menu::VolumeContext::new(
        action(VolumeAction::Open),
        entry.can_mount().then(|| action(VolumeAction::Mount)),
        entry.can_unmount().then(|| action(VolumeAction::Unmount)),
        entry.can_eject().then(|| action(VolumeAction::Eject)),
    )
}

fn run_volume_action(
    action: VolumeAction,
    entry: &VolumeEntry,
    parent: &gtk::Widget,
    volumes: &Rc<Volumes>,
) {
    match (action, entry) {
        (VolumeAction::Open, VolumeEntry::Mounted(volume)) => {
            volumes.open(volume.mount_path.clone());
        }
        (VolumeAction::Open, VolumeEntry::Unmounted(volume)) => {
            mount_volume_async(volume, parent, volumes, true);
        }
        (VolumeAction::Mount, VolumeEntry::Unmounted(volume)) => {
            mount_volume_async(volume, parent, volumes, false);
        }
        (
            VolumeAction::Unmount,
            VolumeEntry::Mounted(MountVolume {
                mount: Some(mount), ..
            }),
        ) => {
            unmount_async(mount, entry.name(), parent, volumes);
        }
        (VolumeAction::Eject, _) => {
            if let Some(target) = entry.eject_target() {
                eject_async(target, entry.name(), parent, volumes);
            }
        }
        _ => {}
    }
}

/// A mount operation parented to the window holding `widget`, so password
/// prompts (encrypted drives) and "device is busy" dialogs appear over it.
fn mount_operation(widget: &gtk::Widget) -> gtk::MountOperation {
    let window = widget.root().and_downcast::<gtk::Window>();
    gtk::MountOperation::new(window.as_ref())
}

fn mount_volume_async(
    volume: &UnmountedVolume,
    parent: &gtk::Widget,
    volumes: &Rc<Volumes>,
    open: bool,
) {
    let name = volume.name.clone();
    volumes.status(format!("Mounting {name}..."));

    let gio_volume = volume.volume.clone();
    let volumes = Rc::clone(volumes);
    volume.volume.mount(
        gio::MountMountFlags::NONE,
        Some(&mount_operation(parent)),
        gio::Cancellable::NONE,
        move |result| {
            if let Err(error) = result {
                volumes.failed("mount", &name, &error);
                return;
            }

            match gio_volume.get_mount().and_then(|mount| mount.root().path()) {
                Some(path) if open => volumes.open(path),
                Some(path) => volumes.status(format!("Mounted {name} at {}", path.display())),
                None => volumes.status(format!("Mounted {name}")),
            }
        },
    );
}

fn unmount_async(mount: &gio::Mount, name: &str, parent: &gtk::Widget, volumes: &Rc<Volumes>) {
    let name = name.to_string();
    volumes.status(format!("Unmounting {name}..."));

    let volumes = Rc::clone(volumes);
    mount.unmount_with_operation(
        gio::MountUnmountFlags::NONE,
        Some(&mount_operation(parent)),
        gio::Cancellable::NONE,
        move |result| match result {
            Ok(()) => volumes.status(format!("Unmounted {name}")),
            Err(error) => volumes.failed("unmount", &name, &error),
        },
    );
}

fn eject_async(target: EjectTarget, name: &str, parent: &gtk::Widget, volumes: &Rc<Volumes>) {
    let name = name.to_string();
    volumes.status(format!("Ejecting {name}..."));

    let operation = mount_operation(parent);
    let flags = gio::MountUnmountFlags::NONE;
    let cancellable = gio::Cancellable::NONE;
    let volumes = Rc::clone(volumes);
    let done = move |result: Result<(), glib::Error>| match result {
        Ok(()) => volumes.status(format!("{name} can now be safely removed")),
        Err(error) => volumes.failed("eject", &name, &error),
    };

    match target {
        EjectTarget::Drive(drive) => {
            drive.eject_with_operation(flags, Some(&operation), cancellable, done)
        }
        EjectTarget::StopDrive(drive) => drive.stop(flags, Some(&operation), cancellable, done),
        EjectTarget::Mount(mount) => {
            mount.eject_with_operation(flags, Some(&operation), cancellable, done)
        }
        EjectTarget::Volume(volume) => {
            volume.eject_with_operation(flags, Some(&operation), cancellable, done)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{should_show_mount, udev_property};

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
    fn reads_udev_filesystem_type() {
        let udev = "S:disk/by-label/TRANSFER\nE:ID_FS_LABEL=TRANSFER\nE:ID_FS_TYPE=exfat\n";

        assert_eq!(udev_property(udev, "ID_FS_TYPE").as_deref(), Some("exfat"));
        assert_eq!(udev_property(udev, "ID_FS_UUID"), None);
    }
}
