//! On-disk cache for remote artwork.
//!
//! GTK loads a local image straight from the main loop, but a remote one would
//! block it on the network — and this window takes an exclusive keyboard grab,
//! so a stalled main loop is one the user cannot even Escape out of. Everything
//! here downloads on a worker thread and hands back a local path.

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::Duration,
};

use directories::ProjectDirs;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Time allowed to receive the response headers.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Total budget for the body, so a stalled transfer cannot pin a worker thread
/// forever. Safe here, unlike on a streaming response, because these are
/// size-bounded downloads that should finish in one go.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub fn is_remote(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// The cached file for `url`, if it has already been downloaded.
///
/// Cheap enough for the main loop: it stats one path and never opens a socket.
pub fn cached(subdir: &str, url: &str) -> Option<PathBuf> {
    let path = path_for(subdir, url)?;
    path.is_file().then_some(path)
}

/// Downloads `url` into the cache unless it is already there.
///
/// Blocking — worker threads only. Returns `None` on any failure, since a
/// missing picture is a cosmetic problem and never worth failing a search over.
pub fn fetch(subdir: &str, url: &str, max_bytes: u64) -> Option<PathBuf> {
    let path = path_for(subdir, url)?;
    if path.is_file() {
        return Some(path);
    }

    let response = ureq::get(url)
        .config()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(BODY_TIMEOUT))
        .build()
        .call()
        .ok()?;
    let bytes = response
        .into_body()
        .into_with_config()
        .limit(max_bytes)
        .read_to_vec()
        .ok()?;

    // Written via a neighbouring temporary and renamed, so a download cut off
    // halfway cannot leave a truncated file that later reads treat as a hit.
    let temporary = path.with_extension("part");
    fs::write(&temporary, &bytes).ok()?;
    fs::rename(&temporary, &path).ok()?;
    Some(path)
}

/// The cache path for a URL.
///
/// The file name is a hash of the URL and never any part of it, so nothing a
/// `get_results` command prints can escape the cache directory.
fn path_for(subdir: &str, url: &str) -> Option<PathBuf> {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    Some(directory(subdir)?.join(format!("{:016x}", hasher.finish())))
}

fn directory(subdir: &str) -> Option<PathBuf> {
    let dir = ProjectDirs::from("io.github", "ionix", "ioexplorer")?
        .cache_dir()
        .join(subdir);
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_urls_are_treated_as_remote() {
        assert!(is_remote("https://example.com/icon.png"));
        assert!(is_remote("http://example.com/icon.png"));
        assert!(!is_remote("file:///home/user/icon.png"));
        assert!(!is_remote("/home/user/icon.png"));
        assert!(!is_remote("folder-symbolic"));
    }

    #[test]
    fn a_url_always_maps_to_the_same_path_and_different_urls_do_not_collide() {
        let one = path_for("spotlight-test", "https://example.com/a.png").expect("a path");
        let again = path_for("spotlight-test", "https://example.com/a.png").expect("a path");
        let other = path_for("spotlight-test", "https://example.com/b.png").expect("a path");

        assert_eq!(one, again);
        assert_ne!(one, other);
    }

    #[test]
    fn the_cached_name_carries_nothing_from_the_url() {
        let path = path_for("spotlight-test", "https://example.com/../../escape.png")
            .expect("a path")
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();

        assert_eq!(path.len(), 16);
        assert!(path.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
