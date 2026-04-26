use std::{fs, io, path::PathBuf};

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

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct CustomActionConfig {
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub run_on_each: bool,
    #[serde(default)]
    pub filters: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub default_view: ViewMode,
    pub show_hidden: bool,
    pub icon_size: i32,
    pub sidebar_width: i32,
    pub custom_css: Option<PathBuf>,
    pub list_columns: ListColumns,
    #[serde(default)]
    pub actions: Vec<CustomActionConfig>,
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
            actions: Vec::new(),
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

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = Self::config_path() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, contents)
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

[[actions]]
label = "Open in Editor"
command = "code --reuse-window"
run_on_each = true
filters = ["*.txt", "*.md"]
"#,
        )
        .expect("valid config");

        assert_eq!(parsed.default_view, ViewMode::List);
        assert!(parsed.show_hidden);
        assert!(!parsed.list_columns.kind);
        assert_eq!(parsed.actions.len(), 1);
        assert_eq!(parsed.actions[0].label, "Open in Editor");
        assert!(parsed.actions[0].run_on_each);
    }

    #[test]
    fn missing_run_on_each_defaults_to_false() {
        let parsed: CustomActionConfig = toml::from_str(
            r#"
label = "Open in Editor"
command = "code --reuse-window"
filters = ["*.txt"]
"#,
        )
        .expect("valid action config");

        assert!(!parsed.run_on_each);
    }

    #[test]
    fn serializes_actions_as_toml_array() {
        let config = AppConfig {
            actions: vec![CustomActionConfig {
                label: "Open in Editor".to_string(),
                command: "code --reuse-window".to_string(),
                run_on_each: true,
                filters: vec!["*.txt".to_string(), "*.md".to_string()],
            }],
            ..Default::default()
        };

        let contents = toml::to_string_pretty(&config).expect("serializable config");

        assert!(contents.contains("[[actions]]"));
        assert!(contents.contains("label = \"Open in Editor\""));
        assert!(contents.contains("run_on_each = true"));
        assert!(contents.contains("filters = ["));
        assert!(contents.contains("\"*.txt\""));
        assert!(contents.contains("\"*.md\""));
    }
}
