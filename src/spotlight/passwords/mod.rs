//! Password managers: searching a vault, and copying a secret out of one —
//! through whichever manager's command-line client is installed.
//!
//! Shaped like [`crate::spotlight::vpn`]: the provider set is a plain enum, so
//! adding a second manager means one variant, one arm in each dispatch below,
//! and one new file. Nothing in the window, the results or the config plumbing
//! changes.
//!
//! Two things make this different from every other prefix, and both are why the
//! code below is not simply the VPN's with the nouns swapped:
//!
//! *A listing is not a secret, and a secret is never listed.* The search returns
//! names, logins and URLs; the password itself is fetched by a second command at
//! the moment the user activates a row, and goes straight to the clipboard.
//! [`SpotlightResult`](crate::spotlight::results::SpotlightResult) rows are
//! cloned on every keystroke and derive `Debug`, so a row that carried a
//! password would scatter copies of it through the process and into any trace of
//! one. [`SecretRequest`] carries identifiers instead.
//!
//! *A secret never reaches a command line.* `sh -c` is how the rest of the
//! launcher runs things, and an argument there is readable in `/proc` by anyone
//! for as long as the process lives. Every client here is invoked as an argv
//! with no shell, and its credentials are handed over through the environment.

mod base64;
mod passwork;
mod passwork_v4;

use std::{
    fmt,
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{config::SpotlightPasswordsConfig, launcher::spawn::on_path};

/// How long a client may take before it is killed and reported as unreachable.
///
/// Longer than the VPN's: that talks to a daemon on a local socket, this one
/// talks to a vault server over the network, and a first call may be paying for
/// a TLS handshake and a token refresh on top of the query.
const MAX_RUNTIME: Duration = Duration::from_secs(10);
/// How often the child is checked for exit and for its time budget.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Cap on a client's reply. A large vault's listing is a few hundred kilobytes.
const MAX_OUTPUT: u64 = 8 * 1024 * 1024;
/// Cap on the entries kept from one listing.
const MAX_ENTRIES: usize = 2000;
/// How long resolved credentials are reused.
///
/// The point is the token command, not the token: it may be `pass`, and a fresh
/// `gpg` prompt for every search would make the prefix unusable. Short enough
/// that a rotated token is picked up without a restart.
const CREDENTIAL_TTL: Duration = Duration::from_secs(300);

/// A password manager this module knows how to drive.
///
/// Two backends for the same product, because Passwork changed its API wholesale
/// between major versions and the two share no endpoint. Which one a server
/// speaks is not something a client can shrug off: every v1 path 404s on a v6
/// server, and the reply is an HTML error page the official client dies trying
/// to parse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    /// Passwork 7 and later, driven through the official `passwork-cli`.
    Passwork,
    /// Passwork 6 and earlier, spoken directly over its v4 HTTP API.
    ///
    /// No CLI: no released version of `passwork-python` speaks v4, so there is
    /// nothing to shell out to.
    PassworkV4,
}

impl Provider {
    /// Every provider, in the order [`detect`] tries them.
    pub const ALL: [Provider; 2] = [Provider::Passwork, Provider::PassworkV4];

    /// The name the config uses.
    pub fn id(self) -> &'static str {
        match self {
            Self::Passwork => "passwork",
            Self::PassworkV4 => "passwork-v4",
        }
    }

    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::Passwork | Self::PassworkV4 => "Passwork",
        }
    }

    /// The executable that has to be installed, for a provider driven by one.
    ///
    /// `None` for a provider that talks to the vault itself, which has nothing
    /// on `PATH` to look for and is therefore always available.
    pub fn requirement(self) -> Option<&'static str> {
        match self {
            Self::Passwork => Some(passwork::PROGRAM),
            Self::PassworkV4 => None,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Passwork | Self::PassworkV4 => "dialog-password-symbolic",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|provider| provider.id().eq_ignore_ascii_case(name))
    }

    /// Whether this provider can run on this machine at all.
    pub fn available(self) -> bool {
        self.requirement().is_none_or(on_path)
    }

    /// The environment variables a CLI-driven provider reads its credentials
    /// from, in the order [`Resolved`] fills them.
    fn credential_vars(self) -> CredentialVars {
        match self {
            Self::Passwork | Self::PassworkV4 => passwork::CREDENTIAL_VARS,
        }
    }

    /// Lists the entries matching `query`. Blocking — worker threads only.
    fn search(
        self,
        resolved: &Resolved,
        session: &SessionCache,
        query: &str,
    ) -> Result<Vec<Entry>, String> {
        match self {
            Self::Passwork => passwork::search(resolved, query),
            Self::PassworkV4 => passwork_v4::search(resolved, session, query),
        }
    }

    /// Fetches one secret. Blocking.
    fn fetch(
        self,
        resolved: &Resolved,
        session: &SessionCache,
        request: &SecretRequest,
    ) -> Result<String, String> {
        match self {
            Self::Passwork => passwork::fetch(resolved, request),
            Self::PassworkV4 => passwork_v4::fetch(resolved, session, request),
        }
    }
}

