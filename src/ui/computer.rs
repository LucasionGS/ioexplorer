use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

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
    cpu_label: gtk::Label,
    gpu_label: gtk::Label,
    ram_label: gtk::Label,
    volumes: RefCell<Vec<MountVolume>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SystemSpecs {
    cpu: String,
    gpu: String,
    ram: String,
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

        let cpu_label = system_stat_value_label();
        let gpu_label = system_stat_value_label();
        let ram_label = system_stat_value_label();
        let stats_section = system_stats_section(&cpu_label, &gpu_label, &ram_label);
        let stats_separator = gtk::Separator::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["computer-stats-separator"])
            .build();

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
        content.append(&stats_separator);
        content.append(&stats_section);

        let root = gtk::ScrolledWindow::builder()
            .child(&content)
            .css_classes(["content-scroll", "computer-page-scroll"])
            .build();

        Self {
            root,
            list,
            cpu_label,
            gpu_label,
            ram_label,
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
        self.refresh_system_specs();
    }

    fn refresh_system_specs(&self) {
        let specs = system_specs();
        self.cpu_label.set_text(&specs.cpu);
        self.gpu_label.set_text(&specs.gpu);
        self.ram_label.set_text(&specs.ram);
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

fn system_stats_section(
    cpu_label: &gtk::Label,
    gpu_label: &gtk::Label,
    ram_label: &gtk::Label,
) -> gtk::Box {
    let title = gtk::Label::builder()
        .label("System specs")
        .xalign(0.0)
        .css_classes(["computer-stats-title"])
        .build();

    let stats = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .css_classes(["computer-stats"])
        .build();
    stats.append(&title);
    stats.append(&system_stat_row("CPU", cpu_label));
    stats.append(&system_stat_row("GPU", gpu_label));
    stats.append(&system_stat_row("RAM", ram_label));
    stats
}

fn system_stat_row(label: &str, value: &gtk::Label) -> gtk::Box {
    let name = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .width_chars(5)
        .css_classes(["computer-stat-name"])
        .build();

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .css_classes(["computer-stat-row"])
        .build();
    row.append(&name);
    row.append(value);
    row
}

fn system_stat_value_label() -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dim-label", "computer-stat-value"])
        .build()
}

fn system_specs() -> SystemSpecs {
    SystemSpecs {
        cpu: cpu_specs(),
        gpu: gpu_specs(),
        ram: ram_specs(),
    }
}

fn cpu_specs() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|contents| cpu_specs_from_cpuinfo(&contents))
        .unwrap_or_else(|| "CPU info unavailable".to_string())
}

fn cpu_specs_from_cpuinfo(cpuinfo: &str) -> Option<String> {
    let mut model = None::<String>;
    let mut logical_threads = 0usize;
    let mut core_count = None::<usize>;

    for line in cpuinfo.lines() {
        let Some((key, value)) = key_value(line) else {
            continue;
        };

        match key {
            "model name" | "Hardware" | "Processor" if model.is_none() && !value.is_empty() => {
                model = Some(value.to_string());
            }
            "processor" => logical_threads += 1,
            "cpu cores" if core_count.is_none() => {
                core_count = value.parse::<usize>().ok();
            }
            _ => {}
        }
    }

    let model = model?;
    Some(match (core_count, logical_threads) {
        (Some(cores), threads) if cores > 0 && threads > cores => format!(
            "{model} - {} / {}",
            count_label(cores, "core"),
            count_label(threads, "thread")
        ),
        (Some(cores), _) if cores > 0 => format!("{model} - {}", count_label(cores, "core")),
        (_, threads) if threads > 1 => format!("{model} - {}", count_label(threads, "thread")),
        _ => model,
    })
}

fn gpu_specs() -> String {
    lspci_gpu_specs()
        .or_else(drm_gpu_specs)
        .unwrap_or_else(|| "GPU info unavailable".to_string())
}

fn lspci_gpu_specs() -> Option<String> {
    let output = Command::new("lspci").arg("-mm").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|stdout| gpu_specs_from_lspci(&stdout))
}

fn gpu_specs_from_lspci(lspci_output: &str) -> Option<String> {
    let mut gpus = Vec::<String>::new();

    for line in lspci_output.lines() {
        let fields = quoted_fields(line);
        let Some(class) = fields.first() else {
            continue;
        };
        if !gpu_pci_class(class) {
            continue;
        }

        let vendor = fields.get(1).map(String::as_str).unwrap_or_default();
        let device = fields.get(2).map(String::as_str).unwrap_or_default();
        let name = normalize_whitespace(&format!("{vendor} {device}"));
        if !name.is_empty() && !gpus.contains(&name) {
            gpus.push(name);
        }
    }

    (!gpus.is_empty()).then(|| gpus.join("; "))
}

