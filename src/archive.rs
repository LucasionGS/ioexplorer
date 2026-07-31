//! Unpacking archives into a folder of their own.
//!
//! The work goes to `tar` and `unzip` rather than being decoded in process:
//! they are already on every system this ships to, they get the edge cases
//! right that a hand-rolled reader gets wrong, and running them out of process
//! means a malformed archive cannot take the file manager down with it.

use std::{
    ffi::{OsStr, OsString},
    fs,
    path::Path,
};

/// An archive this knows how to unpack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
}

/// Recognized suffixes, compound ones first so `.tar.gz` is never read as a
/// bare `.gz` — and so `photos.tar.gz` unpacks into `photos` rather than
/// `photos.tar`.
const SUFFIXES: [(&str, ArchiveFormat); 4] = [
    (".tar.gz", ArchiveFormat::TarGz),
    (".tgz", ArchiveFormat::TarGz),
    (".zip", ArchiveFormat::Zip),
    (".tar", ArchiveFormat::Tar),
];

/// What a name says a file is, and what to call the folder it unpacks into.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveName {
    pub format: ArchiveFormat,
    /// The name with its archive suffix taken off, in its original case.
    pub stem: String,
}

/// Reads an archive's format off its name, or `None` for anything else.
///
/// Names only: opening every file in a folder to sniff its magic bytes would
/// mean a read per entry just to decide whether to offer a menu item.
pub fn recognize(name: &str) -> Option<ArchiveName> {
    let lowercased = name.to_ascii_lowercase();

    SUFFIXES.iter().find_map(|(suffix, format)| {
        let stem = lowercased.strip_suffix(suffix)?;
        // A file called nothing but `.zip` is a hidden file, and has no name
        // left to unpack into.
        if stem.is_empty() {
            return None;
        }

        // Lowercasing ASCII leaves every byte where it was, so the stem's
        // length indexes the original name just as well.
        Some(ArchiveName {
            format: *format,
            stem: name.get(..stem.len())?.to_string(),
        })
    })
}

/// Unpacks `archive` into `destination`, which must not already exist.
///
/// A failure takes the destination back out with it. It was created here and
/// nothing else has had a chance to put anything in it, so a half-unpacked
/// folder left behind after an error is only litter.
pub async fn extract(
    format: ArchiveFormat,
    archive: &Path,
    destination: &Path,
) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| {
        format!(
            "failed to create {}: {error}",
            destination
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )
    })?;

    match run_first_available(commands(format, archive, destination)).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(error) = fs::remove_dir_all(destination) {
                tracing::warn!(%error, path = %destination.display(), "failed to clean up after a failed extraction");
            }
            Err(error)
        }
    }
}

/// Runs the first command that starts, and takes its verdict as final.
///
/// The alternatives are for tools that may not be installed, not for retrying
/// a failure: a tool that ran and gave up partway has already written files,
/// and turning a second one loose on the same folder would unpack over the top
/// of them.
async fn run_first_available(commands: Vec<Vec<OsString>>) -> Result<(), String> {
    let mut spawn_error = None;

    for command in commands {
        let argv = command
            .iter()
            .map(|argument| argument.as_os_str())
            .collect::<Vec<&OsStr>>();
        let process = match gio::Subprocess::newv(
            &argv,
            gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
        ) {
            Ok(process) => process,
            Err(error) => {
                spawn_error = Some(format!(
                    "failed to start {}: {error}",
                    command_name(&command)
                ));
                continue;
            }
        };

        let (_, stderr) = process
            .communicate_future(None)
            .await
            .map_err(|error| format!("{} failed: {error}", command_name(&command)))?;

        if process.is_successful() {
            return Ok(());
        }

        return Err(format!(
            "{} exited with status {}{}",
            command_name(&command),
            process.status(),
            stderr_summary(stderr.as_ref())
        ));
    }

    Err(spawn_error.unwrap_or_else(|| "no extraction command available".to_string()))
}

fn commands(format: ArchiveFormat, archive: &Path, destination: &Path) -> Vec<Vec<OsString>> {
    let archive = archive.as_os_str().to_os_string();
    let destination = destination.as_os_str().to_os_string();

    match format {
        // `unzip` is the usual one; `bsdtar` covers the systems that ship
        // libarchive's tar instead and have no `unzip` at all.
        ArchiveFormat::Zip => vec![
            command(["unzip", "-q", "-o"], &archive, ["-d"], &destination),
            command(["bsdtar", "-x", "-f"], &archive, ["-C"], &destination),
        ],
        ArchiveFormat::Tar => vec![command(["tar", "-x", "-f"], &archive, ["-C"], &destination)],
        ArchiveFormat::TarGz => {
            vec![command(
                ["tar", "-x", "-z", "-f"],
                &archive,
                ["-C"],
                &destination,
            )]
        }
    }
}

/// Builds `<leading> <archive> <trailing> <destination>`.
///
/// Both paths are absolute, so neither can be mistaken for an option however
/// the file happens to be named.
fn command<const LEADING: usize, const TRAILING: usize>(
    leading: [&str; LEADING],
    archive: &OsString,
    trailing: [&str; TRAILING],
    destination: &OsString,
) -> Vec<OsString> {
    leading
        .into_iter()
        .map(OsString::from)
        .chain([archive.clone()])
        .chain(trailing.into_iter().map(OsString::from))
        .chain([destination.clone()])
        .collect()
}

