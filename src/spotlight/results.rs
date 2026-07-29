//! The result model and the providers that populate it.

use std::path::{Path, PathBuf};

use directories::UserDirs;

use crate::{
    bookmarks,
    launcher::{
        app_index::{AppEntry, AppIndex, IconRef},
        frecency::{FRECENCY_MAX_BONUS, Frecency},
        fuzzy::{self, Field},
    },
    spotlight::{
        calc,
        custom_results::CustomResult,
        paths::{self, PathCandidate},
        prefixes::{
            DEFAULT_RESULTS_ICON_SIZE, Prefix, PrefixKind, PrefixTable, build_action_line,
            build_command_line,
        },
        preview::Preview,
        software::{self, Catalog, Category, Item, SoftwareQuery},
        ssh::{self, SshHost},
        vpn,
        windows::OpenWindow,
    },
};

/// What activating a result does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Activation {
    LaunchApp(String),
    /// Switch to an already-open window, by the opaque handle its compositor
    /// backend issued.
    FocusWindow(String),
    OpenPath(PathBuf),
    RunShell(String),
    RunInTerminal(String),
    CopyText(String),
    /// Rewrite the entry text, e.g. accepting a prefix hint or a path completion.
    Replace(String),
    /// Open the chat view and send `prompt` to the provider at this index.
    AskAi {
        provider: usize,
        prompt: String,
    },
    /// Informational rows such as the help listing.
    Inert,
}

#[derive(Clone, Debug)]
pub struct SpotlightResult {
    pub title: String,
    pub subtitle: String,
    pub icon: IconRef,
    /// Drawn at the trailing edge of the row. Used by `get_results` prefixes,
    /// whose rows carry their own artwork.
    pub trailing_icon: Option<IconRef>,
    /// Pixel size for `trailing_icon`, so a prefix returning photographs can
    /// show them large enough to recognise.
    pub trailing_icon_size: i32,
    /// Shown in the panel beside the list while this row is selected or
    /// hovered. Only `get_results` rows carry one.
    pub preview: Option<Preview>,
    pub primary: Activation,
    pub secondary: Option<Activation>,
    /// Text Tab rewrites the entry to, when this row is selected.
    pub completion: Option<String>,
    pub frecency_key: Option<String>,
    pub score: i32,
}

impl SpotlightResult {
    fn new(title: impl Into<String>, subtitle: impl Into<String>, icon: IconRef) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            icon,
            trailing_icon: None,
            trailing_icon_size: DEFAULT_RESULTS_ICON_SIZE,
            preview: None,
            primary: Activation::Inert,
            secondary: None,
            completion: None,
            frecency_key: None,
            score: 0,
        }
    }
}

/// Sorts by score, then by the tie-breakers that keep ordering stable across runs.
pub fn sort_results(results: &mut [SpotlightResult]) {
    results.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.title.chars().count().cmp(&right.title.chars().count()))
            .then_with(|| left.title.cmp(&right.title))
    });
}

/// Builds the default, no-prefix results: open windows, applications, places,
/// and bookmarks.
pub fn default_results(
    query: &str,
    index: &AppIndex,
    windows: &[OpenWindow],
    frecency: &Frecency,
    now_secs: u64,
    limit: usize,
) -> Vec<SpotlightResult> {
    let mut results = Vec::new();

    // Deliberately only for a query the user has actually typed. On the opening
    // state every open window would match, and a dozen of them would push the
    // most-used applications off a list that exists to show exactly those. The
    // window prefix is the place to browse them all.
    if !query.trim().is_empty() {
        for mut row in window_results(query, windows, index, limit) {
            row.score = row.score.saturating_add(RUNNING_WINDOW_BONUS);
            results.push(row);
        }
    }

    for entry in index.entries() {
        let Some(found) = fuzzy::match_fields(query, &app_fields(entry)) else {
            continue;
        };

        let key = format!("app:{}", entry.desktop_id);
        let mut result = SpotlightResult::new(
            entry.name.clone(),
            entry
                .generic_name
                .clone()
                .or_else(|| entry.comment.clone())
                .unwrap_or_else(|| "Application".to_string()),
            entry.icon.clone(),
        );
        result.primary = Activation::LaunchApp(entry.desktop_id.clone());
        result.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
        result.frecency_key = Some(key);
        results.push(result);
    }

    for (name, path, icon) in places() {
        push_path_result(
            &mut results,
            query,
            &name,
            &path,
            &icon,
            "Folder",
            frecency,
            now_secs,
        );
    }

    for path in bookmarks::load() {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let icon = IconRef::from_icon_name("user-bookmarks-symbolic");
        push_path_result(
            &mut results,
            query,
            &name,
            &path,
            &icon,
            "Bookmark",
            frecency,
            now_secs,
        );
    }

    if query.is_empty() {
        // Every entry ties at zero, so the usual length tie-break would show an
        // arbitrary set of short names. Lead with what the user actually opens.
        sort_by_usage_then_name(&mut results);
    } else {
        sort_results(&mut results);
    }
    results.truncate(limit);
    results
}

/// Ordering for the opening state: most-used first, then alphabetical.
fn sort_by_usage_then_name(results: &mut [SpotlightResult]) {
    results.sort_by(|left, right| {
        right.score.cmp(&left.score).then_with(|| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.title.cmp(&right.title))
        })
    });
}

fn app_fields(entry: &AppEntry) -> Vec<Field<'_>> {
    let mut fields = vec![Field::new(entry.name.as_str(), 100)];
    if let Some(generic_name) = &entry.generic_name {
        fields.push(Field::new(generic_name.as_str(), 70));
    }
    if let Some(exec_name) = &entry.exec_name {
        fields.push(Field::new(exec_name.as_str(), 60));
    }
    fields.extend(
        entry
            .keywords
            .iter()
            .map(|keyword| Field::new(keyword.as_str(), 50)),
    );
    if let Some(comment) = &entry.comment {
        fields.push(Field::new(comment.as_str(), 30));
    }
    fields.extend(
        entry
            .categories
            .iter()
            .map(|category| Field::new(category.as_str(), 20)),
    );
    fields
}

#[allow(clippy::too_many_arguments)]
fn push_path_result(
    results: &mut Vec<SpotlightResult>,
    query: &str,
    name: &str,
    path: &Path,
    icon: &IconRef,
    kind: &str,
    frecency: &Frecency,
    now_secs: u64,
) {
    let Some(found) = fuzzy::match_fields(
        query,
        &[
            Field::new(name, 100),
            Field::new(&path.to_string_lossy(), 40),
        ],
    ) else {
        return;
    };

    let key = format!("path:{}", path.display());
    let mut result = SpotlightResult::new(
        name.to_string(),
        format!("{kind} · {}", paths::display_path(path)),
        icon.clone(),
    );
    result.primary = Activation::OpenPath(path.to_path_buf());
    result.secondary = path
        .parent()
        .map(|parent| Activation::OpenPath(parent.to_path_buf()));
    result.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
    result.frecency_key = Some(key);
    results.push(result);
}

/// The XDG user directories offered alongside applications.
pub fn places() -> Vec<(String, PathBuf, IconRef)> {
    let Some(dirs) = UserDirs::new() else {
        return Vec::new();
    };

    let mut places = vec![(
        "Home".to_string(),
        dirs.home_dir().to_path_buf(),
        IconRef::from_icon_name("user-home-symbolic"),
    )];

    let optional: [(&str, Option<&Path>, &str); 5] = [
        (
            "Documents",
            dirs.document_dir(),
            "folder-documents-symbolic",
        ),
        ("Downloads", dirs.download_dir(), "folder-download-symbolic"),
        ("Pictures", dirs.picture_dir(), "folder-pictures-symbolic"),
        ("Music", dirs.audio_dir(), "folder-music-symbolic"),
        ("Videos", dirs.video_dir(), "folder-videos-symbolic"),
    ];
    for (name, path, icon) in optional {
        if let Some(path) = path {
            places.push((
                name.to_string(),
                path.to_path_buf(),
                IconRef::from_icon_name(icon),
            ));
        }
    }

    places
}

