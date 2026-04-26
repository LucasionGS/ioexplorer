use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{FileIcon, FileItem, FileKind, Provider, ProviderError, ProviderResult, ProviderUri};

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";
const APPIMAGE_ICON_EXTENSIONS: [&str; 4] = ["png", "svg", "xpm", "ico"];

#[derive(Debug, Default)]
pub struct LocalProvider;

impl LocalProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn path_for(&self, uri: &ProviderUri) -> ProviderResult<PathBuf> {
        uri.local_path()
    }
}

impl Provider for LocalProvider {
    fn id(&self) -> &'static str {
        "local"
    }

    fn name(&self) -> &'static str {
        "Local Files"
    }

    fn root(&self) -> ProviderUri {
        ProviderUri::root(self.id())
    }

    fn list(&self, uri: &ProviderUri) -> ProviderResult<Vec<FileItem>> {
        let path = self.path_for(uri)?;
        let mut items = Vec::new();

        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let item = item_from_path(uri, entry.path())?;
            items.push(item);
        }

        items.sort_by(|left, right| {
            right
                .kind
                .eq(&FileKind::Directory)
                .cmp(&left.kind.eq(&FileKind::Directory))
                .then_with(|| {
                    left.display_name()
                        .to_lowercase()
                        .cmp(&right.display_name().to_lowercase())
                })
        });

        Ok(items)
    }

    fn metadata(&self, uri: &ProviderUri) -> ProviderResult<FileItem> {
        item_from_path(
            &uri.parent().unwrap_or_else(|| ProviderUri::root(self.id())),
            self.path_for(uri)?,
        )
    }
}

fn item_from_path(parent: &ProviderUri, path: PathBuf) -> ProviderResult<FileItem> {
    let metadata = fs::symlink_metadata(&path)?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_dir() {
        FileKind::Directory
    } else if file_type.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ProviderError::UnsupportedInput(path.display().to_string()))?
        .to_string();
    let display_metadata = display_metadata_for_path(&path, &metadata, kind, &name);

    Ok(FileItem {
        uri: parent.child(&name),
        hidden: name.starts_with('.'),
        name,
        display_name: display_metadata.display_name,
        icon: display_metadata.icon,
        kind,
        size: (kind == FileKind::File).then_some(metadata.len()),
        modified: metadata.modified().ok(),
    })
}

#[derive(Default)]
struct DisplayMetadata {
    display_name: Option<String>,
    icon: Option<FileIcon>,
}

fn display_metadata_for_path(
    path: &Path,
    metadata: &fs::Metadata,
    kind: FileKind,
    name: &str,
) -> DisplayMetadata {
    if kind != FileKind::File {
        return DisplayMetadata::default();
    }

    if is_desktop_file(name) {
        desktop_file_metadata(path)
    } else if is_appimage_file(name) {
        appimage_metadata(path, metadata).unwrap_or_default()
    } else {
        DisplayMetadata::default()
    }
}

fn is_desktop_file(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".desktop")
}

fn is_appimage_file(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".appimage")
}

fn desktop_file_metadata(path: &Path) -> DisplayMetadata {
    let Ok(contents) = fs::read_to_string(path) else {
        return DisplayMetadata::default();
    };
    desktop_metadata_from_str(&contents, path.parent())
}

fn desktop_metadata_from_str(contents: &str, base_dir: Option<&Path>) -> DisplayMetadata {
    let locale_names = locale_name_keys();
    let mut in_desktop_entry = false;
    let mut best_name: Option<(usize, String)> = None;
    let mut icon = None;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = &line[1..line.len() - 1] == "Desktop Entry";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = desktop_unescape(value.trim());
        if let Some(rank) = desktop_name_rank(key, &locale_names) {
            if best_name
                .as_ref()
                .is_none_or(|(best_rank, _)| rank < *best_rank)
            {
                best_name = Some((rank, value));
            }
        } else if key == "Icon" && !value.is_empty() {
            icon = Some(desktop_icon_from_value(&value, base_dir));
        }
    }

    DisplayMetadata {
        display_name: best_name.map(|(_, name)| name),
        icon,
    }
}

