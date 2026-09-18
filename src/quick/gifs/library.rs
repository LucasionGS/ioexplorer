//! The saved GIFs: a plain folder of images, plus an index beside them holding
//! what the files themselves cannot — tags, the link each was saved from, and
//! when it was last used.
//!
//! The folder is the source of truth. Anything dropped in by hand shows up,
//! untagged; anything deleted by hand simply disappears, and its index entry
//! with it the next time the index is written.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::config::write_atomic;

/// Hidden, so it does not show up as one of the images in a file manager.
const INDEX_NAME: &str = ".ioexplorer-gifs.toml";

/// The image formats the tab saves and shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageKind {
    Gif,
    Png,
    Jpeg,
    Webp,
}

impl ImageKind {
    /// Judged by content, never by name: a `.gif` link often serves WebP.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    pub fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "gif" => Some(Self::Gif),
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Gif => "image/gif",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
struct Meta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    /// The link the image was saved from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// Seconds since the epoch.
    #[serde(default)]
    added: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    last_used: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Index {
    #[serde(default)]
    files: BTreeMap<String, Meta>,
}

/// One saved image.
#[derive(Clone, Debug, PartialEq)]
pub struct Gif {
    pub path: PathBuf,
    pub file_name: String,
    pub kind: ImageKind,
    pub tags: Vec<String>,
    pub source: Option<String>,
    added: u64,
    last_used: u64,
}

impl Gif {
    /// Its tags, or failing those its file name, for the footer.
    pub fn label(&self) -> String {
        if self.tags.is_empty() {
            self.file_name.clone()
        } else {
            self.tags.join(", ")
        }
    }

