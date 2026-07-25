//! Persisted launch history used to bias search ranking toward what the user
//! actually opens.
//!
//! The bonus is deliberately capped below the fuzzy matcher's prefix bonus, so
//! history reorders results *within* a quality tier without ever letting a
//! heavily used entry outrank a better textual match.

use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use directories::UserDirs;
use serde::{Deserialize, Serialize};

/// Largest bonus any entry can earn. Below [`crate::launcher::fuzzy`]'s prefix bonus.
pub const FRECENCY_MAX_BONUS: i32 = 120;
/// Days after which an entry's recency weight halves.
pub const HALF_LIFE_DAYS: f64 = 14.0;
/// Cap on stored entries, pruned by launch count when exceeded.
const MAX_ENTRIES: usize = 500;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Usage {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Frecency {
    #[serde(default)]
    entries: HashMap<String, Usage>,
}

impl Frecency {
    pub fn load() -> Self {
        let Some(path) = storage_path() else {
            return Self::default();
        };

        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to parse launch history, starting empty");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = storage_path() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, contents)
    }

    /// Records a launch of `key`, pruning the least-used entries when full.
    pub fn record(&mut self, key: &str, now_secs: u64) {
        let usage = self.entries.entry(key.to_string()).or_default();
        usage.count = usage.count.saturating_add(1);
        usage.last_used = now_secs;

        if self.entries.len() > MAX_ENTRIES {
            self.prune(MAX_ENTRIES);
        }
    }

    /// Ranking bonus for `key`, always within `0..=max_bonus`.
    pub fn bonus(&self, key: &str, now_secs: u64, max_bonus: i32) -> i32 {
        let Some(usage) = self.entries.get(key) else {
            return 0;
        };

        let ceiling = f64::from(self.max_count()).ln_1p();
        if ceiling <= f64::EPSILON {
            return 0;
        }

        let volume = f64::from(usage.count).ln_1p() / ceiling;
        let age_days = now_secs.saturating_sub(usage.last_used) as f64 / 86_400.0;
        let recency = 0.5_f64.powf(age_days / HALF_LIFE_DAYS);

        (f64::from(max_bonus) * volume * recency).round() as i32
    }

    fn max_count(&self) -> u32 {
        self.entries
            .values()
            .map(|usage| usage.count)
            .max()
            .unwrap_or(0)
    }

    /// Keeps the `limit` most-launched entries, breaking ties by recency.
    fn prune(&mut self, limit: usize) {
        let mut ranked = self
            .entries
            .iter()
            .map(|(key, usage)| (key.clone(), *usage))
            .collect::<Vec<_>>();
        ranked.sort_by(|(left_key, left), (right_key, right)| {
            right
                .count
                .cmp(&left.count)
                .then(right.last_used.cmp(&left.last_used))
                .then(left_key.cmp(right_key))
        });
        ranked.truncate(limit);

        self.entries = ranked.into_iter().collect();
    }
}

/// Seconds since the Unix epoch, saturating to zero if the clock is before it.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub fn storage_path() -> Option<PathBuf> {
    UserDirs::new().map(|dirs| {
        dirs.home_dir()
            .join(".local/state/ioexplorer/spotlight-usage.toml")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn unknown_keys_earn_no_bonus() {
        let frecency = Frecency::default();

        assert_eq!(frecency.bonus("app:missing", 0, FRECENCY_MAX_BONUS), 0);
    }

    #[test]
    fn record_increments_count_and_stamps_time() {
        let mut frecency = Frecency::default();
        frecency.record("app:firefox", 1_000);
        frecency.record("app:firefox", 2_000);

        let usage = frecency
            .entries
            .get("app:firefox")
            .copied()
            .expect("recorded usage");
        assert_eq!(usage.count, 2);
        assert_eq!(usage.last_used, 2_000);
    }

    #[test]
    fn bonus_grows_with_launch_count() {
        let mut frecency = Frecency::default();
        for _ in 0..10 {
            frecency.record("app:often", 0);
        }
        frecency.record("app:rarely", 0);

        let often = frecency.bonus("app:often", 0, FRECENCY_MAX_BONUS);
        let rarely = frecency.bonus("app:rarely", 0, FRECENCY_MAX_BONUS);

        assert!(often > rarely, "{often} > {rarely}");
    }

    #[test]
    fn bonus_decays_with_age() {
        let mut frecency = Frecency::default();
        frecency.record("app:firefox", 0);

        let fresh = frecency.bonus("app:firefox", 0, FRECENCY_MAX_BONUS);
        let stale = frecency.bonus("app:firefox", 28 * DAY, FRECENCY_MAX_BONUS);

        assert!(fresh > stale, "{fresh} > {stale}");
    }

    #[test]
    fn bonus_never_exceeds_the_cap() {
        let mut frecency = Frecency::default();
        for _ in 0..10_000 {
            frecency.record("app:firefox", 500);
        }

        let bonus = frecency.bonus("app:firefox", 500, FRECENCY_MAX_BONUS);

        assert!((0..=FRECENCY_MAX_BONUS).contains(&bonus), "{bonus}");
    }

    #[test]
    fn prune_keeps_the_most_launched_entries() {
        let mut frecency = Frecency::default();
        for _ in 0..5 {
            frecency.record("app:keep", 10);
        }
        frecency.record("app:drop", 10);

        frecency.prune(1);

        assert!(frecency.entries.get("app:keep").copied().is_some());
        assert!(frecency.entries.get("app:drop").copied().is_none());
    }

    #[test]
    fn round_trips_through_toml() {
        let mut frecency = Frecency::default();
        frecency.record("app:firefox", 1_234);

        let contents = toml::to_string_pretty(&frecency).expect("serializable");
        let parsed: Frecency = toml::from_str(&contents).expect("parsable");

        assert_eq!(
            parsed.entries.get("app:firefox").copied(),
            frecency.entries.get("app:firefox").copied()
        );
    }
}