/// The row offered when the text matches an alphanumeric prefix but has not yet
/// committed to it, e.g. `g` before the space.
pub fn hint_result(prefix: &Prefix) -> SpotlightResult {
    let mut result = SpotlightResult::new(
        prefix.label.clone(),
        format!("{} · press Tab", prefix.description),
        IconRef::from_icon_name(prefix.icon.clone()),
    );
    result.primary = Activation::Replace(format!("{} ", prefix.key));
    result.completion = Some(format!("{} ", prefix.key));
    // Above any fuzzy score so the hint stays pinned to the top.
    result.score = i32::MAX;
    result
}

/// Builds the results for an active prefix.
pub fn prefixed_results(
    prefix: &Prefix,
    arg: &str,
    table: &PrefixTable,
    limit: usize,
) -> Vec<SpotlightResult> {
    match &prefix.kind {
        PrefixKind::Shell => shell_results(arg),
        PrefixKind::OpenPath => path_results(arg, limit),
        PrefixKind::Calculator => calculator_results(arg),
        PrefixKind::Help => help_results(table),
        PrefixKind::FileSearch => Vec::new(), // filled asynchronously by the walker
        PrefixKind::Windows => Vec::new(),    // filled asynchronously by the compositor query
        PrefixKind::Ssh => Vec::new(),        // filled by the window from the ssh config it loaded
        PrefixKind::Software => Vec::new(),   // filled by the window from the catalog it resolved
        PrefixKind::Vpn(_) => Vec::new(),     // filled asynchronously by the vpn client
        PrefixKind::CustomResults { .. } => Vec::new(), // filled asynchronously by the runner
        PrefixKind::Command { command, terminal } => {
            command_results(prefix, command, *terminal, arg)
        }
        PrefixKind::Ai(index) => ai_results(prefix, *index, arg),
    }
}

/// The single row that opens a chat. Enter on it hands the prompt to the model.
fn ai_results(prefix: &Prefix, index: usize, arg: &str) -> Vec<SpotlightResult> {
    let icon = IconRef::from_icon_name(prefix.icon.clone());

    if arg.trim().is_empty() {
        return vec![SpotlightResult::new(
            prefix.label.clone(),
            format!("{} · type a question", prefix.description),
            icon,
        )];
    }

    let mut result = SpotlightResult::new(arg.to_string(), format!("Ask {}", prefix.label), icon);
    result.primary = Activation::AskAi {
        provider: index,
        prompt: arg.to_string(),
    };
    result.frecency_key = Some(format!("ai:{}", prefix.key));
    vec![result]
}

/// The trailing "Ask …" row offered on a plain query when a provider is marked
/// `default = true`. Sorted last so it never displaces a real match.
pub fn default_ai_result(
    provider_index: usize,
    label: &str,
    icon: &str,
    prompt: &str,
) -> SpotlightResult {
    let mut result = SpotlightResult::new(
        format!("Ask {label}"),
        prompt.to_string(),
        IconRef::from_icon_name(icon),
    );
    result.primary = Activation::AskAi {
        provider: provider_index,
        prompt: prompt.to_string(),
    };
    result.score = i32::MIN + 1;
    result
}

// -- open windows ----------------------------------------------------------

/// How much a running window outranks the launcher entry for the same app.
///
/// Large enough to clear any fuzzy-score difference between the window's title
/// and the app's name: if a copy is already running, switching to it is almost
/// always what was meant, and launching a second one is a click away either way.
const RUNNING_WINDOW_BONUS: i32 = FRECENCY_MAX_BONUS + 200;

/// Builds one row per open window, fuzzy-filtered by `query`.
///
/// With an empty query the compositor's own ordering is kept — it is
/// most-recently-used, which is exactly what an unfiltered switcher should show.
pub fn window_results(
    query: &str,
    windows: &[OpenWindow],
    index: &AppIndex,
    limit: usize,
) -> Vec<SpotlightResult> {
    let mut results = Vec::new();

    for window in windows {
        let app = index.find_by_app_id(&window.app_id);
        let app_name = app.map_or(window.app_id.as_str(), |entry| entry.name.as_str());

        let Some(found) = fuzzy::match_fields(
            query,
            &[
                Field::new(window.heading(), 100),
                Field::new(app_name, 90),
                Field::new(window.app_id.as_str(), 60),
                Field::new(window.workspace.as_str(), 20),
            ],
        ) else {
            continue;
        };

        let mut result = window_row(window, app_name, app);
        result.score = found.score;
        results.push(result);
    }

    if !query.trim().is_empty() {
        sort_results(&mut results);
    }
    results.truncate(limit);
    results
}

fn window_row(window: &OpenWindow, app_name: &str, app: Option<&AppEntry>) -> SpotlightResult {
    let icon = app.map_or_else(
        // No desktop entry matched, so the app id is the only lead left. It is
        // often also a valid icon name, and `image_for` falls back on its own
        // when it is not.
        || IconRef::from_icon_name(window.app_id.clone()),
        |entry| entry.icon.clone(),
    );

    let mut result = SpotlightResult::new(
        window.heading(),
        window_subtitle(window, app_name),
        icon.clone(),
    );
    result.primary = Activation::FocusWindow(window.handle.clone());
    result.preview = Some(Preview::icon(icon.0, window_caption(window, app_name)));
    // Keyed on the app rather than the handle: a window address is valid only
    // until the window closes, so recording one would teach the ranking nothing.
    result.frecency_key = Some(format!("window:{}", window.app_id));
    result
}

fn window_subtitle(window: &OpenWindow, app_name: &str) -> String {
    let mut parts = vec![app_name.to_string()];

    let location = window.location();
    if !location.is_empty() {
        parts.push(location);
    }
    // Worth saying, because it explains away the quirks that come with it —
    // blurry scaling, a missing icon, a class that matches no desktop entry.
    if window.xwayland {
        parts.push("XWayland".to_string());
    }
    if window.focused {
        parts.push("focused".to_string());
    }

    parts.join(" · ")
}

/// The detail block under the preview artwork.
fn window_caption(window: &OpenWindow, app_name: &str) -> String {
    let mut lines = vec![app_name.to_string()];

    // The title is already the row heading, but the row ellipsizes it and this
    // panel does not — for a browser tab or a document that is the whole point.
    let title = window.title.trim();
    if !title.is_empty() && title != app_name {
        lines.push(title.to_string());
    }

    let location = window.location();
    if !location.is_empty() {
        lines.push(location);
    }
    if window.xwayland {
        lines.push("XWayland".to_string());
    }

    lines.join("\n")
}

/// The row shown instead of a window list when there is nothing to list.
pub fn windows_notice(prefix: &Prefix, note: &str) -> SpotlightResult {
    SpotlightResult::new(
        prefix.label.clone(),
        note,
        IconRef::from_icon_name(prefix.icon.clone()),
    )
}

// -- vpn -------------------------------------------------------------------

/// Artwork for the row that connects, and for a location row.
const VPN_CONNECT_ICON: &str = "network-vpn-symbolic";
/// Artwork for the row that disconnects, and for the disconnected state.
const VPN_DISCONNECT_ICON: &str = "network-vpn-disconnected-symbolic";