/// The first available provider, or `None` when nothing here can run.
///
/// Only ever finds a CLI-driven provider. One that talks to a vault directly is
/// "available" everywhere, so detection would always return the first of them
/// and no configuration could be wrong enough to say otherwise — which is not
/// detection, it is a default wearing its coat. Those are named explicitly.
pub fn detect() -> Option<Provider> {
    Provider::ALL
        .into_iter()
        .find(|provider| provider.requirement().is_some_and(on_path))
}

/// The provider the password prefix should use, honouring the config.
///
/// A named provider is still required to be available, for the same reason the
/// VPN insists on it: a prefix that exists only to fail at the point of use is
/// worse than one that never appeared.
pub fn resolve(config: &SpotlightPasswordsConfig) -> Option<Provider> {
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
            "ignoring the passwords prefix: unknown provider"
        );
        return None;
    };
    if !provider.available() {
        tracing::warn!(
            provider = provider.id(),
            program = provider.requirement(),
            "ignoring the passwords prefix: the configured provider's client is not installed"
        );
        return None;
    }

    Some(provider)
}

// -- credentials -------------------------------------------------------------

/// The environment variables one CLI-driven provider reads, in config order:
/// host, token, refresh token, master key.
type CredentialVars = [&'static str; 4];

/// A credential that cannot be printed by accident.
///
/// Same idea as [`ApiKey`](crate::spotlight::ai::ApiKey), and here for the same
/// reason: no `Display`, and `Debug` redacts, so a `tracing::warn!(?resolved)`
/// anywhere up the stack is structurally incapable of leaking one.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

/// Where a client's credentials come from — never the credentials themselves.
///
/// Held on the window and cloned into worker threads, so it deliberately cannot
/// hold a secret: it names the host outright, because a URL is not one, and
/// names *commands* for the three things that are. A struct that cannot carry a
/// secret cannot leak one into a log line or a core dump.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CredentialSource {
    host: Option<String>,
    token_command: Option<String>,
    refresh_token_command: Option<String>,
    master_key_command: Option<String>,
}

/// The answers those commands gave. Short-lived, and never stored on a row.
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    host: Option<String>,
    /// The API token. For [`Provider::PassworkV4`] this is the API *key*, which
    /// is exchanged for a session token rather than sent as one.
    token: Option<Secret>,
    refresh_token: Option<Secret>,
    master_key: Option<Secret>,
}

impl CredentialSource {
    pub fn from_config(config: &SpotlightPasswordsConfig) -> Self {
        Self {
            host: trimmed(config.host.as_deref()),
            token_command: trimmed(config.token_command.as_deref()),
            refresh_token_command: trimmed(config.refresh_token_command.as_deref()),
            master_key_command: trimmed(config.master_key_command.as_deref()),
        }
    }

    /// Runs the credential commands.
    ///
    /// Only what the config actually specifies is filled in. Anything left unset
    /// stays `None` rather than becoming empty, so a CLI-driven provider falls
    /// through to the environment IoExplorer itself was started with — which is
    /// the whole of the setup for a user who exports `PASSWORK_TOKEN` in their
    /// systemd unit and writes nothing here.
    fn resolve(&self, provider: Provider) -> Result<Resolved, String> {
        let [_, token_var, refresh_var, master_var] = provider.credential_vars();
        let run = |command: &Option<String>, name: &str| -> Result<Option<Secret>, String> {
            let Some(command) = command else {
                return Ok(None);
            };
            // The variable name, never the value: this is the one place in the
            // module where a secret is in hand, and naming it is the whole of
            // what a failure is allowed to say.
            run_credential_command(command)
                .map(|value| Some(Secret(value)))
                .map_err(|error| format!("{name}: {error}"))
        };

        Ok(Resolved {
            host: self.host.clone(),
            token: run(&self.token_command, token_var)?,
            refresh_token: run(&self.refresh_token_command, refresh_var)?,
            master_key: run(&self.master_key_command, master_var)?,
        })
    }
}