    fn matches(&self, words: &[String]) -> bool {
        let stem = Path::new(&self.file_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let candidates: Vec<&str> = self
            .tags
            .iter()
            .flat_map(|tag| split_words(tag))
            .chain(split_words(&stem))
            .collect();
        words.iter().all(|word| {
            candidates
                .iter()
                .any(|candidate| candidate.starts_with(word.as_str()))
        })
    }
}

pub struct Library {
    directory: PathBuf,
    index: Index,
}

impl Library {
    pub fn open(directory: PathBuf) -> Self {
        let index = match fs::read_to_string(directory.join(INDEX_NAME)) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot parse the GIF index; tags are unavailable");
                Index::default()
            }),
            Err(_) => Index::default(),
        };
        Self { directory, index }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Every image in the folder, most recently used first, then newest.
    pub fn list(&self) -> Vec<Gif> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let mut gifs: Vec<Gif> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let file_name = path.file_name()?.to_str()?.to_string();
                if file_name.starts_with('.') || !entry.file_type().ok()?.is_file() {
                    return None;
                }
                let kind = ImageKind::from_extension(&path)?;
                let meta = match self.index.files.get(&file_name) {
                    Some(meta) => meta.clone(),
                    // Dropped in by hand: dated by the file itself.
                    None => Meta {
                        added: modified_secs(&entry.metadata().ok()?),
                        ..Meta::default()
                    },
                };
                Some(Gif {
                    path,
                    file_name,
                    kind,
                    tags: meta.tags,
                    source: meta.source,
                    added: meta.added,
                    last_used: meta.last_used,
                })
            })
            .collect();
        gifs.sort_by(|a, b| {
            (b.last_used, b.added)
                .cmp(&(a.last_used, a.added))
                .then_with(|| a.file_name.cmp(&b.file_name))
        });
        gifs
    }

    /// Images whose tags or file name hold every word of `query`, each as the
    /// start of a word.
    pub fn search(&self, query: &str) -> Vec<Gif> {
        let words: Vec<String> = split_words(&query.to_lowercase())
            .map(str::to_string)
            .collect();
        self.list()
            .into_iter()
            .filter(|gif| gif.matches(&words))
            .collect()
    }

    /// The saved image with exactly these contents, if any.
    pub fn find_same(&self, bytes: &[u8]) -> Option<PathBuf> {
        self.list()
            .into_iter()
            .map(|gif| gif.path)
            .filter(|path| fs::metadata(path).is_ok_and(|meta| meta.len() == bytes.len() as u64))
            .find(|path| fs::read(path).is_ok_and(|contents| contents == bytes))
    }

    /// The saved image that came from `source`, if any.
    pub fn find_source(&self, source: &str) -> Option<PathBuf> {
        self.index
            .files
            .iter()
            .find(|(_, meta)| meta.source.as_deref() == Some(source))
            .map(|(name, _)| self.directory.join(name))
            .filter(|path| path.is_file())
    }

    pub fn save(
        &mut self,
        bytes: &[u8],
        kind: ImageKind,
        source: Option<String>,
        tags: Vec<String>,
    ) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.directory)?;
        let stem = source
            .as_deref()
            .and_then(stem_from_url)
            .or_else(|| tags.first().map(|tag| sanitise(tag)))
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| format!("image-{}", now_secs()));
        let path = unique_path(&self.directory, &stem, kind.extension());
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();

        // Beside the target and renamed, so an interrupted write never leaves
        // a truncated image in the folder.
        let partial = path.with_extension("part");
        fs::write(&partial, bytes)?;
        fs::rename(&partial, &path)?;

        self.index.files.insert(
            file_name,
            Meta {
                tags,
                source,
                added: now_secs(),
                last_used: 0,
            },
        );
        self.write_index()?;
        Ok(path)
    }

    pub fn set_tags(&mut self, file_name: &str, tags: Vec<String>) -> io::Result<()> {
        self.entry(file_name).tags = tags;
        self.write_index()
    }

    pub fn touch(&mut self, file_name: &str) -> io::Result<()> {
        self.entry(file_name).last_used = now_secs();
        self.write_index()
    }

    /// Moves the image to the trash, where a mistake can still be undone.
    pub fn remove(&mut self, file_name: &str) -> io::Result<()> {
        let path = self.directory.join(file_name);
        use gtk::gio::prelude::*;
        let file = gtk::gio::File::for_path(&path);
        if let Err(error) = file.trash(gtk::gio::Cancellable::NONE) {
            tracing::info!(%error, "cannot trash the image, deleting it");
            fs::remove_file(&path)?;
        }
        self.index.files.remove(file_name);
        self.write_index()
    }

    fn entry(&mut self, file_name: &str) -> &mut Meta {
        let path = self.directory.join(file_name);
        self.index
            .files
            .entry(file_name.to_string())
            .or_insert_with(|| Meta {
                added: fs::metadata(&path)
                    .map(|meta| modified_secs(&meta))
                    .unwrap_or_default(),
                ..Meta::default()
            })
    }

    /// Writes the index, dropping entries whose files are gone.
    fn write_index(&mut self) -> io::Result<()> {
        let directory = self.directory.clone();
        self.index
            .files
            .retain(|name, _| directory.join(name).is_file());
        let contents = toml::to_string_pretty(&self.index).map_err(io::Error::other)?;
        write_atomic(&self.directory.join(INDEX_NAME), &contents)
    }
}

