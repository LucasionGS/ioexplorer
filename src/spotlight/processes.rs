//! Running processes: sampling `/proc`, and the signals a row can send.
//!
//! Everything here is read from `/proc` directly rather than shelling out to
//! `ps`. That is not about avoiding a dependency — it is about the numbers. CPU
//! usage is a *rate*, and a rate needs two samples: `ps` would report a
//! process's average since it started, which for a browser open all day is a
//! number that never moves. Keeping the previous tick counts here means the list
//! shows what the machine is doing now, which is the only reason to open a task
//! manager.
//!
//! Sampling runs on a worker thread for the same reason the window list does: a
//! few hundred processes is a few hundred file reads, and the main loop belongs
//! to a keyboard-grabbing overlay where a stall cannot even be escaped from.
//! Signals are sent through `kill`, which is a shell builtin and so always
//! present; the pid is an integer and the signal comes from a closed set, so
//! nothing user-controlled reaches that command line.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Where the kernel exposes process state.
const PROC_ROOT: &str = "/proc";
/// Where user names are looked up. Read once per launcher run.
const PASSWD_PATH: &str = "/etc/passwd";

/// Clock ticks per second, as `/proc` reports CPU time.
///
/// This is `USER_HZ`, which the kernel fixes at 100 for the `/proc` interface on
/// every architecture — deliberately independent of the internal `HZ`, precisely
/// so userspace can hard-code it. Reading it properly means `sysconf(_SC_CLK_TCK)`
/// and so a libc dependency, for a value that has not changed in decades.
const TICKS_PER_SECOND: f64 = 100.0;

/// Fallback page size, used only if it cannot be derived from `/proc` itself.
const FALLBACK_PAGE_KB: u64 = 4;

/// Cap on the processes collected, so a fork bomb cannot be enumerated without
/// bound. Far past what a desktop runs.
const MAX_PROCESSES: usize = 8192;

/// Cap on a `/proc` file read. `cmdline` is capped by the kernel at a page or
/// two, but a hostile `/proc` mount is not something to take on faith.
const MAX_PROC_FILE_BYTES: usize = 128 * 1024;

// -- the model -------------------------------------------------------------

/// One running process, with its resource use worked out.
#[derive(Clone, Debug, PartialEq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    /// The kernel's short name for it, which is what a user recognises.
    pub name: String,
    /// The full command line, or `[name]` for a kernel thread.
    pub command: String,
    pub state: char,
    pub uid: u32,
    /// The owner's login name, or `uid 1000` when there is no name to be had.
    pub user: String,
    pub threads: u32,
    pub rss_kb: u64,
    /// Percent of one core since the previous sample. A process spread over
    /// four cores can legitimately report 400.
    pub cpu_percent: f64,
    /// Total CPU time this process has used, in seconds.
    pub cpu_seconds: f64,
    /// How long it has been running.
    pub age: Duration,
    /// A kernel worker rather than something anyone started.
    pub kernel_thread: bool,
}

impl Process {
    /// The state letter written out. The kernel's own set, which is short and
    /// stable — anything unrecognised is shown as the letter itself.
    pub fn state_label(&self) -> String {
        match self.state {
            'R' => "Running".to_string(),
            'S' => "Sleeping".to_string(),
            'D' => "Waiting on disk".to_string(),
            'I' => "Idle".to_string(),
            'T' => "Stopped".to_string(),
            't' => "Traced".to_string(),
            'Z' => "Zombie".to_string(),
            'X' | 'x' => "Dead".to_string(),
            other => format!("State {other}"),
        }
    }

    /// Whether this process is suspended, so a row can offer the useful half of
    /// the stop/continue pair rather than both.
    pub fn stopped(&self) -> bool {
        self.state == 'T' || self.state == 't'
    }

    /// Memory as a fraction of the machine's, for the row's meter.
    pub fn memory_fraction(&self, total_kb: u64) -> f64 {
        match total_kb {
            0 => 0.0,
            total => (self.rss_kb as f64 / total as f64).clamp(0.0, 1.0),
        }
    }
}

