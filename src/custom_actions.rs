use std::path::PathBuf;

use crate::{
    config::CustomActionConfig,
    providers::{FileItem, FileKind},
};
use url::Url;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ActionCommandVariable {
    pub placeholder: &'static str,
    pub description: &'static str,
}

pub const ACTION_COMMAND_VARIABLES: &[ActionCommandVariable] = &[
    ActionCommandVariable {
        placeholder: "{path}",
        description: "Full local path",
    },
    ActionCommandVariable {
        placeholder: "{name}",
        description: "File or folder name",
    },
    ActionCommandVariable {
        placeholder: "{parent}",
        description: "Containing folder path",
    },
    ActionCommandVariable {
        placeholder: "{stem}",
        description: "Name without extension",
    },
    ActionCommandVariable {
        placeholder: "{extension}",
        description: "Extension without the dot",
    },
    ActionCommandVariable {
        placeholder: "{uri}",
        description: "file:// URI",
    },
    ActionCommandVariable {
        placeholder: "{kind}",
        description: "file, folder, symlink, or other",
    },
];

const IMAGE_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "gif", "heic", "heif", "ico", "jpeg", "jpg", "jxl", "png", "svg", "tif", "tiff",
    "webp", "xpm",
];
const VIDEO_EXTENSIONS: &[&str] = &[
    "avi", "flv", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "ogm", "ogv", "webm", "wmv",
];
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aiff", "alac", "flac", "m4a", "mp3", "oga", "ogg", "opus", "wav", "wma",
];
const TEXT_EXTENSIONS: &[&str] = &[
    "conf", "css", "csv", "ini", "json", "log", "md", "rs", "sh", "toml", "txt", "xml", "yaml",
    "yml",
];

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ActionTarget {
    path: PathBuf,
    name: String,
    kind: FileKind,
}

impl ActionTarget {
    pub fn from_item(item: &FileItem) -> Option<Self> {
        let path = item.uri.local_path().ok()?;
        Some(Self {
            path,
            name: item.name.clone(),
            kind: item.kind,
        })
    }

    pub fn current_folder(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path,
            name,
            kind: FileKind::Directory,
        }
    }
}

pub fn command_uses_variables(command: &str) -> bool {
    ACTION_COMMAND_VARIABLES
        .iter()
        .any(|variable| command.contains(variable.placeholder))
}

pub fn action_command_line(command: &str, targets: &[ActionTarget]) -> String {
    let command = command.trim();
    if command_uses_variables(command) {
        expand_command_variables(command, targets)
    } else {
        append_path_arguments(command, targets)
    }
}

pub fn expand_command_variables(command: &str, targets: &[ActionTarget]) -> String {
    ACTION_COMMAND_VARIABLES
        .iter()
        .fold(command.to_string(), |expanded, variable| {
            expanded.replace(
                variable.placeholder,
                &target_values_argument_list(targets, variable.placeholder),
            )
        })
}

impl ActionTarget {
    fn variable_value(&self, placeholder: &str) -> String {
        match placeholder {
            "{path}" => self.path.to_string_lossy().into_owned(),
            "{name}" => self.name.clone(),
            "{parent}" => self
                .path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "{stem}" => self
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| self.name.clone()),
            "{extension}" => self
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_string)
                .unwrap_or_default(),
            "{uri}" => Url::from_file_path(&self.path)
                .map(|uri| uri.to_string())
                .unwrap_or_else(|()| self.path.to_string_lossy().into_owned()),
            "{kind}" => action_kind_label(self.kind).to_string(),
            _ => String::new(),
        }
    }
}

fn append_path_arguments(command: &str, targets: &[ActionTarget]) -> String {
    let paths = targets
        .iter()
        .map(|target| shell_quote(&target.path.to_string_lossy()))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        command.to_string()
    } else {
        format!("{} {}", command, paths.join(" "))
    }
}

