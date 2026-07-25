//! Bounded background filesystem search behind the `/` prefix.
//!
//! Follows the codebase's existing concurrency idiom — `std::thread` + `mpsc`,
//! drained from the GTK main loop — rather than introducing an async runtime.
//! Cancellation is a monotonic generation counter: each keystroke bumps it and
//! spawns a fresh worker, and both the worker and the main thread discard
//! anything stamped with an older generation. No locks, no joins, no leaks.

use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::launcher::fuzzy;

const MAX_DEPTH: usize = 6;
const MAX_ENTRIES_VISITED: usize = 40_000;
const MAX_HITS: usize = 200;
const MAX_DURATION: Duration = Duration::from_millis(1_500);
/// How often the elapsed-time budget is rechecked, in entries visited.
const TIME_CHECK_INTERVAL: usize = 256;
const BATCH_SIZE: usize = 25;
const BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Directories that are almost never what the user is looking for and are
/// expensive to walk. Dot-prefixed names are skipped separately.
const SKIPPED_DIRS: [&str; 5] = ["node_modules", "target", "__pycache__", "venv", "build"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileHit {
    pub path: PathBuf,
    pub is_dir: bool,
    pub score: i32,
}

#[derive(Debug)]
pub enum SearchEvent {
    Batch { generation: u64, hits: Vec<FileHit> },
    Done { generation: u64, truncated: bool },
}

impl SearchEvent {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Batch { generation, .. } | Self::Done { generation, .. } => *generation,
        }
    }
}

pub struct FileSearch {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<SearchEvent>,
    receiver: mpsc::Receiver<SearchEvent>,
}

impl FileSearch {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    /// The generation currently accepted by [`FileSearch::drain`].
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Invalidates any in-flight search without starting a new one.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Starts a search, superseding any in-flight one. Returns its generation.
    pub fn start(&self, query: &str, roots: Vec<PathBuf>) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let query = query.to_string();
        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);
        thread::spawn(move || {
            walk(&query, roots, generation, &counter, &sender);
        });

        generation
    }

    /// Collects events that are still current, discarding superseded ones.
    pub fn drain(&self) -> Vec<SearchEvent> {
        let current = self.current_generation();
        let mut events = Vec::new();

        while let Ok(event) = self.receiver.try_recv() {
            if event.generation() == current {
                events.push(event);
            }
        }

        events
    }
}

impl Default for FileSearch {
    fn default() -> Self {
        Self::new()
    }
}

fn walk(
    query: &str,
    roots: Vec<PathBuf>,
    generation: u64,
    counter: &Arc<AtomicU64>,
    sender: &mpsc::Sender<SearchEvent>,
) {
    let started = Instant::now();
    let mut queue = roots
        .into_iter()
        .map(|root| (root, 0usize))
        .collect::<VecDeque<_>>();
    // Canonicalized so overlapping roots (e.g. ~/Documents and ~) never double-walk.
    let mut visited = HashSet::new();
    let mut batch = Vec::new();
    let mut last_flush = Instant::now();
    let mut hits = 0usize;
    let mut entries_seen = 0usize;
    let mut truncated = false;

    while let Some((dir, depth)) = queue.pop_front() {
        if is_stale(generation, counter) {
            return;
        }

        let Ok(canonical) = fs::canonicalize(&dir) else {
            continue;
        };
        if !visited.insert(canonical) {
            continue;
        }

        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            entries_seen += 1;
            if entries_seen > MAX_ENTRIES_VISITED {
                truncated = true;
                break;
            }
            if entries_seen.is_multiple_of(TIME_CHECK_INTERVAL) {
                if is_stale(generation, counter) {
                    return;
                }
                if started.elapsed() > MAX_DURATION {
                    truncated = true;
                    break;
                }
            }

            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }

            // symlink_metadata, not metadata: a single `~/link -> /` would
            // otherwise turn this bounded walk into an unbounded one.
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            let is_dir = metadata.is_dir();

            if is_dir && depth < MAX_DEPTH {
                queue.push_back((entry.path(), depth + 1));
            }

            if let Some(found) = fuzzy::match_query(query, &name) {
                batch.push(FileHit {
                    path: entry.path(),
                    is_dir,
                    score: found.score,
                });
                hits += 1;

                if hits >= MAX_HITS {
                    truncated = true;
                    break;
                }
            }
        }

        let should_flush = batch.len() >= BATCH_SIZE || last_flush.elapsed() >= BATCH_INTERVAL;
        if !batch.is_empty() && should_flush {
            if sender
                .send(SearchEvent::Batch {
                    generation,
                    hits: std::mem::take(&mut batch),
                })
                .is_err()
            {
                return;
            }
            last_flush = Instant::now();
        }

        if truncated {
            break;
        }
    }

    if !batch.is_empty() {
        let _ = sender.send(SearchEvent::Batch {
            generation,
            hits: batch,
        });
    }
    let _ = sender.send(SearchEvent::Done {
        generation,
        truncated,
    });
}