/// One sweep of `/proc`: every process, plus what the machine as a whole is
/// doing. The totals ride along because they come from the same sweep and the
/// header shows them next to the list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub processes: Vec<Process>,
    /// System-wide CPU use, 0 to 100 across every core.
    pub cpu_percent: f64,
    pub cores: usize,
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
}

impl Snapshot {
    /// Memory in use as a fraction of the total.
    pub fn memory_fraction(&self) -> f64 {
        match self.mem_total_kb {
            0 => 0.0,
            total => (self.mem_used_kb as f64 / total as f64).clamp(0.0, 1.0),
        }
    }

    pub fn find(&self, pid: i32) -> Option<&Process> {
        self.processes.iter().find(|process| process.pid == pid)
    }
}

// -- sampling --------------------------------------------------------------

/// The counters a rate is measured against.
#[derive(Clone, Debug)]
struct Baseline {
    at: Instant,
    /// CPU ticks per pid at that moment.
    ticks: HashMap<i32, u64>,
    /// System-wide busy and total jiffies.
    busy: u64,
    total: u64,
}

/// Raw counters for one process, before any rate is worked out.
struct RawProcess {
    pid: i32,
    ppid: i32,
    name: String,
    command: String,
    state: char,
    uid: u32,
    threads: u32,
    rss_kb: u64,
    ticks: u64,
    /// Ticks since boot at which it started.
    start_ticks: u64,
    kernel_thread: bool,
}

/// One sweep, before the previous sweep is subtracted from it.
struct RawSample {
    processes: Vec<RawProcess>,
    busy: u64,
    total: u64,
    cores: usize,
    /// Seconds since boot.
    uptime: f64,
    mem_total_kb: u64,
    mem_used_kb: u64,
}

/// Reads `/proc` and turns consecutive readings into rates.
///
/// Holds the previous reading, so it must outlive a single sample — that is the
/// whole reason it is a struct rather than a function.
pub struct Sampler {
    root: PathBuf,
    users: HashMap<u32, String>,
    page_kb: u64,
    baseline: Option<Baseline>,
}

impl Sampler {
    pub fn new() -> Self {
        let root = PathBuf::from(PROC_ROOT);
        let page_kb = detect_page_kb(&root);
        Self {
            users: read_user_names(Path::new(PASSWD_PATH)),
            root,
            page_kb,
            baseline: None,
        }
    }

    /// Takes a reading, measuring rates against the previous one.
    pub fn sample(&mut self, now: Instant) -> Snapshot {
        let raw = self.read_raw();
        let (snapshot, baseline) = self.resolve(raw, now);
        self.baseline = Some(baseline);
        snapshot
    }

    /// Reads every counter this sweep needs. No rates, no interpretation.
    fn read_raw(&self) -> RawSample {
        let (busy, total, cores) = read_cpu_totals(&self.root);
        let (mem_total_kb, mem_used_kb) = read_memory(&self.root);
        let uptime = read_uptime(&self.root);

        let mut processes = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            tracing::warn!(root = %self.root.display(), "cannot list /proc");
            return RawSample {
                processes,
                busy,
                total,
                cores,
                uptime,
                mem_total_kb,
                mem_used_kb,
            };
        };

        for entry in entries.flatten() {
            if processes.len() >= MAX_PROCESSES {
                tracing::warn!("more processes than the list can hold; stopping the sweep");
                break;
            }
            // Only the numeric entries are processes; the rest of /proc is
            // kernel state.
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            // A process that exits mid-sweep leaves a directory that no longer
            // reads. That is ordinary, not an error worth reporting.
            if let Some(process) = self.read_process(pid, &entry.path()) {
                processes.push(process);
            }
        }

