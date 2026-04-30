use std::{cell::RefCell, env, path::PathBuf, rc::Rc};

use gtk::prelude::*;

use crate::{config::AppConfig, theme, ui::window::AppWindow};

const SELECTOR_APP_ID: &str = "io.github.ionix.IoExplorer.Selector";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectorMode {
    Open,
    Save,
    SaveFiles,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectorOptions {
    pub mode: SelectorMode,
    pub title: String,
    pub accept_label: String,
    pub multiple: bool,
    pub directory: bool,
    pub current_folder: Option<PathBuf>,
    pub current_name: Option<String>,
    pub current_file: Option<PathBuf>,
    pub file_names: Vec<String>,
}

pub fn is_chooser_invocation(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--chooser")
}

pub fn run_from_args(args: &[String]) -> glib::ExitCode {
    let options = match parse_selector_args(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("ioexplorer chooser: {error}");
            return glib::ExitCode::FAILURE;
        }
    };

    run(options)
}

pub fn parse_selector_args(args: &[String]) -> Result<SelectorOptions, String> {
    let mut options = SelectorOptions::default();
    let mut saw_chooser = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--chooser" => saw_chooser = true,
            "--chooser-mode" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--chooser-mode requires a value".to_string())?;
                options.mode = parse_selector_mode(value)?;
            }
            "--title" => options.title = next_arg(args, &mut index, "--title")?,
            "--accept-label" => {
                options.accept_label = next_arg(args, &mut index, "--accept-label")?
            }
            "--multiple" => options.multiple = true,
            "--directory" => options.directory = true,
            "--current-folder" => {
                options.current_folder = Some(PathBuf::from(next_arg(
                    args,
                    &mut index,
                    "--current-folder",
                )?));
            }
            "--current-name" => {
                options.current_name = Some(next_arg(args, &mut index, "--current-name")?);
            }
            "--current-file" => {
                options.current_file =
                    Some(PathBuf::from(next_arg(args, &mut index, "--current-file")?));
            }
            "--file-name" => {
                options
                    .file_names
                    .push(next_arg(args, &mut index, "--file-name")?);
            }
            unknown => return Err(format!("unknown chooser argument `{unknown}`")),
        }
        index += 1;
    }

    if !saw_chooser {
        return Err("missing --chooser".to_string());
    }

    options.normalize();
    Ok(options)
}

impl Default for SelectorOptions {
    fn default() -> Self {
        Self {
            mode: SelectorMode::Open,
            title: "Select File".to_string(),
            accept_label: "Select".to_string(),
            multiple: false,
            directory: false,
            current_folder: None,
            current_name: None,
            current_file: None,
            file_names: Vec::new(),
        }
    }
}

impl SelectorOptions {
    fn normalize(&mut self) {
        match self.mode {
            SelectorMode::Open => {
                if self.directory {
                    self.title = default_if_empty(&self.title, "Select Folder");
                    self.accept_label = default_if_empty(&self.accept_label, "Select Folder");
                } else {
                    self.title = default_if_empty(&self.title, "Open File");
                    self.accept_label = default_if_empty(&self.accept_label, "Open");
                }
            }
            SelectorMode::Save => {
                self.title = default_if_empty(&self.title, "Save File");
                self.accept_label = default_if_empty(&self.accept_label, "Save");
                self.multiple = false;
                self.directory = false;
            }
            SelectorMode::SaveFiles => {
                self.title = default_if_empty(&self.title, "Save Files");
                self.accept_label = default_if_empty(&self.accept_label, "Choose Folder");
                self.multiple = false;
                self.directory = true;
            }
        }
    }

    pub(crate) fn start_folder(&self) -> PathBuf {
        if let Some(path) = self
            .current_file
            .as_ref()
            .and_then(|path| path.parent())
            .filter(|path| path.is_dir())
        {
            return path.to_path_buf();
        }

        if let Some(path) = self.current_folder.as_ref().filter(|path| path.is_dir()) {
            return path.clone();
        }

        home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }

    pub(crate) fn initial_name(&self) -> String {
        self.current_name
            .clone()
            .or_else(|| {
                self.current_file
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default()
    }
}

fn run(options: SelectorOptions) -> glib::ExitCode {
    let result = Rc::new(RefCell::new(None::<Vec<String>>));
    let app = gtk::Application::builder()
        .application_id(SELECTOR_APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_startup(|_| {
        let config = AppConfig::load();
        theme::install(&config);
    });

    let activate_options = options.clone();
    let activate_result = Rc::clone(&result);
    app.connect_activate(move |app| {
        let config = AppConfig::load();
        let window = AppWindow::new_for_chooser(
            app,
            config,
            activate_options.clone(),
            Rc::clone(&activate_result),
        );
        window.present();
    });

    app.run_with_args(&["ioexplorer-selector"]);

    let Some(uris) = result.borrow().clone() else {
        return glib::ExitCode::FAILURE;
    };

    for uri in uris {
        println!("{uri}");
    }
    glib::ExitCode::SUCCESS
}

fn parse_selector_mode(value: &str) -> Result<SelectorMode, String> {
    match value {
        "open" => Ok(SelectorMode::Open),
        "save" => Ok(SelectorMode::Save),
        "save-files" => Ok(SelectorMode::SaveFiles),
        _ => Err(format!("unknown chooser mode `{value}`")),
    }
}

fn next_arg(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn default_if_empty(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_open_directory_selector_args() {
        let parsed = parse_selector_args(&args(&[
            "--chooser",
            "--chooser-mode",
            "open",
            "--directory",
            "--multiple",
            "--current-folder",
            "/tmp",
        ]))
        .expect("valid selector args");

        assert_eq!(parsed.mode, SelectorMode::Open);
        assert!(parsed.directory);
        assert!(parsed.multiple);
        assert_eq!(parsed.current_folder, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn parses_save_selector_args() {
        let parsed = parse_selector_args(&args(&[
            "--chooser",
            "--chooser-mode",
            "save",
            "--current-name",
            "report.pdf",
            "--accept-label",
            "Export",
        ]))
        .expect("valid selector args");

        assert_eq!(parsed.mode, SelectorMode::Save);
        assert_eq!(parsed.current_name.as_deref(), Some("report.pdf"));
        assert_eq!(parsed.accept_label, "Export");
    }

    #[test]
    fn rejects_unknown_selector_mode() {
        let error = parse_selector_args(&args(&["--chooser", "--chooser-mode", "print"]))
            .expect_err("invalid mode");

        assert!(error.contains("unknown chooser mode"));
    }
}