/// Builds the rows for the VPN prefix: the action the current state calls for,
/// then the locations that match the query.
///
/// The action row is pinned rather than scored on an empty query — it is the one
/// row that says what the VPN is doing right now, and burying it under two
/// hundred locations would hide the answer to the question the prefix is usually
/// opened to ask. Once the user types, it competes like everything else, so
/// searching for a city does not keep an unrelated Disconnect at the top.
pub fn vpn_results(
    prefix: &Prefix,
    arg: &str,
    provider: vpn::Provider,
    state: &vpn::VpnState,
    frecency: &Frecency,
    now_secs: u64,
    limit: usize,
) -> Vec<SpotlightResult> {
    let query = arg.trim();
    let mut results = Vec::new();

    for location in &state.locations {
        let Some(found) = fuzzy::match_fields(
            query,
            &[
                Field::new(location.name.as_str(), 100),
                Field::new(location.nickname.as_str(), 80),
                Field::new(location.region.as_str(), 60),
            ],
        ) else {
            continue;
        };

        let key = format!("vpn:{}:{}", provider.id(), location.target);
        let mut row = vpn_location_row(prefix, provider, location, state);
        row.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
        row.frecency_key = Some(key);
        results.push(row);
    }

    sort_results(&mut results);

    let action = vpn_action_row(provider, state);
    match query.is_empty() {
        true => {
            results.truncate(limit.saturating_sub(1));
            results.insert(0, action);
        }
        false => {
            if let Some(found) = fuzzy::match_fields(
                query,
                &[
                    Field::new(action.title.as_str(), 100),
                    Field::new(action.subtitle.as_str(), 50),
                ],
            ) {
                let mut action = action;
                action.score = found.score;
                results.push(action);
                sort_results(&mut results);
            }
            results.truncate(limit);
        }
    }
    results
}

/// The row for what the VPN's current state calls for: disconnect when it is up,
/// connect to the client's own choice of location when it is not.
fn vpn_action_row(provider: vpn::Provider, state: &vpn::VpnState) -> SpotlightResult {
    let status = &state.status;
    let connected = status.connected || status.connecting;

    let (title, icon, line) = match connected {
        true => (
            "Disconnect".to_string(),
            VPN_DISCONNECT_ICON,
            provider.disconnect_line(),
        ),
        false => (
            "Connect".to_string(),
            VPN_CONNECT_ICON,
            provider.connect_best_line(),
        ),
    };

    let subtitle = match connected {
        true => status.summary(),
        false => format!("{} · best location", status.summary()),
    };

    let mut row = SpotlightResult::new(title, subtitle, IconRef::from_icon_name(icon));
    row.primary = Activation::RunShell(line.clone());
    // Connecting takes several seconds and prints as it goes, so the terminal is
    // not a second way to do the same thing — it is the way to watch it happen.
    row.secondary = Some(Activation::RunInTerminal(line));
    row.preview = Some(Preview::icon(icon, vpn_caption(provider, state)));
    row
}

fn vpn_location_row(
    prefix: &Prefix,
    provider: vpn::Provider,
    location: &vpn::Location,
    state: &vpn::VpnState,
) -> SpotlightResult {
    let line = provider.connect_line(&location.target);
    let current = state.status.connected
        && state
            .status
            .location
            .as_deref()
            .is_some_and(|at| at == location.nickname || at == location.name);

    let mut subtitle = location.summary();
    if current {
        subtitle = match subtitle.is_empty() {
            true => "Connected".to_string(),
            false => format!("{subtitle} · connected"),
        };
    }

    let icon = match location.best {
        true => VPN_CONNECT_ICON,
        false => "network-workgroup-symbolic",
    };
    let mut row = SpotlightResult::new(
        location.name.clone(),
        subtitle,
        IconRef::from_icon_name(icon),
    );
    row.primary = Activation::RunShell(line.clone());
    row.secondary = Some(Activation::RunInTerminal(line.clone()));
    row.preview = Some(Preview::icon(
        icon,
        vpn_location_caption(location, state, &line),
    ));
    row.completion = Some(format!("{} {}", prefix.key, location.name));
    row
}

/// The preview under the action row: the client's own status text, unedited.
fn vpn_caption(provider: vpn::Provider, state: &vpn::VpnState) -> String {
    let details = state.status.details.trim();
    match details.is_empty() {
        true => format!("{}\n\n{}", provider.label(), state.status.summary()),
        false => format!("{}\n\n{details}", provider.label()),
    }
}

fn vpn_location_caption(location: &vpn::Location, state: &vpn::VpnState, line: &str) -> String {
    let mut lines = vec![location.name.clone()];

    let summary = location.summary();
    if !summary.is_empty() {
        lines.push(summary);
    }
    lines.push(String::new());
    lines.push(state.status.summary());
    lines.push(String::new());
    lines.push(line.to_string());
    lines.join("\n")
}

/// The row shown instead of a VPN list when there is nothing to list.
pub fn vpn_notice(prefix: &Prefix, note: &str) -> SpotlightResult {
    SpotlightResult::new(
        prefix.label.clone(),
        note,
        IconRef::from_icon_name(prefix.icon.clone()),
    )
}

// -- ssh hosts -------------------------------------------------------------

/// Artwork for a host that the config knows about.
const SSH_HOST_ICON: &str = "network-server-symbolic";
/// Artwork for the ad-hoc row, which is deliberately not the same: the two rows
/// do different things, and at the top of the list that has to be visible.
const SSH_ADHOC_ICON: &str = "network-transmit-receive-symbolic";

/// Builds the rows for the `ssh` prefix: the configured hosts that match the
/// query, led by an ad-hoc row for connecting to whatever was typed.
pub fn ssh_results(
    prefix: &Prefix,
    arg: &str,
    hosts: &[SshHost],
    frecency: &Frecency,
    now_secs: u64,
    limit: usize,
) -> Vec<SpotlightResult> {
    let query = arg.trim();
    let mut results = Vec::new();

    for host in hosts {
        let Some(found) = fuzzy::match_fields(
            query,
            &[
                Field::new(host.alias.as_str(), 100),
                Field::new(host.hostname(), 70),
                Field::new(host.user().unwrap_or_default(), 40),
                Field::new(host.option("ProxyJump").unwrap_or_default(), 30),
            ],
        ) else {
            continue;
        };

        let key = format!("ssh:{}", host.alias);
        let mut row = ssh_host_row(prefix, host);
        row.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
        row.frecency_key = Some(key);
        results.push(row);
    }

    sort_results(&mut results);

    match ssh_adhoc_row(query, hosts) {
        // The ad-hoc row is pinned rather than scored: it is the row that says
        // what Enter will do with the text as typed, so it belongs at the top
        // whatever the configured hosts happen to score.
        Some(row) => {
            results.truncate(limit.saturating_sub(1));
            results.insert(0, row);
        }
        None => results.truncate(limit),
    }
    results
}

fn ssh_host_row(prefix: &Prefix, host: &SshHost) -> SpotlightResult {
    let line = ssh::connect_command(&host.alias);

    let mut row = SpotlightResult::new(
        host.alias.clone(),
        host.summary(),
        IconRef::from_icon_name(SSH_HOST_ICON),
    );
    row.primary = Activation::RunInTerminal(line.clone());
    // Not a second way to connect but a way *not* to: the command is often
    // wanted in a script, a note, or another terminal that is already open.
    row.secondary = Some(Activation::CopyText(line));
    row.preview = Some(Preview::icon(SSH_HOST_ICON, host.details()));
    row.completion = Some(format!("{} {}", prefix.key, host.alias));
    row
}

