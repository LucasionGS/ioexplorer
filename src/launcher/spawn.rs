//! Process-spawning helpers shared by the launcher surfaces.
//!
//! Every child is reaped on a detached thread so a launcher daemon that runs for
//! days does not accumulate zombies.

use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    thread,
};

/// Resolves the file-manager binary: explicit override, then a sibling of the
/// running executable (so a `target/debug` build finds its own peer), then PATH.
pub fn ioexplorer_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("IOEXPLORER_APP") {
        return PathBuf::from(path);
    }

    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join("ioexplorer");
        if sibling.is_file() {
            return sibling;
        }
    }

    PathBuf::from("ioexplorer")
}

/// Opens a path in the file manager.
pub fn launch_in_ioexplorer(path: &Path) -> io::Result<()> {
    spawn_detached(Command::new(ioexplorer_binary()).arg(path))
}

/// Spawns a command and reaps it on a background thread.
pub fn spawn_detached(command: &mut Command) -> io::Result<()> {
    let mut child = command.spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Runs a shell command line, mirroring how custom actions are executed.
pub fn spawn_shell_line(line: &str, argv0: &str) -> io::Result<()> {
    spawn_detached(Command::new("sh").arg("-c").arg(line).arg(argv0))
}

/// Runs a shell command line inside the user's terminal emulator.
pub fn spawn_in_terminal(line: &str) -> io::Result<()> {
    let mut last_error = None;

    for terminal in terminal_candidates() {
        match spawn_detached(
            Command::new(&terminal)
                .arg("-e")
                .arg("sh")
                .arg("-c")
                .arg(line),
        ) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| io::Error::other("no terminal emulator found")))
}

fn terminal_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(terminal) = std::env::var("TERMINAL")
        && !terminal.trim().is_empty()
    {
        candidates.push(terminal);
    }
    candidates.extend(
        [
            "x-terminal-emulator",
            "kitty",
            "alacritty",
            "foot",
            "wezterm",
            "gnome-terminal",
            "konsole",
            "xterm",
        ]
        .into_iter()
        .map(str::to_string),
    );
    candidates
}