impl Resolved {
    /// The credentials as environment variables, for a provider driven by a CLI.
    fn environment(&self, provider: Provider) -> Vec<(&'static str, String)> {
        let [host_var, token_var, refresh_var, master_var] = provider.credential_vars();
        let mut environment = Vec::new();

        if let Some(host) = &self.host {
            environment.push((host_var, host.clone()));
        }
        for (variable, secret) in [
            (token_var, &self.token),
            (refresh_var, &self.refresh_token),
            (master_var, &self.master_key),
        ] {
            if let Some(secret) = secret {
                environment.push((variable, secret.expose().to_string()));
            }
        }

        environment
    }

    /// The vault's base URL, without the trailing slash a copy-pasted address
    /// carries — `https://host//api/v4/…` is a 404 on some deployments.
    fn host(&self) -> Result<&str, String> {
        self.host
            .as_deref()
            .map(|host| host.trim_end_matches('/'))
            .filter(|host| !host.is_empty())
            .ok_or_else(|| "no host configured".to_string())
    }

    fn token(&self) -> Result<&Secret, String> {
        self.token
            .as_ref()
            .ok_or_else(|| "no token_command configured".to_string())
    }
}

/// Runs a credential command and returns the single line it printed.
///
/// A shell line, unlike everything else here, because that is what makes
/// `pass show x | head -1` work — and unlike a client invocation it is the
/// user's own text, with no untrusted value interpolated into it.
fn run_credential_command(command: &str) -> Result<String, String> {
    let output = capture(
        "sh",
        &[
            "-c".to_string(),
            command.to_string(),
            "ioexplorer".to_string(),
        ],
        &[],
    )?;

    // Trailing newlines are what a command prints, not part of the secret, and
    // a token with one appended fails authentication in a way that reads like a
    // wrong token.
    match output.trim() {
        "" => Err("the command printed nothing".to_string()),
        value => Ok(value.to_string()),
    }
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// -- the vault ---------------------------------------------------------------

/// One entry as the vault listed it. Metadata only — no secret ever lands here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Entry {
    /// The vault's own identifier, and what a secret is later fetched by.
    pub id: String,
    /// Whether `id` names a shortcut rather than a password. The client asks for
    /// the two with different flags.
    pub shortcut: bool,
    /// The row's heading.
    pub name: String,
    /// The account the entry is for. Empty when the vault did not say.
    pub login: String,
    pub url: String,
    /// Where the entry lives, as the vault named it. Empty when unnamed.
    pub folder: String,
    pub tags: Vec<String>,
    /// The custom field holding a one-time-password secret, when the listing
    /// named one. `None` means no code row is offered for this entry.
    pub totp_field: Option<String>,
}

impl Entry {
    /// The row's subtitle: everything about the entry that is not its heading.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.login.is_empty() {
            parts.push(self.login.clone());
        }
        if !self.folder.is_empty() {
            parts.push(self.folder.clone());
        }
        if !self.url.is_empty() {
            parts.push(self.url.clone());
        }
        parts.join(" · ")
    }
}

/// One reply: the entries the vault matched.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VaultState {
    /// The query the vault was asked, so the window can tell a listing that
    /// answers what the user is typing from one that answers what they typed
    /// two keystrokes ago.
    pub query: String,
    pub entries: Vec<Entry>,
}

/// Which of an entry's secrets to fetch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretField {
    Password,
    Login,
    /// The current one-time code, derived by the client from the named field.
    Totp(String),
}

impl SecretField {
    /// What the row says it will put on the clipboard.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Login => "username",
            Self::Totp(_) => "one-time code",
        }
    }
}

/// Everything needed to fetch one secret, and nothing more.
///
/// This is what an [`Activation`](crate::spotlight::results::Activation) carries
/// in place of the secret itself. It is safe to clone, to keep in a row, and to
/// print.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretRequest {
    pub provider: Provider,
    pub id: String,
    pub shortcut: bool,
    pub field: SecretField,
}

// -- the background source ---------------------------------------------------

#[derive(Debug)]
enum PasswordEvent {
    Ready { generation: u64, state: VaultState },
    Failed { generation: u64, error: String },
}

impl PasswordEvent {
    fn generation(&self) -> u64 {
        match self {
            Self::Ready { generation, .. } | Self::Failed { generation, .. } => *generation,
        }
    }
}

