//! VPN control: reporting the connection, listing locations, and connecting to
//! one — through whichever VPN's command-line client is installed.
//!
//! The provider set is a plain enum rather than a trait object, the same shape
//! [`crate::spotlight::ai`] uses: adding a second VPN means one variant, one arm
//! in each dispatch below, and one new file. Nothing in the window, the results
//! or the config plumbing changes.
//!
//! Which provider is in play is an *environment* question, not a configuration
//! one: the prefix only exists when a client is actually on `PATH`, so an
//! IoExplorer install carrying this feature does not offer a VPN to someone who
//! has none. A user with several installed, or one whose client this module
//! would not have picked, names it in the config instead.
//!
//! Every query runs on a worker thread. They are local sockets and answer in
//! milliseconds, but they are still subprocesses, and `std::process::Command`
//! cannot be given a deadline — a wedged VPN daemon would otherwise block the
//! main loop of a layer surface the user cannot escape from.

mod windscribe;

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

use crate::{config::SpotlightVpnConfig, launcher::spawn::on_path};

/// How long a client may take before it is killed and reported as unreachable.
const MAX_RUNTIME: Duration = Duration::from_secs(5);
/// How often the child is checked for exit and for its time budget.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Cap on a client's reply. A full location list is tens of kilobytes.
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// Cap on the locations kept from one listing.
const MAX_LOCATIONS: usize = 2000;

/// A VPN whose command-line client this module knows how to drive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Windscribe,
}

impl Provider {
    /// Every provider, in the order [`detect`] tries them.
    pub const ALL: [Provider; 1] = [Provider::Windscribe];

    /// The name the config uses.
    pub fn id(self) -> &'static str {
        match self {
            Self::Windscribe => "windscribe",
        }
    }

    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::Windscribe => "Windscribe",
        }
    }

    /// The executable that has to be installed for this provider to work.
    pub fn program(self) -> &'static str {
        match self {
            Self::Windscribe => windscribe::PROGRAM,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Windscribe => "network-vpn-symbolic",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|provider| provider.id().eq_ignore_ascii_case(name))
    }

    /// Whether this provider's client is installed.
    pub fn installed(self) -> bool {
        on_path(self.program())
    }

    /// The shell line that connects to `target`.
    pub fn connect_line(self, target: &str) -> String {
        match self {
            Self::Windscribe => windscribe::connect_line(target),
        }
    }

    /// The shell line that lets the client pick the location itself.
    pub fn connect_best_line(self) -> String {
        match self {
            Self::Windscribe => windscribe::connect_line(windscribe::BEST_TARGET),
        }
    }

    /// The shell line that disconnects.
    pub fn disconnect_line(self) -> String {
        match self {
            Self::Windscribe => windscribe::disconnect_line(),
        }
    }

    fn status_args(self) -> &'static [&'static str] {
        match self {
            Self::Windscribe => windscribe::STATUS_ARGS,
        }
    }

    fn locations_args(self) -> &'static [&'static str] {
        match self {
            Self::Windscribe => windscribe::LOCATIONS_ARGS,
        }
    }

    fn parse_status(self, output: &str) -> Status {
        match self {
            Self::Windscribe => windscribe::parse_status(output),
        }
    }

    fn parse_locations(self, output: &str) -> Vec<Location> {
        match self {
            Self::Windscribe => windscribe::parse_locations(output),
        }
    }
}

/// The first installed provider, or `None` when the machine has no VPN client
/// this module can drive.
pub fn detect() -> Option<Provider> {
    Provider::ALL
        .into_iter()
        .find(|provider| provider.installed())
}

/// The provider the VPN prefix should use, honouring the config.
///
/// A named provider is still required to be installed. The alternative — showing
/// the prefix and failing at the point of use — trades a line in the log for a
/// prefix that never works, which is the worse deal.
pub fn resolve(config: &SpotlightVpnConfig) -> Option<Provider> {
    if !config.enabled {
        return None;
    }

    let Some(name) = config
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return detect();
    };

    let Some(provider) = Provider::parse(name) else {
        tracing::warn!(
            provider = name,
            supported = ?Provider::ALL.map(Provider::id),
            "ignoring the vpn prefix: unknown provider"
        );
        return None;
    };
    if !provider.installed() {
        tracing::warn!(
            provider = provider.id(),
            program = provider.program(),
            "ignoring the vpn prefix: the configured provider's client is not installed"
        );
        return None;
    }

    Some(provider)
}

/// What the client says about the connection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Status {
    pub connected: bool,
    /// A connection in progress. Neither connected nor idle, and the row has to
    /// say so rather than offering to connect again.
    pub connecting: bool,
    pub logged_in: bool,
    /// Where the client says the connection lands, when it says at all.
    pub location: Option<String>,
    /// The client's reply verbatim, for the preview panel.
    pub details: String,
}

impl Status {
    /// The one-line summary shown under the action row.
    pub fn summary(&self) -> String {
        if !self.logged_in {
            return "Not logged in".to_string();
        }
        if self.connecting {
            return "Connecting…".to_string();
        }
        match (self.connected, self.location.as_deref()) {
            (true, Some(location)) => format!("Connected · {location}"),
            (true, None) => "Connected".to_string(),
            (false, _) => "Disconnected".to_string(),
        }
    }
}

