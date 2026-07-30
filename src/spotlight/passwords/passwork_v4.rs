//! Passwork's v4 HTTP API, as spoken by Passwork 6 and earlier.
//!
//! No CLI drives this one. `passwork-cli` — every released version of it — only
//! knows the v1 API that Passwork 7 introduced, and the two share no endpoint:
//! against a v6 server every v1 path returns an HTML 404 that the official
//! client dies trying to parse as JSON. So this talks to the vault directly.
//!
//! The shape is: exchange the API key for a session token, search, then fetch
//! one entry. Metadata comes back in the search; the password comes back only
//! from the per-entry call, which is what lets the rows hold identifiers and
//! nothing else.
//!
//! # On the `crypted` fields
//!
//! Passwork can encrypt password values in the browser, and the API field names
//! say so — `cryptedPassword`, `cryptedKey`. Whether it *does* is a per-instance
//! setting: an instance with client-side encryption off serves a stub crypto
//! port whose "encrypt" is base64, and returns base64 in those fields. This
//! module handles that case and refuses the other one rather than guessing:
//! [`base64::decode_text`] fails on anything that is not text, and a real
//! ciphertext is not. The alternative — putting undecryptable bytes on the
//! clipboard and calling them a password — is the one outcome worth ruling out.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::spotlight::passwords::{
    Entry, Resolved, Secret, SecretField, SecretRequest, SessionCache, base64,
};

/// Everything is under this prefix; the version is the whole point of the module.
const API: &str = "/api/v4";

/// The vault rejects a shorter search outright (`minQueryLength2Chars`), so
/// anything shorter asks for the recently-used list instead.
const MIN_QUERY: usize = 2;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap on a reply. A search of a large vault is a few hundred kilobytes.
const MAX_BODY: u64 = 8 * 1024 * 1024;

/// How much of a session's advertised lifetime is used before re-logging in.
///
/// A token that expires while the request carrying it is in flight fails the
/// action the user just took, so the last slice of the lifetime is left unused.
const SESSION_SAFETY: f64 = 0.8;
/// Used when the server does not say how long a token lasts.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(600);

/// A logged-in session. Held between calls so typing does not log in per query.
///
/// Safe to derive `Debug` on: the only sensitive field is a [`Secret`], which
/// redacts itself.
#[derive(Debug)]
pub struct Session {
    token: Secret,
    /// The host it was issued by, so a config edit does not reuse another
    /// server's token.
    host: String,
    obtained: Instant,
    ttl: Duration,
}

impl Session {
    fn usable(&self, host: &str) -> bool {
        self.host == host && self.obtained.elapsed() < self.ttl.mul_f64(SESSION_SAFETY)
    }
}

/// Lists the entries matching `query`.
pub fn search(
    resolved: &Resolved,
    session: &SessionCache,
    query: &str,
) -> Result<Vec<Entry>, String> {
    let host = resolved.host()?;
    let query = query.trim();

    let document = with_session(resolved, session, host, |token| {
        // The opening state — the prefix typed, nothing after it — would
        // otherwise be an error from the vault. Recently-used is the better
        // answer to "show me something" anyway.
        match query.chars().count() >= MIN_QUERY {
            true => post(
                host,
                "/passwords/search",
                token,
                &serde_json::json!({ "query": query }),
            ),
            false => get(host, "/passwords/recent", token),
        }
    })?;

    Ok(items(&document).iter().filter_map(parse_entry).collect())
}

/// Fetches one secret.
pub fn fetch(
    resolved: &Resolved,
    session: &SessionCache,
    request: &SecretRequest,
) -> Result<String, String> {
    let host = resolved.host()?;

    // A shortcut is a pointer to a password in someone else's vault, and the
    // API keeps the two on separate paths.
    let path = match request.shortcut {
        true => format!("/sharing/shortcut/{}/", request.id),
        false => format!("/passwords/{}", request.id),
    };
    let document = with_session(resolved, session, host, |token| get(host, &path, token))?;
    let entry = document.get("data").unwrap_or(&document);
    // A shortcut wraps the password it points at.
    let entry = entry.get("password").unwrap_or(entry);

    match &request.field {
        // Plaintext in the reply; no decoding, and nothing to fail.
        SecretField::Login => Ok(string(entry, &["login"])),
        SecretField::Password => {
            let crypted = string(entry, &["cryptedPassword"]);
            if crypted.is_empty() {
                return Err("the vault returned no password for this entry".to_string());
            }
            base64::decode_text(&crypted).map_err(|_| encrypted_vault_error())
        }
        // Reached only if a code row was built, which needs a `totp` custom
        // field — see `parse_entry`, which never marks one on this provider.
        SecretField::Totp(_) => {
            Err("one-time codes are not supported on this Passwork version yet".to_string())
        }
    }
}

