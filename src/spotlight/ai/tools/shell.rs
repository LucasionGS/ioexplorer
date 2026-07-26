//! Running a shell command and giving the model back what it printed.
//!
//! The rest of the side-effecting tools are fire-and-forget spawns: `open_path`
//! and `launch_app` hand something off to another process and there is nothing
//! to report but "done". A command is different — its *output is the answer*.
//! `uname -a` that reports only "Started: uname -a" tells the model nothing, so
//! it either guesses or asks the user something it could have found out itself.
//!
//! That makes this the one side-effecting tool that cannot run on the main
//! thread: waiting for a command to exit would freeze a `KeyboardMode::Exclusive`
//! overlay the user cannot even Escape out of. It goes through
//! [`super::ToolRunner`] like the read-only tools, and its result reaches the
//! model by the same route.

use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::ToolOutcome;

/// Cap on captured output. Beyond this the tail is dropped and the model is
/// told — unlike `read_file`, which refuses outright, because a command's first
/// lines are usually the answer and there is no way to ask for "less" of it.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// How often the exit status is checked while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(15);
/// Grace period for the reader to drain the pipe after the command exits.
///
/// The reader cannot simply be joined: a command that backgrounds something
/// (`foo &`) leaves a grandchild holding the write end open, so the pipe never
/// reaches EOF and a join would block until that grandchild died.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Runs `line` through `sh -c`, blocking until it exits or `timeout` elapses.
///
/// Blocking by contract — call it from a worker thread, never the main loop.
/// stdout and stderr are merged, because a model diagnosing a failure needs the
/// error text in the same place and the same order as the output.
pub fn run(line: &str, timeout: Duration) -> ToolOutcome {
    let line = line.trim();
    if line.is_empty() {
        return ToolOutcome::Error("nothing to run".to_string());
    }

    let (reader, writer) = match std::io::pipe() {
        Ok(pair) => pair,
        Err(error) => return ToolOutcome::Error(format!("cannot create a pipe: {error}")),
    };
    let Ok(second) = writer.try_clone() else {
        return ToolOutcome::Error("cannot duplicate the output pipe".to_string());
    };

    // stdin is /dev/null on purpose: a command that stops to ask something would
    // otherwise sit there until the timeout with nobody able to answer it.
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(line)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(second);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ToolOutcome::Error(format!("cannot run: {error}")),
    };
    // Releases this process's copies of the write end. Without it the pipe has a
    // writer that never writes, and the reader never sees EOF.
    drop(command);

    let sink = Arc::new(Mutex::new(Captured::default()));
    let finished = Arc::new(AtomicBool::new(false));
    spawn_reader(reader, Arc::clone(&sink), Arc::clone(&finished));

    let status = wait_for(&mut child, timeout);
    wait_for_drain(&finished);

    let captured = std::mem::take(&mut *sink.lock().unwrap_or_else(|error| error.into_inner()));
    report(status, captured, timeout)
}

/// Output collected so far. Shared rather than returned, so the wait can give up
/// on the reader without losing what it already read.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Drains the pipe on its own thread.
///
/// It keeps reading past [`MAX_OUTPUT_BYTES`] and discards the excess rather
/// than stopping. Stopping would leave the pipe full, and a command that writes
/// more than the buffer holds would block on `write` until the timeout killed
/// it — turning a chatty command into a failure.
fn spawn_reader(
    mut reader: std::io::PipeReader,
    sink: Arc<Mutex<Captured>>,
    finished: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let Ok(mut captured) = sink.lock() else {
                break;
            };
            let room = MAX_OUTPUT_BYTES.saturating_sub(captured.bytes.len());
            if read > room {
                captured.truncated = true;
            }
            captured.bytes.extend_from_slice(&chunk[..read.min(room)]);
        }
        finished.store(true, Ordering::Release);
    });
}