fn is_stale(generation: u64, counter: &Arc<AtomicU64>) -> bool {
    counter.load(Ordering::Relaxed) != generation
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    /// Collects every event for `generation`, giving the worker time to finish.
    fn collect(search: &FileSearch, generation: u64) -> Vec<FileHit> {
        let deadline = Instant::now() + StdDuration::from_secs(10);
        let mut hits = Vec::new();

        while Instant::now() < deadline {
            match search.receiver.recv_timeout(StdDuration::from_millis(200)) {
                Ok(SearchEvent::Batch {
                    generation: found,
                    hits: batch,
                }) if found == generation => hits.extend(batch),
                Ok(SearchEvent::Done {
                    generation: found, ..
                }) if found == generation => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        hits
    }

    #[test]
    fn finds_matching_names_below_the_roots() {
        let temp = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp.path().join("docs")).expect("dir");
        fs::write(temp.path().join("docs/readme.md"), b"x").expect("file");

        let search = FileSearch::new();
        let generation = search.start("readme", vec![temp.path().to_path_buf()]);
        let hits = collect(&search, generation);

        assert!(
            hits.iter().any(|hit| hit.path.ends_with("readme.md")),
            "expected readme.md in {hits:?}"
        );
    }

    #[test]
    fn skips_hidden_and_denylisted_directories() {
        let temp = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp.path().join(".git")).expect("dir");
        fs::write(temp.path().join(".git/target-file"), b"x").expect("file");
        fs::create_dir_all(temp.path().join("node_modules")).expect("dir");
        fs::write(temp.path().join("node_modules/target-file"), b"x").expect("file");
        fs::write(temp.path().join("target-file"), b"x").expect("file");

        let search = FileSearch::new();
        let generation = search.start("target-file", vec![temp.path().to_path_buf()]);
        let hits = collect(&search, generation);

        assert_eq!(
            hits.len(),
            1,
            "only the top-level file should match: {hits:?}"
        );
    }

    #[test]
    fn respects_the_depth_cap() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut deep = temp.path().to_path_buf();
        for level in 0..(MAX_DEPTH + 3) {
            deep = deep.join(format!("level{level}"));
        }
        fs::create_dir_all(&deep).expect("deep dirs");
        fs::write(deep.join("needle-file"), b"x").expect("file");

        let search = FileSearch::new();
        let generation = search.start("needle-file", vec![temp.path().to_path_buf()]);
        let hits = collect(&search, generation);

        assert!(hits.is_empty(), "depth cap should exclude {hits:?}");
    }

    #[test]
    fn a_symlink_loop_does_not_hang_the_walk() {
        let temp = tempfile::tempdir().expect("temp dir");
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).expect("dir");
        std::os::unix::fs::symlink(temp.path(), nested.join("loop")).expect("symlink");
        fs::write(temp.path().join("loop-marker"), b"x").expect("file");

        let search = FileSearch::new();
        let generation = search.start("loop-marker", vec![temp.path().to_path_buf()]);
        let hits = collect(&search, generation);

        assert_eq!(hits.len(), 1, "the walk must terminate: {hits:?}");
    }

    #[test]
    fn cancelling_discards_stale_results() {
        let temp = tempfile::tempdir().expect("temp dir");
        fs::write(temp.path().join("stale-file"), b"x").expect("file");

        let search = FileSearch::new();
        search.start("stale-file", vec![temp.path().to_path_buf()]);
        search.cancel();
        thread::sleep(StdDuration::from_millis(200));

        assert!(search.drain().is_empty(), "stale batches must be dropped");
    }

    #[test]
    fn starting_a_new_search_supersedes_the_previous_generation() {
        let search = FileSearch::new();
        let first = search.start("a", Vec::new());
        let second = search.start("b", Vec::new());

        assert!(second > first);
        assert_eq!(search.current_generation(), second);
    }
}
