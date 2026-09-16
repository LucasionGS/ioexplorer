//! `ioexplorer-shot`: a screenshot tool that freezes the screen first.
//!
//! Every mode starts the same way — capture every output, and ask the
//! compositor where the windows are — so what is saved is the moment the key
//! was pressed, not whatever the screen shows once a selection is finished.
//! `region` then opens the interactive overlay over that frozen frame; the
//! other modes pick their area straight from the snapshot and exit.

mod canvas;
mod capture;
mod compositor;
mod deliver;
mod geometry;
mod overlay;
mod record;
mod tools;

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    thread,
    time::Instant,
};

use gtk::{gdk, gio, glib, prelude::*};

use crate::{config::AppConfig, theme};

use capture::FrozenOutput;
use compositor::Scene;
use deliver::Destination;
use geometry::Rect;
use overlay::{Finish, Overlay, Purpose};
use tools::Annotation;

const APP_ID: &str = "io.github.ionix.IoExplorer.Shot";

const USAGE: &str = "\
Usage: ioexplorer-shot [MODE] [OPTIONS]
       ioexplorer-shot record [region|window|screen] [OPTIONS]
       ioexplorer-shot stop

Modes:
  region    Freeze the screen and select an area, a window or a screen (default)
  window    Capture the focused window
  screen    Capture the focused screen
  all       Capture every screen, in their arranged layout

Recording:
  record [MODE]  Record video of an area, the focused window or the focused
                 screen. Running any record command again, or `stop`, stops
                 the recording and saves it. `all` cannot be recorded.
  stop           Stop a recording in progress

Options:
  -o, --output PATH   Write to PATH instead of the screenshot or recording
                      folder (`-` writes a screenshot to stdout)
      --no-save       Do not save a screenshot file
      --no-copy       Do not copy the result to the clipboard
      --no-notify     Do not show a notification
      --mic           Record the microphone, mixed with the output audio
      --no-mic        Do not record the microphone
      --no-audio      Do not record output audio
  -h, --help          Show this help

In region mode:
  Drag             Select an area; hold Shift for a square
  Click / Enter    Capture the window, or the screen, under the pointer
  R / P            Region tool / pen tool
  W / S / A        Capture the focused window / this screen / all screens
  Ctrl+Z           Undo the last pen stroke (Ctrl+Shift+Z redoes)
  Esc, right-click Cancel

Exit status is 0 when a shot or recording was saved, 1 when it was cancelled or
failed, and 2 for invalid arguments.";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    #[default]
    Region,
    Window,
    Screen,
    All,
}

/// What the command line asks for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Action {
    #[default]
    Screenshot,
    Record,
    Stop,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ShotArgs {
    action: Action,
    mode: Mode,
    output: Option<PathBuf>,
    no_save: bool,
    no_copy: bool,
    no_notify: bool,
    /// `Some` only when `--mic` or `--no-mic` was given, so the config decides
    /// otherwise.
    microphone: Option<bool>,
    no_audio: bool,
    help: bool,
}

