use std::{fs, io, path::PathBuf};

use directories::UserDirs;
use serde::{Deserialize, Serialize};

use crate::{
    config::{AppConfig, ViewMode, clamp_icon_size},
    sorting::SortOrder,
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct AppState {
    pub layout: ViewMode,
    pub show_hidden: bool,
    pub icon_size: i32,
    pub sort: SortOrder,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct StoredState {
    layout: Option<ViewMode>,
    show_hidden: Option<bool>,
    icon_size: Option<i32>,
    // A table, so it has to be written after every scalar above it.
    sort: Option<SortOrder>,
}

impl AppState {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            layout: config.default_view,
            show_hidden: config.show_hidden,
            icon_size: clamp_icon_size(config.icon_size),
            sort: config.sort,
        }
    }

    pub fn load(config: &AppConfig) -> Self {
        let fallback = Self::from_config(config);
        let Some(path) = storage_path() else {
            return fallback;
        };

        match fs::read_to_string(path) {
            Ok(contents) => parse_state(&contents, fallback).unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to parse state, using config defaults");
                fallback
            }),
            Err(_) => fallback,
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = storage_path() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let stored = StoredState {
            layout: Some(self.layout),
            show_hidden: Some(self.show_hidden),
            icon_size: Some(clamp_icon_size(self.icon_size)),
            sort: Some(self.sort),
        };
        let contents = toml::to_string_pretty(&stored).map_err(io::Error::other)?;
        fs::write(path, contents)
    }
}

pub fn storage_path() -> Option<PathBuf> {
    UserDirs::new().map(|dirs| dirs.home_dir().join(".local/state/ioexplorer/state"))
}

fn parse_state(contents: &str, fallback: AppState) -> Result<AppState, toml::de::Error> {
    let stored: StoredState = toml::from_str(contents)?;
    Ok(AppState {
        layout: stored.layout.unwrap_or(fallback.layout),
        show_hidden: stored.show_hidden.unwrap_or(fallback.show_hidden),
        icon_size: stored
            .icon_size
            .map(clamp_icon_size)
            .unwrap_or(fallback.icon_size),
        sort: stored.sort.unwrap_or(fallback.sort),
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        config::ViewMode,
        sorting::{SortKey, SortOrder},
    };

    use super::{AppState, parse_state};

    fn fallback_state() -> AppState {
        AppState {
            layout: ViewMode::Icon,
            show_hidden: false,
            icon_size: 128,
            sort: SortOrder::default(),
        }
    }

    #[test]
    fn parses_persisted_state_values() {
        let parsed = parse_state(
            "layout = \"list\"\nshow-hidden = true\nicon-size = 96\n",
            fallback_state(),
        )
        .expect("valid state");

        assert_eq!(parsed.layout, ViewMode::List);
        assert!(parsed.show_hidden);
        assert_eq!(parsed.icon_size, 96);
    }

    #[test]
    fn missing_state_values_fall_back_to_config() {
        let fallback = AppState {
            layout: ViewMode::List,
            show_hidden: true,
            icon_size: 144,
            ..fallback_state()
        };

        let parsed = parse_state("show-hidden = false\n", fallback).expect("valid state");

        assert_eq!(parsed.layout, ViewMode::List);
        assert!(!parsed.show_hidden);
        assert_eq!(parsed.icon_size, 144);
    }

    #[test]
    fn clamps_persisted_icon_size() {
        let parsed = parse_state("icon-size = 999\n", fallback_state()).expect("valid state");

        assert_eq!(parsed.icon_size, 256);
    }

    #[test]
    fn parses_the_persisted_sort_order() {
        let parsed = parse_state(
            "icon-size = 96\n\n[sort]\nkey = \"modified\"\ndescending = true\nfolders_first = false\n",
            fallback_state(),
        )
        .expect("valid state");

        assert_eq!(
            parsed.sort,
            SortOrder {
                key: SortKey::Modified,
                descending: true,
                folders_first: false,
            }
        );
    }

    #[test]
    fn a_state_file_without_a_sort_keeps_the_config_default() {
        let fallback = AppState {
            sort: SortOrder {
                key: SortKey::Size,
                descending: true,
                folders_first: true,
            },
            ..fallback_state()
        };

        let parsed = parse_state("icon-size = 96\n", fallback).expect("valid state");

        assert_eq!(parsed.sort, fallback.sort);
    }

    /// The stored sort is a TOML table, so it has to be serialized after every
    /// scalar in `StoredState` or the write fails outright.
    #[test]
    fn a_saved_state_round_trips() {
        let state = AppState {
            layout: ViewMode::List,
            show_hidden: true,
            icon_size: 96,
            sort: SortOrder {
                key: SortKey::Extension,
                descending: true,
                folders_first: false,
            },
        };

        let stored = super::StoredState {
            layout: Some(state.layout),
            show_hidden: Some(state.show_hidden),
            icon_size: Some(state.icon_size),
            sort: Some(state.sort),
        };
        let contents = toml::to_string_pretty(&stored).expect("serialize");

        assert_eq!(
            parse_state(&contents, fallback_state()).expect("valid"),
            state
        );
    }
}
