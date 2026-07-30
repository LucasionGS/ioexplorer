//! Passwork: the commands `passwork-cli` answers to, and the parsing of what it
//! prints back.
//!
//! The client has no listing command of its own — `get` wants an id it cannot
//! help you find — so browsing goes through its `api` escape hatch and the
//! vault's own search endpoint. That reply is JSON, which is a better starting
//! point than the VPN's prose, but not a schema: this is a self-hosted product
//! whose deployments differ in version, so every field is read defensively and a
//! shape this file has not seen still yields usable rows. The one field without
//! which a row can do nothing is the id.
//!
//! What the search returns is metadata. Passwork encrypts the password and the
//! custom values, and the search endpoint hands back neither in a form this
//! could read — which is exactly the property the rows want: [`parse_entries`]
//! *cannot* accidentally surface a secret, because it is never given one.

use serde_json::Value;

use crate::spotlight::passwords::{
    CredentialVars, Entry, Provider, Resolved, SecretField, SecretRequest, capture,
};

pub const PROGRAM: &str = "passwork-cli";

/// What the client says when it tries to parse an HTML error page as JSON.
///
/// The signature of pointing this provider at a Passwork 6 server: every v1
/// endpoint 404s, the body is a web page, and the client reports a JSON error
/// that says nothing about the actual problem.
const NOT_JSON: &str = "Expecting value: line 1 column 1";

/// Lists the entries matching `query`, by running the client.
pub fn search(resolved: &Resolved, query: &str) -> Result<Vec<Entry>, String> {
    let output = capture(
        PROGRAM,
        &search_args(query),
        &resolved.environment(Provider::Passwork),
    )
    .map_err(explain)?;

    parse_entries(&output).map_err(explain)
}

/// Fetches one secret, by running the client.
pub fn fetch(resolved: &Resolved, request: &SecretRequest) -> Result<String, String> {
    capture(
        PROGRAM,
        &secret_args(request),
        &resolved.environment(Provider::Passwork),
    )
    .map_err(explain)
}

/// Turns the client's least helpful failure into the one-line fix for it.
///
/// `passwork-cli` speaks only the v1 API that Passwork 7 introduced. Against an
/// older server it gets an HTML 404 and dies parsing it, reporting a column
/// number — which sends the user to look at their token, their host and their
/// config, none of which are wrong.
fn explain(error: String) -> String {
    match error.contains(NOT_JSON) || error.contains("did not return JSON") {
        true => format!(
            "{PROGRAM} could not read the reply — if this server is Passwork 6 or older, \
             set provider = \"passwork-v4\""
        ),
        false => error,
    }
}

/// Host, token, refresh token, master key — the order [`CredentialSource`]
/// fills them in.
///
/// [`CredentialSource`]: crate::spotlight::passwords::CredentialSource
pub const CREDENTIAL_VARS: CredentialVars = [
    "PASSWORK_HOST",
    "PASSWORK_TOKEN",
    "PASSWORK_REFRESH_TOKEN",
    "PASSWORK_MASTER_KEY",
];

/// The vault's search endpoint, reached through the client's `api` mode.
const SEARCH_ENDPOINT: &str = "v1/items/search";

/// Custom-field types Passwork marks a one-time-password secret with.
const TOTP_TYPE: &str = "totp";
/// What a TOTP field tends to be called where its type did not survive the trip.
///
/// A fallback, not the rule: Passwork's own `type` is the answer whenever the
/// deployment sends it in the listing, and only a vault that withholds it makes
/// this the difference between offering a code row and not.
const TOTP_NAMES: &[&str] = &[
    "totp",
    "otp",
    "2fa",
    "mfa",
    "one-time password",
    "otp secret",
];

/// `passwork-cli api --method GET --endpoint v1/items/search`.
///
/// The query is the vault's, not a local filter: a Passwork install can hold
/// tens of thousands of entries across vaults the user only partly has access
/// to, and enumerating all of them to filter three off the top would be slower
/// and would pull far more of the vault onto this machine than the search the
/// user actually asked for.
///
/// An empty query is sent as no query at all rather than `query=""`, which is
/// what makes the opening state — the prefix typed, nothing after it — a listing
/// rather than a search for the empty string.
pub fn search_args(query: &str) -> Vec<String> {
    let query = query.trim();
    let mut args = vec![
        "api".to_string(),
        "--method".to_string(),
        "GET".to_string(),
        "--endpoint".to_string(),
        SEARCH_ENDPOINT.to_string(),
    ];

    if !query.is_empty() {
        args.push("--params".to_string());
        // Built by the JSON writer rather than by formatting: the user's query
        // is arbitrary text, and a quote in it would otherwise produce a
        // document the client rejects with a parse error for a search that is
        // perfectly reasonable.
        args.push(serde_json::json!({ "query": query }).to_string());
    }

    args
}