/// One location the client offers to connect to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Location {
    /// What the connect command is handed. Never shown.
    pub target: String,
    /// The row's heading — the city, or the provider's own name for the
    /// automatic choice.
    pub name: String,
    /// The group the client filed it under, e.g. `US East`.
    pub region: String,
    /// The server's own name within the city, e.g. `Big Apple`.
    pub nickname: String,
    /// Link speed as the client reported it, e.g. `10 Gbps`. Empty when absent.
    pub speed: String,
    /// Marked unavailable by the client. Still listed, because a location that
    /// is temporarily down is one the user may be looking for precisely to find
    /// out that it is down.
    pub disabled: bool,
    /// The "let the client choose" entry, which is not a place.
    pub best: bool,
}

impl Location {
    /// The row's subtitle: everything about the location that is not its name.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.region.is_empty() {
            parts.push(self.region.clone());
        }
        if !self.nickname.is_empty() && self.nickname != self.name {
            parts.push(self.nickname.clone());
        }
        if !self.speed.is_empty() {
            parts.push(self.speed.clone());
        }
        if self.disabled {
            parts.push("Unavailable".to_string());
        }
        parts.join(" · ")
    }
}

/// One reply: what the client says, and where it will let you go.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VpnState {
    pub status: Status,
    pub locations: Vec<Location>,
}

#[derive(Debug)]
enum VpnEvent {
    Ready { generation: u64, state: VpnState },
    Failed { generation: u64, error: String },
}

impl VpnEvent {
    fn generation(&self) -> u64 {
        match self {
            Self::Ready { generation, .. } | Self::Failed { generation, .. } => *generation,
        }
    }
}

/// Background source of the VPN's state.
pub struct VpnSource {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<VpnEvent>,
    receiver: mpsc::Receiver<VpnEvent>,
}

impl VpnSource {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
        }
    }

    /// Asks the client for its state, superseding any in-flight request.
    pub fn refresh(&self, provider: Provider) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);

        thread::spawn(move || {
            let event = match query(provider) {
                Ok(state) => VpnEvent::Ready { generation, state },
                Err(error) => {
                    tracing::warn!(%error, provider = provider.id(), "failed to query the vpn");
                    VpnEvent::Failed { generation, error }
                }
            };
            // A reply the user has already moved past would only make the list
            // flicker back to a state that has since changed.
            if counter.load(Ordering::Relaxed) == generation {
                let _ = sender.send(event);
            }
        });
    }

    /// The newest reply that is still current, if one arrived.
    pub fn drain(&self) -> Option<Result<VpnState, String>> {
        let current = self.generation.load(Ordering::Relaxed);
        let mut latest = None;

        while let Ok(event) = self.receiver.try_recv() {
            if event.generation() == current {
                latest = Some(match event {
                    VpnEvent::Ready { state, .. } => Ok(state),
                    VpnEvent::Failed { error, .. } => Err(error),
                });
            }
        }

        latest
    }
}

impl Default for VpnSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs both client queries. Blocking — worker threads only.
///
/// Only the status query is allowed to fail the whole reply. Listing locations
/// needs an account the status query does not: logged out, or on a plan the
/// client will not enumerate for, the list comes back empty or refused while the
/// status stays perfectly informative — and telling the user they are logged out
/// is more useful than reporting that a list could not be fetched.
fn query(provider: Provider) -> Result<VpnState, String> {
    let status = capture(provider.program(), provider.status_args())?;
    let status = provider.parse_status(&status);

    let mut locations = match capture(provider.program(), provider.locations_args()) {
        Ok(output) => provider.parse_locations(&output),
        Err(error) => {
            tracing::debug!(%error, provider = provider.id(), "cannot list vpn locations");
            Vec::new()
        }
    };
    locations.truncate(MAX_LOCATIONS);

    Ok(VpnState { status, locations })
}

/// Runs a client command under a deadline and returns its stdout.
///
/// The deadline is the point of the whole function: a VPN client talks to a
/// background daemon, and a daemon that has stopped answering leaves the client
/// waiting rather than exiting. `Command::output` would wait with it.
fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {program}: {error}"))?;

    // Drained on their own threads: waiting on a child whose pipe has filled
    // would deadlock, and the location list is larger than a pipe buffer.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || read_capped(stdout));
    let errors = thread::spawn(move || read_capped(stderr));

    let started = Instant::now();
    let failed = loop {
        match child.try_wait() {
            Ok(Some(status)) => break !status.success(),
            Ok(None) => {}
            Err(error) => return Err(format!("cannot wait for {program}: {error}")),
        }
        if started.elapsed() > MAX_RUNTIME {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{program} did not answer within {}s",
                MAX_RUNTIME.as_secs()
            ));
        }
        thread::sleep(POLL_INTERVAL);
    };

    // Killing or reaping the child closes the pipes, so both readers finish.
    let output = reader.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();

    if failed {
        // A client that fails usually says why on one of the two streams, and
        // that sentence is the whole value of the row the user ends up seeing.
        let detail = match stderr.trim().is_empty() {
            true => output.trim(),
            false => stderr.trim(),
        };
        return match detail.is_empty() {
            true => Err(format!("{program} {} failed", args.join(" "))),
            false => Err(detail.to_string()),
        };
    }

    Ok(output)
}

