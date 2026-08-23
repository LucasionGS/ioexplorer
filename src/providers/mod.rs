pub mod local;

use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    time::SystemTime,
};

use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unsupported provider `{0}`")]
    UnsupportedProvider(String),
    #[error("unsupported path or URI `{0}`")]
    UnsupportedInput(String),
    #[error("provider URI must contain an absolute path")]
    RelativePath,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct ProviderUri {
    provider: String,
    path: String,
}

impl ProviderUri {
    pub fn local(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let normalized = if path.as_os_str().is_empty() {
            "/".to_string()
        } else {
            path.to_string_lossy().to_string()
        };

        Self {
            provider: "local".to_string(),
            path: normalize_absolute_path(&normalized),
        }
    }

    pub fn root(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            path: "/".to_string(),
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn display_path(&self) -> String {
        if self.provider == "local" {
            self.path.clone()
        } else {
            self.to_string()
        }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.path == "/" {
            return None;
        }

        let parent = Path::new(&self.path).parent()?;
        Some(Self {
            provider: self.provider.clone(),
            path: normalize_absolute_path(&parent.to_string_lossy()),
        })
    }

    pub fn child(&self, name: &str) -> Self {
        let child = if self.path == "/" {
            format!("/{name}")
        } else {
            format!("{}/{}", self.path, name)
        };

        Self {
            provider: self.provider.clone(),
            path: normalize_absolute_path(&child),
        }
    }

    pub fn local_path(&self) -> ProviderResult<std::path::PathBuf> {
        if self.provider != "local" {
            return Err(ProviderError::UnsupportedProvider(self.provider.clone()));
        }
        Ok(std::path::PathBuf::from(&self.path))
    }

    pub fn to_file_uri(&self) -> Option<String> {
        let path = self.local_path().ok()?;
        Url::from_file_path(path).ok().map(|url| url.to_string())
    }
}

impl fmt::Display for ProviderUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}://{}", self.provider, self.path)
    }
}

impl FromStr for ProviderUri {
    type Err = ProviderError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ProviderError::UnsupportedInput(input.to_string()));
        }

        if Path::new(trimmed).is_absolute() {
            return Ok(Self::local(trimmed));
        }

        if trimmed.starts_with("file://") {
            let url = Url::parse(trimmed)
                .map_err(|_| ProviderError::UnsupportedInput(input.to_string()))?;
            let path = url
                .to_file_path()
                .map_err(|_| ProviderError::UnsupportedInput(input.to_string()))?;
            return Ok(Self::local(path));
        }

        if let Ok(url) = Url::parse(trimmed) {
            let provider = url.scheme();
            if provider == "local" {
                let path = url.path();
                if !path.starts_with('/') {
                    return Err(ProviderError::RelativePath);
                }
                return Ok(Self {
                    provider: provider.to_string(),
                    path: normalize_absolute_path(path),
                });
            }
            return Err(ProviderError::UnsupportedProvider(provider.to_string()));
        }

        Err(ProviderError::UnsupportedInput(input.to_string()))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FileKind {
    Directory,
    File,
    /// A symlink whose target is gone. A working link takes the kind of what it
    /// points at, so this variant is never the answer to "is it a link?" —
    /// `FileItem::link` is. Named for what it means to keep it that way.
    BrokenLink,
    Other,
}

impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Directory => "Folder",
            Self::File => "File",
            Self::BrokenLink => "Broken link",
            Self::Other => "Other",
        }
    }

    pub fn icon_name(self) -> &'static str {
        match self {
            Self::Directory => "folder-symbolic",
            Self::File => "text-x-generic-symbolic",
            Self::BrokenLink => "emblem-symbolic-link-symbolic",
            Self::Other => "unknown-symbolic",
        }
    }
}

/// What a symlink points at.
///
/// `target` is the raw `readlink` value because that is what the user wrote and
/// what a properties pane should show; `resolved` is the canonical path because
/// that is the only thing safe to navigate to. A broken link has the first and
/// not the second.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LinkInfo {
    pub target: PathBuf,
    pub resolved: Option<PathBuf>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum FileIcon {
    Themed(String),
    Path(PathBuf),
}

#[derive(Debug, Clone)]
pub struct FileItem {
    pub uri: ProviderUri,
    pub name: String,
    pub display_name: Option<String>,
    pub icon: Option<FileIcon>,
    pub kind: FileKind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// `None` where the filesystem does not record a birth time.
    pub created: Option<SystemTime>,
    pub hidden: bool,
    /// Set only for a symlink. `kind` describes the target, so this is the one
    /// thing that says an entry is a link at all.
    pub link: Option<LinkInfo>,
}

impl FileItem {
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// What the Kind column shows. A working link reads as a link to whatever
    /// it points at, which `kind` alone can no longer say.
    pub fn kind_label(&self) -> &'static str {
        match (&self.link, self.kind) {
            (Some(_), FileKind::Directory) => "Folder link",
            (Some(_), FileKind::File) => "File link",
            (Some(_), _) => "Broken link",
            (None, kind) => kind.label(),
        }
    }

    /// A link whose target is gone. `kind` is `BrokenLink` for exactly these.
    pub fn is_broken_link(&self) -> bool {
        self.link
            .as_ref()
            .is_some_and(|link| link.resolved.is_none())
    }

    /// The resolved target, for a link that points at a folder.
    pub fn linked_folder(&self) -> Option<&Path> {
        (self.kind == FileKind::Directory)
            .then(|| self.link.as_ref()?.resolved.as_deref())
            .flatten()
    }
}

pub trait Provider {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn root(&self) -> ProviderUri;
    fn list(&self, uri: &ProviderUri) -> ProviderResult<Vec<FileItem>>;
    fn metadata(&self, uri: &ProviderUri) -> ProviderResult<FileItem>;
}

fn normalize_absolute_path(path: &str) -> String {
    let mut normalized = path.replace("//", "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.is_empty() || !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn parses_absolute_local_path() {
        let uri = ProviderUri::from_str("/home/example").expect("local path");
        assert_eq!(uri.provider(), "local");
        assert_eq!(uri.path(), "/home/example");
    }

    #[test]
    fn parses_file_uri() {
        let uri = ProviderUri::from_str("file:///tmp/ioexplorer").expect("file uri");
        assert_eq!(uri.path(), "/tmp/ioexplorer");
    }

    #[test]
    fn builds_parent_uri() {
        let uri = ProviderUri::local("/home/example/Downloads");
        assert_eq!(uri.parent().expect("parent").path(), "/home/example");
    }
}
