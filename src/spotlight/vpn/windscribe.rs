//! Windscribe: the command lines `windscribe-cli` answers to, and the parsing of
//! what it prints back.
//!
//! Both replies are human-readable text rather than a machine format — the
//! client has no JSON mode — so the parsing is written to survive the client
//! changing its wording. Status is read as `Key: value` pairs and only a handful
//! of keys are looked for; a location line is split from its ends inwards. A
//! field that stops arriving costs that field and nothing else, and the raw
//! status text is kept verbatim for the preview, so anything this file fails to
//! interpret is still in front of the user.

use crate::spotlight::vpn::{Location, Status};

pub const PROGRAM: &str = "windscribe-cli";
pub const STATUS_ARGS: &[&str] = &["status"];
pub const LOCATIONS_ARGS: &[&str] = &["locations"];

/// The keyword that hands the choice of server back to the client.
pub const BEST_TARGET: &str = "best";
/// How the client spells the automatic entry in its location list.
const BEST_PREFIX: &str = "Best Location";
/// What the client appends to a location it will not currently connect to.
const DISABLED_MARKER: &str = "(Disabled)";

/// `windscribe-cli connect <target>`.
///
/// The target is shell-quoted, which matters for more than tidiness: a location
/// is two or three words (`connect 'Big Apple'`), so an unquoted one would reach
/// the client as separate arguments and be read as a protocol.
pub fn connect_line(target: &str) -> String {
    format!(
        "{PROGRAM} connect {}",
        crate::custom_actions::shell_quote(target)
    )
}

pub fn disconnect_line() -> String {
    format!("{PROGRAM} disconnect")
}

/// Reads `windscribe-cli status`.
///
/// ```text
/// Internet connectivity: available
/// Login state: Logged in
/// Firewall state: Off
/// Connect state: Connected
/// Data usage: 19.54 GB / Unlimited
/// ```
pub fn parse_status(output: &str) -> Status {
    let mut status = Status {
        details: output.trim().to_string(),
        ..Default::default()
    };

    for (key, value) in output.lines().filter_map(split_pair) {
        let key = key.to_ascii_lowercase();
        let lower = value.to_ascii_lowercase();

        if key.contains("login state") {
            // Matched positively rather than by ruling out "logged out": an
            // unfamiliar answer should not read as a working session.
            status.logged_in = lower.starts_with("logged in");
        } else if key.contains("connect state") {
            status.connecting = lower.starts_with("connecting");
            status.connected = lower.starts_with("connected");
            // Some builds name the location on this line, others on one of
            // their own. Whatever trails the state word is that name — but only
            // where there is a connection for it to describe: `Disconnected
            // (firewall on)` trails something that is not a location.
            if status.connected {
                status.location = trailing_location(value);
            }
        } else if status.location.is_none() && (key == "location" || key.contains("connected to")) {
            status.location = non_empty(value);
        }
    }

    status
}

/// The location a `Connect state:` line carries, if it carries one.
///
/// `Connected` on its own says nothing about where; `Connected to Big Apple` and
/// `Connected (Big Apple)` both name it. The separators are stripped rather than
/// matched exactly, so a form this file has not seen still yields the name
/// instead of a fragment of one.
fn trailing_location(value: &str) -> Option<String> {
    let rest = value
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest)
        .unwrap_or_default();

    let trimmed = rest
        .trim()
        .trim_start_matches("to ")
        .trim_matches(['(', ')', '-', ':', ' ']);

    non_empty(trimmed)
}

/// Reads `windscribe-cli locations`.
///
/// ```text
/// Best Location - Hyggenhagen (10 Gbps)
/// US East - New York - Big Apple (10 Gbps)
/// US East - Chicago - Bulls (Disabled) (10 Gbps)
/// ```
pub fn parse_locations(output: &str) -> Vec<Location> {
    output.lines().filter_map(parse_location).collect()
}

