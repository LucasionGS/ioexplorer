use std::{cell::RefCell, collections::BTreeSet, fs, path::Path, process::Command, rc::Rc};

use gtk::prelude::*;

use crate::ui::{
    context_menu::ContextMenu,
    volumes::{MountVolume, UnmountedVolume, VolumeAction, VolumeEntry, Volumes},
};

const UNMOUNTED_ROW_CLASS: &str = "computer-volume-row-unmounted";

pub struct ComputerPage {
    pub root: gtk::ScrolledWindow,
    pub list: gtk::ListBox,
    cpu_label: gtk::Label,
    gpu_label: gtk::Label,
    ram_label: gtk::Label,
    entries: Rc<RefCell<Vec<VolumeEntry>>>,
    volumes: Rc<Volumes>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SystemSpecs {
    cpu: String,
    gpu: String,
    ram: String,
}

impl ComputerPage {
    pub fn new(volumes: Rc<Volumes>) -> Self {
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

        let page = Self {
            root,
            list,
            cpu_label,
            gpu_label,
            ram_label,
            entries: Rc::new(RefCell::new(Vec::new())),
            volumes,
        };
        page.install_list_handlers();
        page
    }

    fn install_list_handlers(&self) {
        // A list box drops headers set outside its header func, so the section
        // title is placed here: above the first unmounted row.
        self.list.set_header_func(|row, before| {
            let starts_section = row.has_css_class(UNMOUNTED_ROW_CLASS)
                && !before.is_some_and(|before| before.has_css_class(UNMOUNTED_ROW_CLASS));
            if !starts_section {
                row.set_header(None::<&gtk::Widget>);
            } else if row.header().is_none() {
                row.set_header(Some(&section_header("Not mounted")));
            }
        });

        let entries = Rc::clone(&self.entries);
        let volumes = Rc::clone(&self.volumes);
        self.list.connect_row_activated(move |list, row| {
            let entry = entries.borrow().get(row.index().max(0) as usize).cloned();
            if let Some(entry) = entry {
                volumes.run(VolumeAction::Open, &entry, list.upcast_ref());
            }
        });

        let right_click = gtk::GestureClick::new();
        right_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let list = self.list.clone();
        let entries = Rc::clone(&self.entries);
        let volumes = Rc::clone(&self.volumes);
        right_click.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = list.row_at_y(y as i32) else {
                return;
            };
            let Some(entry) = entries.borrow().get(row.index().max(0) as usize).cloned() else {
                return;
            };

            gesture.set_state(gtk::EventSequenceState::Claimed);
            list.select_row(Some(&row));
            let context = volumes.context_menu(&entry, list.upcast_ref());
            ContextMenu::popup_at(&list, x, y, &context);
        });
        self.list.add_controller(right_click);
    }

    pub fn refresh(&self) {
        self.refresh_volumes();
        self.refresh_system_specs();
    }

    /// Rebuild the volume list without re-reading the system specs, which
    /// spawns `lspci` and is too slow to repeat on every device event.
    pub fn refresh_volumes(&self) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }

        let entries = self.volumes.entries();

        if entries.is_empty() {
            self.list.append(&empty_row());
        }
        for entry in &entries {
            self.list.append(&volume_row(entry, &self.volumes));
        }
        *self.entries.borrow_mut() = entries;
    }

    fn refresh_system_specs(&self) {
        let specs = system_specs();
        self.cpu_label.set_text(&specs.cpu);
        self.gpu_label.set_text(&specs.gpu);
        self.ram_label.set_text(&specs.ram);
    }

    pub fn volume_count(&self) -> usize {
        self.entries
            .borrow()
            .iter()
            .filter(|entry| matches!(entry, VolumeEntry::Mounted(_)))
            .count()
    }

    pub fn unmounted_count(&self) -> usize {
        self.entries.borrow().len() - self.volume_count()
    }
}

fn volume_row(entry: &VolumeEntry, volumes: &Rc<Volumes>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .css_classes(["computer-volume-row"])
        .build();

    let actions = volume_action_buttons(entry, volumes);
    let (icon, details) = match entry {
        VolumeEntry::Mounted(volume) => (&volume.icon, mounted_details(volume, &actions)),
        VolumeEntry::Unmounted(volume) => {
            row.add_css_class(UNMOUNTED_ROW_CLASS);
            (&volume.icon, unmounted_details(volume, &actions))
        }
    };

    let icon = gtk::Image::from_gicon(icon);
    icon.set_pixel_size(32);
    icon.set_valign(gtk::Align::Start);
    icon.add_css_class("computer-volume-icon");

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

fn volume_action_buttons(entry: &VolumeEntry, volumes: &Rc<Volumes>) -> gtk::Box {
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .valign(gtk::Align::Center)
        .css_classes(["computer-volume-actions"])
        .build();

    let actions = [
        (
            entry.can_mount(),
            VolumeAction::Mount,
            "Mount",
            "Mount this volume",
        ),
        (
            entry.can_unmount(),
            VolumeAction::Unmount,
            "Unmount",
            "Unmount this volume",
        ),
        (
            entry.can_eject(),
            VolumeAction::Eject,
            "Eject",
            "Eject the drive so it can be safely removed",
        ),
    ];
    for (available, action, label, tooltip) in actions {
        if !available {
            continue;
        }

        let button = gtk::Button::builder()
            .label(label)
            .tooltip_text(tooltip)
            .css_classes(["flat", "computer-volume-action"])
            .build();
        button.set_focusable(false);
        let entry = entry.clone();
        let volumes = Rc::clone(volumes);
        button.connect_clicked(move |button| {
            volumes.run(action, &entry, button.upcast_ref());
        });
        buttons.append(&button);
    }

    buttons
}

fn unmounted_details(volume: &UnmountedVolume, actions: &gtk::Box) -> gtk::Box {
    let name_label = gtk::Label::builder()
        .label(&volume.name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["computer-volume-name"])
        .build();

    let mut parts = vec![volume.device.clone(), volume.fs_type.clone()];
    if let Some(size) = volume.size.filter(|size| *size > 0) {
        parts.push(format_bytes(size));
    }
    let device_label = gtk::Label::builder()
        .label(join_non_empty(parts))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label"])
        .build();
    let state_label = gtk::Label::builder()
        .label("Not mounted - open to mount")
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label"])
        .build();

    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(5)
        .hexpand(true)
        .build();
    details.append(&title_row(&name_label, actions));
    details.append(&device_label);
    details.append(&state_label);
    details
}

fn mounted_details(volume: &MountVolume, actions: &gtk::Box) -> gtk::Box {
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
    details.append(&title_row(&name_label, actions));
    details.append(&device_label);
    details.append(&mount_label);
    details.append(&usage);
    details
}

/// The volume name with its action buttons beside it, so the buttons do not
/// squeeze the usage bar below.
fn title_row(name_label: &gtk::Label, actions: &gtk::Box) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    row.append(name_label);
    row.append(actions);
    row
}

fn section_header(title: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["computer-section-title"])
        .build()
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
    join_non_empty(parts)
}

fn join_non_empty(parts: Vec<String>) -> String {
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
    use super::{
        cpu_specs_from_cpuinfo, format_bytes, gpu_specs_from_lspci, ram_specs_from_meminfo,
        usage_fraction, usage_summary,
    };

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