/// Credentials already resolved, and when.
struct CachedCredentials {
    source: CredentialSource,
    provider: Provider,
    resolved: Resolved,
    at: Instant,
}

/// A vault session held between calls, for a provider that logs in.
pub(super) type SessionCache = Mutex<Option<passwork_v4::Session>>;

/// Background source of vault listings, and the synchronous path that fetches a
/// single secret.
///
/// Both live here so they share one credential cache and one session: a search
/// and the copy that follows it should not run the user's `gpg` twice, nor log
/// in to the vault twice.
pub struct PasswordSource {
    generation: Arc<AtomicU64>,
    sender: mpsc::Sender<PasswordEvent>,
    receiver: mpsc::Receiver<PasswordEvent>,
    credentials: Arc<Mutex<Option<CachedCredentials>>>,
    session: Arc<SessionCache>,
}

impl PasswordSource {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sender,
            receiver,
            credentials: Arc::new(Mutex::new(None)),
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Asks the vault for the entries matching `query`, superseding any
    /// in-flight request.
    pub fn refresh(&self, provider: Provider, source: &CredentialSource, query: &str) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let sender = self.sender.clone();
        let counter = Arc::clone(&self.generation);
        let credentials = Arc::clone(&self.credentials);
        let session = Arc::clone(&self.session);
        let source = source.clone();
        let query = query.to_string();

        thread::spawn(move || {
            let event = match search(provider, &source, &credentials, &session, &query) {
                Ok(state) => PasswordEvent::Ready { generation, state },
                Err(error) => {
                    tracing::warn!(
                        %error,
                        provider = provider.id(),
                        "failed to search the password vault"
                    );
                    PasswordEvent::Failed { generation, error }
                }
            };
            // A reply the user has already typed past would only make the list
            // flicker back to an older search.
            if counter.load(Ordering::Relaxed) == generation {
                let _ = sender.send(event);
            }
        });
    }

    /// The newest reply that is still current, if one arrived.
    pub fn drain(&self) -> Option<Result<VaultState, String>> {
        let current = self.generation.load(Ordering::Relaxed);
        let mut latest = None;

        while let Ok(event) = self.receiver.try_recv() {
            if event.generation() == current {
                latest = Some(match event {
                    PasswordEvent::Ready { state, .. } => Ok(state),
                    PasswordEvent::Failed { error, .. } => Err(error),
                });
            }
        }

        latest
    }

    /// Fetches one secret. Blocking, and called from the main loop.
    ///
    /// The one place in the launcher that deliberately blocks. The alternative is
    /// to close the window and copy whenever the answer turns up, which silently
    /// puts a password on the clipboard at a moment the user is no longer
    /// thinking about it — and leaves them pasting the *previous* clipboard entry
    /// in the meantime. Pressing Enter is the commitment; [`MAX_RUNTIME`] bounds
    /// what it costs.
    pub fn fetch_secret(
        &self,
        request: &SecretRequest,
        source: &CredentialSource,
    ) -> Result<String, String> {
        let resolved = credentials(request.provider, source, &self.credentials)?;
        let secret = request.provider.fetch(&resolved, &self.session, request)?;

        // A client prints the value and a newline; the newline is the print, not
        // the secret, and pasting it into a login form submits it.
        match secret.trim_end_matches(['\n', '\r']) {
            "" => Err(format!(
                "{} returned an empty {}",
                request.provider.label(),
                request.field.label()
            )),
            secret => Ok(secret.to_string()),
        }
    }
}

impl Default for PasswordSource {
    fn default() -> Self {
        Self::new()
    }
}

fn credentials(
    provider: Provider,
    source: &CredentialSource,
    cache: &Mutex<Option<CachedCredentials>>,
) -> Result<Resolved, String> {
    // A poisoned lock means a worker panicked mid-resolve. Recovering the guard
    // and re-resolving is right: the cache holds no invariant that a panic could
    // have broken, only a value that may never have been written.
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());

    if let Some(cached) = cache.as_ref()
        && cached.provider == provider
        && &cached.source == source
        && cached.at.elapsed() < CREDENTIAL_TTL
    {
        return Ok(cached.resolved.clone());
    }

    let resolved = source.resolve(provider)?;
    *cache = Some(CachedCredentials {
        source: source.clone(),
        provider,
        resolved: resolved.clone(),
        at: Instant::now(),
    });
    Ok(resolved)
}