/// `passwork-cli get`, asking for exactly one field.
///
/// Bare `get` prints the password; `--field` names anything else the entry
/// carries; `--totp-code` names the field holding the shared secret and prints
/// the current code derived from it rather than the secret. The last of those is
/// the reason a code row exists at all — the secret itself never leaves the
/// client.
pub fn secret_args(request: &SecretRequest) -> Vec<String> {
    let selector = match request.shortcut {
        true => "--shortcut-id",
        false => "--password-id",
    };
    let mut args = vec!["get".to_string(), selector.to_string(), request.id.clone()];

    match &request.field {
        SecretField::Password => {}
        SecretField::Login => {
            args.push("--field".to_string());
            args.push("login".to_string());
        }
        SecretField::Totp(field) => {
            args.push("--totp-code".to_string());
            args.push(field.clone());
        }
    }

    args
}

/// Reads the search reply.
///
/// ```json
/// {
///   "items": [
///     {
///       "id": "6690…",
///       "name": "GitHub",
///       "login": "lucasion",
///       "url": "https://github.com",
///       "folderName": "Dev",
///       "vaultName": "Work",
///       "tags": ["ci"],
///       "customs": [{ "name": "TOTP", "type": "totp" }]
///     }
///   ]
/// }
/// ```
pub fn parse_entries(output: &str) -> Result<Vec<Entry>, String> {
    let document: Value = serde_json::from_str(output.trim())
        .map_err(|error| format!("{PROGRAM} did not return JSON: {error}"))?;

    Ok(items(&document).iter().filter_map(parse_entry).collect())
}

/// The array of entries, wherever this deployment put it.
///
/// `{"items": […]}` is what the endpoint documents and what the client's own
/// wrapper reads. The others cost a line each and mean a deployment that wraps
/// its replies differently degrades to working rather than to an empty list.
fn items(document: &Value) -> &[Value] {
    const EMPTY: &[Value] = &[];

    [
        document.get("items"),
        document.get("data").and_then(|data| data.get("items")),
        Some(document),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_array)
    .map_or(EMPTY, Vec::as_slice)
}

fn parse_entry(item: &Value) -> Option<Entry> {
    // Read before unwrapping: a shortcut is fetched by its own id, not by the id
    // of the password it points at, and that one lives on the outer object.
    let shortcut_id = string(item, &["shortcutId"]);
    // A shortcut listing nests the entry it points at, and that is where the
    // name and login are. A plain listing is already the entry.
    let details = item.get("item").unwrap_or(item);

    let id = match shortcut_id.is_empty() {
        true => string(details, &["id", "_id", "passwordId"]),
        false => shortcut_id.clone(),
    };
    // Without an id there is nothing to fetch, and a row that cannot be
    // activated is worse than one that is not there.
    if id.is_empty() {
        return None;
    }

    let name = match string(details, &["name", "title"]) {
        // Named after its login, or failing that its id: an unnamed entry is
        // still one the user may be looking for, and a blank row is unclickable
        // in practice even though it works.
        name if name.is_empty() => match string(details, &["login", "username"]) {
            login if login.is_empty() => id.clone(),
            login => login,
        },
        name => name,
    };

    Some(Entry {
        id,
        shortcut: !shortcut_id.is_empty(),
        name,
        login: string(details, &["login", "username"]),
        url: string(details, &["url", "link"]),
        folder: location(details),
        tags: strings(details, "tags"),
        totp_field: totp_field(details),
    })
}

/// Where the entry lives, as `Vault / Folder`.
///
/// Names only. The ids are in the reply too and would always be present, but a
/// row reading `Vault 6690f2… / 6690f3…` tells the user less than no subtitle
/// at all.
fn location(item: &Value) -> String {
    [
        string(item, &["vaultName", "vault"]),
        string(item, &["folderName", "folder", "path"]),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" / ")
}

/// The custom field holding a one-time-password secret, if the listing named one.
///
/// The name is what matters: `get --totp-code` looks the field up by it. The
/// *value* is encrypted here and stays that way — the client decrypts it and
/// derives the code, and neither ever passes through this process.
fn totp_field(item: &Value) -> Option<String> {
    let Some(Value::Array(customs)) = item.get("customs") else {
        return None;
    };

    customs.iter().find_map(|custom| {
        let name = custom.get("name").and_then(Value::as_str)?.trim();
        if name.is_empty() {
            return None;
        }

        let kind = custom
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        // The type is the vault's own answer and is taken whenever it is
        // legible. A deployment that encrypts it sends ciphertext here, which
        // matches nothing, and the name is the only lead left.
        let matched = kind.eq_ignore_ascii_case(TOTP_TYPE)
            || TOTP_NAMES
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate));

        matched.then(|| name.to_string())
    })
}