/// The row that connects to exactly what was typed.
///
/// Absent when the text is not a destination ssh would accept, and absent when
/// it names a configured host — that entry already connects there, and offering
/// the same connection twice at the top of the list is noise.
fn ssh_adhoc_row(query: &str, hosts: &[SshHost]) -> Option<SpotlightResult> {
    if !ssh::is_plausible_destination(query) {
        return None;
    }
    if hosts.iter().any(|host| host.alias == query) {
        return None;
    }

    let line = ssh::connect_command(query);
    let mut row = SpotlightResult::new(
        query.to_string(),
        "Connect · not in your SSH config".to_string(),
        IconRef::from_icon_name(SSH_ADHOC_ICON),
    );
    row.primary = Activation::RunInTerminal(line.clone());
    row.secondary = Some(Activation::CopyText(line.clone()));
    row.preview = Some(Preview::icon(
        SSH_ADHOC_ICON,
        format!("{query}\n\nNot declared in your SSH config\n\n{line}"),
    ));
    // No frecency key on purpose: the row is pinned to the top regardless, so a
    // ranking bonus would buy nothing and would leave a typo in the history.
    Some(row)
}

/// The row shown instead of a host list when there is nothing to list.
pub fn ssh_notice(prefix: &Prefix, note: &str) -> SpotlightResult {
    SpotlightResult::new(
        prefix.label.clone(),
        note,
        IconRef::from_icon_name(prefix.icon.clone()),
    )
}

// -- software --------------------------------------------------------------

/// The band software rows sit in on a plain query.
///
/// Low enough that a real application, path or bookmark always outranks them —
/// what is installed beats what could be — while still ordering the software
/// rows among themselves by how well they matched.
const SOFTWARE_SEARCH_BASE: i32 = i32::MIN + 10_000;

/// How far the "Install Software" row sits above the app rows beneath it, so it
/// reads as the heading for what follows.
const SOFTWARE_SECTION_BONUS: i32 = 1_000;

/// Builds the rows for the software prefix: categories at the top level, the
/// apps of one category once the user has entered it.
pub fn software_results(
    prefix_key: &str,
    arg: &str,
    catalog: &Catalog,
    keep_open: bool,
    frecency: &Frecency,
    now_secs: u64,
    limit: usize,
) -> Vec<SpotlightResult> {
    match software::parse_arg(catalog, arg) {
        SoftwareQuery::Items { category, filter } => {
            let mut results = Vec::new();
            for item in &category.items {
                let Some(found) =
                    fuzzy::match_fields(filter, &software_item_fields(category, item))
                else {
                    continue;
                };
                let mut row = software_item_row(prefix_key, category, item, keep_open);
                let key = software_frecency_key(category, item);
                row.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
                row.frecency_key = Some(key);
                results.push(row);
            }

            // With nothing typed the category's own order is kept: it is the
            // order the catalog declares, and an empty query has nothing to
            // rank by.
            if !filter.is_empty() {
                sort_results(&mut results);
            }
            results.truncate(limit);
            results
        }
        SoftwareQuery::Categories { filter: "" } => catalog
            .categories()
            .iter()
            .take(limit)
            .map(|category| software_category_row(prefix_key, category))
            .collect(),
        SoftwareQuery::Categories { filter } => {
            let mut results = Vec::new();

            for category in catalog.categories() {
                let Some(found) = fuzzy::match_fields(
                    filter,
                    &[
                        Field::new(category.label.as_str(), 100),
                        Field::new(category.id.as_str(), 80),
                    ],
                ) else {
                    continue;
                };
                let mut row = software_category_row(prefix_key, category);
                row.score = found.score;
                results.push(row);
            }

            // Apps are offered from the top level too, so `install gimp` reaches
            // GIMP without having to know it lives under Creativity.
            for (category, item) in catalog.items() {
                let Some(found) =
                    fuzzy::match_fields(filter, &software_item_fields(category, item))
                else {
                    continue;
                };
                let mut row = software_item_row(prefix_key, category, item, keep_open);
                let key = software_frecency_key(category, item);
                row.score = found.score + frecency.bonus(&key, now_secs, FRECENCY_MAX_BONUS);
                row.frecency_key = Some(key);
                results.push(row);
            }

            sort_results(&mut results);
            results.truncate(limit);
            results
        }
    }
}

/// The software rows offered on a plain, unprefixed query: the apps that match,
/// led by a row that opens the catalog.
///
/// Anything already installed is left out. Its launcher entry is the row the
/// user wants, and offering to install what is sitting right above is noise.
pub fn software_search_results(
    prefix_key: &str,
    query: &str,
    catalog: &Catalog,
    index: &AppIndex,
    keep_open: bool,
    limit: usize,
) -> Vec<SpotlightResult> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();

    if let Some(found) = fuzzy::match_fields(
        query,
        &[
            Field::new("Install Software", 100),
            Field::new("packages", 60),
            Field::new("apps", 60),
        ],
    ) {
        let mut row = SpotlightResult::new(
            "Install Software",
            "Browse apps by category",
            IconRef::from_icon_name(software::SOFTWARE_ICON),
        );
        let entry = format!("{prefix_key} ");
        row.primary = Activation::Replace(entry.clone());
        row.completion = Some(entry);
        row.score = SOFTWARE_SEARCH_BASE + SOFTWARE_SECTION_BONUS + found.score;
        results.push(row);
    }

    for (category, item) in catalog.items() {
        if is_installed(index, &item.name) {
            continue;
        }
        let Some(found) = fuzzy::match_fields(query, &software_item_fields(category, item)) else {
            continue;
        };
        let mut row = software_item_row(prefix_key, category, item, keep_open);
        row.score = SOFTWARE_SEARCH_BASE + found.score;
        // No frecency key: these rows are pinned to the bottom of a plain query
        // whatever their history, so a bonus would buy nothing.
        results.push(row);
    }

    sort_results(&mut results);
    results.truncate(limit);
    results
}

/// Whether the catalog entry named `name` already has a desktop entry.
///
/// Both spellings are tried because the two rarely agree: CurseForge ships
/// `curseforge.desktop`, while Visual Studio Code ships `code.desktop` and is
/// only recognisable by its name.
fn is_installed(index: &AppIndex, name: &str) -> bool {
    index.find_by_app_id(name).is_some() || index.find_by_app_id(&software::slug(name)).is_some()
}

fn software_item_fields<'a>(category: &'a Category, item: &'a Item) -> Vec<Field<'a>> {
    let mut fields = vec![
        Field::new(item.name.as_str(), 100),
        Field::new(item.description.as_str(), 40),
        Field::new(category.label.as_str(), 20),
    ];
    fields.extend(
        item.keywords
            .iter()
            .map(|keyword| Field::new(keyword.as_str(), 60)),
    );
    fields
}

fn software_frecency_key(category: &Category, item: &Item) -> String {
    format!("software:{}:{}", category.id, software::slug(&item.name))
}

/// A category row. Activating it does not close the window — it rewrites the
/// entry, which is what makes the section a menu rather than a flat list.
fn software_category_row(prefix_key: &str, category: &Category) -> SpotlightResult {
    let entry = software::category_query(prefix_key, category);
    let mut row = SpotlightResult::new(
        category.label.clone(),
        match category.items.len() {
            1 => "1 app".to_string(),
            count => format!("{count} apps"),
        },
        IconRef::from_icon_name(category.icon.clone()),
    );
    row.primary = Activation::Replace(entry.clone());
    row.completion = Some(entry);
    row
}