/// Waits for the command, killing it once `timeout` has elapsed.
///
/// `None` means it was still running and was stopped. Only the direct child is
/// killed: `sh -c` execs a lone command in place, so that is normally the
/// command itself, but a pipeline's members can outlive this.
fn wait_for(child: &mut std::process::Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            // Treat an unwaitable child as finished; there is nothing left to
            // poll for, and the output that arrived is still worth reporting.
            Err(_) => return None,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Gives the reader a moment to pick up whatever was written just before exit.
fn wait_for_drain(finished: &AtomicBool) {
    let deadline = Instant::now() + DRAIN_GRACE;
    while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Turns an exit status and its output into something the model can act on.
fn report(status: Option<ExitStatus>, captured: Captured, timeout: Duration) -> ToolOutcome {
    let mut body = String::from_utf8_lossy(&captured.bytes)
        .trim_end()
        .to_string();
    if captured.truncated {
        body.push_str(&format!(
            "\n[output truncated at {} KiB]",
            MAX_OUTPUT_BYTES / 1024
        ));
    }

    let Some(status) = status else {
        // The same opening phrase either way: the model has to be able to tell a
        // timeout from a silent success, and that turns on this sentence.
        let waited = describe_timeout(timeout);
        return ToolOutcome::Error(match body.is_empty() {
            true => format!("Still running after {waited} — stopped. No output."),
            false => format!("Still running after {waited} — stopped.\n{body}"),
        });
    };

    if status.success() {
        return match body.is_empty() {
            // Said explicitly rather than as an empty string: silence and
            // success are the same thing for most commands, and a bare "" reads
            // like the tool failed to report.
            true => ToolOutcome::Ok("(exit 0, no output)".to_string()),
            false => ToolOutcome::Ok(body),
        };
    }

    let how = describe(status);
    match body.is_empty() {
        true => ToolOutcome::Error(format!("{how}, no output")),
        false => ToolOutcome::Error(format!("{how}\n{body}")),
    }
}

/// A configured timeout is whole seconds; sub-second ones only turn up in tests,
/// where rounding to `0s` would read as no wait at all.
fn describe_timeout(timeout: Duration) -> String {
    match timeout.as_secs() {
        0 => format!("{}ms", timeout.as_millis()),
        seconds => format!("{seconds}s"),
    }
}

fn describe(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "exited abnormally".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(10);

    /// The whole point: what the command printed comes back, not "Started: …".
    #[test]
    fn returns_what_the_command_printed() {
        let outcome = run("echo hello world", LIMIT);
        assert_eq!(outcome, ToolOutcome::Ok("hello world".to_string()));
    }

    #[test]
    fn merges_stderr_into_the_output() {
        let outcome = run("echo out; echo err >&2", LIMIT);
        let text = outcome.text();
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("err"), "{text}");
    }

    /// A failure has to arrive as an error *with* its diagnostics, or the model
    /// cannot tell a broken command from an empty one.
    #[test]
    fn a_non_zero_exit_is_an_error_carrying_the_status() {
        let outcome = run("echo nope >&2; exit 3", LIMIT);
        assert!(outcome.is_error());
        assert!(outcome.text().contains("exit 3"), "{}", outcome.text());
        assert!(outcome.text().contains("nope"), "{}", outcome.text());
    }

    #[test]
    fn success_without_output_says_so() {
        assert_eq!(
            run("true", LIMIT),
            ToolOutcome::Ok("(exit 0, no output)".to_string())
        );
    }

    #[test]
    fn an_empty_command_is_refused_without_spawning() {
        assert!(run("   ", LIMIT).is_error());
    }

    /// The timeout has to fire on a command that would otherwise never return,
    /// and say so — a silent empty result would read as success.
    #[test]
    fn a_hanging_command_is_stopped_and_reported() {
        let outcome = run("sleep 30", Duration::from_millis(200));
        assert!(outcome.is_error());
        assert!(
            outcome.text().contains("Still running"),
            "{}",
            outcome.text()
        );
    }

    /// Output written before the hang must survive the kill: it is often the
    /// only clue about where the command got stuck.
    #[test]
    fn output_written_before_a_timeout_is_kept() {
        let outcome = run("echo progress; sleep 30", Duration::from_millis(400));
        assert!(outcome.text().contains("progress"), "{}", outcome.text());
    }

    /// A backgrounded grandchild holds the write end of the pipe open, so this
    /// must not wait on EOF — it would hang for the grandchild's whole life.
    #[test]
    fn a_backgrounded_child_does_not_hold_the_call_open() {
        let started = Instant::now();
        let outcome = run("echo done; sleep 20 &", Duration::from_secs(10));
        assert!(outcome.text().contains("done"), "{}", outcome.text());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    /// More output than the cap must not deadlock the command on a full pipe.
    #[test]
    fn oversized_output_is_truncated_rather_than_blocking() {
        let outcome = run("yes hello | head -c 200000", Duration::from_secs(20));
        assert!(!outcome.is_error(), "{}", outcome.text());
        assert!(
            outcome.text().contains("truncated"),
            "expected a truncation note"
        );
        assert!(outcome.text().len() < MAX_OUTPUT_BYTES + 1024);
    }

    #[test]
    fn stdin_is_closed_so_a_prompting_command_cannot_hang() {
        let started = Instant::now();
        let outcome = run("cat", Duration::from_secs(10));
        assert_eq!(outcome, ToolOutcome::Ok("(exit 0, no output)".to_string()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
