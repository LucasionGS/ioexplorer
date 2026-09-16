//! Where a finished shot goes: a file, the clipboard, stdout, and a
//! notification saying so.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use gtk::{gdk, glib, prelude::*};

use crate::config::{ShotConfig, write_bytes_atomic};

/// Where the output of one shot should go, after CLI flags have been applied
/// over the config.
#[derive(Clone, Debug, PartialEq)]
pub struct Destination {
    /// Save into this directory under a generated name.
    pub directory: Option<PathBuf>,
    pub file_name: String,
    /// An exact path from `--output`, overriding `directory`. `-` is stdout.
    pub explicit: Option<PathBuf>,
    pub copy: bool,
    pub notify: bool,
}

impl Destination {
    pub fn from_config(config: &ShotConfig) -> Self {
        Self {
            directory: config.save.then(|| config.directory_path()),
            file_name: config.file_name.clone(),
            explicit: None,
            copy: config.copy,
            notify: config.notify,
        }
    }
}

/// What actually happened, for the notification and the exit status.
#[derive(Debug, Default)]
pub struct Delivered {
    pub path: Option<PathBuf>,
    pub copied: bool,
    /// The clipboard is being served by this process, which must therefore
    /// stay alive until something else takes the clipboard over.
    pub serving_clipboard: bool,
}

pub fn deliver(
    texture: &gdk::Texture,
    png: &[u8],
    destination: &Destination,
) -> Result<Delivered, String> {
    let mut delivered = Delivered::default();

    match destination.explicit.as_deref() {
        Some(path) if path == Path::new("-") => {
            std::io::stdout()
                .lock()
                .write_all(png)
                .map_err(|error| format!("cannot write the image to stdout: {error}"))?;
        }
        Some(path) => {
            write_bytes_atomic(path, png)
                .map_err(|error| format!("cannot save {}: {error}", path.display()))?;
            delivered.path = Some(path.to_path_buf());
        }
        None => {
            if let Some(directory) = &destination.directory {
                let stem = expand_file_name(
                    &destination.file_name,
                    &glib::DateTime::now_local().ok(),
                    "png",
                );
                let path = unique_path(directory, &stem, "png", |path| path.exists());
                fs::create_dir_all(directory)
                    .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
                write_bytes_atomic(&path, png)
                    .map_err(|error| format!("cannot save {}: {error}", path.display()))?;
                delivered.path = Some(path);
            }
        }
    }

    if destination.copy {
        match copy_with_wl_copy(png) {
            Ok(()) => delivered.copied = true,
            Err(error) => {
                tracing::info!(%error, "wl-copy unavailable, serving the clipboard in-process");
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_texture(texture);
                    delivered.copied = true;
                    delivered.serving_clipboard = true;
                }
            }
        }
    }

    if destination.notify {
        notify_success(&delivered);
    }

    Ok(delivered)
}

/// Expands `strftime`-style fields in the configured name and makes the
/// result safe to use as a single path component.
pub fn expand_file_name(template: &str, now: &Option<glib::DateTime>, extension: &str) -> String {
    let expanded = now
        .as_ref()
        .and_then(|now| now.format(template).ok())
        .map(|name| name.to_string())
        .unwrap_or_else(|| template.to_string());

    // A slash from a template like ``%Y/%m` would silently create directories.
    let cleaned: String = expanded
        .trim()
        .chars()
        .map(|character| match character {
            '/' | '\0' => '-',
            other => other,
        })
        .collect();
    let cleaned = cleaned
        .trim_end_matches(&format!(".{extension}"))
        .trim_matches('.')
        .to_string();

    if cleaned.is_empty() {
        "Screenshot".to_string()
    } else {
        cleaned
    }
}

/// `directory/stem.ext`, or `stem-2.ext`, `stem-3.ext`… if taken. Two shots in
/// the same second must not overwrite each other.
pub fn unique_path(
    directory: &Path,
    stem: &str,
    extension: &str,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let first = directory.join(format!("{stem}.{extension}"));
    if !exists(&first) {
        return first;
    }
    (2..)
        .map(|index| directory.join(format!("{stem}-{index}.{extension}")))
        .find(|candidate| !exists(candidate))
        .expect("an unbounded range always yields a free name")
}