fn software_item_row(
    prefix_key: &str,
    category: &Category,
    item: &Item,
    keep_open: bool,
) -> SpotlightResult {
    let mut row = SpotlightResult::new(
        item.name.clone(),
        format!("Install · {}", item.description),
        IconRef::from_icon_name(item.icon.clone()),
    );
    row.primary = Activation::RunInTerminal(software::install_line(item, keep_open));
    // Not a second way to install but a way *not* to: the command is often
    // wanted in a script, a note, or a terminal that is already open.
    row.secondary = Some(Activation::CopyText(item.command.clone()));
    row.preview = Some(Preview::icon(
        item.icon.clone(),
        format!("{}\n\n{}\n\n{}", item.name, item.description, item.command),
    ));
    row.completion = Some(format!("{prefix_key} {} {}", category.id, item.name));
    row
}

fn shell_results(arg: &str) -> Vec<SpotlightResult> {
    if arg.trim().is_empty() {
        return vec![SpotlightResult::new(
            "Run a command",
            "Type a shell command to run",
            IconRef::from_icon_name("utilities-terminal-symbolic"),
        )];
    }

    let mut result = SpotlightResult::new(
        arg.to_string(),
        "Run · Ctrl+Enter to run in a terminal",
        IconRef::from_icon_name("utilities-terminal-symbolic"),
    );
    result.primary = Activation::RunShell(arg.to_string());
    result.secondary = Some(Activation::RunInTerminal(arg.to_string()));
    vec![result]
}

fn command_results(
    prefix: &Prefix,
    command: &str,
    terminal: bool,
    arg: &str,
) -> Vec<SpotlightResult> {
    let line = build_command_line(command, arg);
    let mut result = SpotlightResult::new(
        if arg.trim().is_empty() {
            prefix.label.clone()
        } else {
            format!("{} — {arg}", prefix.label)
        },
        prefix.description.clone(),
        IconRef::from_icon_name(prefix.icon.clone()),
    );
    result.primary = if terminal {
        Activation::RunInTerminal(line.clone())
    } else {
        Activation::RunShell(line.clone())
    };
    result.secondary = Some(Activation::RunInTerminal(line));
    result.frecency_key = Some(format!("prefix:{}", prefix.key));
    vec![result]
}

fn path_results(arg: &str, limit: usize) -> Vec<SpotlightResult> {
    let candidates = paths::complete(arg, limit);
    if candidates.is_empty() {
        let expanded = paths::expand_tilde(arg.trim());
        if expanded.exists() {
            return vec![open_path_result(&PathCandidate {
                path: expanded.clone(),
                is_dir: expanded.is_dir(),
                score: 0,
            })];
        }
        return vec![SpotlightResult::new(
            "No matching path",
            "Type a folder or file path",
            IconRef::from_icon_name("folder-open-symbolic"),
        )];
    }

    candidates.iter().map(open_path_result).collect()
}

fn open_path_result(candidate: &PathCandidate) -> SpotlightResult {
    let name = candidate
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| candidate.path.display().to_string());

    let mut result = SpotlightResult::new(
        name,
        paths::display_path(&candidate.path),
        IconRef::from_icon_name(if candidate.is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        }),
    );
    result.primary = Activation::OpenPath(candidate.path.clone());
    result.secondary = candidate
        .path
        .parent()
        .map(|parent| Activation::OpenPath(parent.to_path_buf()));
    result.completion = Some(format!("> {}", paths::completion_text(candidate)));
    result.frecency_key = Some(format!("path:{}", candidate.path.display()));
    result.score = candidate.score;
    result
}

/// Wraps filesystem-search hits, which arrive asynchronously.
pub fn file_hit_results(
    hits: &[crate::spotlight::file_search::FileHit],
    limit: usize,
) -> Vec<SpotlightResult> {
    let mut results = hits
        .iter()
        .map(|hit| {
            let name = hit
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| hit.path.display().to_string());

            let mut result = SpotlightResult::new(
                name,
                paths::display_path(&hit.path),
                IconRef::from_icon_name(if hit.is_dir {
                    "folder-symbolic"
                } else {
                    "text-x-generic-symbolic"
                }),
            );
            result.primary = Activation::OpenPath(hit.path.clone());
            result.secondary = hit
                .path
                .parent()
                .map(|parent| Activation::OpenPath(parent.to_path_buf()));
            result.frecency_key = Some(format!("path:{}", hit.path.display()));
            result.score = hit.score;
            result
        })
        .collect::<Vec<_>>();

    sort_results(&mut results);
    results.truncate(limit);
    results
}

/// The trailing row shown when the filesystem walk hit one of its bounds.
pub fn truncation_notice(shown: usize) -> SpotlightResult {
    let mut result = SpotlightResult::new(
        format!("Showing the first {shown} matches"),
        "Narrow your search to see more",
        IconRef::from_icon_name("view-more-symbolic"),
    );
    // Below every real hit, so it always lands at the bottom.
    result.score = i32::MIN;
    result
}

/// Wraps the rows a `get_results` command returned.
///
/// The command's own ordering is preserved — it knows what is most relevant to
/// its query, and nothing here can second-guess that.
pub fn custom_result_rows(
    prefix: &Prefix,
    results: &[CustomResult],
    action: Option<&str>,
    terminal: bool,
    icon_size: i32,
    limit: usize,
) -> Vec<SpotlightResult> {
    results
        .iter()
        .take(limit)
        .map(|item| {
            // A value identical to the title — the whole shape of the one-row-
            // per-line format — would otherwise print the same text twice.
            let subtitle = if item.value == item.title {
                String::new()
            } else {
                item.value.clone()
            };
            let mut row = SpotlightResult::new(
                item.title.clone(),
                subtitle,
                IconRef::from_icon_name(prefix.icon.clone()),
            );
            row.trailing_icon = item.icon.clone().map(IconRef);
            row.trailing_icon_size = icon_size;
            row.preview = item.preview.clone();
            match action {
                Some(action) => {
                    let line = build_action_line(action, &item.value);
                    row.primary = if terminal {
                        Activation::RunInTerminal(line.clone())
                    } else {
                        Activation::RunShell(line.clone())
                    };
                    row.secondary = Some(Activation::RunInTerminal(line));
                }
                // With no `action` there is nothing to run, so the value is at
                // least worth putting on the clipboard.
                None => row.primary = Activation::CopyText(item.value.clone()),
            }
            row
        })
        .collect()
}

/// The row shown before a `get_results` prefix has anything to show.
pub fn custom_results_notice(prefix: &Prefix, note: &str) -> SpotlightResult {
    SpotlightResult::new(
        prefix.label.clone(),
        note,
        IconRef::from_icon_name(prefix.icon.clone()),
    )
}

/// The row shown when a `get_results` command failed or returned nonsense.
pub fn custom_results_error(error: &str) -> SpotlightResult {
    SpotlightResult::new(
        "Cannot get results",
        error,
        IconRef::from_icon_name("dialog-warning-symbolic"),
    )
}

fn calculator_results(arg: &str) -> Vec<SpotlightResult> {
    if arg.trim().is_empty() {
        return vec![SpotlightResult::new(
            "Calculate",
            "Type an expression, e.g. 2^10 or sqrt(2)",
            IconRef::from_icon_name("accessories-calculator-symbolic"),
        )];
    }

    match calc::eval(arg) {
        Ok(value) => {
            let text = calc::format_result(value);
            let mut result = SpotlightResult::new(
                text.clone(),
                format!("{arg} · Enter to copy"),
                IconRef::from_icon_name("accessories-calculator-symbolic"),
            );
            result.primary = Activation::CopyText(text);
            vec![result]
        }
        Err(error) => vec![SpotlightResult::new(
            "Cannot calculate",
            error.to_string(),
            IconRef::from_icon_name("dialog-warning-symbolic"),
        )],
    }
}

