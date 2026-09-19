//! Getting the picked text to the application that had focus.
//!
//! Typing goes through `wtype`, which uses the virtual-keyboard protocol and
//! can type any character, emoji included. Without it, `ydotool` can still
//! press Ctrl+V with the text on the clipboard, which reaches most
//! applications, though not a terminal. Both run only after the menu is gone,
//! so focus is back where the text belongs.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use super::history;
use crate::{config::QuickInsert, launcher::spawn::on_path, shot::deliver::wl_copy};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Inserted {
    /// The text was put on the clipboard.
    pub copied: bool,
    /// The clipboard is served from this process, which has to outlive the
    /// paste — `wl-copy` was not there to hand it to.
    pub serve_clipboard: bool,
}

pub fn insert(text: &str, mode: QuickInsert) -> Inserted {
    let mut inserted = Inserted::default();
    let wants_copy = matches!(mode, QuickInsert::Copy | QuickInsert::Both);
    if wants_copy {
        copy(text, &mut inserted);
    }
    if mode == QuickInsert::Copy {
        return inserted;
    }

    match type_text(text) {
        Ok(()) => {}
        Err(error) if on_path("ydotool") => {
            tracing::info!(%error, "cannot type directly, pasting instead");
            if !inserted.copied {
                copy(text, &mut inserted);
            }
            if let Err(error) = paste() {
                tracing::warn!(%error, "cannot paste");
                notify_copied(text, &error);
            }
        }
        Err(error) => {
            tracing::warn!(%error, "cannot type the picked text");
            if !inserted.copied {
                copy(text, &mut inserted);
            }
            notify_copied(text, &error);
        }
    }
    inserted
}

fn copy(text: &str, inserted: &mut Inserted) {
    history::mark_own_copy();
    match wl_copy("text/plain;charset=utf-8", text.as_bytes()) {
        Ok(()) => inserted.copied = true,
        Err(error) => {
            tracing::info!(%error, "wl-copy unavailable, serving the clipboard in-process");
            if let Some(display) = gtk::gdk::Display::default() {
                use gtk::prelude::*;
                display.clipboard().set_text(text);
                inserted.copied = true;
                inserted.serve_clipboard = true;
            }
        }
    }
}

fn type_text(text: &str) -> Result<(), String> {
    if !on_path("wtype") {
        return Err("wtype is not installed".to_string());
    }
    // Through stdin, so text that starts with `-` is not read as an option.
    let mut child = Command::new("wtype")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run wtype: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "wtype has no stdin".to_string())?
        .write_all(text.as_bytes())
        .map_err(|error| format!("cannot send the text to wtype: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wtype did not finish: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "wtype failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Presses Ctrl+V, for what can only be pasted: an image on the clipboard.
pub fn paste_shortcut() -> Result<(), String> {
    if on_path("wtype") {
        let status = Command::new("wtype")
            .args(["-M", "ctrl", "-k", "v", "-m", "ctrl"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| format!("cannot run wtype: {error}"))?;
        if status.success() {
            return Ok(());
        }
        tracing::info!(%status, "wtype could not paste, trying ydotool");
    }
    paste()
}

/// Ctrl+V, by Linux key code: 29 is left Ctrl and 47 is V.
fn paste() -> Result<(), String> {
    let status = Command::new("ydotool")
        .args(["key", "29:1", "47:1", "47:0", "29:0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("cannot run ydotool: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ydotool exited with {status}"))
    }
}

/// Typing failed, so at least say where the text went.
fn notify_copied(text: &str, error: &str) {
    let spawned = Command::new("notify-send")
        .args([
            "--app-name",
            "ioexplorer-quick",
            "--icon",
            "edit-paste",
            &format!("Copied {text}"),
            &format!("It could not be typed ({error}). Install wtype to type directly."),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(error) = spawned {
        tracing::debug!(%error, "cannot send a notification");
    }
}
