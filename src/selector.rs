use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
    rc::Rc,
};

use gtk::prelude::*;
use url::Url;

use crate::{
    config::AppConfig,
    providers::{FileItem, FileKind, Provider, ProviderUri, local::LocalProvider},
    state::AppState,
    theme,
};

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

    fn start_folder(&self) -> PathBuf {
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

    fn initial_name(&self) -> String {
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
        let selector =
            SelectorWindow::new(app, activate_options.clone(), Rc::clone(&activate_result));
        selector.present();
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

struct SelectorWindow {
    app: gtk::Application,
    window: gtk::ApplicationWindow,
    options: SelectorOptions,
    provider: LocalProvider,
    current_folder: RefCell<PathBuf>,
    entries: RefCell<Vec<FileItem>>,
    selected_indices: RefCell<BTreeSet<usize>>,
    list_box: gtk::ListBox,
    path_entry: gtk::Entry,
    name_entry: gtk::Entry,
    status_label: gtk::Label,
    accept_button: gtk::Button,
    result: Rc<RefCell<Option<Vec<String>>>>,
    show_hidden: Cell<bool>,
}

impl SelectorWindow {
    fn new(
        app: &gtk::Application,
        options: SelectorOptions,
        result: Rc<RefCell<Option<Vec<String>>>>,
    ) -> Rc<Self> {
        let config = AppConfig::load();
        let state = AppState::load(&config);
        let provider = LocalProvider::new();
        let current_folder = options.start_folder();

        let up_button = selector_icon_button("go-up-symbolic", "Up");
        let refresh_button = selector_icon_button("view-refresh-symbolic", "Refresh");
        let path_entry = gtk::Entry::builder()
            .hexpand(true)
            .text(current_folder.to_string_lossy())
            .css_classes(["path-entry"])
            .build();
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["topbar"])
            .build();
        toolbar.append(&up_button);
        toolbar.append(&refresh_button);
        toolbar.append(&path_entry);

        let list_box = gtk::ListBox::builder()
            .selection_mode(if options.multiple {
                gtk::SelectionMode::Multiple
            } else {
                gtk::SelectionMode::Single
            })
            .activate_on_single_click(false)
            .css_classes(["content-list", "selector-list"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list_box)
            .vexpand(true)
            .css_classes(["content-scroll"])
            .build();

        let name_entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Filename")
            .text(options.initial_name())
            .visible(options.mode == SelectorMode::Save)
            .build();
        let cancel_button = gtk::Button::with_label("Cancel");
        let accept_button = gtk::Button::with_label(&options.accept_label);
        accept_button.add_css_class("suggested-action");
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["dim-label", "selector-status"])
            .build();

        let bottom = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["selector-actions"])
            .build();
        bottom.append(&name_entry);
        bottom.append(&status_label);
        bottom.append(&cancel_button);
        bottom.append(&accept_button);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["selector-window"])
            .build();
        root.append(&toolbar);
        root.append(&scroll);
        root.append(&bottom);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(&options.title)
            .default_width(820)
            .default_height(560)
            .child(&root)
            .build();

        let this = Rc::new(Self {
            app: app.clone(),
            window,
            options,
            provider,
            current_folder: RefCell::new(current_folder),
            entries: RefCell::new(Vec::new()),
            selected_indices: RefCell::new(BTreeSet::new()),
            list_box,
            path_entry,
            name_entry,
            status_label,
            accept_button,
            result,
            show_hidden: Cell::new(state.show_hidden),
        });

        let select_from_path = Rc::clone(&this);
        this.path_entry.connect_activate(move |entry| {
            select_from_path.navigate_to_path(PathBuf::from(entry.text().as_str()))
        });

        let refresh = Rc::clone(&this);
        refresh.refresh();
        refresh_button.connect_clicked(move |_| refresh.refresh());

        let go_up = Rc::clone(&this);
        up_button.connect_clicked(move |_| go_up.go_up());

        let row_activated = Rc::clone(&this);
        this.list_box.connect_row_activated(move |_, row| {
            row_activated.activate_row(row.index() as usize);
        });

        let selection_changed = Rc::clone(&this);
        this.list_box.connect_selected_rows_changed(move |list| {
            selection_changed.update_selection_from_list(list);
        });

        let accept = Rc::clone(&this);
        this.accept_button.connect_clicked(move |_| accept.accept());

        let save_accept = Rc::clone(&this);
        this.name_entry
            .connect_activate(move |_| save_accept.accept());

        let cancel_app = this.app.clone();
        cancel_button.connect_clicked(move |_| cancel_app.quit());

        let close_app = this.app.clone();
        this.window.connect_close_request(move |_| {
            close_app.quit();
            glib::Propagation::Proceed
        });

        this
    }

    fn present(&self) {
        self.window.present();
    }

    fn refresh(&self) {
        let folder = self.current_folder.borrow().clone();
        self.load_folder(folder);
    }

    fn load_folder(&self, folder: PathBuf) {
        self.path_entry.set_text(&folder.to_string_lossy());
        self.selected_indices.borrow_mut().clear();
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        match self.provider.list(&ProviderUri::local(&folder)) {
            Ok(mut entries) => {
                if !self.show_hidden.get() {
                    entries.retain(|entry| !entry.hidden);
                }

                for item in &entries {
                    self.list_box.append(&selector_row(item));
                }
                *self.current_folder.borrow_mut() = folder;
                *self.entries.borrow_mut() = entries;
                self.status_label.set_text("Choose a location");
                self.update_accept_state();
            }
            Err(error) => {
                self.status_label
                    .set_text(&format!("Unable to open folder: {error}"));
                self.update_accept_state();
            }
        }
    }

    fn navigate_to_path(&self, path: PathBuf) {
        if path.is_dir() {
            self.load_folder(path);
        } else if path.is_file() && self.options.mode == SelectorMode::Save {
            if let Some(parent) = path.parent() {
                self.load_folder(parent.to_path_buf());
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                self.name_entry.set_text(name);
            }
        } else {
            self.status_label.set_text("Enter an existing folder path");
        }
    }

    fn go_up(&self) {
        let parent = self.current_folder.borrow().parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.load_folder(parent);
        }
    }

    fn activate_row(&self, index: usize) {
        let Some(item) = self.entries.borrow().get(index).cloned() else {
            return;
        };

        let Ok(path) = item.uri.local_path() else {
            return;
        };

        if item.kind == FileKind::Directory {
            if self.options.mode == SelectorMode::Open && self.options.directory {
                self.selected_indices.borrow_mut().clear();
                self.selected_indices.borrow_mut().insert(index);
                self.accept();
            } else {
                self.load_folder(path);
            }
            return;
        }

        match self.options.mode {
            SelectorMode::Open => self.accept(),
            SelectorMode::Save => {
                self.name_entry.set_text(&item.name);
                self.update_accept_state();
            }
            SelectorMode::SaveFiles => {}
        }
    }

    fn update_selection_from_list(&self, list: &gtk::ListBox) {
        let mut selected = self.selected_indices.borrow_mut();
        selected.clear();
        for row in list.selected_rows() {
            let index = row.index();
            if index >= 0 {
                selected.insert(index as usize);
            }
        }
        drop(selected);
        self.update_accept_state();
    }

    fn update_accept_state(&self) {
        let sensitive = match self.options.mode {
            SelectorMode::Open if self.options.directory => true,
            SelectorMode::Open => !self.selected_paths(false).is_empty(),
            SelectorMode::Save => !self.name_entry.text().trim().is_empty(),
            SelectorMode::SaveFiles => true,
        };
        self.accept_button.set_sensitive(sensitive);
    }

    fn selected_paths(&self, directories_only: bool) -> Vec<PathBuf> {
        let entries = self.entries.borrow();
        self.selected_indices
            .borrow()
            .iter()
            .filter_map(|index| entries.get(*index))
            .filter(|item| !directories_only || item.kind == FileKind::Directory)
            .filter_map(|item| item.uri.local_path().ok())
            .collect()
    }

    fn accept(&self) {
        let paths = match self.options.mode {
            SelectorMode::Open if self.options.directory => {
                let selected = self.selected_paths(true);
                if selected.is_empty() {
                    vec![self.current_folder.borrow().clone()]
                } else {
                    selected
                }
            }
            SelectorMode::Open => {
                let selected = self.selected_paths(false);
                if selected.is_empty() {
                    self.status_label.set_text("Select a file first");
                    return;
                }
                selected
                    .into_iter()
                    .filter(|path| path.is_file())
                    .take(if self.options.multiple { usize::MAX } else { 1 })
                    .collect::<Vec<_>>()
            }
            SelectorMode::Save => {
                let name = self.name_entry.text().trim().to_string();
                if name.is_empty() || name.contains('/') {
                    self.status_label.set_text("Enter a valid filename");
                    return;
                }
                vec![self.current_folder.borrow().join(name)]
            }
            SelectorMode::SaveFiles => {
                let selected_folder = self.selected_paths(true).into_iter().next();
                let target_folder =
                    selected_folder.unwrap_or_else(|| self.current_folder.borrow().clone());
                if self.options.file_names.is_empty() {
                    vec![target_folder]
                } else {
                    self.options
                        .file_names
                        .iter()
                        .map(|name| target_folder.join(name))
                        .collect()
                }
            }
        };

        let uris = paths
            .iter()
            .filter_map(|path| file_uri(path))
            .collect::<Vec<_>>();
        if uris.is_empty() {
            self.status_label.set_text("Nothing selected");
            return;
        }

        *self.result.borrow_mut() = Some(uris);
        self.app.quit();
    }
}

fn selector_row(item: &FileItem) -> gtk::ListBoxRow {
    let icon = gtk::Image::builder()
        .icon_name(item.kind.icon_name())
        .pixel_size(22)
        .build();
    let name = gtk::Label::builder()
        .label(item.display_name())
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let kind = gtk::Label::builder()
        .label(item.kind.label())
        .css_classes(["dim-label"])
        .build();
    let row_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .css_classes(["selector-row"])
        .build();
    row_content.append(&icon);
    row_content.append(&name);
    row_content.append(&kind);

    gtk::ListBoxRow::builder().child(&row_content).build()
}

fn selector_icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .css_classes(["toolbar-button"])
        .build();
    button.set_focusable(false);
    button
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

fn file_uri(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
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