fn read_capped(stream: Option<impl Read>) -> String {
    let mut text = String::new();
    if let Some(stream) = stream {
        let _ = stream.take(MAX_OUTPUT).read_to_string(&mut text);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_are_named_and_parsed_case_insensitively() {
        assert_eq!(Provider::parse("windscribe"), Some(Provider::Windscribe));
        assert_eq!(Provider::parse("  Windscribe "), Some(Provider::Windscribe));
        assert_eq!(Provider::parse("mullvad"), None);
        assert_eq!(Provider::parse(""), None);

        assert_eq!(Provider::Windscribe.id(), "windscribe");
        assert_eq!(Provider::Windscribe.program(), "windscribe-cli");
    }

    #[test]
    fn a_disabled_prefix_resolves_to_nothing_without_touching_the_path() {
        let config = SpotlightVpnConfig {
            enabled: false,
            provider: Some("windscribe".to_string()),
            ..Default::default()
        };

        assert_eq!(resolve(&config), None);
    }

    /// A name no provider answers to would otherwise silently fall back to
    /// detection, which is the one outcome the user did not ask for.
    #[test]
    fn an_unknown_provider_name_is_refused_rather_than_detected() {
        let config = SpotlightVpnConfig {
            provider: Some("nordvpn".to_string()),
            ..Default::default()
        };

        assert_eq!(resolve(&config), None);
    }

    /// Detection is a `PATH` scan, so what it finds depends on the machine. The
    /// invariant that holds everywhere: whatever it returns is installed.
    #[test]
    fn detection_only_ever_returns_an_installed_provider() {
        if let Some(provider) = detect() {
            assert!(provider.installed(), "{} is not installed", provider.id());
        }
    }

    #[test]
    fn the_status_summary_reads_as_a_sentence() {
        let connected = Status {
            connected: true,
            logged_in: true,
            location: Some("Big Apple".to_string()),
            ..Default::default()
        };
        assert_eq!(connected.summary(), "Connected · Big Apple");

        let idle = Status {
            logged_in: true,
            ..Default::default()
        };
        assert_eq!(idle.summary(), "Disconnected");

        let connecting = Status {
            logged_in: true,
            connecting: true,
            ..Default::default()
        };
        assert_eq!(connecting.summary(), "Connecting…");

        // Logged out outranks everything: it is the reason nothing else works.
        assert_eq!(Status::default().summary(), "Not logged in");
    }

    #[test]
    fn a_location_summarises_everything_but_its_heading() {
        let location = Location {
            name: "New York".to_string(),
            region: "US East".to_string(),
            nickname: "Big Apple".to_string(),
            speed: "10 Gbps".to_string(),
            ..Default::default()
        };

        assert_eq!(location.summary(), "US East · Big Apple · 10 Gbps");
    }

    /// The heading is already the nickname for a location that has no city, so
    /// repeating it in the subtitle would print it twice.
    #[test]
    fn a_summary_does_not_repeat_the_heading() {
        let best = Location {
            name: "Hyggenhagen".to_string(),
            nickname: "Hyggenhagen".to_string(),
            speed: "10 Gbps".to_string(),
            best: true,
            ..Default::default()
        };

        assert_eq!(best.summary(), "10 Gbps");
    }

    #[test]
    fn a_client_that_does_not_exist_is_reported_rather_than_panicking() {
        let error = capture("a-vpn-client-that-does-not-exist", &["status"])
            .expect_err("a missing program must fail");

        assert!(error.contains("cannot run"), "{error}");
    }

    /// The deadline is the reason this module runs commands itself rather than
    /// calling `Command::output`, so it is worth pinning.
    #[test]
    fn a_client_that_never_answers_is_killed() {
        let started = Instant::now();

        let error = capture("sleep", &["120"]).expect_err("a hung client must fail");

        assert!(error.contains("did not answer"), "{error}");
        assert!(
            started.elapsed() < MAX_RUNTIME + Duration::from_secs(2),
            "the deadline was not enforced"
        );
    }

    #[test]
    fn a_failing_client_reports_what_it_printed() {
        let error = capture("sh", &["-c", "echo 'not logged in' >&2; exit 1"])
            .expect_err("a non-zero exit must fail");

        assert_eq!(error, "not logged in");
    }

    #[test]
    fn a_superseded_query_never_reports() {
        let source = VpnSource::new();
        source.refresh(Provider::Windscribe);
        // Bumping the counter is what a newer request does; the worker sees it
        // and drops its reply.
        source.generation.fetch_add(1, Ordering::Relaxed);

        thread::sleep(Duration::from_millis(300));
        assert!(source.drain().is_none());
    }
}
