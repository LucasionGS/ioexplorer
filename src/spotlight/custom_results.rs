//! Result rows produced by a user command behind a `get_results` prefix.
//!
//! Same concurrency idiom as [`crate::spotlight::file_search`] and
//! [`crate::spotlight::ai::AiSession`]: `std::thread` + `mpsc`, drained from the
//! GTK main loop, with a monotonic generation counter standing in for
//! cancellation. The debounce lives inside the worker rather than on a GLib
//! timer — a keystroke during the wait bumps the generation and the worker
//! returns without ever spawning the process.

use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::spotlight::{
    image_cache,
    preview::{Preview, PreviewKind},
};

/// How long the command may run before it is killed. A launcher cannot wait
/// longer than this and stay usable.
const MAX_RUNTIME: Duration = Duration::from_secs(5);
/// How often the child is checked for exit, staleness and its time budget.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Cap on stdout, so a runaway command cannot exhaust memory.
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// Cap on rows kept from one run.
const MAX_RESULTS: usize = 200;
/// Cap on a downloaded icon.
const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;
/// Cache subdirectory for row icons.
const ICON_CACHE_SUBDIR: &str = "spotlight-icons";

/// One row the command returned.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CustomResult {
    pub title: String,
    /// Substituted into the prefix's `action` template when the row is picked.
    pub value: String,
    /// Already resolved to something GTK can load without blocking: an icon
    /// name, an absolute path, or a `file://` URI. A remote icon is downloaded
    /// on the worker thread first, so the main loop never touches the network.
    pub icon: Option<String>,
    /// Large text or artwork shown beside the list while this row is the one
    /// the user is pointing at. An image here is deliberately *not* fetched
    /// with the row — see [`crate::spotlight::preview`].
    pub preview: Option<Preview>,
}

#[derive(Debug)]
pub enum ResultsEvent {
    Ready {
        generation: u64,
        results: Vec<CustomResult>,
    },
    Failed {
        generation: u64,
        error: String,
    },
}

impl ResultsEvent {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Ready { generation, .. } | Self::Failed { generation, .. } => *generation,
        }
    }
}

pub struct CustomResultsRunner {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<ResultsEvent>,
    receiver: mpsc::Receiver<ResultsEvent>,
}

impl CustomResultsRunner {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    /// The generation currently accepted by [`CustomResultsRunner::drain`].
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Invalidates any in-flight run without starting a new one.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Starts a run after `delay`, superseding any in-flight one.
    pub fn start(&self, line: String, delay: Duration) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);
        thread::spawn(move || {
            if !delay.is_zero() {
                thread::sleep(delay);
            }

            let is_stale = || counter.load(Ordering::Relaxed) != generation;
            if is_stale() {
                return;
            }

            let event = match run(&line, &is_stale) {
                // Superseded mid-run; the newer worker owns the UI now.
                Ok(None) => return,
                Ok(Some(mut results)) => {
                    resolve_icons(&mut results, &is_stale);
                    ResultsEvent::Ready {
                        generation,
                        results,
                    }
                }
                Err(error) => ResultsEvent::Failed { generation, error },
            };
            let _ = sender.send(event);
        });

        generation
    }

    /// Collects events that are still current, discarding superseded ones.
    pub fn drain(&self) -> Vec<ResultsEvent> {
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

impl Default for CustomResultsRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs the command line and parses its stdout. `Ok(None)` means superseded.
fn run(line: &str, is_stale: &dyn Fn() -> bool) -> Result<Option<Vec<CustomResult>>, String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(line)
        .arg("ioexplorer-spotlight")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Diagnostics belong in the user's own log, not in the parsed payload.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run the command: {error}"))?;

    // Drained on its own thread: waiting on a child whose stdout pipe has
    // filled would deadlock, and a search command can easily print more than a
    // pipe buffer holds.
    let stdout = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut text = String::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(MAX_OUTPUT).read_to_string(&mut text);
        }
        text
    });

    let started = Instant::now();
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {}
            Err(error) => return Err(format!("cannot wait for the command: {error}")),
        }

        if is_stale() {
            reap(&mut child);
            return Ok(None);
        }
        if started.elapsed() > MAX_RUNTIME {
            reap(&mut child);
            break true;
        }

        thread::sleep(POLL_INTERVAL);
    };

    // Killing the child closes the pipe, so the reader always finishes.
    let output = reader.join().unwrap_or_default();
    if timed_out {
        return Err(format!(
            "the command did not finish within {}s",
            MAX_RUNTIME.as_secs()
        ));
    }

    parse(&output).map(Some)
}

fn reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Parses the `{"results": [...]}` payload, dropping rows with no title.
pub fn parse(output: &str) -> Result<Vec<CustomResult>, String> {
    let output = output.trim();
    if output.is_empty() {
        return Ok(Vec::new());
    }

    let payload: Payload = serde_json::from_str(output)
        .map_err(|error| format!("the command did not return valid JSON: {error}"))?;

    Ok(payload
        .results
        .into_iter()
        .filter(|item| !item.title.trim().is_empty())
        .take(MAX_RESULTS)
        .map(|item| CustomResult {
            title: item.title.trim().to_string(),
            value: item.value,
            icon: item
                .icon
                .map(|icon| icon.trim().to_string())
                .filter(|icon| !icon.is_empty()),
            preview: item.preview.and_then(RawPreview::into_preview),
        })
        .collect())
}

#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    results: Vec<RawResult>,
}

#[derive(Deserialize)]
struct RawResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    preview: Option<RawPreview>,
}

#[derive(Deserialize)]
struct RawPreview {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    caption: Option<String>,
}