/// The first of `keys` the item carries as a non-empty string.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotlight::passwords::Provider;

    /// Shaped after a real `v1/items/search` reply, trimmed to the fields the
    /// rows read.
    const SEARCH: &str = r#"{
      "items": [
        {
          "id": "6690f2a1",
          "name": "GitHub",
          "login": "lucasion",
          "url": "https://github.com",
          "vaultName": "Work",
          "folderName": "Dev",
          "tags": ["ci", ""],
          "passwordEncrypted": "U2FsdGVkX1+not-a-secret-here",
          "customs": [
            { "name": "Recovery", "type": "text", "value": "U2FsdGVkX1" },
            { "name": "TOTP", "type": "totp", "value": "U2FsdGVkX1" }
          ]
        },
        {
          "id": "6690f2a2",
          "name": "Router",
          "vaultName": "Home"
        }
      ]
    }"#;

    #[test]
    fn an_entry_carries_what_a_row_needs() {
        let entries = parse_entries(SEARCH).expect("valid json");

        let github = &entries[0];
        assert_eq!(github.id, "6690f2a1");
        assert!(!github.shortcut);
        assert_eq!(github.name, "GitHub");
        assert_eq!(github.login, "lucasion");
        assert_eq!(github.url, "https://github.com");
        assert_eq!(github.folder, "Work / Dev");
        assert_eq!(github.tags, vec!["ci".to_string()]);
        assert_eq!(
            github.summary(),
            "lucasion · Work / Dev · https://github.com"
        );
    }

    /// The property that makes the listing safe to cache and to log: the reply
    /// contains ciphertext, and nothing carries it into an entry.
    #[test]
    fn nothing_encrypted_survives_into_an_entry() {
        let entries = parse_entries(SEARCH).expect("valid json");

        let printed = format!("{entries:?}");
        assert!(!printed.contains("U2FsdGVkX1"), "{printed}");
    }

    #[test]
    fn an_entry_with_only_a_name_still_parses() {
        let router = &parse_entries(SEARCH).expect("valid json")[1];

        assert_eq!(router.name, "Router");
        assert_eq!(router.login, "");
        assert_eq!(router.folder, "Home");
        assert_eq!(router.totp_field, None);
        assert_eq!(router.summary(), "Home");
    }

    /// `--totp-code` looks the field up by name, so the name is what has to
    /// survive parsing — and only for the field the vault typed as a TOTP, not
    /// the other custom sitting next to it.
    #[test]
    fn a_totp_custom_is_found_by_its_type() {
        let github = &parse_entries(SEARCH).expect("valid json")[0];

        assert_eq!(github.totp_field.as_deref(), Some("TOTP"));
    }

    /// Some deployments encrypt the custom's type along with its value. The name
    /// is then the only lead, and losing the code row over it would be a silent
    /// downgrade for a vault that is otherwise working.
    #[test]
    fn a_totp_custom_is_still_found_when_its_type_is_unreadable() {
        let entries = parse_entries(
            r#"{"items":[{"id":"1","name":"x","customs":[
                 {"name":"2FA","type":"U2FsdGVkX1+ciphertext"}]}]}"#,
        )
        .expect("valid json");

        assert_eq!(entries[0].totp_field.as_deref(), Some("2FA"));
    }

    /// The fallback is a list of names, not a guess at every custom field. An
    /// entry whose extra field is a PIN must not grow a code row that fails.
    #[test]
    fn an_unrelated_custom_is_not_mistaken_for_a_totp() {
        let entries = parse_entries(
            r#"{"items":[{"id":"1","name":"x","customs":[
                 {"name":"PIN","type":"U2FsdGVkX1+ciphertext"}]}]}"#,
        )
        .expect("valid json");

        assert_eq!(entries[0].totp_field, None);
    }

    /// A shortcut points at a password in another vault, and the client fetches
    /// it with a different flag.
    #[test]
    fn a_shortcut_is_marked_and_carries_its_own_id() {
        let entries = parse_entries(
            r#"{"items":[{"shortcutId":"sc-1","item":{"id":"pw-1","name":"Shared"}}]}"#,
        )
        .expect("valid json");

        // The nested item is the one with the name; the shortcut id is what the
        // client is later handed.
        assert_eq!(entries[0].name, "Shared");
        assert!(entries[0].shortcut);
        assert_eq!(entries[0].id, "sc-1");

        // The same reply flattened, which some versions send instead.
        let entries =
            parse_entries(r#"{"items":[{"id":"pw-1","shortcutId":"sc-1","name":"Shared"}]}"#)
                .expect("valid json");
        assert!(entries[0].shortcut);
        assert_eq!(entries[0].id, "sc-1");
    }

    /// An entry with no id cannot be fetched, so a row for it would only fail
    /// once the user had already committed to it.
    #[test]
    fn an_entry_without_an_id_is_dropped() {
        let entries = parse_entries(r#"{"items":[{"name":"Nameless"},{"id":"1","name":"Real"}]}"#)
            .expect("valid json");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Real");
    }

    /// A blank heading is a row the user cannot see to click, even though it
    /// works perfectly.
    #[test]
    fn an_unnamed_entry_borrows_a_heading() {
        let entries = parse_entries(r#"{"items":[{"id":"1","login":"root"},{"id":"2"}]}"#)
            .expect("valid json");

        assert_eq!(entries[0].name, "root");
        assert_eq!(entries[1].name, "2");
    }

    /// A deployment that wraps its replies differently should degrade to working.
    #[test]
    fn a_bare_array_is_read_as_a_listing() {
        let entries = parse_entries(r#"[{"id":"1","name":"Bare"}]"#).expect("valid json");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Bare");
    }

    #[test]
    fn an_empty_vault_is_not_an_error() {
        assert!(
            parse_entries(r#"{"items":[]}"#)
                .expect("valid json")
                .is_empty()
        );
        assert!(parse_entries("{}").expect("valid json").is_empty());
    }

    /// The client prints a traceback on stdout for some failures, which reaches
    /// here as text that is not JSON. Reporting that beats an empty list, which
    /// reads as "the vault has nothing".
    #[test]
    fn a_reply_that_is_not_json_is_reported() {
        let error = parse_entries("Traceback (most recent call last):").expect_err("not json");

        assert!(error.contains("did not return JSON"), "{error}");
    }

    /// The failure everyone pointing this at an older server will hit exactly
    /// once. The client's own words are a column number, which sends the user to
    /// audit a token and a host that are both perfectly correct.
    #[test]
    fn a_version_mismatch_names_the_provider_that_would_work() {
        let error =
            explain("Error making API call: Expecting value: line 1 column 1 (char 0)".to_string());

        assert!(error.contains("passwork-v4"), "{error}");
        assert!(error.contains("Passwork 6"), "{error}");
    }

    /// Only that one failure is rewritten. A genuine error — a bad token, an
    /// unreachable host — already says what is wrong, and burying it under a
    /// version hint would send the user chasing the wrong thing.
    #[test]
    fn an_ordinary_failure_is_passed_through_untouched() {
        let error = explain("Error: Required value not provided via PASSWORK_TOKEN".to_string());

        assert_eq!(
            error,
            "Error: Required value not provided via PASSWORK_TOKEN"
        );
    }

    /// The prefix opens on a listing rather than on a search for the empty
    /// string, which some deployments answer with nothing at all.
    #[test]
    fn an_empty_query_asks_for_no_query() {
        assert_eq!(
            search_args("   "),
            vec!["api", "--method", "GET", "--endpoint", "v1/items/search"]
        );
    }

    /// The query is arbitrary user text landing inside a JSON document. Building
    /// that by hand is how a search for `say "hi"` becomes a parse error.
    #[test]
    fn a_query_is_encoded_rather_than_formatted() {
        let args = search_args(r#"say "hi" \ now"#);

        assert_eq!(args[5], "--params");
        assert_eq!(args[6], r#"{"query":"say \"hi\" \\ now"}"#);
    }

    #[test]
    fn each_secret_is_asked_for_by_name() {
        let request = |field| SecretRequest {
            provider: Provider::Passwork,
            id: "6690f2a1".to_string(),
            shortcut: false,
            field,
        };

        assert_eq!(
            secret_args(&request(SecretField::Password)),
            vec!["get", "--password-id", "6690f2a1"]
        );
        assert_eq!(
            secret_args(&request(SecretField::Login)),
            vec!["get", "--password-id", "6690f2a1", "--field", "login"]
        );
        // The shared secret stays inside the client: what comes back is the code
        // it derived, which is worthless a minute from now.
        assert_eq!(
            secret_args(&request(SecretField::Totp("TOTP".to_string()))),
            vec!["get", "--password-id", "6690f2a1", "--totp-code", "TOTP"]
        );
    }

    #[test]
    fn a_shortcut_is_fetched_with_its_own_flag() {
        let args = secret_args(&SecretRequest {
            provider: Provider::Passwork,
            id: "sc-1".to_string(),
            shortcut: true,
            field: SecretField::Password,
        });

        assert_eq!(args, vec!["get", "--shortcut-id", "sc-1"]);
    }
}