fn locale_name_keys() -> Vec<String> {
    let locale = env::var("LC_MESSAGES")
        .or_else(|_| env::var("LANG"))
        .unwrap_or_default();
    let locale = locale
        .split('.')
        .next()
        .unwrap_or_default()
        .split('@')
        .next()
        .unwrap_or_default();
    if locale.is_empty() || locale == "C" || locale == "POSIX" {
        return Vec::new();
    }

    let mut keys = vec![format!("Name[{locale}]")];
    if let Some(language) = locale.split('_').next()
        && language != locale
    {
        keys.push(format!("Name[{language}]"));
    }
    keys
}

fn desktop_name_rank(key: &str, locale_names: &[String]) -> Option<usize> {
    locale_names
        .iter()
        .position(|locale_key| locale_key == key)
        .or_else(|| (key == "Name").then_some(locale_names.len()))
}

fn desktop_icon_from_value(value: &str, base_dir: Option<&Path>) -> FileIcon {
    let icon_path = Path::new(value);
    if icon_path.is_absolute() && icon_path.exists() {
        return FileIcon::Path(icon_path.to_path_buf());
    }
    if let Some(base_dir) = base_dir {
        let candidate = base_dir.join(value);
        if candidate.exists() {
            return FileIcon::Path(candidate);
        }
    }
    FileIcon::Themed(value.to_string())
}

fn desktop_unescape(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }

        match chars.next() {
            Some('s') => result.push(' '),
            Some('n') => result.push('\n'),
            Some('t') => result.push('\t'),
            Some('r') => result.push('\r'),
            Some('\\') => result.push('\\'),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }
    result
}

fn appimage_metadata(path: &Path, file_metadata: &fs::Metadata) -> Option<DisplayMetadata> {
    let offset = squashfs_offset(path)?;
    let files = appimage_file_list(path, offset)?;
    let desktop_path = files
        .iter()
        .filter(|file| file.ends_with(".desktop"))
        .min_by_key(|file| file.split('/').count())?;
    let desktop_contents =
        String::from_utf8(appimage_file_contents(path, offset, desktop_path)?).ok()?;
    let mut display_metadata = desktop_metadata_from_str(&desktop_contents, None);

    match display_metadata.icon.clone() {
        Some(FileIcon::Themed(icon_name)) => {
            if let Some(icon_path) =
                extract_appimage_icon(path, file_metadata, offset, &files, &icon_name)
            {
                display_metadata.icon = Some(FileIcon::Path(icon_path));
            }
        }
        Some(FileIcon::Path(_)) => {}
        None => {
            if let Some(icon_path) =
                extract_appimage_icon(path, file_metadata, offset, &files, ".DirIcon")
            {
                display_metadata.icon = Some(FileIcon::Path(icon_path));
            }
        }
    }

    Some(display_metadata)
}

fn squashfs_offset(path: &Path) -> Option<u64> {
    let mut file = fs::File::open(path).ok()?;
    let mut offset = 0_u64;
    let mut overlap = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }

        let mut chunk = overlap.clone();
        chunk.extend_from_slice(&buffer[..read]);
        if let Some(position) = chunk
            .windows(SQUASHFS_MAGIC.len())
            .position(|window| window == SQUASHFS_MAGIC)
        {
            return Some(offset.saturating_sub(overlap.len() as u64) + position as u64);
        }

        overlap = chunk
            .iter()
            .rev()
            .take(SQUASHFS_MAGIC.len() - 1)
            .copied()
            .collect::<Vec<_>>();
        overlap.reverse();
        offset += read as u64;
    }
}