/// What to say when a field that should hold text holds ciphertext.
///
/// Worth spelling out rather than reporting "decode failed": the user's vault is
/// working perfectly and their config is right, and the thing they need to know
/// is that this specific feature cannot serve them.
fn encrypted_vault_error() -> String {
    "this vault uses client-side encryption, which IoExplorer cannot decrypt".to_string()
}

// -- session -----------------------------------------------------------------

/// A session token, logging in if the held one is missing, stale, or rejected.
///
/// `force` re-logs in even when the cached token looks fine, which is how a
/// token the server has invalidated early — a restart, a revoked key — is
/// recovered from without the user seeing anything.
fn session_token(
    resolved: &Resolved,
    cache: &SessionCache,
    host: &str,
    force: bool,
) -> Result<Secret, String> {
    // A poisoned lock means a worker panicked mid-login. Re-logging in is right:
    // there is no invariant here a panic could have broken, only a token that
    // may never have been written.
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());

    if !force
        && let Some(session) = cache.as_ref()
        && session.usable(host)
    {
        return Ok(session.token.clone());
    }

    let key = resolved.token()?;
    let session = login(host, key)?;
    let token = session.token.clone();
    *cache = Some(session);
    Ok(token)
}

/// `POST /api/v4/auth/login/{apiKey}`.
///
/// The key travels in the path, which is the API's own design and not something
/// this can improve on. It is validated first: a key with a `/` or a `?` in it
/// would silently address a different endpoint, and no legitimate key has one.
fn login(host: &str, key: &Secret) -> Result<Session, String> {
    let key = key.expose();
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
    {
        return Err("the API key contains characters a URL path cannot carry".to_string());
    }

    let response = ureq::post(format!("{host}{API}/auth/login/{key}"))
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .build()
        // Passwork answers a login with no body at all with a 411; an explicit
        // empty JSON body is what its own clients send.
        .header("content-type", "application/json")
        .send("{}")
        .map_err(|error| network_error(&error, host))?;

    let document = read_json(response, host, "log in").map_err(|error| error.message(host))?;
    let data = document.get("data").cloned().unwrap_or(Value::Null);

    let token = string(&data, &["token"]);
    if token.is_empty() {
        return Err(format!("{} rejected the API key", host_label(host)));
    }

    let ttl = data
        .get("tokenTtl")
        .and_then(Value::as_u64)
        .map(Duration::from_secs)
        .filter(|ttl| !ttl.is_zero())
        .unwrap_or(DEFAULT_SESSION_TTL);

    Ok(Session {
        token: Secret(token),
        host: host.to_string(),
        obtained: Instant::now(),
        ttl,
    })
}

// -- requests ----------------------------------------------------------------

/// Why a call failed, split only as far as the retry needs.
///
/// A rejected token is the one failure worth telling apart: it is the expected
/// end of every session, and recovering from it silently is the difference
/// between a launcher that works all day and one that needs a restart at noon.
enum CallError {
    Unauthorized,
    Other(String),
}

impl CallError {
    fn message(self, host: &str) -> String {
        match self {
            Self::Unauthorized => format!("{} rejected the API key", host_label(host)),
            Self::Other(message) => message,
        }
    }
}

/// Runs a call with a session token, logging in again if the token was rejected.
///
/// The retry is once, and only for a rejection. A key that is genuinely wrong
/// fails the second attempt the same way, and looping on it would turn one bad
/// config line into a login flood.
fn with_session<T>(
    resolved: &Resolved,
    cache: &SessionCache,
    host: &str,
    call: impl Fn(&Secret) -> Result<T, CallError>,
) -> Result<T, String> {
    let token = session_token(resolved, cache, host, false)?;
    match call(&token) {
        Err(CallError::Unauthorized) => {
            let token = session_token(resolved, cache, host, true)?;
            call(&token).map_err(|error| error.message(host))
        }
        other => other.map_err(|error| error.message(host)),
    }
}

fn get(host: &str, path: &str, token: &Secret) -> Result<Value, CallError> {
    let response = ureq::get(format!("{host}{API}{path}"))
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .build()
        .header("passwork-auth", token.expose())
        .call()
        .map_err(|error| CallError::Other(network_error(&error, host)))?;

    read_json(response, host, path)
}

fn post(host: &str, path: &str, token: &Secret, body: &Value) -> Result<Value, CallError> {
    let response = ureq::post(format!("{host}{API}{path}"))
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .build()
        .header("content-type", "application/json")
        .header("passwork-auth", token.expose())
        .send(body.to_string())
        .map_err(|error| CallError::Other(network_error(&error, host)))?;

    read_json(response, host, path)
}