        RawSample {
            processes,
            busy,
            total,
            cores,
            uptime,
            mem_total_kb,
            mem_used_kb,
        }
    }

    fn read_process(&self, pid: i32, dir: &Path) -> Option<RawProcess> {
        let stat = read_capped(&dir.join("stat"))?;
        let parsed = parse_stat(&stat)?;

        // The directory is owned by whoever owns the process, so the owner comes
        // from a stat call that has already happened rather than from parsing
        // `status` — one fewer file read per process, several hundred times.
        let uid = fs::metadata(dir).map(uid_of).unwrap_or(0);

        let command = read_capped(&dir.join("cmdline")).unwrap_or_default();
        let command = command
            .split('\0')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        // An empty command line means the kernel owns it: kworkers and the like
        // have no arguments at all. `ps` writes those in brackets, and so do we.
        let kernel_thread = command.trim().is_empty();

        Some(RawProcess {
            pid,
            ppid: parsed.ppid,
            command: match kernel_thread {
                true => format!("[{}]", parsed.name),
                false => command,
            },
            name: parsed.name,
            state: parsed.state,
            uid,
            threads: parsed.threads,
            rss_kb: parsed.rss_pages.saturating_mul(self.page_kb),
            ticks: parsed.ticks,
            start_ticks: parsed.start_ticks,
            kernel_thread,
        })
    }

    /// Turns a raw sweep into rates, against the previous sweep if there is one.
    fn resolve(&self, raw: RawSample, now: Instant) -> (Snapshot, Baseline) {
        let previous = self.baseline.as_ref();
        let elapsed = previous.map(|baseline| now.saturating_duration_since(baseline.at));

        let mut ticks = HashMap::with_capacity(raw.processes.len());
        let mut processes = Vec::with_capacity(raw.processes.len());

        for process in raw.processes {
            ticks.insert(process.pid, process.ticks);

            let age_secs = (raw.uptime - process.start_ticks as f64 / TICKS_PER_SECOND).max(0.0);
            let cpu_percent = match (previous, elapsed) {
                // The usual path: how much CPU it took since the last sweep.
                (Some(baseline), Some(elapsed)) if elapsed > Duration::ZERO => {
                    let before = baseline.ticks.get(&process.pid).copied().unwrap_or(0);
                    // A process that started since the last sweep has no earlier
                    // reading, and `saturating_sub` keeps a restarted pid — same
                    // number, different process — from reading as negative work.
                    let delta = process.ticks.saturating_sub(before) as f64;
                    delta / TICKS_PER_SECOND / elapsed.as_secs_f64() * 100.0
                }
                // The first sweep has nothing to subtract, so the average over
                // the process's life stands in. It is the honest answer to "what
                // has this been doing", and it means the opening list is ranked
                // rather than uniformly zero.
                _ => match age_secs > 0.0 {
                    true => process.ticks as f64 / TICKS_PER_SECOND / age_secs * 100.0,
                    false => 0.0,
                },
            };

            processes.push(Process {
                pid: process.pid,
                ppid: process.ppid,
                name: process.name,
                command: process.command,
                state: process.state,
                uid: process.uid,
                user: self.user_name(process.uid),
                threads: process.threads,
                rss_kb: process.rss_kb,
                cpu_percent: cpu_percent.max(0.0),
                cpu_seconds: process.ticks as f64 / TICKS_PER_SECOND,
                age: Duration::from_secs_f64(age_secs),
                kernel_thread: process.kernel_thread,
            });
        }

        let cpu_percent = match previous {
            Some(baseline) => {
                let busy = raw.busy.saturating_sub(baseline.busy) as f64;
                let total = raw.total.saturating_sub(baseline.total) as f64;
                match total > 0.0 {
                    true => (busy / total * 100.0).clamp(0.0, 100.0),
                    // Two sweeps inside one jiffy: nothing has been measured, so
                    // the previous answer is as good as it gets.
                    false => 0.0,
                }
            }
            _ => match raw.total > 0 {
                true => (raw.busy as f64 / raw.total as f64 * 100.0).clamp(0.0, 100.0),
                false => 0.0,
            },
        };

        let snapshot = Snapshot {
            processes,
            cpu_percent,
            cores: raw.cores,
            mem_total_kb: raw.mem_total_kb,
            mem_used_kb: raw.mem_used_kb,
        };
        let baseline = Baseline {
            at: now,
            ticks,
            busy: raw.busy,
            total: raw.total,
        };

        (snapshot, baseline)
    }

    fn user_name(&self, uid: u32) -> String {
        self.users
            .get(&uid)
            .cloned()
            // Not every account is in `/etc/passwd` — LDAP and systemd-homed put
            // them elsewhere — so the number stands in rather than nothing.
            .unwrap_or_else(|| format!("uid {uid}"))
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

fn uid_of(metadata: fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.uid()
}

/// Reads a `/proc` file, refusing an implausibly large one.
fn read_capped(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > MAX_PROC_FILE_BYTES {
        return None;
    }
    // `cmdline` holds raw argv, which is not guaranteed to be UTF-8.
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

struct ParsedStat {
    name: String,
    state: char,
    ppid: i32,
    threads: u32,
    ticks: u64,
    start_ticks: u64,
    rss_pages: u64,
}

/// Parses one `/proc/[pid]/stat` line.
///
/// The awkward part is the second field: it is the executable name in
/// parentheses, and it may contain both spaces and parentheses of its own —
/// `(Web Content)` and `((sd-pam))` are both real. Splitting on whitespace from
/// the left is therefore wrong. The last `)` in the line is unambiguous, because
/// every field after it is a number.
fn parse_stat(line: &str) -> Option<ParsedStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }

    let name = line[open + 1..close].to_string();
    let fields = line[close + 1..].split_whitespace().collect::<Vec<_>>();

    // Field 3 of the line is index 0 here, so every documented field number is
    // three less. Named rather than inlined, since an off-by-one silently
    // reports somebody else's number.
    let field = |number: usize| fields.get(number - 3).copied().unwrap_or("0");
    let number = |value: &str| value.parse::<u64>().unwrap_or(0);

    Some(ParsedStat {
        name,
        state: field(3).chars().next().unwrap_or('?'),
        ppid: field(4).parse().unwrap_or(0),
        threads: field(20).parse().unwrap_or(1),
        // utime and stime: time in user code and in the kernel on its behalf.
        // The `c`-prefixed pair that follows is deliberately left out — it is
        // time already accounted to reaped children.
        ticks: number(field(14)).saturating_add(number(field(15))),
        start_ticks: number(field(22)),
        rss_pages: number(field(24)),
    })
}