fn appimage_file_list(path: &Path, offset: u64) -> Option<Vec<String>> {
    let output = Command::new("unsquashfs")
        .arg("-o")
        .arg(offset.to_string())
        .arg("-lc")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(normalize_unsquashfs_path)
        .collect::<Vec<_>>();
    (!files.is_empty()).then_some(files)
}

fn normalize_unsquashfs_path(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line == "squashfs-root" {
        return None;
    }

    let path = line.strip_prefix("squashfs-root/").unwrap_or(line);
    let path = path
        .strip_prefix("./")
        .unwrap_or(path)
        .trim_start_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

fn appimage_file_contents(path: &Path, offset: u64, internal_path: &str) -> Option<Vec<u8>> {
    for candidate in appimage_path_variants(internal_path) {
        let output = Command::new("unsquashfs")
            .arg("-o")
            .arg(offset.to_string())
            .arg("-cat")
            .arg(path)
            .arg(candidate)
            .output()
            .ok()?;
        if output.status.success() && !output.stdout.is_empty() {
            return Some(output.stdout);
        }
    }
    None
}

fn appimage_path_variants(internal_path: &str) -> [String; 2] {
    let normalized = internal_path.trim_start_matches('/').to_string();
    [normalized.clone(), format!("squashfs-root/{normalized}")]
}

fn extract_appimage_icon(
    appimage_path: &Path,
    metadata: &fs::Metadata,
    offset: u64,
    files: &[String],
    icon_name: &str,
) -> Option<PathBuf> {
    let mut candidates = appimage_icon_candidates(files, icon_name);
    candidates.sort_by_key(|candidate| appimage_icon_candidate_score(candidate, icon_name));

    for candidate in candidates {
        let cache_path = appimage_icon_cache_path(appimage_path, metadata, &candidate)?;
        if cache_path.exists() {
            return Some(cache_path);
        }

        let bytes = appimage_file_contents(appimage_path, offset, &candidate)?;
        if let Some(parent) = cache_path.parent()
            && fs::create_dir_all(parent).is_ok()
            && fs::write(&cache_path, bytes).is_ok()
        {
            return Some(cache_path);
        }
    }

    None
}

fn appimage_icon_candidates(files: &[String], icon_name: &str) -> Vec<String> {
    if icon_name == ".DirIcon" {
        return files
            .iter()
            .filter(|file| file_name(file).is_some_and(|name| name == ".DirIcon"))
            .cloned()
            .collect();
    }

    let icon_path = Path::new(icon_name);
    let wanted_extension = icon_path
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            APPIMAGE_ICON_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
    let wanted_stem = wanted_extension
        .and_then(|_| icon_path.file_stem().and_then(|stem| stem.to_str()))
        .unwrap_or(icon_name);

    files
        .iter()
        .filter(|file| {
            let Some(name) = file_name(file) else {
                return false;
            };
            let path = Path::new(name);
            let stem_matches = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem == wanted_stem);
            let extension_matches = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    wanted_extension
                        .map(|wanted| extension.eq_ignore_ascii_case(wanted))
                        .unwrap_or_else(|| {
                            APPIMAGE_ICON_EXTENSIONS
                                .iter()
                                .any(|known| extension.eq_ignore_ascii_case(known))
                        })
                });

            stem_matches && extension_matches
        })
        .cloned()
        .collect()
}

fn file_name(path: &str) -> Option<&str> {
    Path::new(path).file_name().and_then(|name| name.to_str())
}

fn appimage_icon_candidate_score(path: &str, icon_name: &str) -> i32 {
    if icon_name == ".DirIcon" {
        return path.split('/').count() as i32;
    }

    let lower = path.to_ascii_lowercase();
    let mut score = 100;
    if !path.contains('/') {
        score -= 40;
    }
    if lower.contains("/usr/share/icons/hicolor/scalable/apps/") {
        score -= 30;
    } else if lower.contains("/usr/share/icons/hicolor/") {
        score -= 20;
    } else if lower.contains("/usr/share/pixmaps/") {
        score -= 10;
    }
    if lower.ends_with(".svg") {
        score -= 8;
    } else if lower.ends_with(".png") {
        score -= 4;
    }
    score - icon_size_hint(path).unwrap_or_default().min(512) / 64
}

