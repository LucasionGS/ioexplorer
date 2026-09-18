//! What the quick menu remembers between openings: the recently used
//! characters of each tab, and the skin tone last chosen.

use std::{fs, path::PathBuf};

use directories::UserDirs;
use serde::{Deserialize, Serialize};

use crate::config::{QuickTab, write_atomic};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct QuickState {
    /// `0` for none, then lightest to darkest.
    #[serde(default)]
    pub skin_tone: u8,
    /// Most recent first.
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub emoji: Vec<String>,
}

impl QuickState {
    pub fn load() -> Self {
        let Some(path) = storage_path() else {
            return Self::default();
        };
        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to parse the quick menu state, starting empty");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = storage_path() else {
            return;
        };
        let result = toml::to_string_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|contents| write_atomic(&path, &contents));
        if let Err(error) = result {
            tracing::warn!(%error, "cannot save the quick menu state");
        }
    }

    /// Saved GIFs keep their own use times, in the GIF library's index.
    pub fn recent(&self, tab: QuickTab) -> &[String] {
        match tab {
            QuickTab::Symbols => &self.symbols,
            QuickTab::Emoji => &self.emoji,
            QuickTab::Gif => &[],
        }
    }

    /// Moves `text` to the front of `tab`'s recents, keeping at most `limit`.
    pub fn record(&mut self, tab: QuickTab, text: &str, limit: usize) {
        let recent = match tab {
            QuickTab::Symbols => &mut self.symbols,
            QuickTab::Emoji => &mut self.emoji,
            QuickTab::Gif => return,
        };
        if limit == 0 {
            return;
        }
        recent.retain(|used| used != text);
        recent.insert(0, text.to_string());
        recent.truncate(limit);
    }
}

fn storage_path() -> Option<PathBuf> {
    UserDirs::new().map(|dirs| dirs.home_dir().join(".local/state/ioexplorer/quick.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_moves_to_the_front_and_caps() {
        let mut state = QuickState::default();
        state.record(QuickTab::Symbols, "→", 3);
        state.record(QuickTab::Symbols, "★", 3);
        state.record(QuickTab::Symbols, "→", 3);
        assert_eq!(state.symbols, ["→", "★"]);

        state.record(QuickTab::Symbols, "a", 3);
        state.record(QuickTab::Symbols, "b", 3);
        assert_eq!(state.symbols, ["b", "a", "→"]);
        assert!(state.emoji.is_empty());
    }

    #[test]
    fn a_zero_limit_remembers_nothing_new() {
        let mut state = QuickState::default();
        state.record(QuickTab::Emoji, "😀", 0);
        assert!(state.emoji.is_empty());
    }

    #[test]
    fn round_trips() {
        let mut state = QuickState {
            skin_tone: 3,
            ..QuickState::default()
        };
        state.record(QuickTab::Emoji, "👍🏽", 10);
        let contents = toml::to_string_pretty(&state).unwrap();
        assert_eq!(toml::from_str::<QuickState>(&contents).unwrap(), state);
        assert_eq!(
            toml::from_str::<QuickState>("").unwrap(),
            QuickState::default()
        );
    }
}