fn parse_location(line: &str) -> Option<Location> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Stripped from the right in the order the client appends them, so a
    // disabled location keeps its speed rather than losing the whole tail.
    let (line, speed) = split_speed(line);
    let disabled = line.ends_with(DISABLED_MARKER);
    let line = line
        .trim_end_matches(DISABLED_MARKER)
        .trim_end()
        .to_string();

    let parts = line
        .split(" - ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    let (region, name, nickname) = match parts.as_slice() {
        // `Region - City - Nickname`, the shape of every real location.
        [region, city, nickname] => (*region, *city, *nickname),
        // `Best Location - Hyggenhagen`: a choice, not a place, so the server's
        // own name is all there is to show.
        [_, nickname] => ("", *nickname, *nickname),
        // Not a shape the client prints today. Kept connectable rather than
        // dropped: a line this file does not recognise is still a location the
        // user can see in their own client.
        [single] => ("", *single, *single),
        _ => return None,
    };

    let best = line.starts_with(BEST_PREFIX);
    Some(Location {
        // The nickname is what the client's own `connect` documents as unique;
        // a city name can appear in more than one region.
        target: match best {
            true => BEST_TARGET.to_string(),
            false => nickname.to_string(),
        },
        name: name.to_string(),
        region: region.to_string(),
        nickname: nickname.to_string(),
        speed,
        disabled,
        best,
    })
}

/// Splits a trailing `(10 Gbps)` off a location line.
fn split_speed(line: &str) -> (&str, String) {
    let Some(open) = line.rfind('(') else {
        return (line, String::new());
    };
    if !line.ends_with(')') {
        return (line, String::new());
    }

    let inner = line[open + 1..line.len() - 1].trim();
    // Only a speed, so `(Disabled)` on a location with no speed of its own is
    // left on the line for the marker check to find.
    if !inner
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
    {
        return (line, String::new());
    }

    (line[..open].trim_end(), inner.to_string())
}

/// Splits a `Key: value` line, ignoring anything else.
fn split_pair(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    Some((key.trim(), value.trim()))
}