impl RawPreview {
    /// An unrecognised `type` or an empty `content` drops the preview and keeps
    /// the row: a command that grows a preview kind we do not know yet should
    /// degrade to a plain list, not vanish from it.
    fn into_preview(self) -> Option<Preview> {
        let content = self.content.trim();
        if content.is_empty() {
            return None;
        }

        Some(Preview {
            kind: PreviewKind::parse(&self.kind)?,
            content: content.to_string(),
            caption: self
                .caption
                .map(|caption| caption.trim().to_string())
                .filter(|caption| !caption.is_empty()),
        })
    }
}

/// Downloads remote icons into the cache and rewrites them to local paths.
///
/// GTK loads a local icon straight from the main loop, but a remote one would
/// block it on the network — so the fetch happens here, on the worker. An icon
/// that cannot be fetched is dropped rather than failing the whole run.
fn resolve_icons(results: &mut [CustomResult], is_stale: &dyn Fn() -> bool) {
    for result in results.iter_mut() {
        let Some(url) = result
            .icon
            .as_deref()
            .filter(|icon| image_cache::is_remote(icon))
        else {
            continue;
        };
        if is_stale() {
            return;
        }

        result.icon = image_cache::fetch(ICON_CACHE_SUBDIR, url, MAX_ICON_BYTES)
            .map(|path| path.to_string_lossy().into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_payload() {
        let results = parse(
            r#"{
                "results": [
                    {
                        "title": "Result 1",
                        "value": "https://example.com/result1",
                        "icon": "https://example.com/icon1.png"
                    },
                    { "title": "Result 2", "value": "https://example.com/result2" }
                ]
            }"#,
        )
        .expect("valid payload");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result 1");
        assert_eq!(results[0].value, "https://example.com/result1");
        assert_eq!(
            results[0].icon.as_deref(),
            Some("https://example.com/icon1.png")
        );
        assert_eq!(results[1].icon, None, "an omitted icon stays absent");
    }

    #[test]
    fn no_output_is_no_results_rather_than_an_error() {
        assert_eq!(parse("").expect("empty output"), Vec::new());
        assert_eq!(parse("   \n").expect("blank output"), Vec::new());
        assert_eq!(parse(r#"{"results": []}"#).expect("empty list"), Vec::new());
    }

    #[test]
    fn rows_without_a_title_are_dropped() {
        let results = parse(r#"{"results": [{"value": "x"}, {"title": " ", "value": "y"}]}"#)
            .expect("valid payload");

        assert!(results.is_empty());
    }

    #[test]
    fn a_blank_icon_is_treated_as_absent() {
        let results =
            parse(r#"{"results": [{"title": "a", "value": "b", "icon": "  "}]}"#).expect("valid");

        assert_eq!(results[0].icon, None);
    }

    #[test]
    fn malformed_json_reports_an_error() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"results": "nope"}"#).is_err());
    }

    #[test]
    fn the_row_count_is_capped() {
        let rows = (0..MAX_RESULTS + 50)
            .map(|index| format!(r#"{{"title": "row {index}", "value": "{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");

        let results = parse(&format!(r#"{{"results": [{rows}]}}"#)).expect("valid payload");

        assert_eq!(results.len(), MAX_RESULTS);
    }

    #[test]
    fn a_preview_is_parsed_for_both_kinds() {
        let results = parse(
            r#"{
                "results": [
                    {
                        "title": "a",
                        "preview": { "type": "image", "content": "https://example.com/big.png" }
                    },
                    { "title": "b", "preview": { "type": "text", "content": "line one" } }
                ]
            }"#,
        )
        .expect("valid payload");

        assert_eq!(
            results[0].preview,
            Some(Preview {
                kind: PreviewKind::Image,
                content: "https://example.com/big.png".to_string(),
                caption: None,
            })
        );
        assert_eq!(
            results[1].preview,
            Some(Preview {
                kind: PreviewKind::Text,
                content: "line one".to_string(),
                caption: None,
            })
        );
    }

    #[test]
    fn an_unusable_preview_drops_without_taking_the_row_with_it() {
        let results = parse(
            r#"{
                "results": [
                    { "title": "unknown kind", "preview": { "type": "video", "content": "x" } },
                    { "title": "no content", "preview": { "type": "text", "content": "  " } },
                    { "title": "no preview" }
                ]
            }"#,
        )
        .expect("valid payload");

        assert_eq!(results.len(), 3, "every row survives");
        assert!(results.iter().all(|result| result.preview.is_none()));
    }

    #[test]
    fn a_command_that_prints_a_payload_is_run_and_parsed() {
        let runner = CustomResultsRunner::new();
        runner.start(
            r#"printf '{"results":[{"title":"one","value":"1"}]}'"#.to_string(),
            Duration::ZERO,
        );

        let results = wait_for_event(&runner);
        let ResultsEvent::Ready { results, .. } = results else {
            panic!("expected results, got {results:?}");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "one");
    }

    #[test]
    fn a_command_printing_junk_reports_a_failure() {
        let runner = CustomResultsRunner::new();
        runner.start("printf 'not json'".to_string(), Duration::ZERO);

        assert!(matches!(
            wait_for_event(&runner),
            ResultsEvent::Failed { .. }
        ));
    }

    #[test]
    fn a_superseded_run_never_reports() {
        let runner = CustomResultsRunner::new();
        runner.start("printf '{}'".to_string(), Duration::from_secs(30));
        runner.cancel();

        thread::sleep(Duration::from_millis(200));
        assert!(runner.drain().is_empty());
    }

    fn wait_for_event(runner: &CustomResultsRunner) -> ResultsEvent {
        for _ in 0..200 {
            if let Some(event) = runner.drain().pop() {
                return event;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("the runner reported nothing");
    }
}