/// Reads a reply, turning both HTTP and API-level failures into one sentence.
fn read_json(
    response: ureq::http::Response<ureq::Body>,
    host: &str,
    what: &str,
) -> Result<Value, CallError> {
    let status = response.status().as_u16();
    let (_parts, body) = response.into_parts();
    let text = body
        .into_with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|error| {
            CallError::Other(format!(
                "cannot read the reply from {}: {error}",
                host_label(host)
            ))
        })?;

    // The single most likely misconfiguration, and the one whose default error
    // is least helpful: a v1-era config pointed at this module, or this module
    // pointed at a Passwork 7 server. Both answer with an HTML error page.
    if text.trim_start().starts_with('<') {
        return Err(CallError::Other(match status {
            404 => format!(
                "{} has no v4 API at {what} — if it is Passwork 7 or later, use provider = \"passwork\"",
                host_label(host)
            ),
            _ => format!(
                "{} returned a web page rather than API data",
                host_label(host)
            ),
        }));
    }

    let document: Value = serde_json::from_str(&text).map_err(|error| {
        CallError::Other(format!("{} did not return JSON: {error}", host_label(host)))
    })?;

    // The API reports its own failures inside a 200 as often as not, so the
    // status alone is not the answer.
    let failed = !(200..300).contains(&status)
        || document
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("error"));
    if failed {
        return Err(api_error(&document, status, host));
    }

    Ok(document)
}

/// The most specific failure the reply supports.
fn api_error(document: &Value, status: u16, host: &str) -> CallError {
    let detail = [
        document
            .get("data")
            .and_then(|data| data.get("errorMessage"))
            .and_then(Value::as_str),
        document.get("code").and_then(Value::as_str),
        document.get("message").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|detail| !detail.is_empty());

    // An expired session and a wrong key look identical from here, and both are
    // answered by logging in again — which distinguishes them for free.
    let rejected = matches!(status, 401 | 403)
        || detail.is_some_and(|detail| {
            matches!(
                detail,
                "unauthorized" | "invalidToken" | "tokenExpired" | "accessTokenExpired"
            )
        });
    if rejected {
        return CallError::Unauthorized;
    }

    CallError::Other(match (detail, status) {
        (Some(detail), _) => humanise(detail),
        (None, status) => format!("{} answered {status}", host_label(host)),
    })
}

/// Turns the API's camel-case error codes into something readable.
fn humanise(code: &str) -> String {
    match code {
        "minQueryLength2Chars" => "type at least two characters".to_string(),
        "accessDenied" => "access denied".to_string(),
        "notFound" => "the vault no longer has this entry".to_string(),
        other => other.to_string(),
    }
}

fn network_error(error: &ureq::Error, host: &str) -> String {
    match error {
        ureq::Error::HostNotFound => format!("cannot resolve {}", host_label(host)),
        ureq::Error::ConnectionFailed => format!("cannot reach {}", host_label(host)),
        ureq::Error::Timeout(_) => format!("{} did not answer in time", host_label(host)),
        other => format!("{}: {other}", host_label(host)),
    }
}

/// The host without its scheme, which is all a one-line row has room for.
fn host_label(host: &str) -> &str {
    host.trim_start_matches("https://")
        .trim_start_matches("http://")
}

// -- parsing -----------------------------------------------------------------

fn items(document: &Value) -> &[Value] {
    const EMPTY: &[Value] = &[];

    [document.get("data"), Some(document)]
        .into_iter()
        .flatten()
        .find_map(Value::as_array)
        .map_or(EMPTY, Vec::as_slice)
}

/// One search result. Metadata only — the v4 search does not return secrets at
/// all, which is what makes the listing safe to hold between keystrokes.
fn parse_entry(item: &Value) -> Option<Entry> {
    let shortcut = item
        .get("shortcutId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    let id = match shortcut {
        true => string(item, &["shortcutId"]),
        false => string(item, &["id"]),
    };
    // Without an id there is nothing to fetch, and a row that cannot be
    // activated is worse than one that is not there.
    if id.is_empty() {
        return None;
    }

    let login = string(item, &["login"]);
    let name = match string(item, &["name"]) {
        name if name.is_empty() => match login.is_empty() {
            true => id.clone(),
            false => login.clone(),
        },
        name => name,
    };

    Some(Entry {
        id,
        shortcut,
        name,
        login,
        url: string(item, &["url"]),
        // Already a readable `Vault / Folder` trail in this API, unlike v1 where
        // it has to be assembled from two names.
        folder: string(item, &["path"]),
        tags: strings(item, "tags"),
        // Deliberately never set. Deriving a code needs HMAC-SHA1 over the
        // shared secret, which the v1 path gets for free from the CLI and this
        // one would have to implement; offering a row that cannot deliver is
        // worse than not offering it.
        totp_field: None,
    })
}

fn string(item: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| item.get(*key))
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn strings(item: &Value, key: &str) -> Vec<String> {
    let Some(Value::Array(values)) = item.get(key) else {
        return Vec::new();
    };

    values
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}