/// Runs the vault search. Blocking — worker threads only.
fn search(
    provider: Provider,
    source: &CredentialSource,
    cache: &Mutex<Option<CachedCredentials>>,
    session: &SessionCache,
    query: &str,
) -> Result<VaultState, String> {
    let resolved = credentials(provider, source, cache)?;
    let mut entries = provider.search(&resolved, session, query)?;
    entries.truncate(MAX_ENTRIES);

    Ok(VaultState {
        query: query.to_string(),
        entries,
    })
}

/// Runs a client command under a deadline and returns its stdout.
///
/// `environment` is added to the child's, not substituted for it: the client
/// reads the rest of its configuration from the environment IoExplorer inherited,
/// and a user who exports everything there configures nothing else.
///
/// Nothing here reaches a shell. `args` is an argv, so a vault entry named
/// `"; rm -rf ~"` is one argument rather than two commands, and a credential is
/// an environment variable rather than a word in a command line that `ps` would
/// print for every user on the machine.
pub(super) fn capture(
    program: &str,
    args: &[String],
    environment: &[(&'static str, String)],
) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .envs(environment.iter().map(|(key, value)| (*key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {program}: {error}"))?;

    // Drained on their own threads: waiting on a child whose pipe has filled
    // would deadlock, and a vault listing is larger than a pipe buffer.
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
        // A client that fails says why — a missing token, an expired session, a
        // host it cannot reach — and that sentence is the whole value of the row
        // the user ends up seeing.
        let detail = match stderr.trim().is_empty() {
            true => output.trim(),
            false => stderr.trim(),
        };
        return match detail.is_empty() {
            true => Err(format!("{program} failed")),
            false => Err(first_line(detail)),
        };
    }

    Ok(output)
}

/// The first line of a client's complaint.
///
/// A Python client that fails on an unexpected error prints a traceback, and a
/// row is one line: the last thing the user needs is forty frames of it
/// ellipsized into a subtitle.
fn first_line(detail: &str) -> String {
    detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(detail)
        .to_string()
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

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn providers_are_named_and_parsed_case_insensitively() {
        assert_eq!(Provider::parse("passwork"), Some(Provider::Passwork));
        assert_eq!(Provider::parse("  Passwork "), Some(Provider::Passwork));
        assert_eq!(Provider::parse("bitwarden"), None);
        assert_eq!(Provider::parse(""), None);

        assert_eq!(Provider::parse("passwork-v4"), Some(Provider::PassworkV4));
        assert_eq!(Provider::parse("PASSWORK-V4"), Some(Provider::PassworkV4));

        assert_eq!(Provider::Passwork.id(), "passwork");
        assert_eq!(Provider::Passwork.requirement(), Some("passwork-cli"));
        // Nothing on `PATH` to look for: it talks to the vault itself.
        assert_eq!(Provider::PassworkV4.requirement(), None);
        assert!(Provider::PassworkV4.available());
    }

    /// The whole point of the section defaulting off. A user who has never heard
    /// of this feature must not get a prefix that talks to a vault, even with a
    /// client installed.
    #[test]
    fn the_default_config_resolves_to_nothing() {
        assert_eq!(resolve(&SpotlightPasswordsConfig::default()), None);
    }

    /// A name no provider answers to would otherwise silently fall back to
    /// detection, which is the one outcome the user did not ask for.
    #[test]
    fn an_unknown_provider_name_is_refused_rather_than_detected() {
        let config = SpotlightPasswordsConfig {
            enabled: true,
            provider: Some("1password".to_string()),
            ..Default::default()
        };

        assert_eq!(resolve(&config), None);
    }

    /// Detection is a `PATH` scan, so what it finds depends on the machine. Two
    /// invariants hold everywhere: whatever it returns is installed, and it is
    /// never a provider that has nothing to install — one of those would be
    /// "detected" on every machine on earth, which is a default, not detection.
    #[test]
    fn detection_only_ever_returns_an_installed_provider() {
        if let Some(provider) = detect() {
            assert!(provider.available(), "{} is not installed", provider.id());
            assert!(
                provider.requirement().is_some(),
                "{} has no client to detect",
                provider.id()
            );
        }
    }

    /// The HTTP provider is available everywhere, so naming it is the only way
    /// to get it — and naming it has to work even with no client installed.
    #[test]
    fn the_http_provider_resolves_when_it_is_named() {
        let config = SpotlightPasswordsConfig {
            enabled: true,
            provider: Some("passwork-v4".to_string()),
            ..Default::default()
        };

        assert_eq!(resolve(&config), Some(Provider::PassworkV4));
    }

    /// The credential source is cloned into worker threads, held on the window
    /// and compared on every query. It must describe *where* secrets come from
    /// without ever becoming one — including after a resolve, which is the point
    /// at which a cache-in-the-wrong-place bug would show up.
    #[test]
    fn a_credential_source_holds_commands_rather_than_secrets() {
        // Prints `hunter2` without the word appearing in the command, so the
        // assertion below is about the resolved value rather than the text.
        let source = CredentialSource::from_config(&SpotlightPasswordsConfig {
            host: Some("  https://vault.example.com  ".to_string()),
            token_command: Some("printf 'hunt%s2' er".to_string()),
            master_key_command: Some("   ".to_string()),
            ..Default::default()
        });

        assert_eq!(source.host.as_deref(), Some("https://vault.example.com"));
        // Whitespace is not a command, and running it would only produce an
        // error the user cannot act on.
        assert_eq!(source.master_key_command, None);

        let resolved = source
            .resolve(Provider::Passwork)
            .expect("the command runs");
        assert!(
            resolved
                .environment(Provider::Passwork)
                .contains(&("PASSWORK_TOKEN", "hunter2".to_string()))
        );
        // And the resolved value redacts too, so it cannot be traced by accident.
        assert!(!format!("{resolved:?}").contains("hunter2"));

        let printed = format!("{source:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
    }

    /// What a user who exports `PASSWORK_TOKEN` in their systemd unit and writes
    /// nothing in the config gets: no overrides at all, so the client reads the
    /// environment it inherited.
    #[test]
    fn an_empty_credential_source_overrides_nothing() {
        let resolved = CredentialSource::default()
            .resolve(Provider::Passwork)
            .expect("no commands to run");

        assert!(resolved.environment(Provider::Passwork).is_empty());
    }

    #[test]
    fn credential_commands_fill_the_variables_the_client_reads() {
        let source = CredentialSource::from_config(&SpotlightPasswordsConfig {
            host: Some("https://vault.example.com".to_string()),
            token_command: Some("printf 'a-token\n'".to_string()),
            master_key_command: Some("echo a-key".to_string()),
            ..Default::default()
        });

        let resolved = source
            .resolve(Provider::Passwork)
            .expect("the commands run");

        assert_eq!(
            resolved.environment(Provider::Passwork),
            vec![
                ("PASSWORK_HOST", "https://vault.example.com".to_string()),
                // Trimmed: a token with a trailing newline fails authentication
                // in a way that reads like a wrong token.
                ("PASSWORK_TOKEN", "a-token".to_string()),
                ("PASSWORK_MASTER_KEY", "a-key".to_string()),
            ]
        );
    }

    /// A command that prints nothing has failed, whatever its exit code says: an
    /// empty token would otherwise reach the client and come back as a login
    /// error pointing nowhere near the actual problem.
    #[test]
    fn a_credential_command_that_prints_nothing_is_an_error() {
        let source = CredentialSource::from_config(&SpotlightPasswordsConfig {
            token_command: Some("true".to_string()),
            ..Default::default()
        });

        let error = source
            .resolve(Provider::Passwork)
            .expect_err("an empty token must fail");

        assert!(error.contains("PASSWORK_TOKEN"), "{error}");
    }

    /// The error has to name the variable, because the user's config has three
    /// credential commands in it and the message is all they get.
    #[test]
    fn a_failing_credential_command_names_what_it_was_for() {
        let source = CredentialSource::from_config(&SpotlightPasswordsConfig {
            master_key_command: Some("echo 'no such key' >&2; exit 1".to_string()),
            ..Default::default()
        });

        let error = source
            .resolve(Provider::Passwork)
            .expect_err("a failing command must fail");

        assert!(error.starts_with("PASSWORK_MASTER_KEY:"), "{error}");
        assert!(error.contains("no such key"), "{error}");
    }

    #[test]
    fn resolved_credentials_are_reused_within_their_ttl() {
        let cache = Mutex::new(None);
        // Counts its own invocations, so a second resolve is visible.
        let directory = tempfile::tempdir().expect("a temp dir");
        let counter = directory.path().join("runs");
        let source = CredentialSource::from_config(&SpotlightPasswordsConfig {
            token_command: Some(format!("printf x >> {}; printf a-token", counter.display())),
            ..Default::default()
        });

        for _ in 0..3 {
            credentials(Provider::Passwork, &source, &cache).expect("the command runs");
        }

        let runs = std::fs::read_to_string(&counter).unwrap_or_default();
        assert_eq!(runs.len(), 1, "the token command ran {} times", runs.len());
    }

    /// A config reload that changes the token command has to take effect, or the
    /// user is stuck with the credentials of a config they have already edited.
    #[test]
    fn changing_the_credential_source_re_resolves() {
        let cache = Mutex::new(None);
        let first = CredentialSource::from_config(&SpotlightPasswordsConfig {
            token_command: Some("printf first".to_string()),
            ..Default::default()
        });
        let second = CredentialSource::from_config(&SpotlightPasswordsConfig {
            token_command: Some("printf second".to_string()),
            ..Default::default()
        });

        credentials(Provider::Passwork, &first, &cache).expect("the command runs");
        let resolved = credentials(Provider::Passwork, &second, &cache).expect("the command runs");

        assert_eq!(
            resolved.environment(Provider::Passwork),
            vec![("PASSWORK_TOKEN", "second".to_string())]
        );
    }

    /// The whole non-GUI path in one test: credentials resolved, a client run
    /// with them in its environment, and its reply turned into rows' worth of
    /// entries. The stub stands in for `passwork-cli` and prints what a vault
    /// returns — including the ciphertext a real reply carries, so the assertion
    /// that none of it survives is being made against the real shape.
    #[test]
    fn a_search_runs_the_client_and_returns_entries() {
        // Echoes the token it was given, so the environment hand-off is part of
        // what this asserts rather than something taken on faith.
        let reply = r#"{"items":[{"id":"6690f2a1","name":"GitHub","login":"%s",
             "passwordEncrypted":"U2FsdGVkX1+ciphertext",
             "customs":[{"name":"TOTP","type":"totp","value":"U2FsdGVkX1+more"}]}]}"#;

        let environment = CredentialSource::from_config(&SpotlightPasswordsConfig {
            token_command: Some("printf lucasion".to_string()),
            ..Default::default()
        })
        .resolve(Provider::Passwork)
        .expect("the token command runs");

        let output = capture(
            "sh",
            &args(&["-c", &format!(r#"printf '{reply}' "$PASSWORK_TOKEN""#)]),
            &environment.environment(Provider::Passwork),
        )
        .expect("the stub client runs");
        let entries = passwork::parse_entries(&output).expect("a parseable reply");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "GitHub");
        assert_eq!(entries[0].login, "lucasion");
        assert_eq!(entries[0].totp_field.as_deref(), Some("TOTP"));

        let printed = format!("{entries:?}");
        assert!(!printed.contains("U2FsdGVkX1"), "{printed}");
    }

    #[test]
    fn an_entry_summarises_everything_but_its_heading() {
        let entry = Entry {
            name: "GitHub".to_string(),
            login: "lucasion".to_string(),
            folder: "Work / Dev".to_string(),
            url: "https://github.com".to_string(),
            ..Default::default()
        };

        assert_eq!(
            entry.summary(),
            "lucasion · Work / Dev · https://github.com"
        );
        assert_eq!(Entry::default().summary(), "");
    }

    #[test]
    fn a_client_that_does_not_exist_is_reported_rather_than_panicking() {
        let error = capture(
            "a-password-client-that-does-not-exist",
            &args(&["get"]),
            &[],
        )
        .expect_err("a missing program must fail");

        assert!(error.contains("cannot run"), "{error}");
    }

    /// The deadline is the reason this module runs commands itself rather than
    /// calling `Command::output`, and it also bounds how long pressing Enter can
    /// freeze the window.
    #[test]
    fn a_client_that_never_answers_is_killed() {
        let started = Instant::now();

        let error = capture("sleep", &args(&["120"]), &[]).expect_err("a hung client must fail");

        assert!(error.contains("did not answer"), "{error}");
        assert!(
            started.elapsed() < MAX_RUNTIME + Duration::from_secs(2),
            "the deadline was not enforced"
        );
    }

    /// End-to-end against a real vault: log in, search, fetch a password.
    ///
    /// Ignored by default — it needs a server, a key and the network, none of
    /// which belong in `cargo test`. It exists because everything else in this
    /// file tests parsing against replies that were written down, and the one
    /// thing that cannot be checked that way is whether the endpoints, the auth
    /// header and the session handshake are right for a live deployment.
    ///
    /// ```text
    /// IOEXPLORER_PASSWORK_HOST=https://vault.example.com \
    /// IOEXPLORER_PASSWORK_TOKEN_COMMAND='cat ~/.vault/passwork' \
    ///   cargo test passwork_v4_against_a_live_vault -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a live Passwork server and credentials"]
    fn passwork_v4_against_a_live_vault() {
        let Ok(host) = std::env::var("IOEXPLORER_PASSWORK_HOST") else {
            panic!("set IOEXPLORER_PASSWORK_HOST");
        };
        let Ok(token_command) = std::env::var("IOEXPLORER_PASSWORK_TOKEN_COMMAND") else {
            panic!("set IOEXPLORER_PASSWORK_TOKEN_COMMAND");
        };

        let source = CredentialSource::from_config(&SpotlightPasswordsConfig {
            enabled: true,
            provider: Some("passwork-v4".to_string()),
            host: Some(host),
            token_command: Some(token_command),
            ..Default::default()
        });
        let vault = PasswordSource::new();
        let resolved = credentials(Provider::PassworkV4, &source, &vault.credentials)
            .expect("the token command runs");

        // An empty query must not error: the vault's own search refuses one, so
        // this is the path that falls back to recently-used.
        let recent = Provider::PassworkV4
            .search(&resolved, &vault.session, "")
            .expect("the opening listing");
        println!("recent entries: {}", recent.len());
        assert!(!recent.is_empty(), "the vault listed nothing at all");

        let entry = &recent[0];
        assert!(!entry.id.is_empty());
        assert!(!entry.name.is_empty());

        // The password itself: printed only as a length, because a passing test
        // should not leave a secret in a terminal scrollback.
        let secret = vault
            .fetch_secret(
                &SecretRequest {
                    provider: Provider::PassworkV4,
                    id: entry.id.clone(),
                    shortcut: entry.shortcut,
                    field: SecretField::Password,
                },
                &source,
            )
            .expect("a password");
        println!("fetched {:?}: {} characters", entry.name, secret.len());
        assert!(!secret.is_empty());
        assert!(
            !secret.contains('\n'),
            "a password came back with a newline in it"
        );

        // The second call must reuse the session rather than logging in again.
        let searched = Provider::PassworkV4
            .search(&resolved, &vault.session, &entry.name)
            .expect("a search");
        println!("search for {:?}: {} entries", entry.name, searched.len());
    }

    /// A Python client that hits an unexpected error prints a traceback. A row is
    /// one line, and the first one is the one worth having.
    #[test]
    fn a_failing_client_reports_one_line_of_what_it_printed() {
        let error = capture(
            "sh",
            &args(&[
                "-c",
                "printf 'Error: no token\\nTraceback\\n  frame\\n' >&2; exit 1",
            ]),
            &[],
        )
        .expect_err("a non-zero exit must fail");

        assert_eq!(error, "Error: no token");
    }

    /// Credentials are added to the child's environment, not substituted for it:
    /// a client also needs `PATH`, `HOME` and the user's CA bundle.
    #[test]
    fn the_child_keeps_its_inherited_environment_and_gains_the_credentials() {
        // SAFETY: single-threaded at this point in the test, and the variable is
        // one this test invented.
        unsafe { std::env::set_var("IOEXPLORER_PASSWORDS_TEST", "inherited") };

        let output = capture(
            "sh",
            &args(&[
                "-c",
                "printf '%s %s' \"$IOEXPLORER_PASSWORDS_TEST\" \"$PASSWORK_TOKEN\"",
            ]),
            &[("PASSWORK_TOKEN", "injected".to_string())],
        )
        .expect("the command runs");

        assert_eq!(output, "inherited injected");
    }

    /// The reason every client invocation is an argv rather than a shell line.
    /// A vault the user does not control names its own entries.
    #[test]
    fn an_argument_is_never_read_as_a_command() {
        let output = capture("printf", &args(&["%s", "a'; rm -rf ~; echo '"]), &[])
            .expect("the command runs");

        assert_eq!(output, "a'; rm -rf ~; echo '");
    }

    #[test]
    fn a_superseded_search_never_reports() {
        let source = PasswordSource::new();
        source.refresh(Provider::Passwork, &CredentialSource::default(), "github");
        // Bumping the counter is what a newer request does; the worker sees it
        // and drops its reply.
        source.generation.fetch_add(1, Ordering::Relaxed);

        thread::sleep(Duration::from_millis(300));
        assert!(source.drain().is_none());
    }
}