fn drm_gpu_specs() -> Option<String> {
    let entries = fs::read_dir("/sys/class/drm").ok()?;
    let mut gpus = BTreeSet::<String>::new();

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with("card") || file_name.contains('-') {
            continue;
        }

        let device_dir = entry.path().join("device");
        let vendor = read_trimmed(device_dir.join("vendor")).unwrap_or_default();
        let device = read_trimmed(device_dir.join("device")).unwrap_or_default();
        if vendor.is_empty() && device.is_empty() {
            continue;
        }
        let driver = fs::read_link(device_dir.join("driver"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();

        let mut parts = Vec::new();
        parts.push(format!("PCI {vendor}:{device}"));
        if !driver.is_empty() {
            parts.push(driver);
        }
        gpus.insert(parts.join(" - "));
    }

    (!gpus.is_empty()).then(|| gpus.into_iter().collect::<Vec<_>>().join("; "))
}

fn gpu_pci_class(class: &str) -> bool {
    matches!(
        class,
        "VGA compatible controller" | "3D controller" | "Display controller"
    )
}

fn quoted_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for character in line.chars() {
        if !in_quotes {
            if character == '"' {
                in_quotes = true;
                field.clear();
            }
            continue;
        }

        if escaped {
            field.push(character);
            escaped = false;
            continue;
        }

        match character {
            '\\' => escaped = true,
            '"' => {
                fields.push(field.clone());
                field.clear();
                in_quotes = false;
            }
            _ => field.push(character),
        }
    }

    fields
}

fn ram_specs() -> String {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| ram_specs_from_meminfo(&contents))
        .unwrap_or_else(|| "RAM info unavailable".to_string())
}

fn ram_specs_from_meminfo(meminfo: &str) -> Option<String> {
    let total = meminfo_kib(meminfo, "MemTotal")?;
    let available = meminfo_kib(meminfo, "MemAvailable");
    let swap = meminfo_kib(meminfo, "SwapTotal");

    let mut parts = vec![format!(
        "{} total",
        format_bytes(total.saturating_mul(1024))
    )];
    if let Some(available) = available {
        parts.push(format!(
            "{} available",
            format_bytes(available.saturating_mul(1024))
        ));
    }
    if let Some(swap) = swap.filter(|swap| *swap > 0) {
        parts.push(format!("{} swap", format_bytes(swap.saturating_mul(1024))));
    }

    Some(parts.join(" - "))
}

fn meminfo_kib(meminfo: &str, name: &str) -> Option<u64> {
    for line in meminfo.lines() {
        let Some((key, value)) = key_value(line) else {
            continue;
        };
        if key != name {
            continue;
        }
        return value.split_whitespace().next()?.parse::<u64>().ok();
    }
    None
}

fn key_value(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    Some((key.trim(), value.trim()))
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|contents| contents.trim().to_string())
        .filter(|contents| !contents.is_empty())
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn count_label(count: usize, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
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

    use super::{
        cpu_specs_from_cpuinfo, format_bytes, gpu_specs_from_lspci, ram_specs_from_meminfo,
        should_show_mount, usage_fraction, usage_summary,
    };

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

    #[test]
    fn parses_cpu_specs() {
        let cpuinfo = r#"
processor   : 0
model name  : Example CPU 9000
cpu cores   : 1

processor   : 1
model name  : Example CPU 9000
cpu cores   : 1
"#;

        assert_eq!(
            cpu_specs_from_cpuinfo(cpuinfo).as_deref(),
            Some("Example CPU 9000 - 1 core / 2 threads")
        );
    }

    #[test]
    fn parses_gpu_specs_from_lspci() {
        let lspci = r#"
00:02.0 "VGA compatible controller" "Intel Corporation" "Arc Graphics"
01:00.0 "3D controller" "NVIDIA Corporation" "GeForce RTX 4070"
02:00.0 "Audio device" "NVIDIA Corporation" "High Definition Audio"
"#;

        assert_eq!(
            gpu_specs_from_lspci(lspci).as_deref(),
            Some("Intel Corporation Arc Graphics; NVIDIA Corporation GeForce RTX 4070")
        );
    }

    #[test]
    fn parses_ram_specs() {
        let meminfo = r#"
MemTotal:       1048576 kB
MemAvailable:    524288 kB
SwapTotal:       262144 kB
"#;

        assert_eq!(
            ram_specs_from_meminfo(meminfo).as_deref(),
            Some("1.0 GB total - 512.0 MB available - 256.0 MB swap")
        );
    }
}
