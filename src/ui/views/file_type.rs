//! What kind of file an entry is, as far as its name can tell: the category it
//! is tinted by, the words the Kind column uses, and the icon it is drawn with.
//!
//! Everything here comes from the shared-mime-info globs through
//! `gio::content_type_guess`, which reads no file contents. A listing of ten
//! thousand files has to be drawn without opening any of them.

use std::{cell::RefCell, collections::HashMap, path::Path};

use gio::prelude::*;

use crate::{
    config::IconStyle,
    providers::{FileItem, FileKind},
};

/// The broad family a file belongs to. Each one has a tint in the stylesheet
/// (`--io-ft-*`), which the symbolic icon style paints the icon with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileCategory {
    Folder,
    Image,
    Video,
    Audio,
    Archive,
    Document,
    Spreadsheet,
    Presentation,
    Pdf,
    Code,
    Text,
    Executable,
    DiskImage,
    Font,
    Other,
}

impl FileCategory {
    pub fn of(item: &FileItem) -> Self {
        if item.kind == FileKind::Directory {
            return Self::Folder;
        }
        content_type(item)
            .map(|content_type| Self::from_content_type(&content_type))
            .unwrap_or(Self::Other)
    }

    /// The class carrying this category's tint.
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Folder => "ft-folder",
            Self::Image => "ft-image",
            Self::Video => "ft-video",
            Self::Audio => "ft-audio",
            Self::Archive => "ft-archive",
            Self::Document => "ft-document",
            Self::Spreadsheet => "ft-spreadsheet",
            Self::Presentation => "ft-presentation",
            Self::Pdf => "ft-pdf",
            Self::Code => "ft-code",
            Self::Text => "ft-text",
            Self::Executable => "ft-executable",
            Self::DiskImage => "ft-disk",
            Self::Font => "ft-font",
            Self::Other => "ft-other",
        }
    }

    /// The generic glyph the symbolic style draws this family with.
    fn glyph_name(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Image => "image-x-generic",
            Self::Video => "video-x-generic",
            Self::Audio => "audio-x-generic",
            Self::Archive => "package-x-generic",
            Self::Document | Self::Pdf => "x-office-document",
            Self::Spreadsheet => "x-office-spreadsheet",
            Self::Presentation => "x-office-presentation",
            Self::Executable => "application-x-executable",
            Self::DiskImage => "media-optical",
            Self::Font => "font-x-generic",
            Self::Code | Self::Text | Self::Other => "text-x-generic",
        }
    }

    pub const ALL_CLASSES: [&'static str; 15] = [
        "ft-folder",
        "ft-image",
        "ft-video",
        "ft-audio",
        "ft-archive",
        "ft-document",
        "ft-spreadsheet",
        "ft-presentation",
        "ft-pdf",
        "ft-code",
        "ft-text",
        "ft-executable",
        "ft-disk",
        "ft-font",
        "ft-other",
    ];

    fn from_content_type(content_type: &str) -> Self {
        let is_a = |parent: &str| gio::content_type_is_a(content_type, parent);
        let (media, subtype) = content_type.split_once('/').unwrap_or((content_type, ""));

        match media {
            "inode" if subtype == "directory" => return Self::Folder,
            "image" => return Self::Image,
            "video" => return Self::Video,
            "audio" => return Self::Audio,
            "font" => return Self::Font,
            _ => {}
        }

        if content_type == "application/pdf" {
            Self::Pdf
        } else if subtype.contains("spreadsheet")
            || subtype.contains("ms-excel")
            || subtype == "csv"
        {
            Self::Spreadsheet
        } else if subtype.contains("presentation") || subtype.contains("ms-powerpoint") {
            Self::Presentation
        } else if subtype.contains("opendocument.text")
            || subtype.contains("wordprocessing")
            || subtype == "msword"
            || subtype == "rtf"
            || subtype == "epub+zip"
        {
            Self::Document
        } else if DISK_IMAGES.iter().any(|parent| is_a(parent)) {
            Self::DiskImage
        } else if ARCHIVES.iter().any(|parent| is_a(parent)) || PACKAGES.contains(&content_type) {
            Self::Archive
        } else if content_type == "text/plain" || content_type == "text/markdown" {
            Self::Text
        } else if media == "text" || is_a("text/plain") {
            // Before executables: a script is both, and reads as code.
            Self::Code
        } else if EXECUTABLES.iter().any(|parent| is_a(parent)) {
            Self::Executable
        } else if subtype.contains("font") {
            Self::Font
        } else {
            Self::Other
        }
    }
}

