//! SSH hosts: reading the local `~/.ssh/config` and turning its `Host` blocks
//! into rows that open a terminal on the connection.
//!
//! The parse follows `ssh_config(5)` closely enough to be trustworthy — keywords
//! are case-insensitive, an `=` may stand in for the space, `Include` pulls in
//! another file at the point it appears, and the first value for a keyword wins
//! — while deliberately stopping short of *resolving* a configuration. Nothing
//! here decides what ssh will actually do: `Host *` defaults are not folded into
//! every entry and `Match` blocks are not evaluated, because both depend on the
//! connection being made. The rows show what the file declares for a host, and
//! the connection is handed to `ssh` itself, which does the resolving properly.
//!
//! Unlike the window list this is a local file read of a few kilobytes, so it
//! runs inline rather than on a worker thread: there is no process to hang, and
//! a thread would only add a frame of latency to something that is already done.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::spotlight::paths;

/// The config file read when no other path is given.
pub const DEFAULT_CONFIG: &str = "~/.ssh/config";

/// Cap on a single config file. A real one is a few kilobytes; this exists so a
/// stray multi-gigabyte file cannot be read into memory.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// How deep `Include` may nest. Matches OpenSSH's own limit.
const MAX_INCLUDE_DEPTH: usize = 16;
/// Total files one load may touch, so a directory of includes that reference
/// each other cannot fan out without bound. Depth alone does not bound this: a
/// glob can pull in many files at the same depth.
const MAX_FILES: usize = 64;
/// Cap on the hosts collected. Far past any hand-written config.
const MAX_HOSTS: usize = 2000;

/// Longest destination accepted for an ad-hoc connection. A hostname is capped
/// at 253 characters, and the user part adds a little.
const MAX_DESTINATION_CHARS: usize = 320;

/// One `Host` entry, as the file declares it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshHost {
    /// The pattern the block was declared under, and what gets passed to `ssh`.
    pub alias: String,
    /// Every keyword declared for this host, in file order.
    ///
    /// Kept as a list rather than a map for two reasons: the preview shows the
    /// block as written, and a keyword may legitimately repeat — `LocalForward`
    /// and `IdentityFile` both accumulate.
    pub options: Vec<(String, String)>,
    /// Which file declared it, so the preview can say where to go and edit.
    pub source: PathBuf,
}

impl SshHost {
    fn new(alias: String, source: PathBuf) -> Self {
        Self {
            alias,
            options: Vec::new(),
            source,
        }
    }

    /// The first value for `keyword`, case-insensitively.
    ///
    /// First rather than last on purpose: ssh takes the earliest value it sees
    /// for a keyword and ignores the rest.
    pub fn option(&self, keyword: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(keyword))
            .map(|(_, value)| value.as_str())
    }

    /// The host ssh will actually dial. Without a `HostName` the alias is the
    /// hostname, which is how a bare `Host build-box` entry works.
    pub fn hostname(&self) -> &str {
        self.option("HostName").unwrap_or(&self.alias)
    }

    pub fn user(&self) -> Option<&str> {
        self.option("User")
    }

    pub fn port(&self) -> Option<&str> {
        self.option("Port")
    }

    /// `user@host:port`, with the parts the file did not set left out.
    ///
    /// For display only — `ssh` is handed the alias, so it applies the whole
    /// block rather than just the three fields shown here.
    pub fn destination(&self) -> String {
        let mut text = String::new();
        if let Some(user) = self.user() {
            text.push_str(user);
            text.push('@');
        }
        text.push_str(self.hostname());
        if let Some(port) = self.port() {
            text.push(':');
            text.push_str(port);
        }
        text
    }

    /// The one-line subtitle: where this goes, and how it gets there.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.destination()];
        if let Some(jump) = self.option("ProxyJump") {
            parts.push(format!("via {jump}"));
        }
        parts.join(" · ")
    }

    /// The preview body: every keyword the block declares, then its file.
    ///
    /// The whole block rather than a chosen few. This is the panel the user
    /// opens to answer "which key does this use, and is the agent forwarded" —
    /// summarising it away would defeat the point.
    pub fn details(&self) -> String {
        let mut lines = vec![self.destination(), String::new()];

        for (keyword, value) in &self.options {
            lines.push(format!("{keyword} {value}"));
        }
        if self.options.is_empty() {
            lines.push("No options declared".to_string());
        }

        lines.push(String::new());
        lines.push(paths::display_path(&self.source));
        lines.join("\n")
    }
}