/// Tags as typed: separated by commas or spaces, `#` optional, lowercase, no
/// repeats.
pub fn parse_tags(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in input.split(|ch: char| ch == ',' || ch.is_whitespace()) {
        let tag = tag.trim().trim_start_matches('#').to_lowercase();
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

fn split_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

/// A file name stem from a link's last path segment:
/// `https://media.tenor.com/abc/cat-dance.gif?x=1` gives `cat-dance`.
fn stem_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let segment = path.trim_end_matches('/').rsplit('/').next()?;
    let stem = Path::new(segment).file_stem()?.to_str()?;
    let stem = sanitise(stem);
    // Hosts name everything `giphy` or `tenor`, or a bare id; those say
    // nothing, and an untagged image is better named by when it came.
    let meaningless = ["giphy", "tenor", "image", "media", "original", "raw", "200"];
    (stem.len() > 2 && !meaningless.contains(&stem.as_str())).then_some(stem)
}

/// Letters, digits, `-` and `_` only, so no name can leave the folder or
/// upset a shell.
fn sanitise(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    cleaned
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(60)
        .collect()
}

fn unique_path(directory: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = directory.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|counter| directory.join(format!("{stem}-{counter}.{extension}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

fn modified_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";

    #[test]
    fn kinds_are_sniffed_from_content() {
        assert_eq!(ImageKind::sniff(GIF), Some(ImageKind::Gif));
        assert_eq!(
            ImageKind::sniff(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageKind::Png)
        );
        assert_eq!(ImageKind::sniff(b"\xff\xd8\xff\xe0"), Some(ImageKind::Jpeg));
        assert_eq!(
            ImageKind::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageKind::Webp)
        );
        assert_eq!(ImageKind::sniff(b"<!doctype html>"), None);
    }

    #[test]
    fn tags_are_normalised() {
        assert_eq!(
            parse_tags("Cat, #dance  cat,,happy"),
            ["cat", "dance", "happy"]
        );
        assert!(parse_tags("  , ").is_empty());
    }

    #[test]
    fn names_come_from_the_link_when_it_says_something() {
        assert_eq!(
            stem_from_url("https://media.tenor.com/abc/cat-dance.gif?x=1").as_deref(),
            Some("cat-dance")
        );
        assert_eq!(
            stem_from_url("https://media.giphy.com/media/xyz/giphy.gif"),
            None
        );
        assert_eq!(
            stem_from_url("https://x.test/../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(sanitise("../a b/c"), "a-b-c");
    }

    #[test]
    fn saving_tagging_and_searching() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path().to_path_buf());
        assert!(library.list().is_empty());

        let path = library
            .save(
                GIF,
                ImageKind::Gif,
                Some("https://media.tenor.com/a/cat-dance.gif".to_string()),
                parse_tags("cat happy"),
            )
            .unwrap();
        assert_eq!(path, dir.path().join("cat-dance.gif"));

        // A file dropped in by hand shows up too, untagged.
        fs::write(dir.path().join("wave.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        // Anything else in the folder does not.
        fs::write(dir.path().join("notes.txt"), b"hello").unwrap();

        let reopened = Library::open(dir.path().to_path_buf());
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(reopened.search("hap").len(), 1);
        assert_eq!(reopened.search("cat hap")[0].file_name, "cat-dance.gif");
        assert_eq!(reopened.search("wave")[0].kind, ImageKind::Png);
        assert!(reopened.search("dog").is_empty());
        assert_eq!(reopened.search("").len(), 2);

        assert_eq!(reopened.find_same(GIF), Some(path.clone()));
        assert_eq!(
            reopened.find_source("https://media.tenor.com/a/cat-dance.gif"),
            Some(path.clone())
        );

        let mut library = reopened;
        library
            .set_tags("wave.png", parse_tags("hello wave"))
            .unwrap();
        library.touch("wave.png").unwrap();
        let listed = Library::open(dir.path().to_path_buf()).list();
        assert_eq!(listed[0].file_name, "wave.png", "most recently used first");
        assert_eq!(listed[0].label(), "hello, wave");

        // A second save of the same name gets a counter.
        let again = library
            .save(GIF, ImageKind::Gif, None, parse_tags("cat dance"))
            .unwrap();
        assert_eq!(again, dir.path().join("cat.gif"));
        let third = library
            .save(GIF, ImageKind::Gif, None, parse_tags("cat"))
            .unwrap();
        assert_eq!(third, dir.path().join("cat-2.gif"));
    }

    #[test]
    fn removing_forgets_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path().to_path_buf());
        let path = library
            .save(GIF, ImageKind::Gif, None, parse_tags("gone"))
            .unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        library.remove(&name).unwrap();
        assert!(!path.exists());
        assert!(Library::open(dir.path().to_path_buf()).list().is_empty());
    }
}