fn non_empty(value: &str) -> Option<String> {
    match value.trim() {
        "" => None,
        value => Some(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from `windscribe-cli status` on a logged-in, idle client.
    const IDLE: &str = "\
Internet connectivity: available
Login state: Logged in
Firewall state: Off
Connect state: Disconnected
Data usage: 19.54 GB / Unlimited
Update available: 2.23.11";

    #[test]
    fn an_idle_client_reads_as_logged_in_and_disconnected() {
        let status = parse_status(IDLE);

        assert!(status.logged_in);
        assert!(!status.connected);
        assert!(!status.connecting);
        assert_eq!(status.location, None);
        assert_eq!(status.summary(), "Disconnected");
    }

    /// The preview shows the client's own words, so nothing this parser fails to
    /// interpret is lost to the user.
    #[test]
    fn the_reply_is_kept_verbatim_for_the_preview() {
        assert_eq!(parse_status(IDLE).details, IDLE);
        assert!(parse_status(IDLE).details.contains("Data usage"));
    }

    #[test]
    fn a_connected_client_reports_where_it_landed() {
        for line in [
            "Connect state: Connected to Big Apple",
            "Connect state: Connected (Big Apple)",
            "Connect state: Connected - Big Apple",
        ] {
            let status = parse_status(&format!("Login state: Logged in\n{line}"));

            assert!(status.connected, "{line}");
            assert_eq!(status.location.as_deref(), Some("Big Apple"), "{line}");
            assert_eq!(status.summary(), "Connected · Big Apple", "{line}");
        }
    }

    #[test]
    fn a_bare_connected_state_names_no_location() {
        let status = parse_status("Login state: Logged in\nConnect state: Connected");

        assert!(status.connected);
        assert_eq!(status.location, None);
    }

    /// The location may arrive on a line of its own rather than beside the
    /// state, and either way it belongs in the same field.
    #[test]
    fn a_location_on_its_own_line_is_picked_up() {
        let status = parse_status(
            "Login state: Logged in\nConnect state: Connected\nConnected to: Toronto - Maple",
        );

        assert_eq!(status.location.as_deref(), Some("Toronto - Maple"));
    }

    #[test]
    fn connecting_is_neither_connected_nor_idle() {
        let status = parse_status("Login state: Logged in\nConnect state: Connecting…");

        assert!(status.connecting);
        assert!(!status.connected);
        assert_eq!(status.summary(), "Connecting…");
    }

    /// Anything but a positive "Logged in" is treated as logged out: an answer
    /// this parser does not recognise must not read as a working session.
    #[test]
    fn only_a_positive_login_state_counts_as_logged_in() {
        assert!(!parse_status("Login state: Logged out").logged_in);
        assert!(!parse_status("Login state: Logging in").logged_in);
        assert!(!parse_status("").logged_in);
        assert!(parse_status("Login state: Logged in").logged_in);
    }

    /// Trimmed from a live `windscribe-cli locations`.
    const LOCATIONS: &str = "\
Best Location - Hyggenhagen (10 Gbps)
US East - New York - Big Apple (10 Gbps)
US East - Chicago - Bulls (Disabled) (10 Gbps)
US East - Philadelphia - Fresh Prince
Fake Antarctica - Troll - Station (10 Gbps)";

    #[test]
    fn a_location_line_splits_into_region_city_and_nickname() {
        let locations = parse_locations(LOCATIONS);

        let new_york = &locations[1];
        assert_eq!(new_york.region, "US East");
        assert_eq!(new_york.name, "New York");
        assert_eq!(new_york.nickname, "Big Apple");
        assert_eq!(new_york.speed, "10 Gbps");
        assert!(!new_york.disabled);
        assert!(!new_york.best);
        // The nickname is the unique handle; a city can appear in two regions.
        assert_eq!(new_york.target, "Big Apple");
    }

    /// The automatic entry is a choice rather than a place, and it connects
    /// through the client's own keyword instead of the server it happens to name
    /// today.
    #[test]
    fn the_best_location_connects_by_keyword() {
        let best = &parse_locations(LOCATIONS)[0];

        assert!(best.best);
        assert_eq!(best.target, "best");
        assert_eq!(best.name, "Hyggenhagen");
        assert_eq!(best.region, "");
    }

    #[test]
    fn a_disabled_location_keeps_its_speed_and_is_marked() {
        let bulls = &parse_locations(LOCATIONS)[2];

        assert!(bulls.disabled);
        assert_eq!(bulls.nickname, "Bulls");
        assert_eq!(bulls.speed, "10 Gbps");
        assert_eq!(bulls.summary(), "US East · Bulls · 10 Gbps · Unavailable");
    }

    #[test]
    fn a_location_without_a_speed_parses_all_the_same() {
        let philadelphia = &parse_locations(LOCATIONS)[3];

        assert_eq!(philadelphia.name, "Philadelphia");
        assert_eq!(philadelphia.nickname, "Fresh Prince");
        assert_eq!(philadelphia.speed, "");
    }

    /// Only a speed is stripped as one, so a disabled location that never had a
    /// speed does not lose its nickname to the marker.
    #[test]
    fn a_parenthesised_tail_that_is_not_a_speed_is_not_read_as_one() {
        let location = parse_location("US East - Chicago - Bulls (Disabled)").expect("a location");

        assert!(location.disabled);
        assert_eq!(location.nickname, "Bulls");
        assert_eq!(location.speed, "");
    }

    #[test]
    fn blank_lines_are_not_locations() {
        assert!(parse_locations("\n   \n").is_empty());
        assert!(parse_locations("").is_empty());
    }

    /// The whole list is 200 lines of one shape; a change of wording must not
    /// take the list down with it.
    #[test]
    fn an_unfamiliar_line_still_yields_a_usable_row() {
        let location = parse_location("Somewhere").expect("a location");

        assert_eq!(location.name, "Somewhere");
        assert_eq!(location.target, "Somewhere");
    }

    /// A location reaches the shell inside a command line, so its quoting is a
    /// boundary rather than a convenience.
    #[test]
    fn a_connect_line_quotes_its_target() {
        assert_eq!(
            connect_line("Big Apple"),
            "windscribe-cli connect 'Big Apple'"
        );
        assert_eq!(
            connect_line("a'; rm -rf ~"),
            r#"windscribe-cli connect 'a'"'"'; rm -rf ~'"#
        );
        assert_eq!(disconnect_line(), "windscribe-cli disconnect");
    }
}