const ARCHIVES: [&str; 12] = [
    "application/zip",
    "application/x-tar",
    "application/gzip",
    "application/x-7z-compressed",
    "application/vnd.rar",
    "application/x-rar",
    "application/x-xz",
    "application/x-bzip2",
    "application/zstd",
    "application/x-lzma",
    "application/x-lz4",
    "application/x-compressed-tar",
];

const PACKAGES: [&str; 5] = [
    "application/vnd.debian.binary-package",
    "application/x-rpm",
    "application/x-alpm-package",
    "application/vnd.flatpak",
    "application/vnd.snap",
];

const DISK_IMAGES: [&str; 4] = [
    "application/x-cd-image",
    "application/vnd.efi.iso",
    "application/x-raw-disk-image",
    "application/x-iso9660-image",
];

const EXECUTABLES: [&str; 5] = [
    "application/x-executable",
    "application/x-pie-executable",
    "application/x-sharedlib",
    "application/vnd.appimage",
    "application/x-msdownload",
];

/// The guessed MIME type of a file, from its name alone. `None` for a folder
/// or anything that is not a file.
pub fn content_type(item: &FileItem) -> Option<String> {
    if item.kind != FileKind::File {
        return None;
    }

    // Keyed on the extension, since that is all the guess looks at for nearly
    // every name; a bare name like "Makefile" or "Dockerfile" is keyed whole.
    let key = match Path::new(&item.name).extension() {
        Some(extension) => format!(".{}", extension.to_string_lossy().to_lowercase()),
        None => item.name.clone(),
    };

    thread_local! {
        static GUESSES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }
    GUESSES.with(|guesses| {
        if let Some(content_type) = guesses.borrow().get(&key) {
            return Some(content_type.clone());
        }
        let (content_type, _uncertain) = gio::content_type_guess(Some(&item.name), None);
        let content_type = content_type.to_string();
        guesses.borrow_mut().insert(key, content_type.clone());
        Some(content_type)
    })
}

/// What the Kind column and the details pane call an entry: "JPEG image",
/// "Rust source code", "Folder". Links keep the link wording, since that is
/// the most important thing to say about them.
pub fn kind_description(item: &FileItem) -> String {
    if item.link.is_some() || item.kind != FileKind::File {
        return item.kind_label().to_string();
    }

    let Some(content_type) = content_type(item) else {
        return item.kind_label().to_string();
    };
    if gio::content_type_is_unknown(&content_type) {
        return unknown_kind(&item.name);
    }
    let description = gio::content_type_get_description(&content_type);
    capitalise(&description)
}