/// Puts a file on the clipboard as a file — pasting it into a chat or a file
/// manager attaches or copies the file itself, not its path as text.
pub fn copy_file(path: &Path) -> Result<(), String> {
    let uri = glib::filename_to_uri(path, None)
        .map_err(|error| format!("cannot make a URI of {}: {error}", path.display()))?;
    wl_copy("text/uri-list", format!("{uri}\r\n").as_bytes())
}

/// Hands the image to `wl-copy`, which keeps serving it after we exit.
fn copy_with_wl_copy(png: &[u8]) -> Result<(), String> {
    wl_copy("image/png", png)
}

fn wl_copy(mime_type: &str, contents: &[u8]) -> Result<(), String> {
    let mut child = Command::new("wl-copy")
        .args(["--type", mime_type])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run wl-copy: {error}"))?;

    child
        .stdin
        .take()
        .ok_or_else(|| "wl-copy has no stdin".to_string())?
        .write_all(contents)
        .map_err(|error| format!("cannot send the clipboard contents to wl-copy: {error}"))?;

    let status = child
        .wait()
        .map_err(|error| format!("wl-copy did not finish: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("wl-copy exited with {status}"))
    }
}

fn notify_success(delivered: &Delivered) {
    let (summary, body) = match (&delivered.path, delivered.copied) {
        (Some(path), true) => ("Screenshot saved and copied", path.display().to_string()),
        (Some(path), false) => ("Screenshot saved", path.display().to_string()),
        (None, true) => (
            "Screenshot copied",
            "The image is on the clipboard".to_string(),
        ),
        (None, false) => return,
    };
    let icon = delivered
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "camera-photo".to_string());
    notify(summary, &body, &icon);
}

pub fn notify_failure(error: &str) {
    notify("Screenshot failed", error, "dialog-error");
}

/// Fire and forget. A missing `notify-send` costs the notification and nothing
/// else — the shot itself has already been delivered.
pub fn notify(summary: &str, body: &str, icon: &str) {
    let spawned = Command::new("notify-send")
        .args([
            "--app-name",
            "ioexplorer-shot",
            "--icon",
            icon,
            summary,
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(error) = spawned {
        tracing::debug!(%error, "cannot send a notification");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn the_template_is_expanded_from_the_clock() {
        let moment = glib::DateTime::from_local(2026, 9, 16, 14, 5, 9.0).ok();
        assert_eq!(
            expand_file_name("Screenshot_%Y-%m-%d_%H-%M-%S", &moment, "png"),
            "Screenshot_2026-09-16_14-05-09"
        );
    }

    #[test]
    fn a_template_cannot_escape_the_directory_or_double_the_extension() {
        let moment = glib::DateTime::from_local(2026, 9, 16, 14, 5, 9.0).ok();
        assert_eq!(expand_file_name("%Y/%m.png", &moment, "png"), "2026-09");
        assert_eq!(expand_file_name("clip.mp4", &None, "mp4"), "clip");
        // Leading dots are dropped too, so a name cannot make a hidden file.
        assert_eq!(expand_file_name("../shot", &None, "png"), "-shot");
        assert_eq!(expand_file_name("   ", &None, "png"), "Screenshot");
    }

    #[test]
    fn a_taken_name_gets_a_counter() {
        let directory = Path::new("/shots");
        let taken: HashSet<PathBuf> = [directory.join("a.png"), directory.join("a-2.png")].into();

        assert_eq!(
            unique_path(directory, "b", "png", |path| taken.contains(path)),
            directory.join("b.png")
        );
        assert_eq!(
            unique_path(directory, "a", "png", |path| taken.contains(path)),
            directory.join("a-3.png")
        );
    }

    #[test]
    fn saving_writes_the_png_where_asked() {
        let temp = tempfile::tempdir().unwrap();
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            1,
            1,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(vec![1_u8, 2, 3, 255]),
            4,
        )
        .upcast();
        let png = texture.save_to_png_bytes().to_vec();
        let destination = Destination {
            directory: Some(temp.path().join("nested")),
            file_name: "shot".to_string(),
            explicit: None,
            copy: false,
            notify: false,
        };

        let delivered = deliver(&texture, &png, &destination).expect("saved");

        let path = delivered.path.expect("a path");
        assert_eq!(path, temp.path().join("nested/shot.png"));
        assert_eq!(fs::read(path).unwrap(), png);
        assert!(!delivered.copied);
    }
}