/// Reads the user's SSH config.
///
/// A missing file is not an error: plenty of people connect by hostname without
/// ever writing one, and the prefix still works for them through the ad-hoc row.
pub fn load() -> Result<Vec<SshHost>, String> {
    load_from(&paths::expand_tilde(DEFAULT_CONFIG))
}

/// Reads `path` and everything it includes.
pub fn load_from(path: &Path) -> Result<Vec<SshHost>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    // Relative `Include` paths resolve against the config's own directory, which
    // is `~/.ssh` for the real one.
    let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut loader = Loader::new(base);
    loader.read(path, 0)?;
    Ok(loader.hosts)
}

/// Accumulates hosts across a config file and its includes.
struct Loader {
    hosts: Vec<SshHost>,
    /// Alias to index in `hosts`, so a host declared twice merges into one row.
    index: HashMap<String, usize>,
    /// The aliases the options being read belong to. Empty inside a `Match`
    /// block, or before the first `Host` line.
    current: Vec<String>,
    base: PathBuf,
    files: usize,
}

impl Loader {
    fn new(base: PathBuf) -> Self {
        Self {
            hosts: Vec::new(),
            index: HashMap::new(),
            current: Vec::new(),
            base,
            files: 0,
        }
    }

    /// Reads one file into the accumulator.
    ///
    /// `current` deliberately survives across an include and is left as the
    /// included file leaves it: OpenSSH splices the file in at the point of the
    /// directive, so an include inside a `Host` block really does extend that
    /// block, and one that ends with a `Host` line really does open a new one.
    fn read(&mut self, path: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_INCLUDE_DEPTH {
            tracing::warn!(path = %path.display(), "ssh config includes nest too deeply");
            return Ok(());
        }
        if self.files >= MAX_FILES {
            tracing::warn!("ssh config pulls in too many files; stopping");
            return Ok(());
        }
        self.files += 1;

        // A file that is *there* but unreadable is worth reporting: a config the
        // user knows they wrote should not silently produce an empty list.
        let size = fs::metadata(path)
            .map_err(|error| format!("cannot read {}: {error}", paths::display_path(path)))?
            .len();
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "{} is too large to be an ssh config",
                paths::display_path(path)
            ));
        }
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", paths::display_path(path)))?;

        for line in text.lines() {
            let Some((keyword, rest)) = split_directive(line) else {
                continue;
            };

            if keyword.eq_ignore_ascii_case("Host") {
                self.open_block(rest, path);
            } else if keyword.eq_ignore_ascii_case("Match") {
                // Whether a `Match` block applies depends on the connection being
                // made, so its options belong to no listable host.
                self.current.clear();
            } else if keyword.eq_ignore_ascii_case("Include") {
                for included in self.resolve_includes(rest) {
                    self.read(&included, depth + 1)?;
                }
            } else {
                self.push_option(keyword, rest);
            }
        }

        Ok(())
    }

    /// Starts a `Host` block, creating a row per concrete pattern on the line.
    fn open_block(&mut self, rest: &str, source: &Path) {
        self.current.clear();

        for pattern in split_args(rest) {
            // A wildcard or negated pattern is a rule about other hosts, not a
            // host anyone can connect to — `Host *` is the classic example. Its
            // options are still skipped rather than applied to every entry,
            // because which of them survive is ssh's decision, not ours.
            if !is_connectable_pattern(&pattern) {
                continue;
            }
            if !self.index.contains_key(&pattern) && self.hosts.len() >= MAX_HOSTS {
                tracing::warn!("ssh config declares too many hosts; ignoring the rest");
                continue;
            }

            if !self.index.contains_key(&pattern) {
                self.hosts
                    .push(SshHost::new(pattern.clone(), source.to_path_buf()));
                self.index.insert(pattern.clone(), self.hosts.len() - 1);
            }
            // Guards against a pattern repeated on one `Host` line, which would
            // otherwise record every option for it twice.
            if !self.current.contains(&pattern) {
                self.current.push(pattern);
            }
        }
    }

    fn push_option(&mut self, keyword: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        for alias in &self.current {
            if let Some(position) = self.index.get(alias) {
                self.hosts[*position]
                    .options
                    .push((keyword.to_string(), value.to_string()));
            }
        }
    }

    /// Expands the paths on an `Include` line.
    fn resolve_includes(&self, rest: &str) -> Vec<PathBuf> {
        split_args(rest)
            .into_iter()
            .flat_map(|pattern| expand_include(&pattern, &self.base))
            .collect()
    }
}