impl ShotArgs {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut mode_seen = false;
        let mut positionals = 0;
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => parsed.help = true,
                "--no-save" => parsed.no_save = true,
                "--no-copy" => parsed.no_copy = true,
                "--no-notify" => parsed.no_notify = true,
                "--mic" => parsed.microphone = Some(true),
                "--no-mic" => parsed.microphone = Some(false),
                "--no-audio" => parsed.no_audio = true,
                "-o" | "--output" => {
                    let path = args.next().ok_or_else(|| format!("{arg} needs a path"))?;
                    parsed.output = Some(PathBuf::from(path));
                }
                other if other.starts_with("--output=") => {
                    parsed.output = Some(PathBuf::from(&other["--output=".len()..]));
                }
                other if other.starts_with('-') && other != "-" => {
                    return Err(format!("unknown option: {other}"));
                }
                "record" if positionals == 0 => {
                    positionals += 1;
                    parsed.action = Action::Record;
                }
                "stop" if positionals == 0 => {
                    positionals += 1;
                    parsed.action = Action::Stop;
                }
                mode => {
                    positionals += 1;
                    if parsed.action == Action::Stop {
                        return Err(format!("`stop` takes no mode: {mode}"));
                    }
                    if mode_seen {
                        return Err(format!("more than one mode given: {mode}"));
                    }
                    mode_seen = true;
                    parsed.mode = match mode {
                        "region" | "area" => Mode::Region,
                        "window" => Mode::Window,
                        "screen" | "output" | "monitor" => Mode::Screen,
                        "all" | "screens" => Mode::All,
                        other => return Err(format!("unknown mode: {other}")),
                    };
                }
            }
        }

        parsed.validate()?;
        Ok(parsed)
    }

    /// Rejects combinations that would otherwise be silently ignored.
    fn validate(&self) -> Result<(), String> {
        let recording = matches!(self.action, Action::Record | Action::Stop);
        if !recording && (self.microphone.is_some() || self.no_audio) {
            return Err("--mic, --no-mic and --no-audio only apply to `record`".to_string());
        }
        if self.action == Action::Record {
            if self.mode == Mode::All {
                return Err(
                    "all screens cannot be recorded at once; use `record screen` for one"
                        .to_string(),
                );
            }
            if self.no_save {
                return Err("a recording is always saved; --no-save does not apply".to_string());
            }
            if let Some(output) = &self.output {
                if output == std::path::Path::new("-") {
                    return Err("a recording cannot be written to stdout".to_string());
                }
                if output.extension().is_none() {
                    return Err(format!(
                        "{} needs an extension such as .mp4 or .mkv, which picks the container",
                        output.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn record_options(&self) -> record::RecordOptions {
        record::RecordOptions {
            mode: (self.action == Action::Record).then_some(self.mode),
            output: self.output.clone(),
            audio: self.no_audio.then_some(false),
            microphone: self.microphone,
            copy: !self.no_copy,
            notify: !self.no_notify,
        }
    }

    fn destination(&self, config: &AppConfig) -> Destination {
        let mut destination = Destination::from_config(&config.shot);
        if self.no_save {
            destination.directory = None;
        }
        if let Some(output) = &self.output {
            destination.explicit = Some(output.clone());
        }
        // Writing the image to stdout is for piping; a clipboard copy and a
        // notification nobody asked for would only get in the way of that.
        let to_stdout = self
            .output
            .as_deref()
            .is_some_and(|path| path == std::path::Path::new("-"));
        destination.copy = destination.copy && !self.no_copy && !to_stdout;
        destination.notify = destination.notify && !self.no_notify && !to_stdout;
        destination
    }
}

pub fn run() -> glib::ExitCode {
    init_logging();

    let args = match ShotArgs::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("ioexplorer-shot: {error}\n\n{USAGE}");
            return glib::ExitCode::from(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }
    if args.action != Action::Screenshot {
        return record::run(args.record_options());
    }

    // Region mode stays unique, so a second press of the hotkey while the
    // overlay is up does nothing instead of freezing the overlay itself. The
    // immediate modes must always run: they finish in a moment, and one
    // swallowed because an overlay happened to be open is a lost screenshot.
    let flags = match args.mode {
        Mode::Region => gio::ApplicationFlags::empty(),
        _ => gio::ApplicationFlags::NON_UNIQUE,
    };
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();

    let exit_code = Rc::new(Cell::new(0_u8));
    let started = Rc::new(Cell::new(false));

    app.connect_activate({
        let exit_code = Rc::clone(&exit_code);
        move |app| {
            if started.replace(true) {
                return;
            }
            start(app, &args, &exit_code);
        }
    });

    let argv0 = std::env::args().next().unwrap_or_default();
    let status = app.run_with_args(&[argv0]);
    match exit_code.get() {
        0 => status,
        code => glib::ExitCode::from(code),
    }
}

fn start(app: &gtk::Application, args: &ShotArgs, exit_code: &Rc<Cell<u8>>) {
    let Some(display) = gdk::Display::default() else {
        eprintln!("ioexplorer-shot: no display");
        exit_code.set(1);
        return;
    };

    let config = AppConfig::load();
    let destination = args.destination(&config);
    let hold = app.hold();

    // The compositor query runs beside the capture rather than after it: both
    // are only a few milliseconds of IPC each, but the capture is what the user
    // is waiting on.
    let started = Instant::now();
    let scene_worker = thread::spawn(compositor::query);
    let frozen = capture::freeze(&capture::outputs(&display));
    let scene = scene_worker.join().unwrap_or_default();
    tracing::debug!(elapsed = ?started.elapsed(), "screens frozen");

    let outputs = match frozen {
        Ok(outputs) => outputs,
        Err(error) => {
            fail(&error, &destination, exit_code);
            drop(hold);
            return;
        }
    };

    let area = match args.mode {
        Mode::Region => {
            open_overlay(
                app,
                &display,
                &config,
                outputs,
                scene,
                destination,
                exit_code,
                hold,
            );
            return;
        }
        Mode::Window => scene
            .focused_window()
            .map(|window| window.rect)
            .ok_or_else(|| "there is no focused window to capture".to_string()),
        Mode::Screen => focused_screen(&outputs, &scene)
            .ok_or_else(|| "there is no screen to capture".to_string()),
        Mode::All => Rect::bounding(outputs.iter().map(|output| &output.rect))
            .ok_or_else(|| "there are no screens to capture".to_string()),
    };

    match area {
        Ok(area) => finish_shot(app, &outputs, area, &[], &destination, exit_code, hold),
        Err(error) => {
            fail(&error, &destination, exit_code);
            drop(hold);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn open_overlay(
    app: &gtk::Application,
    display: &gdk::Display,
    config: &AppConfig,
    outputs: Vec<FrozenOutput>,
    scene: Scene,
    destination: Destination,
    exit_code: &Rc<Cell<u8>>,
    hold: gio::ApplicationHoldGuard,
) {
    // The toolbar is styled by the bundled and user stylesheets, and the
    // selection outline takes the theme's accent.
    let _user_css = theme::install(config);
    let accent =
        theme::load_generated_settings(theme::effective_custom_css_path(config).as_deref()).accent;

    // The overlay keeps itself alive through its windows' signal handlers only
    // weakly, so something has to own it until it finishes.
    let keep: Rc<RefCell<Option<Rc<Overlay>>>> = Rc::new(RefCell::new(None));
    let frozen = outputs.clone();
    let overlay = Overlay::open(app, display, outputs, scene, accent, Purpose::Screenshot, {
        let app = app.clone();
        let exit_code = Rc::clone(exit_code);
        let keep = Rc::clone(&keep);
        move |finish| {
            match finish {
                Finish::Capture { area, annotations } => {
                    let marks: Vec<&dyn Annotation> = annotations
                        .iter()
                        .map(|annotation| annotation.as_ref())
                        .collect();
                    finish_shot(&app, &frozen, area, &marks, &destination, &exit_code, hold);
                }
                Finish::Cancel => {
                    // A failure status, so a script can tell "nothing was
                    // taken" apart from a shot it can go on to use.
                    exit_code.set(1);
                    drop(hold);
                }
            }
            keep.borrow_mut().take();
        }
    });
    *keep.borrow_mut() = Some(overlay);
}

/// Renders, encodes and delivers a shot, then lets the application exit —
/// unless the clipboard has to be served from this process, in which case it
/// lingers, invisibly, until another client takes the clipboard over.
fn finish_shot(
    app: &gtk::Application,
    outputs: &[FrozenOutput],
    area: Rect,
    annotations: &[&dyn Annotation],
    destination: &Destination,
    exit_code: &Rc<Cell<u8>>,
    hold: gio::ApplicationHoldGuard,
) {
    let started = Instant::now();
    let result = capture::compose(outputs, area, annotations).and_then(|texture| {
        tracing::debug!(elapsed = ?started.elapsed(), "shot composed");
        let png = capture::encode_png(&texture)?;
        tracing::debug!(elapsed = ?started.elapsed(), bytes = png.len(), "shot encoded");
        deliver::deliver(&texture, &png, destination)
    });
    tracing::debug!(elapsed = ?started.elapsed(), "shot delivered");

    match result {
        Ok(delivered) => {
            if let Some(path) = &delivered.path {
                tracing::info!(path = %path.display(), "screenshot saved");
            }
            if delivered.serving_clipboard {
                linger_for_clipboard(app, hold);
                return;
            }
        }
        Err(error) => fail(&error, destination, exit_code),
    }
    drop(hold);
}

/// Keeps the process alive while it owns the clipboard. Without `wl-copy` to
/// hand the image to, exiting would take the copied image with it.
fn linger_for_clipboard(app: &gtk::Application, hold: gio::ApplicationHoldGuard) {
    let Some(display) = gdk::Display::default() else {
        drop(hold);
        return;
    };
    let clipboard = display.clipboard();
    let hold = RefCell::new(Some(hold));
    let app = app.clone();
    clipboard.connect_changed(move |clipboard| {
        if !clipboard.is_local() {
            hold.borrow_mut().take();
            app.quit();
        }
    });
}

fn focused_screen(outputs: &[FrozenOutput], scene: &Scene) -> Option<Rect> {
    let focused = scene.focused_output.as_deref();
    outputs
        .iter()
        .find(|output| Some(output.name.as_str()) == focused)
        .or_else(|| {
            let cursor = scene.cursor?;
            outputs.iter().find(|output| output.rect.contains(cursor))
        })
        .or_else(|| outputs.first())
        .map(|output| output.rect)
}

fn fail(error: &str, destination: &Destination, exit_code: &Rc<Cell<u8>>) {
    tracing::error!(%error, "screenshot failed");
    eprintln!("ioexplorer-shot: {error}");
    if destination.notify {
        deliver::notify_failure(error);
    }
    exit_code.set(1);
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Logs go to stderr: `--output -` puts the image itself on stdout.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShotConfig;

    fn parse(args: &[&str]) -> Result<ShotArgs, String> {
        ShotArgs::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn record_takes_a_mode_and_audio_flags() {
        let args = parse(&["record", "window", "--mic", "--no-copy"]).unwrap();
        assert_eq!(args.action, Action::Record);
        assert_eq!(args.mode, Mode::Window);

        let options = args.record_options();
        assert_eq!(options.mode, Some(Mode::Window));
        assert_eq!(options.microphone, Some(true));
        assert_eq!(options.audio, None, "left to the config");
        assert!(!options.copy);

        let region = parse(&["record", "--no-audio"]).unwrap();
        assert_eq!(region.mode, Mode::Region);
        assert_eq!(region.record_options().audio, Some(false));
    }

    #[test]
    fn stop_reaches_only_a_running_recording() {
        let args = parse(&["stop"]).unwrap();
        assert_eq!(args.action, Action::Stop);
        assert_eq!(args.record_options().mode, None);
        assert!(parse(&["stop", "window"]).is_err());
    }

    #[test]
    fn recording_rejects_what_it_cannot_do() {
        assert!(parse(&["record", "all"]).is_err());
        assert!(parse(&["record", "-o", "-"]).is_err());
        assert!(parse(&["record", "-o", "/tmp/clip"]).is_err());
        assert!(parse(&["record", "--no-save"]).is_err());
        assert!(parse(&["window", "--mic"]).is_err());
        assert!(parse(&["record", "-o", "/tmp/clip.mkv"]).is_ok());
    }

    #[test]
    fn record_is_only_a_command_in_first_position() {
        assert!(parse(&["window", "record"]).is_err());
        assert!(parse(&["record", "record"]).is_err());
    }

    #[test]
    fn region_is_the_default_mode() {
        assert_eq!(parse(&[]).unwrap().mode, Mode::Region);
    }

    #[test]
    fn modes_and_their_aliases_parse() {
        assert_eq!(parse(&["window"]).unwrap().mode, Mode::Window);
        assert_eq!(parse(&["screen"]).unwrap().mode, Mode::Screen);
        assert_eq!(parse(&["monitor"]).unwrap().mode, Mode::Screen);
        assert_eq!(parse(&["all"]).unwrap().mode, Mode::All);
        assert_eq!(parse(&["region"]).unwrap().mode, Mode::Region);
    }

    #[test]
    fn options_parse_in_any_order() {
        let args = parse(&["--no-copy", "all", "-o", "/tmp/x.png", "--no-notify"]).unwrap();
        assert_eq!(args.mode, Mode::All);
        assert_eq!(args.output, Some(PathBuf::from("/tmp/x.png")));
        assert!(args.no_copy && args.no_notify && !args.no_save);

        let joined = parse(&["--output=/tmp/y.png"]).unwrap();
        assert_eq!(joined.output, Some(PathBuf::from("/tmp/y.png")));
    }

    #[test]
    fn mistakes_are_reported_not_guessed() {
        assert!(parse(&["windw"]).is_err());
        assert!(parse(&["window", "all"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["-o"]).is_err());
    }

    #[test]
    fn flags_override_the_config() {
        let config = AppConfig::default();
        let args = parse(&["--no-save", "--no-copy"]).unwrap();
        let destination = args.destination(&config);

        assert_eq!(destination.directory, None);
        assert!(!destination.copy);
        assert!(destination.notify);
    }

    #[test]
    fn a_disabled_config_option_stays_off() {
        let config = AppConfig {
            shot: ShotConfig {
                copy: false,
                ..ShotConfig::default()
            },
            ..AppConfig::default()
        };
        assert!(!parse(&[]).unwrap().destination(&config).copy);
    }

    #[test]
    fn stdout_output_is_quiet() {
        let destination = parse(&["-o", "-"])
            .unwrap()
            .destination(&AppConfig::default());

        assert_eq!(destination.explicit, Some(PathBuf::from("-")));
        assert!(!destination.copy);
        assert!(!destination.notify);
    }

    #[test]
    fn the_focused_screen_prefers_the_compositor_then_the_cursor() {
        use gtk::{gdk, glib};

        let texture: gdk::Texture = gdk::MemoryTexture::new(
            1,
            1,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(vec![0_u8; 4]),
            4,
        )
        .upcast();
        let outputs = vec![
            FrozenOutput {
                name: "DP-1".to_string(),
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
                image: texture.clone(),
            },
            FrozenOutput {
                name: "DP-2".to_string(),
                rect: Rect::new(100.0, 0.0, 100.0, 100.0),
                image: texture,
            },
        ];

        let by_focus = Scene {
            focused_output: Some("DP-2".to_string()),
            cursor: Some(geometry::Point::new(10.0, 10.0)),
            ..Scene::default()
        };
        assert_eq!(focused_screen(&outputs, &by_focus), Some(outputs[1].rect));

        let by_cursor = Scene {
            cursor: Some(geometry::Point::new(150.0, 10.0)),
            ..Scene::default()
        };
        assert_eq!(focused_screen(&outputs, &by_cursor), Some(outputs[1].rect));

        assert_eq!(
            focused_screen(&outputs, &Scene::default()),
            Some(outputs[0].rect)
        );
    }
}