fn help_results(table: &PrefixTable) -> Vec<SpotlightResult> {
    table
        .all()
        .iter()
        .map(|prefix| {
            let mut result = SpotlightResult::new(
                format!("{}  {}", prefix.key, prefix.label),
                prefix.description.clone(),
                IconRef::from_icon_name(prefix.icon.clone()),
            );
            let text = if prefix.is_symbolic() {
                prefix.key.clone()
            } else {
                format!("{} ", prefix.key)
            };
            result.primary = Activation::Replace(text.clone());
            result.completion = Some(text);
            result
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SpotlightConfig;
    use crate::spotlight::{prefixes, preview::PreviewKind};

    fn table() -> PrefixTable {
        prefixes::resolve_with_ai(&SpotlightConfig::default(), None).0
    }

    #[test]
    fn sorting_prefers_higher_scores_then_shorter_titles() {
        let mut results = vec![
            SpotlightResult {
                score: 10,
                ..SpotlightResult::new("Bravo", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 50,
                ..SpotlightResult::new("Alphabetical", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 50,
                ..SpotlightResult::new("Alpha", "", IconRef::fallback())
            },
        ];

        sort_results(&mut results);

        let titles = results.iter().map(|r| r.title.as_str()).collect::<Vec<_>>();
        assert_eq!(titles, vec!["Alpha", "Alphabetical", "Bravo"]);
    }

    #[test]
    fn the_opening_state_leads_with_usage_then_alphabetical() {
        let mut results = vec![
            SpotlightResult {
                score: 0,
                ..SpotlightResult::new("Zebra", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 0,
                ..SpotlightResult::new("Ant", "", IconRef::fallback())
            },
            SpotlightResult {
                score: 40,
                ..SpotlightResult::new("Frequently Used Thing", "", IconRef::fallback())
            },
        ];

        sort_by_usage_then_name(&mut results);

        let titles = results.iter().map(|r| r.title.as_str()).collect::<Vec<_>>();
        assert_eq!(titles, vec!["Frequently Used Thing", "Ant", "Zebra"]);
    }

    #[test]
    fn calculator_reports_the_value_and_offers_a_copy() {
        let results = calculator_results("2^10");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "1024");
        assert_eq!(results[0].primary, Activation::CopyText("1024".to_string()));
    }

    #[test]
    fn calculator_surfaces_parse_errors_without_an_action() {
        let results = calculator_results("1 +");

        assert_eq!(results[0].title, "Cannot calculate");
        assert_eq!(results[0].primary, Activation::Inert);
    }

    fn catalog() -> Catalog {
        Catalog::resolve(&crate::config::SpotlightSoftwareConfig::default())
    }

    fn software_rows(arg: &str) -> Vec<SpotlightResult> {
        software_results(
            "install",
            arg,
            &catalog(),
            true,
            &Frecency::default(),
            0,
            12,
        )
    }

    fn titles(results: &[SpotlightResult]) -> Vec<&str> {
        results.iter().map(|row| row.title.as_str()).collect()
    }

    #[test]
    fn the_software_prefix_opens_on_its_categories() {
        let rows = software_rows("");

        assert_eq!(
            titles(&rows),
            ["Creativity", "Gaming", "Communication", "Development"]
        );
        assert_eq!(rows[0].subtitle, "2 apps");
        assert_eq!(
            rows[0].primary,
            Activation::Replace("install creativity ".to_string())
        );
    }

    #[test]
    fn a_category_row_completes_to_the_same_text_it_activates_to() {
        let rows = software_rows("");

        assert_eq!(rows[0].completion.as_deref(), Some("install creativity "));
    }

    #[test]
    fn entering_a_category_lists_its_apps() {
        let rows = software_rows("creativity ");

        assert_eq!(titles(&rows), ["GIMP", "Krita"]);
        assert_eq!(
            rows[0].primary,
            Activation::RunInTerminal(
                "yay -S --needed gimp; printf '\\n[press Enter to close] '; read -r _".to_string()
            )
        );
        assert_eq!(
            rows[0].secondary,
            Some(Activation::CopyText("yay -S --needed gimp".to_string()))
        );
    }

    #[test]
    fn the_top_level_reaches_an_app_without_its_category() {
        let rows = software_rows("krita");

        assert_eq!(rows[0].title, "Krita");
        assert!(matches!(rows[0].primary, Activation::RunInTerminal(_)));
    }

    #[test]
    fn keeping_the_terminal_open_is_optional() {
        let rows = software_results(
            "install",
            "creativity ",
            &catalog(),
            false,
            &Frecency::default(),
            0,
            12,
        );

        assert_eq!(
            rows[0].primary,
            Activation::RunInTerminal("yay -S --needed gimp".to_string())
        );
    }

    #[test]
    fn a_plain_query_offers_to_install_what_is_missing() {
        let rows = software_search_results(
            "install",
            "discord",
            &catalog(),
            &AppIndex::default(),
            true,
            12,
        );

        assert_eq!(titles(&rows), ["Discord"]);
        // Below every real match, so an app that is actually installed always
        // comes first.
        assert!(rows[0].score < 0);
    }

    #[test]
    fn a_plain_query_stays_quiet_about_what_is_already_installed() {
        let index = AppIndex::from_entries(vec![AppEntry {
            desktop_id: "discord.desktop".to_string(),
            name: "Discord".to_string(),
            generic_name: None,
            comment: None,
            keywords: Vec::new(),
            categories: Vec::new(),
            exec_name: Some("discord".to_string()),
            icon: IconRef::fallback(),
        }]);

        let rows = software_search_results("install", "discord", &catalog(), &index, true, 12);

        assert!(rows.is_empty());
    }

    #[test]
    fn a_plain_query_can_reach_the_catalog_itself() {
        let rows = software_search_results(
            "install",
            "install software",
            &catalog(),
            &AppIndex::default(),
            true,
            12,
        );

        assert_eq!(rows[0].title, "Install Software");
        assert_eq!(rows[0].primary, Activation::Replace("install ".to_string()));
    }

    #[test]
    fn shell_prefix_offers_a_terminal_as_the_secondary_action() {
        let results = shell_results("notify-send hi");

        assert_eq!(
            results[0].primary,
            Activation::RunShell("notify-send hi".to_string())
        );
        assert_eq!(
            results[0].secondary,
            Some(Activation::RunInTerminal("notify-send hi".to_string()))
        );
    }

    #[test]
    fn help_lists_every_prefix_and_completes_to_it() {
        let table = table();
        let results = help_results(&table);

        assert_eq!(results.len(), table.all().len());
        assert!(results.iter().any(|result| result.title.starts_with('=')));
        assert!(results.iter().all(|result| result.completion.is_some()));
    }

    #[test]
    fn a_hint_row_sorts_above_everything_else() {
        let table = table();
        let prefix = table.get("=").expect("calculator prefix");
        let hint = hint_result(prefix);

        let mut results = vec![
            SpotlightResult {
                score: 10_000,
                ..SpotlightResult::new("Loud", "", IconRef::fallback())
            },
            hint,
        ];
        sort_results(&mut results);

        assert_eq!(results[0].title, "Calculate");
    }

    fn window(handle: &str, app_id: &str, title: &str, workspace: &str) -> OpenWindow {
        OpenWindow {
            handle: handle.to_string(),
            title: title.to_string(),
            app_id: app_id.to_string(),
            workspace: workspace.to_string(),
            monitor: "DP-1".to_string(),
            xwayland: false,
            focused: false,
        }
    }

    #[test]
    fn a_window_row_switches_to_it_rather_than_launching() {
        let windows = [window("0xab", "discord", "general - Discord", "2")];

        let rows = window_results("discord", &windows, &AppIndex::default(), 10);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "general - Discord");
        assert_eq!(rows[0].primary, Activation::FocusWindow("0xab".to_string()));
    }

    /// The list is a switcher, so it must say where each window actually is.
    #[test]
    fn a_window_row_reports_where_the_window_lives() {
        let windows = [OpenWindow {
            xwayland: true,
            ..window("0xab", "steam", "Steam", "7")
        }];

        let rows = window_results("steam", &windows, &AppIndex::default(), 10);

        assert_eq!(rows[0].subtitle, "steam · Workspace 7 · DP-1 · XWayland");
    }

    #[test]
    fn a_window_row_previews_the_app_icon_with_its_details() {
        let windows = [window("0xab", "kitty", "~/src", "1")];

        let rows = window_results("kitty", &windows, &AppIndex::default(), 10);

        let preview = rows[0].preview.as_ref().expect("a window previews itself");
        assert_eq!(preview.kind, PreviewKind::Icon);
        assert_eq!(preview.content, "kitty");
        let caption = preview.caption.as_deref().expect("details");
        assert!(caption.contains("~/src"), "{caption}");
        assert!(caption.contains("Workspace 1 · DP-1"), "{caption}");
    }

    /// A window address is only valid while the window exists, so ranking has to
    /// remember the application instead.
    #[test]
    fn window_frecency_is_keyed_on_the_app_not_the_handle() {
        let windows = [window("0xab", "discord", "general - Discord", "2")];

        let rows = window_results("discord", &windows, &AppIndex::default(), 10);

        assert_eq!(rows[0].frecency_key.as_deref(), Some("window:discord"));
    }

    #[test]
    fn an_empty_window_query_keeps_the_compositors_own_ordering() {
        let windows = [
            window("0x1", "discord", "Discord", "2"),
            window("0x2", "kitty", "~", "1"),
            window("0x3", "steam", "Steam", "7"),
        ];

        let rows = window_results("", &windows, &AppIndex::default(), 10);

        let handles = rows
            .iter()
            .map(|row| match &row.primary {
                Activation::FocusWindow(handle) => handle.as_str(),
                other => panic!("expected a focus activation, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(handles, vec!["0x1", "0x2", "0x3"]);
    }

    #[test]
    fn windows_that_match_nothing_are_dropped() {
        let windows = [window("0x1", "discord", "Discord", "2")];

        assert!(window_results("zzzz", &windows, &AppIndex::default(), 10).is_empty());
    }

    /// The whole point of merging windows into plain search: typing an app's
    /// name must reach the copy that is already running before offering a second.
    #[test]
    fn a_running_window_outranks_the_launcher_entry_for_the_same_app() {
        let windows = [window("0xab", "discord", "Discord", "2")];

        let rows = default_results(
            "discord",
            &AppIndex::default(),
            &windows,
            &Frecency::default(),
            0,
            10,
        );

        assert!(!rows.is_empty());
        assert_eq!(
            rows[0].primary,
            Activation::FocusWindow("0xab".to_string()),
            "the running window must come first"
        );
    }

    /// The opening state exists to show the most-used applications; a dozen open
    /// windows would push every one of them out of a twelve-row list.
    #[test]
    fn the_opening_state_does_not_list_open_windows() {
        let windows = [window("0xab", "discord", "Discord", "2")];

        let rows = default_results(
            "",
            &AppIndex::default(),
            &windows,
            &Frecency::default(),
            0,
            10,
        );

        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.primary, Activation::FocusWindow(_))),
            "no window rows on an empty query"
        );
    }

    // -- vpn ---------------------------------------------------------------

    fn vpn_prefix() -> Prefix {
        prefixes::resolve_with_ai(&SpotlightConfig::default(), Some(vpn::Provider::Windscribe))
            .0
            .get("vpn")
            .expect("vpn prefix")
            .clone()
    }

    fn vpn_location(name: &str, region: &str, nickname: &str) -> vpn::Location {
        vpn::Location {
            target: nickname.to_string(),
            name: name.to_string(),
            region: region.to_string(),
            nickname: nickname.to_string(),
            speed: "10 Gbps".to_string(),
            disabled: false,
            best: false,
        }
    }

    fn vpn_state(connected: bool, at: Option<&str>) -> vpn::VpnState {
        vpn::VpnState {
            status: vpn::Status {
                connected,
                connecting: false,
                logged_in: true,
                location: at.map(str::to_string),
                details: "Connect state: whatever the client said".to_string(),
            },
            locations: vec![
                vpn_location("New York", "US East", "Big Apple"),
                vpn_location("Toronto", "Canada East", "Maple"),
            ],
        }
    }

    fn vpn_rows(arg: &str, state: &vpn::VpnState, limit: usize) -> Vec<SpotlightResult> {
        vpn_results(
            &vpn_prefix(),
            arg,
            vpn::Provider::Windscribe,
            state,
            &Frecency::default(),
            0,
            limit,
        )
    }

    /// The state of the connection is the question the prefix is usually opened
    /// to answer, so on an empty query it leads whatever else is listed.
    #[test]
    fn the_action_row_leads_an_unfiltered_list() {
        let rows = vpn_rows("", &vpn_state(false, None), 10);

        assert_eq!(rows[0].title, "Connect");
        assert_eq!(rows[0].subtitle, "Disconnected · best location");
        assert_eq!(
            rows[0].primary,
            Activation::RunShell("windscribe-cli connect 'best'".to_string())
        );
        // Connecting prints as it goes, so the terminal is a way to watch it.
        assert_eq!(
            rows[0].secondary,
            Some(Activation::RunInTerminal(
                "windscribe-cli connect 'best'".to_string()
            ))
        );
        assert_eq!(rows.len(), 3, "and then every location");
    }

    #[test]
    fn a_connected_client_is_offered_the_way_out() {
        let rows = vpn_rows("", &vpn_state(true, Some("Big Apple")), 10);

        assert_eq!(rows[0].title, "Disconnect");
        assert_eq!(rows[0].subtitle, "Connected · Big Apple");
        assert_eq!(
            rows[0].primary,
            Activation::RunShell("windscribe-cli disconnect".to_string())
        );
    }

    #[test]
    fn a_location_row_connects_to_that_location() {
        let rows = vpn_rows("toronto", &vpn_state(false, None), 10);

        assert_eq!(rows[0].title, "Toronto");
        assert_eq!(rows[0].subtitle, "Canada East · Maple · 10 Gbps");
        assert_eq!(
            rows[0].primary,
            Activation::RunShell("windscribe-cli connect 'Maple'".to_string())
        );
        assert_eq!(rows[0].completion.as_deref(), Some("vpn Toronto"));
    }

    /// A city can be reached by its region or by the server's own name, which is
    /// what the user sees in the client's own list.
    #[test]
    fn locations_match_on_region_and_nickname_too() {
        let state = vpn_state(false, None);

        for query in ["big apple", "us east", "new york"] {
            let rows = vpn_rows(query, &state, 10);
            assert_eq!(rows[0].title, "New York", "{query}");
        }
    }

    /// Once the user types, the action row competes like everything else —
    /// searching for a city must not keep an unrelated Disconnect at the top.
    #[test]
    fn a_query_does_not_pin_the_action_row() {
        let rows = vpn_rows("toronto", &vpn_state(true, None), 10);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Toronto");
    }

    /// But it is still reachable by name, which is how a connection is ended
    /// without scrolling past two hundred locations.
    #[test]
    fn the_action_row_can_be_searched_for() {
        let rows = vpn_rows("disc", &vpn_state(true, None), 10);

        assert_eq!(rows[0].title, "Disconnect");
    }

    #[test]
    fn the_row_the_client_is_connected_to_says_so() {
        let rows = vpn_rows("new york", &vpn_state(true, Some("Big Apple")), 10);

        assert_eq!(
            rows[0].subtitle,
            "US East · Big Apple · 10 Gbps · connected"
        );
    }

    #[test]
    fn the_action_row_previews_the_clients_own_status_text() {
        let rows = vpn_rows("", &vpn_state(false, None), 10);
        let preview = rows[0].preview.as_ref().expect("a preview");

        assert_eq!(preview.kind, PreviewKind::Icon);
        assert!(
            preview
                .caption
                .as_deref()
                .expect("a caption")
                .contains("whatever the client said"),
            "{preview:?}"
        );
    }

    #[test]
    fn the_limit_holds_with_the_action_row_included() {
        let mut state = vpn_state(false, None);
        state.locations = (0..50)
            .map(|index| vpn_location(&format!("City {index}"), "Region", "Nickname"))
            .collect();

        assert_eq!(vpn_rows("", &state, 5).len(), 5);
        assert_eq!(vpn_rows("city", &state, 5).len(), 5);
    }

    fn ssh_prefix() -> Prefix {
        table().get("ssh").expect("ssh prefix").clone()
    }

    fn ssh_host(alias: &str, options: &[(&str, &str)]) -> SshHost {
        SshHost {
            alias: alias.to_string(),
            options: options
                .iter()
                .map(|(keyword, value)| ((*keyword).to_string(), (*value).to_string()))
                .collect(),
            source: PathBuf::from("/home/user/.ssh/config"),
        }
    }

    fn ssh_rows(arg: &str, hosts: &[SshHost], limit: usize) -> Vec<SpotlightResult> {
        ssh_results(&ssh_prefix(), arg, hosts, &Frecency::default(), 0, limit)
    }

    #[test]
    fn an_ssh_row_connects_in_a_terminal_and_copies_on_the_secondary() {
        let hosts = [ssh_host(
            "build-box",
            &[("HostName", "build.example.com"), ("User", "lucas")],
        )];

        let rows = ssh_rows("", &hosts, 10);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "build-box");
        assert_eq!(rows[0].subtitle, "lucas@build.example.com");
        assert_eq!(
            rows[0].primary,
            Activation::RunInTerminal("ssh 'build-box'".to_string())
        );
        assert_eq!(
            rows[0].secondary,
            Some(Activation::CopyText("ssh 'build-box'".to_string()))
        );
        assert_eq!(rows[0].completion.as_deref(), Some("ssh build-box"));
    }

    /// The alias is what ssh is given, not the resolved hostname: only ssh can
    /// apply the rest of the block — the identity file, the jump host, the
    /// forwards — and it cannot do that for a bare address.
    #[test]
    fn a_configured_host_is_dialled_by_its_alias() {
        let hosts = [ssh_host("db", &[("HostName", "10.0.0.5")])];

        let rows = ssh_rows("db", &hosts, 10);

        assert_eq!(
            rows[0].primary,
            Activation::RunInTerminal("ssh 'db'".to_string())
        );
    }

    #[test]
    fn an_ssh_row_previews_the_whole_block() {
        let hosts = [ssh_host(
            "build-box",
            &[("HostName", "build.example.com"), ("ForwardAgent", "yes")],
        )];

        let rows = ssh_rows("build-box", &hosts, 10);

        let preview = rows[0].preview.as_ref().expect("a host previews itself");
        assert_eq!(preview.kind, PreviewKind::Icon);
        let caption = preview.caption.as_deref().expect("details");
        assert!(caption.contains("ForwardAgent yes"), "{caption}");
        assert!(caption.contains(".ssh/config"), "{caption}");
    }

    #[test]
    fn hosts_are_matched_on_their_hostname_and_user_too() {
        let hosts = [
            ssh_host("alpha", &[("HostName", "build.example.com")]),
            ssh_host("beta", &[("User", "deploy")]),
            ssh_host("gamma", &[("HostName", "unrelated.internal")]),
        ];

        let titles = |arg: &str| {
            ssh_rows(arg, &hosts, 10)
                .into_iter()
                .map(|row| row.title)
                .collect::<Vec<_>>()
        };

        assert_eq!(titles("example"), vec!["example", "alpha"]);
        assert_eq!(titles("deploy"), vec!["deploy", "beta"]);
    }

    /// The top row always says what Enter will do with the text as typed, so an
    /// unlisted machine is one line away.
    #[test]
    fn the_ad_hoc_row_leads_the_list() {
        let hosts = [ssh_host("build-box", &[])];

        let rows = ssh_rows("build", &hosts, 10);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "build");
        assert_eq!(
            rows[0].primary,
            Activation::RunInTerminal("ssh 'build'".to_string())
        );
        assert!(rows[0].frecency_key.is_none(), "a typo must not be learned");
        assert_eq!(rows[1].title, "build-box");
    }

    #[test]
    fn an_ad_hoc_destination_may_carry_a_user() {
        let rows = ssh_rows("deploy@10.0.0.5", &[], 10);

        assert_eq!(
            rows[0].primary,
            Activation::RunInTerminal("ssh 'deploy@10.0.0.5'".to_string())
        );
    }

    /// Naming a configured host exactly already connects there, so a second row
    /// doing the same thing is noise at the position that matters most.
    #[test]
    fn the_ad_hoc_row_steps_aside_for_a_configured_host() {
        let hosts = [ssh_host("build-box", &[("User", "lucas")])];

        let rows = ssh_rows("build-box", &hosts, 10);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "build-box");
    }

    /// ssh reads a leading dash as an option, so text of that shape is not a
    /// destination and must not be offered as one.
    #[test]
    fn text_that_is_not_a_destination_gets_no_ad_hoc_row() {
        assert!(ssh_rows("-oProxyCommand=id", &[], 10).is_empty());
        assert!(ssh_rows("two words", &[], 10).is_empty());
    }

    /// The ad-hoc row is inserted after the list is cut, so it must not push the
    /// list over the limit the config asked for.
    #[test]
    fn the_ad_hoc_row_counts_against_the_result_limit() {
        let hosts = (0..10)
            .map(|index| ssh_host(&format!("host-{index}"), &[]))
            .collect::<Vec<_>>();

        let rows = ssh_rows("host", &hosts, 4);

        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].title, "host");
    }

    #[test]
    fn user_command_prefixes_expand_their_template() {
        let prefix = Prefix {
            key: "g".to_string(),
            label: "Google".to_string(),
            description: "Search".to_string(),
            icon: "web-browser-symbolic".to_string(),
            kind: PrefixKind::Command {
                command: "xdg-open 'https://example.com/?q={query_url}'".to_string(),
                terminal: false,
            },
        };

        let results = prefixed_results(&prefix, "a b", &table(), 10);

        assert_eq!(
            results[0].primary,
            Activation::RunShell("xdg-open 'https://example.com/?q=a+b'".to_string())
        );
    }
}