/// Busy jiffies, total jiffies, and the core count, from `/proc/stat`.
fn read_cpu_totals(root: &Path) -> (u64, u64, usize) {
    let Some(text) = read_capped(&root.join("stat")) else {
        return (0, 0, 1);
    };

    let mut busy = 0;
    let mut total = 0;
    let mut cores = 0;

    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        // The aggregate line is `cpu  …`; the per-core ones are `cpu0 …`. Both
        // are wanted, for different reasons.
        if rest.starts_with(char::is_numeric) {
            cores += 1;
            continue;
        }

        let values = rest
            .split_whitespace()
            .filter_map(|value| value.parse::<u64>().ok())
            .collect::<Vec<_>>();
        total = values.iter().sum();
        // Idle is field 4 and iowait field 5. iowait counts as idle here: the
        // CPU is not doing anything during it, which is what the number claims
        // to say.
        let idle = values.iter().skip(3).take(2).sum::<u64>();
        busy = total.saturating_sub(idle);
    }

    (busy, total, cores.max(1))
}

/// Total and used memory in kB, from `/proc/meminfo`.
///
/// Used is total minus *available*, not minus free: free excludes the cache the
/// kernel would hand back on demand, which is why "free memory" on Linux looks
/// alarming and means nothing.
fn read_memory(root: &Path) -> (u64, u64) {
    let Some(text) = read_capped(&root.join("meminfo")) else {
        return (0, 0);
    };

    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
            .unwrap_or(0u64)
    };

    let total = value("MemTotal:");
    let available = value("MemAvailable:");
    (total, total.saturating_sub(available))
}

fn read_uptime(root: &Path) -> f64 {
    read_capped(&root.join("uptime"))
        .and_then(|text| text.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

/// Derives the page size in kB by comparing two views of our own memory.
///
/// `statm` counts pages and `status` reports kB, so their ratio is the page
/// size. That avoids both a libc call and an assumption: 4 kB is near-universal
/// but aarch64 kernels are built with 16 and 64 kB pages too, and a process's
/// memory would then be reported at a quarter or a sixteenth of the truth.
fn detect_page_kb(root: &Path) -> u64 {
    let pages = read_capped(&root.join("self/statm"))
        .and_then(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(0);
    let kb = read_capped(&root.join("self/status"))
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0u64);

    match pages > 0 && kb > 0 {
        true => (kb / pages).max(1),
        false => FALLBACK_PAGE_KB,
    }
}

/// Maps uids to login names, from `/etc/passwd`.
fn read_user_names(path: &Path) -> HashMap<u32, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };

    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let uid = fields.nth(1)?.parse().ok()?;
            (!name.is_empty()).then(|| (uid, name.to_string()))
        })
        .collect()
}