fn target_values_argument_list(targets: &[ActionTarget], placeholder: &str) -> String {
    targets
        .iter()
        .map(|target| shell_quote(&target.variable_value(placeholder)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn matching_actions(
    actions: &[CustomActionConfig],
    targets: &[ActionTarget],
) -> Vec<CustomActionConfig> {
    if targets.is_empty() {
        return Vec::new();
    }

    actions
        .iter()
        .filter(|action| action_matches_targets(action, targets))
        .cloned()
        .collect()
}

fn action_matches_targets(action: &CustomActionConfig, targets: &[ActionTarget]) -> bool {
    if action.label.trim().is_empty() || action.command.trim().is_empty() {
        return false;
    }

    targets.iter().all(|target| {
        action.filters.is_empty() || target_matches_any_filter(target, &action.filters)
    })
}

fn target_matches_any_filter(target: &ActionTarget, filters: &[String]) -> bool {
    filters
        .iter()
        .map(|filter| filter.trim())
        .filter(|filter| !filter.is_empty())
        .any(|filter| target_matches_filter(target, filter))
}

fn target_matches_filter(target: &ActionTarget, filter: &str) -> bool {
    match filter.to_ascii_lowercase().as_str() {
        "folder/" | "directory/" => return target.kind == FileKind::Directory,
        "file/" => return target.kind == FileKind::File,
        "image/*" => {
            return target.kind == FileKind::File && extension_in(target, IMAGE_EXTENSIONS);
        }
        "video/*" => {
            return target.kind == FileKind::File && extension_in(target, VIDEO_EXTENSIONS);
        }
        "audio/*" => {
            return target.kind == FileKind::File && extension_in(target, AUDIO_EXTENSIONS);
        }
        "text/*" => return target.kind == FileKind::File && extension_in(target, TEXT_EXTENSIONS),
        _ => {}
    }

    let candidate = if filter.contains('/') {
        target.path.to_string_lossy().into_owned()
    } else {
        target.name.clone()
    };
    wildcard_match(
        &filter.to_ascii_lowercase(),
        &candidate.to_ascii_lowercase(),
    )
}

fn extension_in(target: &ActionTarget, extensions: &[&str]) -> bool {
    target
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extensions.contains(&extension.to_ascii_lowercase().as_str()))
}

fn action_kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "folder",
        FileKind::File => "file",
        FileKind::Symlink => "symlink",
        FileKind::Other => "other",
    }
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut star_index = None;
    let mut star_value_index = 0;

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            star_value_index = value_index;
            pattern_index += 1;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(filters: &[&str]) -> CustomActionConfig {
        CustomActionConfig {
            label: "Action".to_string(),
            command: "echo".to_string(),
            run_on_each: false,
            filters: filters.iter().map(|filter| (*filter).to_string()).collect(),
        }
    }

    fn target(path: &str, kind: FileKind) -> ActionTarget {
        let path = PathBuf::from(path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path.as_os_str().to_str().unwrap_or_default())
            .to_string();
        ActionTarget { path, name, kind }
    }

    #[test]
    fn empty_filters_match_all_targets() {
        assert!(action_matches_targets(
            &action(&[]),
            &[target("/tmp/report.pdf", FileKind::File)]
        ));
    }

    #[test]
    fn folder_keyword_matches_directories() {
        assert!(action_matches_targets(
            &action(&["folder/"]),
            &[target("/tmp/project", FileKind::Directory)]
        ));
        assert!(!action_matches_targets(
            &action(&["folder/"]),
            &[target("/tmp/project.txt", FileKind::File)]
        ));
    }

    #[test]
    fn glob_filters_match_file_names() {
        assert!(action_matches_targets(
            &action(&["*.txt"]),
            &[target("/tmp/notes.TXT", FileKind::File)]
        ));
    }

    #[test]
    fn image_filter_matches_common_image_extensions() {
        assert!(action_matches_targets(
            &action(&["image/*"]),
            &[target("/tmp/photo.webp", FileKind::File)]
        ));
    }

    #[test]
    fn multi_target_actions_require_every_target_to_match() {
        assert!(!action_matches_targets(
            &action(&["*.txt"]),
            &[
                target("/tmp/a.txt", FileKind::File),
                target("/tmp/b.png", FileKind::File)
            ]
        ));
    }

    #[test]
    fn command_variables_expand_to_shell_argument_lists() {
        let target = target("/tmp/archive.tar.gz", FileKind::File);
        assert!(command_uses_variables("code {path}"));
        assert_eq!(
            action_command_line("cp {path} ~/backup/{name}", &[target]),
            "cp '/tmp/archive.tar.gz' ~/backup/'archive.tar.gz'"
        );
    }

    #[test]
    fn action_command_line_appends_all_paths_without_variables() {
        assert_eq!(
            action_command_line(
                "code --reuse-window",
                &[
                    target("/tmp/a file.txt", FileKind::File),
                    target("/tmp/quote's.md", FileKind::File),
                ],
            ),
            "code --reuse-window '/tmp/a file.txt' '/tmp/quote'\"'\"'s.md'"
        );
    }

    #[test]
    fn action_command_line_expands_multi_target_variables() {
        assert_eq!(
            action_command_line(
                "printf '%s\\n' {name}",
                &[
                    target("/tmp/archive.tar.gz", FileKind::File),
                    target("/tmp/project", FileKind::Directory),
                ],
            ),
            "printf '%s\\n' 'archive.tar.gz' 'project'"
        );
    }
}
