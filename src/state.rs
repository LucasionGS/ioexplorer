use std::{fs, io, path::PathBuf};

use directories::UserDirs;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, ViewMode, clamp_icon_size};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct AppState {
    pub layout: ViewMode,
    pub show_hidden: bool,
    pub icon_size: i32,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct StoredState {
    layout: Option<ViewMode>,
    show_hidden: Option<bool>,
    icon_size: Option<i32>,
}

impl AppState {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            layout: config.default_view,
            show_hidden: config.show_hidden,
            icon_size: clamp_icon_size(config.icon_size),
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
    })
}

#[cfg(test)]
mod tests {
    use crate::config::ViewMode;

    use super::{AppState, parse_state};

    #[test]
    fn parses_persisted_state_values() {
        let fallback = AppState {
            layout: ViewMode::Icon,
            show_hidden: false,
            icon_size: 128,
        };

        let parsed = parse_state(
            "layout = \"list\"\nshow-hidden = true\nicon-size = 96\n",
            fallback,
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
        };

        let parsed = parse_state("show-hidden = false\n", fallback).expect("valid state");

        assert_eq!(parsed.layout, ViewMode::List);
        assert!(!parsed.show_hidden);
        assert_eq!(parsed.icon_size, 144);
    }

    #[test]
    fn clamps_persisted_icon_size() {
        let fallback = AppState {
            layout: ViewMode::Icon,
            show_hidden: false,
            icon_size: 128,
        };

        let parsed = parse_state("icon-size = 999\n", fallback).expect("valid state");

        assert_eq!(parsed.icon_size, 256);
    }
}