/// The process's working directory, if it can be read.
///
/// Only for the handful of rows on screen: it is a symlink read per call, and
/// another process's is readable only by its owner or by root.
pub fn working_directory(pid: i32) -> Option<PathBuf> {
    fs::read_link(Path::new(PROC_ROOT).join(pid.to_string()).join("cwd")).ok()
}

// -- the background source -------------------------------------------------

#[derive(Debug)]
pub struct ProcessEvent {
    generation: u64,
    snapshot: Snapshot,
}

/// Background source of process snapshots.
///
/// Same shape as the window list — worker thread, channel, generation counter —
/// with one addition: sampling is skipped while a sweep is already running.
/// Rates are measured between consecutive sweeps, so two overlapping ones would
/// leave the second measuring a few milliseconds and reporting noise.
pub struct ProcessSource {
    sampler: Arc<Mutex<Sampler>>,
    generation: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    sender: mpsc::Sender<ProcessEvent>,
    receiver: mpsc::Receiver<ProcessEvent>,
}

impl ProcessSource {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sampler: Arc::new(Mutex::new(Sampler::new())),
            generation: Arc::new(AtomicU64::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            sender,
            receiver,
        }
    }

    /// Starts a sweep, unless one is already in flight.
    pub fn refresh(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let sampler = Arc::clone(&self.sampler);
        let counter = Arc::clone(&self.generation);
        let running = Arc::clone(&self.running);
        let sender = self.sender.clone();

        thread::spawn(move || {
            let snapshot = match sampler.lock() {
                Ok(mut sampler) => sampler.sample(Instant::now()),
                // A panic in an earlier sweep poisoned the lock. The baseline is
                // the only state it holds, so recovering costs one sample's
                // accuracy and keeps the prefix working.
                Err(poisoned) => poisoned.into_inner().sample(Instant::now()),
            };
            running.store(false, Ordering::SeqCst);

            if counter.load(Ordering::Relaxed) == generation {
                let _ = sender.send(ProcessEvent {
                    generation,
                    snapshot,
                });
            }
        });
    }

    /// Forgets the rate baseline, so the next sweep measures from scratch.
    ///
    /// Called when the launcher is hidden: the gap until it reopens is not a
    /// measurement interval, and treating it as one would divide a minute of CPU
    /// time by an hour of wall clock and report zero for everything.
    pub fn reset(&self) {
        if let Ok(mut sampler) = self.sampler.lock() {
            sampler.baseline = None;
        }
    }

    /// The newest snapshot that is still current, if one arrived.
    pub fn drain(&self) -> Option<Snapshot> {
        let current = self.generation.load(Ordering::Relaxed);
        let mut latest = None;

        while let Ok(event) = self.receiver.try_recv() {
            if event.generation == current {
                latest = Some(event.snapshot);
            }
        }

        latest
    }
}

impl Default for ProcessSource {
    fn default() -> Self {
        Self::new()
    }
}

// -- signals ---------------------------------------------------------------

/// The signals a row can send. A closed set: these are the ones that mean
/// something to a person looking at a list of processes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// Ask it to exit, which lets it save and clean up.
    Term,
    /// Take it out of the kernel's hands. Nothing gets to run first.
    Kill,
    Stop,
    Cont,
}

impl Signal {
    /// The name `kill` knows it by.
    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "TERM",
            Self::Kill => "KILL",
            Self::Stop => "STOP",
            Self::Cont => "CONT",
        }
    }

    /// What the row calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Term => "End process",
            Self::Kill => "Force kill",
            Self::Stop => "Suspend",
            Self::Cont => "Resume",
        }
    }
}