fn command_name(command: &[OsString]) -> String {
    command
        .first()
        .map(|argument| argument.to_string_lossy().into_owned())
        .unwrap_or_else(|| "extraction command".to_string())
}

fn stderr_summary(stderr: Option<&glib::Bytes>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let summary = String::from_utf8_lossy(stderr.as_ref());
    let summary = summary.trim();
    if summary.is_empty() {
        String::new()
    } else {
        format!(": {summary}")
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn is_archive(name: &str) -> bool {
        recognize(name).is_some()
    }

    #[test]
    fn recognizes_the_supported_suffixes() {
        assert_eq!(
            recognize("photos.zip"),
            Some(ArchiveName {
                format: ArchiveFormat::Zip,
                stem: "photos".to_string(),
            })
        );
        assert_eq!(
            recognize("backup.tar"),
            Some(ArchiveName {
                format: ArchiveFormat::Tar,
                stem: "backup".to_string(),
            })
        );
        assert_eq!(
            recognize("release.tgz"),
            Some(ArchiveName {
                format: ArchiveFormat::TarGz,
                stem: "release".to_string(),
            })
        );
    }

    /// The whole reason the suffix table is ordered: a `.tar.gz` read as a
    /// `.tar` would be handed to the wrong `tar` flags and unpack into a
    /// folder still called `photos.tar`.
    #[test]
    fn a_compound_suffix_wins_over_its_tail() {
        let recognized = recognize("photos.tar.gz").expect("an archive");

        assert_eq!(recognized.format, ArchiveFormat::TarGz);
        assert_eq!(recognized.stem, "photos");
    }

    #[test]
    fn suffixes_are_matched_regardless_of_case() {
        let recognized = recognize("Photos.TAR.GZ").expect("an archive");

        assert_eq!(recognized.format, ArchiveFormat::TarGz);
        // The stem keeps the name's own case: it becomes a folder name.
        assert_eq!(recognized.stem, "Photos");
    }

    #[test]
    fn other_files_are_not_archives() {
        assert!(!is_archive("notes.txt"));
        assert!(!is_archive("photo.jpg"));
        assert!(!is_archive("archive"));
        assert!(!is_archive("libc.so.6"));
    }

    /// `.zip` alone is a hidden file, not an archive named nothing.
    #[test]
    fn a_bare_suffix_is_not_an_archive() {
        assert!(!is_archive(".zip"));
        assert!(!is_archive(".tar.gz"));
    }

    #[test]
    fn each_format_passes_the_archive_and_the_destination() {
        for format in [ArchiveFormat::Zip, ArchiveFormat::Tar, ArchiveFormat::TarGz] {
            for command in commands(format, Path::new("/tmp/a.bin"), Path::new("/tmp/out")) {
                assert!(
                    command.iter().any(|argument| argument == "/tmp/a.bin"),
                    "{format:?} does not pass the archive: {command:?}"
                );
                assert!(
                    command.iter().any(|argument| argument == "/tmp/out"),
                    "{format:?} does not pass the destination: {command:?}"
                );
            }
        }
    }

    #[test]
    fn a_zip_falls_back_to_a_second_tool() {
        let commands = commands(
            ArchiveFormat::Zip,
            Path::new("/tmp/a.zip"),
            Path::new("/tmp/out"),
        );

        let names = commands
            .iter()
            .map(|command| command_name(command))
            .collect::<Vec<_>>();
        assert_eq!(names, ["unzip", "bsdtar"]);
    }

    #[test]
    fn a_gzipped_tar_is_decompressed_on_the_way_out() {
        let commands = commands(
            ArchiveFormat::TarGz,
            Path::new("/tmp/a.tar.gz"),
            Path::new("/tmp/out"),
        );

        assert!(commands[0].iter().any(|argument| argument == "-z"));
    }

    #[test]
    fn a_plain_tar_is_not_decompressed() {
        let commands = commands(
            ArchiveFormat::Tar,
            Path::new("/tmp/a.tar"),
            Path::new("/tmp/out"),
        );

        assert!(!commands[0].iter().any(|argument| argument == "-z"));
    }

    /// End to end against the real `tar`, which is on every system this runs on.
    #[test]
    fn extracts_a_gzipped_tar() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("payload");
        fs::create_dir(&source).expect("source dir");
        fs::write(source.join("hello.txt"), "hello").expect("write a file to pack");

        let archive = dir.path().join("payload.tar.gz");
        let packed = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path())
            .arg("payload")
            .status()
            .expect("run tar");
        assert!(packed.success(), "failed to build the archive under test");

        let destination = dir.path().join("unpacked");
        glib::MainContext::default()
            .block_on(extract(ArchiveFormat::TarGz, &archive, &destination))
            .expect("extraction");

        assert_eq!(
            fs::read_to_string(destination.join("payload/hello.txt")).expect("extracted file"),
            "hello"
        );
    }

    /// A destination left behind after a failure would look like a successful
    /// extraction of an empty archive.
    #[test]
    fn a_failed_extraction_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let archive = dir.path().join("broken.tar.gz");
        fs::write(&archive, b"not actually an archive").expect("write the broken archive");

        let destination = dir.path().join("unpacked");
        let error = glib::MainContext::default()
            .block_on(extract(ArchiveFormat::TarGz, &archive, &destination))
            .expect_err("a broken archive cannot be extracted");

        assert!(error.contains("tar"), "unhelpful error: {error}");
        assert!(!destination.exists(), "the destination was left behind");
    }
}