/// "ISO file" for an extension the MIME database does not know.
fn unknown_kind(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty() && extension.len() <= 8)
        .map(|extension| format!("{} file", extension.to_uppercase()))
        .unwrap_or_else(|| "File".to_string())
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The icon an entry is drawn with when it has no icon of its own (a desktop
/// entry or an AppImage brings one) and no thumbnail yet.
pub fn icon(item: &FileItem, style: IconStyle) -> gio::Icon {
    let symbolic = style == IconStyle::Symbolic;
    match item.kind {
        FileKind::Directory => {
            let name = item
                .uri
                .local_path()
                .ok()
                .and_then(|path| special_folder_icon(&path))
                .unwrap_or("folder");
            themed(name, "folder", symbolic)
        }
        // One generic glyph per family rather than the MIME type's own
        // symbolic: icon themes fill gaps in their symbolic sets with the
        // full-colour icon, which then ignores the tint, so a folder of JSON
        // and shell scripts came out half tinted and half not.
        FileKind::File if symbolic => {
            themed(FileCategory::of(item).glyph_name(), "text-x-generic", true)
        }
        FileKind::File => match content_type(item) {
            Some(content_type) => gio::content_type_get_icon(&content_type),
            None => themed("text-x-generic", "text-x-generic", false),
        },
        FileKind::BrokenLink => themed("emblem-symbolic-link", "emblem-symbolic-link", symbolic),
        FileKind::Other => themed("unknown", "text-x-generic", symbolic),
    }
}

fn themed(name: &str, fallback: &str, symbolic: bool) -> gio::Icon {
    let names = if symbolic {
        vec![format!("{name}-symbolic"), format!("{fallback}-symbolic")]
    } else {
        vec![name.to_string(), fallback.to_string()]
    };
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    gio::ThemedIcon::from_names(&names).upcast()
}

/// The icon theme's own picture for the home folder and the XDG folders in it.
fn special_folder_icon(path: &Path) -> Option<&'static str> {
    let dirs = directories::UserDirs::new()?;
    let named = [
        (Some(dirs.home_dir()), "user-home"),
        (dirs.desktop_dir(), "user-desktop"),
        (dirs.document_dir(), "folder-documents"),
        (dirs.download_dir(), "folder-download"),
        (dirs.audio_dir(), "folder-music"),
        (dirs.picture_dir(), "folder-pictures"),
        (dirs.video_dir(), "folder-videos"),
        (dirs.template_dir(), "folder-templates"),
        (dirs.public_dir(), "folder-publicshare"),
    ];
    named
        .into_iter()
        .find(|(dir, _)| dir.is_some_and(|dir| dir == path))
        .map(|(_, icon)| icon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderUri;

    fn file(name: &str) -> FileItem {
        FileItem {
            uri: ProviderUri::local(format!("/tmp/{name}")),
            name: name.to_string(),
            display_name: None,
            icon: None,
            kind: FileKind::File,
            size: None,
            modified: None,
            created: None,
            hidden: false,
            link: None,
        }
    }

    #[test]
    fn files_fall_into_their_family_by_name() {
        assert_eq!(FileCategory::of(&file("photo.JPG")), FileCategory::Image);
        assert_eq!(FileCategory::of(&file("clip.mp4")), FileCategory::Video);
        assert_eq!(FileCategory::of(&file("track.flac")), FileCategory::Audio);
        assert_eq!(FileCategory::of(&file("fonts.zip")), FileCategory::Archive);
        assert_eq!(FileCategory::of(&file("src.tar.gz")), FileCategory::Archive);
        assert_eq!(FileCategory::of(&file("report.pdf")), FileCategory::Pdf);
        assert_eq!(
            FileCategory::of(&file("budget.xlsx")),
            FileCategory::Spreadsheet
        );
        assert_eq!(FileCategory::of(&file("notes.txt")), FileCategory::Text);
        assert_eq!(FileCategory::of(&file("main.rs")), FileCategory::Code);
        assert_eq!(FileCategory::of(&file("data.json")), FileCategory::Code);
        assert_eq!(FileCategory::of(&file("setup.py")), FileCategory::Code);
        assert_eq!(FileCategory::of(&file("install.sh")), FileCategory::Code);
    }

    #[test]
    fn a_folder_is_a_folder_whatever_its_name() {
        let folder = FileItem {
            kind: FileKind::Directory,
            ..file("photos.jpg")
        };

        assert_eq!(FileCategory::of(&folder), FileCategory::Folder);
        assert_eq!(kind_description(&folder), "Folder");
    }

    #[test]
    fn the_kind_names_the_format() {
        assert_eq!(kind_description(&file("photo.png")), "PNG image");
    }

    #[test]
    fn an_unknown_extension_is_named_after_itself() {
        assert_eq!(kind_description(&file("save.qqzz")), "QQZZ file");
        assert_eq!(unknown_kind("README"), "File");
    }

    /// GIO appends the full-colour names as fallbacks on its own; what matters
    /// is that the family's symbolic glyph is asked for first.
    #[test]
    fn a_symbolic_icon_asks_for_its_family_glyph_first() {
        let icon = icon(&file("photo.png"), IconStyle::Symbolic);
        let names = icon
            .downcast_ref::<gio::ThemedIcon>()
            .expect("themed icon")
            .names();

        assert_eq!(names[0], "image-x-generic-symbolic", "{names:?}");
    }
}