/// Sends a signal to a process.
///
/// Two pids are refused outright. `1` is init: signalling it is either a no-op
/// or a catastrophe, and no launcher should be the thing that tries. Our own pid
/// is refused because the obvious mistakes — suspending the launcher from inside
/// the launcher, or killing the daemon mid-keystroke — leave the user with no
/// window to fix it from.
pub fn send(pid: i32, signal: Signal) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!("refusing to signal pid {pid}"));
    }
    if pid == std::process::id() as i32 {
        return Err("that process is the launcher itself".to_string());
    }

    // Both halves are closed values — a fixed signal name and an integer — so
    // there is nothing here for a command line to be tricked by.
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -{} {pid}", signal.name()))
        .output()
        .map_err(|error| format!("cannot run kill: {error}"))?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail
            .trim()
            // `sh: line 1: kill: (123) - Operation not permitted` says the same
            // thing twice; the row has no room for the first half.
            .rsplit(':')
            .next()
            .unwrap_or("")
            .trim();
        let detail = match detail.is_empty() {
            true => "the signal was refused",
            false => detail,
        };
        return Err(format!("cannot {} pid {pid}: {detail}", signal.name()));
    }

    Ok(())
}

// -- formatting ------------------------------------------------------------

/// Formats a size in kB the way a person reads it.
pub fn format_kb(kb: u64) -> String {
    const MB: f64 = 1024.0;
    const GB: f64 = 1024.0 * 1024.0;

    let kb = kb as f64;
    if kb >= GB {
        return format!("{:.1} GB", kb / GB);
    }
    if kb >= MB {
        return format!("{:.0} MB", kb / MB);
    }
    format!("{kb:.0} KB")
}

/// Formats a duration as the two units that matter at its scale.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (days, hours) = (seconds / 86_400, seconds % 86_400 / 3600);
    let (minutes, rest) = (seconds % 3600 / 60, seconds % 60);

    if days > 0 {
        return format!("{days}d {hours}h");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    if minutes > 0 {
        return format!("{minutes}m {rest}s");
    }
    format!("{rest}s")
}