/// Splits a config line into its keyword and the rest of the line.
///
/// Returns `None` for blanks and comments. Only a whole-line `#` is a comment:
/// ssh has no trailing comments, and treating one as such would truncate any
/// value that legitimately contains a `#`.
fn split_directive(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let end = line
        .find(|ch: char| ch.is_whitespace() || ch == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    if keyword.is_empty() {
        return None;
    }

    // The separator is whitespace, an `=`, or both in any order.
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();

    Some((keyword, rest))
}

/// Splits an argument list on whitespace, honouring double quotes.
///
/// Used for the lines that really are lists — `Host` patterns and `Include`
/// paths. Every other keyword keeps its value as written, since options such as
/// `LocalForward` and `SetEnv` take several arguments that only mean anything
/// together.
fn split_args(rest: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;

    for ch in rest.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            ch => {
                current.push(ch);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }

    args
}

/// Whether a `Host` pattern names something a user can connect to.
fn is_connectable_pattern(pattern: &str) -> bool {
    !pattern.is_empty() && !pattern.starts_with('!') && !pattern.contains(['*', '?'])
}

/// Turns one `Include` argument into the files it names.
///
/// Wildcards are supported in the final component, which is the form real
/// configs use (`Include config.d/*`). A wildcard in a directory component is
/// skipped rather than half-handled — walking a tree to find matching parents is
/// a different job from reading a config, and no config in the wild needs it.
fn expand_include(pattern: &str, base: &Path) -> Vec<PathBuf> {
    let path = resolve_include_path(pattern, base);

    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !has_wildcard(name) {
        // An include that names nothing is skipped rather than failing the load:
        // a drop-in directory that has not been created yet is a normal state,
        // not a broken config.
        return match path.is_file() {
            true => vec![path],
            false => Vec::new(),
        };
    }

    let parent = path.parent().unwrap_or(base);
    if parent
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .any(has_wildcard)
    {
        tracing::debug!(pattern, "ignoring an ssh Include with a wildcard directory");
        return Vec::new();
    }

    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };

    let mut matched = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter(|entry| match entry.file_name().to_str() {
            Some(candidate) => glob_match(name, candidate),
            None => false,
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    // `read_dir` yields in whatever order the filesystem stores, and the order
    // of includes decides which value for a keyword wins. Sorting makes a load
    // reproducible.
    matched.sort();
    matched
}

/// Resolves an `Include` argument to a real path.
///
/// Deliberately not `paths::expand_tilde`, which resolves anything relative
/// against the home directory. ssh resolves a relative include against the
/// directory holding the config that named it — for the user's own config those
/// two happen to differ by exactly `.ssh`, so the difference is not academic:
/// `Include config` would otherwise read `~/config`.
fn resolve_include_path(pattern: &str, base: &Path) -> PathBuf {
    if pattern == "~" || pattern.starts_with("~/") {
        return paths::expand_tilde(pattern);
    }

    let path = PathBuf::from(pattern);
    match path.is_absolute() {
        true => path,
        false => base.join(path),
    }
}

fn has_wildcard(text: &str) -> bool {
    text.contains(['*', '?'])
}

/// Matches a filename against a `*`/`?` glob.
///
/// Iterative with a single backtrack point, so a pathological pattern such as
/// `*a*a*a*` cannot take exponential time.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();

    let (mut p, mut n) = (0, 0);
    let mut star: Option<usize> = None;
    let mut resume = 0;

    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                p += 1;
                resume = n;
            }
            Some('?') => {
                p += 1;
                n += 1;
            }
            Some(ch) if *ch == name[n] => {
                p += 1;
                n += 1;
            }
            // The characters disagree, so the last `*` has to swallow one more.
            _ => match star {
                Some(position) => {
                    p = position + 1;
                    resume += 1;
                    n = resume;
                }
                None => return false,
            },
        }
    }

    while pattern.get(p) == Some(&'*') {
        p += 1;
    }
    p == pattern.len()
}