fn icon_size_hint(path: &str) -> Option<i32> {
    path.split('/').find_map(|component| {
        let (width, height) = component.split_once('x')?;
        let width = width.parse::<i32>().ok()?;
        let height = height.parse::<i32>().ok()?;
        (width == height).then_some(width)
    })
}

fn appimage_icon_cache_path(
    appimage_path: &Path,
    metadata: &fs::Metadata,
    internal_icon_path: &str,
) -> Option<PathBuf> {
    let mut hasher = DefaultHasher::new();
    appimage_path.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    modified_parts(metadata.modified().ok()).hash(&mut hasher);
    internal_icon_path.hash(&mut hasher);

    let file_name = file_name(internal_icon_path)?.replace('/', "_");
    Some(
        env::temp_dir()
            .join("ioexplorer-appimage-icons")
            .join(format!("{:x}", hasher.finish()))
            .join(file_name),
    )
}

fn modified_parts(modified: Option<SystemTime>) -> Option<(u64, u32)> {
    modified
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn lists_directories_before_files() {
        let dir = tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("folder")).expect("folder");
        fs::write(dir.path().join("alpha.txt"), "alpha").expect("file");

        let provider = LocalProvider::new();
        let items = provider
            .list(&ProviderUri::local(dir.path()))
            .expect("items");

        assert_eq!(items[0].name, "folder");
        assert_eq!(items[1].name, "alpha.txt");
    }

    #[test]
    fn resolves_local_paths() {
        let provider = LocalProvider::new();
        let path = provider
            .path_for(&ProviderUri::local(Path::new("/tmp")))
            .expect("path");

        assert_eq!(path, PathBuf::from("/tmp"));
    }

    #[test]
    fn parses_desktop_entry_name_and_themed_icon() {
        let metadata = desktop_metadata_from_str(
            r#"
[Other]
Name=Ignore Me

[Desktop Entry]
Type=Application
Name=Example App
Icon=org.example.App
"#,
            None,
        );

        assert_eq!(metadata.display_name.as_deref(), Some("Example App"));
        assert_eq!(
            metadata.icon,
            Some(FileIcon::Themed("org.example.App".into()))
        );
    }

    #[test]
    fn parses_desktop_entry_relative_icon_path() {
        let dir = tempdir().expect("temp dir");
        let icon_path = dir.path().join("icon.png");
        fs::write(&icon_path, "not really a png").expect("icon file");
        let metadata = desktop_metadata_from_str(
            r#"
[Desktop Entry]
Name=Example App
Icon=icon.png
"#,
            Some(dir.path()),
        );

        assert_eq!(metadata.icon, Some(FileIcon::Path(icon_path)));
    }

    #[test]
    fn normalizes_unsquashfs_listing_paths() {
        assert_eq!(
            normalize_unsquashfs_path("squashfs-root/usr/share/applications/app.desktop"),
            Some("usr/share/applications/app.desktop".into())
        );
        assert_eq!(normalize_unsquashfs_path("squashfs-root"), None);
    }

    #[test]
    fn finds_appimage_icon_candidates_by_icon_name() {
        let files = vec![
            "usr/share/icons/hicolor/64x64/apps/org.example.App.png".to_string(),
            "usr/share/applications/org.example.App.desktop".to_string(),
            "usr/share/pixmaps/other.png".to_string(),
        ];

        assert_eq!(
            appimage_icon_candidates(&files, "org.example.App"),
            vec!["usr/share/icons/hicolor/64x64/apps/org.example.App.png".to_string()]
        );
    }
}