/// Formats a CPU percentage, keeping a decimal only while it says something.
pub fn format_percent(percent: f64) -> String {
    match percent >= 100.0 {
        true => format!("{percent:.0}%"),
        false => format!("{percent:.1}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line, trimmed of the fields nothing reads. The name carries both a
    /// space and a parenthesis, which is the case that breaks a naive split.
    const STAT: &str = "1234 (Web Content (1)) S 1200 1234 1234 0 -1 4194304 100 0 0 0 \
        900 300 0 0 20 0 42 0 8800 2000000 65536 18446744073709551615";

    fn sampler(root: &Path) -> Sampler {
        Sampler {
            root: root.to_path_buf(),
            users: HashMap::from([(1000, "lucas".to_string())]),
            page_kb: 4,
            baseline: None,
        }
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the fixture directory");
        }
        fs::write(path, text).expect("write the fixture");
    }

    /// Builds a `/proc` with one process in it.
    fn fake_proc(dir: &Path, ticks: u64, busy: u64) {
        write(&dir.join("1234/stat"), &STAT.replace("900 300", &format!("{ticks} 0")));
        write(&dir.join("1234/cmdline"), "firefox\0-contentproc\0");
        write(
            &dir.join("stat"),
            &format!("cpu  {busy} 0 0 1000 0 0 0 0 0 0\ncpu0 1 2 3 4\ncpu1 1 2 3 4\n"),
        );
        write(&dir.join("meminfo"), "MemTotal:       16000000 kB\nMemAvailable:    6000000 kB\n");
        write(&dir.join("uptime"), "1000.00 900.00\n");
    }

    #[test]
    fn a_stat_line_is_parsed_past_a_name_containing_spaces_and_brackets() {
        let parsed = parse_stat(STAT).expect("the line parses");

        assert_eq!(parsed.name, "Web Content (1)");
        assert_eq!(parsed.state, 'S');
        assert_eq!(parsed.ppid, 1200);
        assert_eq!(parsed.threads, 42);
        // utime + stime, and not the two child fields that follow them.
        assert_eq!(parsed.ticks, 1200);
        assert_eq!(parsed.start_ticks, 8800);
        assert_eq!(parsed.rss_pages, 65536);
    }

    #[test]
    fn a_malformed_stat_line_is_refused_rather_than_panicking() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("1234 no brackets here").is_none());
        // Truncated: every field it cannot see reads as zero rather than panicking.
        let parsed = parse_stat("1 (init) S").expect("a short line still parses");
        assert_eq!(parsed.ticks, 0);
        assert_eq!(parsed.threads, 1);
    }

    #[test]
    fn a_process_carries_its_command_owner_and_memory() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 1200, 5000);

        let snapshot = sampler(dir.path()).sample(Instant::now());

        let process = snapshot.find(1234).expect("the process is listed");
        assert_eq!(process.name, "Web Content (1)");
        assert_eq!(process.command, "firefox -contentproc");
        assert!(!process.kernel_thread);
        // 65536 pages of 4 kB.
        assert_eq!(process.rss_kb, 262_144);
        assert_eq!(process.threads, 42);
        assert_eq!(process.state_label(), "Sleeping");
    }

    /// The kernel's own threads have no command line at all, and a blank row
    /// would say nothing about which one it is.
    #[test]
    fn a_kernel_thread_is_named_in_brackets() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 1200, 5000);
        write(&dir.path().join("1234/cmdline"), "");

        let snapshot = sampler(dir.path()).sample(Instant::now());

        let process = snapshot.find(1234).expect("the process is listed");
        assert!(process.kernel_thread);
        assert_eq!(process.command, "[Web Content (1)]");
    }

    /// The reason `/proc` is read directly rather than shelling out to `ps`: the
    /// number has to be a rate, not a lifetime average.
    #[test]
    fn cpu_use_is_measured_between_two_sweeps() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 1000, 5000);
        let mut sampler = sampler(dir.path());

        let start = Instant::now();
        sampler.sample(start);
        // A second of wall clock later, the process has used half a second of
        // CPU: 50 ticks at 100 per second.
        fake_proc(dir.path(), 1050, 5000);
        let snapshot = sampler.sample(start + Duration::from_secs(1));

        let process = snapshot.find(1234).expect("the process is listed");
        assert!(
            (process.cpu_percent - 50.0).abs() < 0.01,
            "got {}",
            process.cpu_percent
        );
        assert!((process.cpu_seconds - 10.5).abs() < 0.01);
    }

    /// With no previous sweep there is nothing to subtract, so the list would
    /// open with every row at zero — and therefore in no useful order.
    #[test]
    fn the_first_sweep_falls_back_to_the_lifetime_average() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 8800 start ticks against 1000 s of uptime is 912 s of life, in which
        // 100 ticks — one second — of CPU were used.
        fake_proc(dir.path(), 100, 5000);

        let snapshot = sampler(dir.path()).sample(Instant::now());

        let process = snapshot.find(1234).expect("the process is listed");
        assert!(
            (process.cpu_percent - 0.109).abs() < 0.01,
            "got {}",
            process.cpu_percent
        );
    }

    /// A pid is reused after a process exits. Without the guard the new
    /// process's smaller tick count would underflow into an enormous rate.
    #[test]
    fn a_recycled_pid_never_reports_negative_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 9000, 5000);
        let mut sampler = sampler(dir.path());

        let start = Instant::now();
        sampler.sample(start);
        fake_proc(dir.path(), 5, 5000);
        let snapshot = sampler.sample(start + Duration::from_secs(1));

        assert_eq!(snapshot.find(1234).expect("listed").cpu_percent, 0.0);
    }

    #[test]
    fn system_totals_come_from_the_same_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 1000, 5000);
        let mut sampler = sampler(dir.path());

        let start = Instant::now();
        sampler.sample(start);
        // 400 more busy jiffies out of 500 more in total.
        write(
            &dir.path().join("stat"),
            "cpu  5400 0 0 1100 0 0 0 0 0 0\ncpu0 1 2 3 4\ncpu1 1 2 3 4\n",
        );
        let snapshot = sampler.sample(start + Duration::from_secs(1));

        assert!(
            (snapshot.cpu_percent - 80.0).abs() < 0.01,
            "got {}",
            snapshot.cpu_percent
        );
        assert_eq!(snapshot.cores, 2);
        assert_eq!(snapshot.mem_total_kb, 16_000_000);
        // Used is total minus *available*, not minus free.
        assert_eq!(snapshot.mem_used_kb, 10_000_000);
        assert!((snapshot.memory_fraction() - 0.625).abs() < 0.001);
    }

    #[test]
    fn a_directory_that_is_not_a_process_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 1000, 5000);
        write(&dir.path().join("sys/kernel/hostname"), "box\n");
        write(&dir.path().join("irq/0/spurious"), "0\n");

        let snapshot = sampler(dir.path()).sample(Instant::now());

        assert_eq!(snapshot.processes.len(), 1);
    }

    /// A process that exits between listing the directory and reading its files
    /// is ordinary, and must not cost the rest of the sweep.
    #[test]
    fn a_process_that_vanishes_mid_sweep_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        fake_proc(dir.path(), 1000, 5000);
        fs::create_dir_all(dir.path().join("9999")).expect("an empty process directory");

        let snapshot = sampler(dir.path()).sample(Instant::now());

        assert_eq!(snapshot.processes.len(), 1);
        assert!(snapshot.find(9999).is_none());
    }

    #[test]
    fn owners_are_resolved_to_login_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("passwd"),
            "root:x:0:0:root:/root:/bin/bash\nlucas:x:1000:1000::/home/lucas:/bin/bash\n\n",
        );

        let users = read_user_names(&dir.path().join("passwd"));

        assert_eq!(users.get(&0).map(String::as_str), Some("root"));
        assert_eq!(users.get(&1000).map(String::as_str), Some("lucas"));
        assert_eq!(users.len(), 2);
    }

    /// An account that is not in `/etc/passwd` still has to render as something.
    #[test]
    fn an_unknown_uid_falls_back_to_its_number() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(sampler(dir.path()).user_name(4242), "uid 4242");
    }

    #[test]
    fn the_page_size_is_derived_rather_than_assumed() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("self/statm"), "5000 2000 100 1 0 400 0\n");
        write(&dir.path().join("self/status"), "Name:\tioexplorer\nVmRSS:\t   32000 kB\n");

        assert_eq!(detect_page_kb(dir.path()), 16);
    }

    #[test]
    fn an_unreadable_proc_falls_back_to_four_kilobyte_pages() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(detect_page_kb(dir.path()), FALLBACK_PAGE_KB);
    }

    /// Signalling init is either nothing or a catastrophe, and suspending the
    /// launcher from inside the launcher leaves no window to undo it from.
    #[test]
    fn dangerous_pids_are_refused_before_anything_runs() {
        for pid in [0, 1, -1] {
            assert!(send(pid, Signal::Term).is_err(), "pid {pid}");
        }
        let own = std::process::id() as i32;
        let error = send(own, Signal::Stop).expect_err("must refuse");
        assert!(error.contains("launcher itself"), "{error}");
    }

    #[test]
    fn signalling_a_pid_that_is_not_there_reports_why() {
        // Above the pid ceiling on any Linux, so it cannot exist.
        let error = send(i32::MAX, Signal::Term).expect_err("must fail");

        assert!(error.contains("cannot TERM"), "{error}");
    }

    #[test]
    fn sizes_read_the_way_a_person_says_them() {
        assert_eq!(format_kb(0), "0 KB");
        assert_eq!(format_kb(820), "820 KB");
        assert_eq!(format_kb(262_144), "256 MB");
        assert_eq!(format_kb(2_306_867), "2.2 GB");
    }

    #[test]
    fn durations_keep_the_two_units_that_matter() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(605)), "10m 5s");
        assert_eq!(format_duration(Duration::from_secs(9000)), "2h 30m");
        assert_eq!(format_duration(Duration::from_secs(400_000)), "4d 15h");
    }

    #[test]
    fn percentages_drop_the_decimal_once_it_stops_meaning_anything() {
        assert_eq!(format_percent(0.0), "0.0%");
        assert_eq!(format_percent(12.44), "12.4%");
        assert_eq!(format_percent(240.0), "240%");
    }
}