/// The shell line that opens a connection.
pub fn connect_command(destination: &str) -> String {
    format!("ssh {}", crate::custom_actions::shell_quote(destination))
}

/// Whether typed text can be handed to `ssh` as a destination.
///
/// This is a security boundary, not tidiness. The text becomes ssh's first
/// argument, and ssh reads arguments that start with `-` as *options* — a value
/// like `-oProxyCommand=…` would run an arbitrary command. Shell quoting does
/// not help with that, since the shell is not the thing being tricked. So the
/// shape is checked and anything else is refused: no leading dash, no
/// whitespace, and only the characters a destination is actually made of.
pub fn is_plausible_destination(text: &str) -> bool {
    if text.is_empty() || text.chars().count() > MAX_DESTINATION_CHARS {
        return false;
    }
    if text.starts_with('-') {
        return false;
    }

    let mut parts = text.split('@');
    let (user, host) = match (parts.next(), parts.next(), parts.next()) {
        // More than one `@` is not a destination ssh would accept.
        (_, _, Some(_)) => return false,
        (Some(host), None, _) => (None, host),
        (Some(user), Some(host), _) => (Some(user), host),
        _ => return false,
    };

    if host.is_empty() || user.is_some_and(str::is_empty) {
        return false;
    }

    // `:` for an `ssh://` style port, `[]` for a literal IPv6 address, `%` for a
    // link-local scope id.
    text.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ".-_@:[]%".contains(ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# Defaults for everything, and not a host anyone connects to.
Host *
    ServerAliveInterval 60

Host build-box
    HostName build.example.com
    User lucas
    Port 2222
    IdentityFile ~/.ssh/id_ed25519

Host db db-primary
    HostName=10.0.0.5
    ProxyJump build-box

Host web-*
    User deploy

Match host bastion
    ForwardAgent yes

Host bare
"#;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the include directory");
        }
        fs::write(&path, text).expect("write the config");
        path
    }

    fn load_text(text: &str) -> Vec<SshHost> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config", text);
        load_from(&path).expect("the config parses")
    }

    fn host<'a>(hosts: &'a [SshHost], alias: &str) -> &'a SshHost {
        hosts
            .iter()
            .find(|host| host.alias == alias)
            .unwrap_or_else(|| panic!("no host {alias} in {hosts:?}"))
    }

    #[test]
    fn every_concrete_host_becomes_an_entry_in_file_order() {
        let hosts = load_text(CONFIG);

        let aliases = hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect::<Vec<_>>();
        assert_eq!(aliases, vec!["build-box", "db", "db-primary", "bare"]);
    }

    /// `Host *` and `Host web-*` are rules about other hosts; offering them as
    /// rows would mean offering a connection to a pattern.
    #[test]
    fn wildcard_and_negated_patterns_are_not_listed() {
        assert!(is_connectable_pattern("build-box"));

        assert!(!is_connectable_pattern("*"));
        assert!(!is_connectable_pattern("web-*"));
        assert!(!is_connectable_pattern("db?"));
        assert!(!is_connectable_pattern("!secret"));
        assert!(!is_connectable_pattern(""));
    }

    #[test]
    fn a_block_carries_its_options_and_the_file_it_came_from() {
        let hosts = load_text(CONFIG);
        let build = host(&hosts, "build-box");

        assert_eq!(build.hostname(), "build.example.com");
        assert_eq!(build.user(), Some("lucas"));
        assert_eq!(build.port(), Some("2222"));
        assert_eq!(build.option("IdentityFile"), Some("~/.ssh/id_ed25519"));
        assert_eq!(build.options.len(), 4);
        assert!(build.source.ends_with("config"));
    }

    /// The defaults in a `Host *` block depend on what ssh decides at connect
    /// time, so they are not silently attributed to every entry.
    #[test]
    fn wildcard_block_options_do_not_leak_into_real_hosts() {
        let hosts = load_text(CONFIG);

        assert_eq!(
            host(&hosts, "build-box").option("ServerAliveInterval"),
            None
        );
        assert_eq!(host(&hosts, "db").option("User"), None);
    }

    #[test]
    fn one_block_can_declare_several_hosts() {
        let hosts = load_text(CONFIG);

        for alias in ["db", "db-primary"] {
            let entry = host(&hosts, alias);
            assert_eq!(entry.hostname(), "10.0.0.5", "{alias}");
            assert_eq!(entry.option("ProxyJump"), Some("build-box"), "{alias}");
        }
    }

    /// A `Match` block applies to connections rather than to a named host, so
    /// its options must not land on whatever host happened to precede it.
    #[test]
    fn match_blocks_end_the_current_host() {
        let hosts = load_text(CONFIG);

        assert!(!hosts.iter().any(|host| host.alias == "bastion"));
        assert_eq!(host(&hosts, "db-primary").option("ForwardAgent"), None);
        assert_eq!(host(&hosts, "bare").options.len(), 0);
    }

    /// Without a `HostName`, the alias *is* the hostname — a bare `Host` line is
    /// a perfectly ordinary way to write a config.
    #[test]
    fn a_host_without_a_hostname_dials_its_alias() {
        let hosts = load_text(CONFIG);
        let bare = host(&hosts, "bare");

        assert_eq!(bare.hostname(), "bare");
        assert_eq!(bare.destination(), "bare");
    }

    #[test]
    fn keywords_are_case_insensitive_and_may_use_an_equals_separator() {
        let hosts = load_text("Host box\n  hostname=box.example.com\n  USER lucas\n");
        let box_host = host(&hosts, "box");

        assert_eq!(box_host.hostname(), "box.example.com");
        assert_eq!(box_host.user(), Some("lucas"));
    }

    /// ssh takes the first value it sees for a keyword and ignores the rest, so
    /// a host declared twice must not report the later value.
    #[test]
    fn the_first_value_for_a_keyword_wins() {
        let hosts = load_text("Host box\n  User first\n\nHost box\n  User second\n");

        assert_eq!(hosts.len(), 1, "the two blocks describe one host");
        assert_eq!(host(&hosts, "box").user(), Some("first"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let hosts = load_text("# Host commented\n\n   # indented\nHost real\n");

        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real");
    }

    /// ssh has no trailing comments, so a `#` inside a value is part of it.
    #[test]
    fn a_hash_inside_a_value_is_kept() {
        let hosts = load_text("Host box\n  ProxyCommand nc -X 5 %h #1 %p\n");

        assert_eq!(
            host(&hosts, "box").option("ProxyCommand"),
            Some("nc -X 5 %h #1 %p")
        );
    }

    #[test]
    fn quoted_patterns_stay_one_argument() {
        assert_eq!(split_args(r#"a "b c" d"#), vec!["a", "b c", "d"]);
        assert_eq!(split_args("  spaced   out  "), vec!["spaced", "out"]);
        assert_eq!(split_args(""), Vec::<String>::new());
        // An empty quoted string is still an argument, not nothing.
        assert_eq!(split_args(r#""""#), vec![""]);
    }

    /// Also pins where a relative include resolves: against the directory of the
    /// config that named it, not the home directory. For the real config those
    /// differ by exactly `.ssh`, so getting it wrong means `Include config`
    /// quietly reads `~/config`.
    #[test]
    fn an_include_is_spliced_in_where_it_appears() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "extra",
            "Host included\n  HostName in.example.com\n",
        );
        let path = write(
            dir.path(),
            "config",
            "Include extra\n\nHost after\n  User lucas\n",
        );

        let hosts = load_from(&path).expect("the config parses");

        let aliases = hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect::<Vec<_>>();
        assert_eq!(aliases, vec!["included", "after"]);
        assert_eq!(host(&hosts, "included").hostname(), "in.example.com");
    }

    /// OpenSSH splices an included file in at the point of the directive, so an
    /// `Include` inside a block really does extend that block.
    #[test]
    fn an_include_inside_a_block_extends_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "extra", "  User lucas\n");
        let path = write(
            dir.path(),
            "config",
            "Host box\n  Include extra\n  Port 22\n",
        );

        let hosts = load_from(&path).expect("the config parses");

        assert_eq!(host(&hosts, "box").user(), Some("lucas"));
        assert_eq!(host(&hosts, "box").port(), Some("22"));
    }

    #[test]
    fn a_glob_include_pulls_in_every_match_in_a_stable_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "conf.d/20-b", "Host beta\n");
        write(dir.path(), "conf.d/10-a", "Host alpha\n");
        write(dir.path(), "conf.d/nope.bak", "Host skipped\n");
        let path = write(dir.path(), "config", "Include conf.d/*-*\n");

        let hosts = load_from(&path).expect("the config parses");

        let aliases = hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect::<Vec<_>>();
        assert_eq!(aliases, vec!["alpha", "beta"]);
    }

    #[test]
    fn a_missing_include_is_skipped_rather_than_failing_the_load() {
        let hosts = load_text("Include nowhere/at/all\nHost box\n");

        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "box");
    }

    /// A config that includes itself would otherwise recurse until the stack ran
    /// out — and it is an easy mistake to make with a glob.
    #[test]
    fn a_self_including_config_terminates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config", "Include config\nHost box\n");

        let hosts = load_from(&path).expect("the config parses");

        assert_eq!(hosts.len(), 1, "the host is recorded once per file read");
    }

    #[test]
    fn a_missing_config_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        let hosts = load_from(&dir.path().join("nothing-here")).expect("a missing file is fine");

        assert!(hosts.is_empty());
    }

    /// A file that exists but cannot be read is worth saying out loud: silently
    /// showing nothing would look like an empty config.
    #[test]
    fn an_oversized_config_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config", &"#\n".repeat(600_000));

        let error = load_from(&path).expect_err("must refuse");

        assert!(error.contains("too large"), "{error}");
    }

    #[test]
    fn globs_match_the_way_a_shell_would() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*.conf", "work.conf"));
        assert!(glob_match("conf-??", "conf-01"));
        assert!(glob_match("a*b*c", "axxbyyc"));

        assert!(!glob_match("*.conf", "work.conf.bak"));
        assert!(!glob_match("conf-??", "conf-012"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn the_summary_names_the_destination_and_the_jump() {
        let hosts = load_text(CONFIG);

        assert_eq!(
            host(&hosts, "build-box").summary(),
            "lucas@build.example.com:2222"
        );
        assert_eq!(host(&hosts, "db").summary(), "10.0.0.5 · via build-box");
    }

    /// The preview exists to answer questions about the entry, so it has to show
    /// the block as written rather than a chosen few fields.
    #[test]
    fn the_details_list_every_declared_option_and_the_source() {
        let hosts = load_text(CONFIG);
        let details = host(&hosts, "build-box").details();

        assert!(
            details.starts_with("lucas@build.example.com:2222"),
            "{details}"
        );
        for expected in [
            "HostName build.example.com",
            "User lucas",
            "Port 2222",
            "IdentityFile ~/.ssh/id_ed25519",
        ] {
            assert!(
                details.contains(expected),
                "{expected} missing from {details}"
            );
        }
        assert!(details.contains("config"), "{details}");
    }

    #[test]
    fn a_host_with_no_options_still_previews() {
        let hosts = load_text(CONFIG);

        assert!(
            host(&hosts, "bare")
                .details()
                .contains("No options declared")
        );
    }

    #[test]
    fn the_connect_command_quotes_its_destination() {
        assert_eq!(connect_command("build-box"), "ssh 'build-box'");
        assert_eq!(connect_command("a'; rm -rf ~"), r#"ssh 'a'"'"'; rm -rf ~'"#);
    }

    /// ssh reads a leading `-` as an option, and `-oProxyCommand=…` runs an
    /// arbitrary command — quoting does not help, so the shape is refused.
    #[test]
    fn a_destination_that_could_pass_ssh_an_option_is_refused() {
        assert!(!is_plausible_destination("-oProxyCommand=id"));
        assert!(!is_plausible_destination("-L"));
        assert!(!is_plausible_destination("host -oProxyCommand=id"));
    }

    #[test]
    fn ordinary_destinations_are_accepted() {
        assert!(is_plausible_destination("build-box"));
        assert!(is_plausible_destination("lucas@build.example.com"));
        assert!(is_plausible_destination("10.0.0.5"));
        assert!(is_plausible_destination("[fe80::1%eth0]"));
        assert!(is_plausible_destination("host:2222"));
    }

    #[test]
    fn malformed_destinations_are_refused() {
        assert!(!is_plausible_destination(""));
        assert!(!is_plausible_destination("two words"));
        assert!(!is_plausible_destination("user@"));
        assert!(!is_plausible_destination("@host"));
        assert!(!is_plausible_destination("a@b@c"));
        assert!(!is_plausible_destination("host;reboot"));
        assert!(!is_plausible_destination("$(id)"));
        assert!(!is_plausible_destination(&"a".repeat(400)));
    }
}
