use std::{fs, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const MIN_ICON_SIZE: i32 = 48;
pub const MAX_ICON_SIZE: i32 = 256;

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewMode {
    List,
    #[default]
    Icon,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListColumns {
    pub size: bool,
    pub kind: bool,
    pub modified: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub default_view: ViewMode,
    pub show_hidden: bool,
    pub icon_size: i32,
    pub sidebar_width: i32,
    pub custom_css: Option<PathBuf>,
    pub list_columns: ListColumns,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_view: ViewMode::Icon,
            show_hidden: false,
            icon_size: 128,
            sidebar_width: 220,
            custom_css: None,
            list_columns: ListColumns {
                size: true,
                kind: true,
                modified: true,
            },
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };

        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to parse config, using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        ProjectDirs::from("io.github", "ionix", "ioexplorer")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }
}

pub fn clamp_icon_size(icon_size: i32) -> i32 {
    icon_size.clamp(MIN_ICON_SIZE, MAX_ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_view_mode_names() {
        let parsed: AppConfig = toml::from_str(
            r#"
default_view = "list"
show_hidden = true
icon_size = 64
sidebar_width = 210

[list_columns]
size = true
kind = false
modified = true
"#,
        )
        .expect("valid config");

        assert_eq!(parsed.default_view, ViewMode::List);
        assert!(parsed.show_hidden);
        assert!(!parsed.list_columns.kind);
    }
}
