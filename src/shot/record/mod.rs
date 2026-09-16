//! `ioexplorer-shot record`: screen recording through `wf-recorder`.
//!
//! One recording runs at a time, enforced by the application id: running any
//! `record` command, or `stop`, while one is in progress reaches the running
//! process instead of starting another, and stops it. The same key that
//! started a recording therefore ends it.
//!
//! A recording goes through a few phases:
//!
//! 1. **Choosing.** `region` freezes the screen and opens the overlay, confined
//!    to one output; `window` and `screen` take their area from the compositor.
//! 2. **Recording.** Audio is probed and, for output plus microphone, mixed.
//!    `wf-recorder` writes an MPEG-TS stream beside the final file, and an
//!    indicator with a Stop button appears on another screen.
//! 3. **Finishing.** The recorder is asked to stop with SIGINT, which lets it
//!    flush; it is killed if it does not. The stream is remuxed, without
//!    re-encoding, into the final container, and the temporary stream deleted.

mod audio;
mod indicator;
mod recorder;

use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use gtk::{gdk, gio, glib, prelude::*};

use super::{
    Mode,
    capture::{self, OutputInfo},
    compositor::{self, Scene},
    deliver,
    geometry::Rect,
    overlay::{Finish, Overlay, Purpose},
};
use crate::{
    config::{AppConfig, RecordConfig},
    theme,
};

use audio::PreparedAudio;
use indicator::Indicator;
use recorder::Placement;

const APP_ID: &str = "io.github.ionix.IoExplorer.Shot.Recorder";

/// How long the recorder gets to finish writing after SIGINT before it is
/// killed. Flushing an encoder is quick; this only matters when it has hung.
const STOP_DEADLINE: Duration = Duration::from_secs(8);

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// A recording request, after the command line and config are combined.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordOptions {
    /// `None` for `stop`, which only ever reaches a running recording.
    pub mode: Option<Mode>,
    pub output: Option<PathBuf>,
    pub audio: Option<bool>,
    pub microphone: Option<bool>,
    pub copy: bool,
    pub notify: bool,
}

pub fn run(options: RecordOptions) -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    let exit_code = Rc::new(Cell::new(0_u8));
    let session: Rc<RefCell<Option<Rc<Recording>>>> = Rc::new(RefCell::new(None));

    // Only the first activation starts anything. Every later one comes from
    // another invocation while this one runs, and means "stop".
    app.connect_activate({
        let exit_code = Rc::clone(&exit_code);
        let session = Rc::clone(&session);
        move |app| {
            let existing = session.borrow().clone();
            match existing {
                Some(recording) => recording.interrupt(),
                None => {
                    let Some(mode) = options.mode else {
                        eprintln!("ioexplorer-shot: no recording is in progress");
                        exit_code.set(1);
                        return;
                    };
                    let recording = Recording::new(app, &options, mode, &exit_code);
                    *session.borrow_mut() = Some(Rc::clone(&recording));
                    recording.start();
                }
            }
        }
    });

    let argv0 = std::env::args().next().unwrap_or_default();
    let status = app.run_with_args(&[argv0]);
    match exit_code.get() {
        0 => status,
        code => glib::ExitCode::from(code),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Choosing,
    Recording,
    Stopping,
    Done,
}

struct Recording {
    app: gtk::Application,
    config: RecordConfig,
    mode: Mode,
    explicit_output: Option<PathBuf>,
    audio_requested: bool,
    microphone_requested: bool,
    copy: bool,
    notify: bool,
    accent: gdk::RGBA,
    exit_code: Rc<Cell<u8>>,

    phase: Cell<Phase>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    overlay: RefCell<Option<Rc<Overlay>>>,
    outputs: RefCell<Vec<OutputInfo>>,
    scene: RefCell<Scene>,
    process: RefCell<Option<gio::Subprocess>>,
    audio: RefCell<PreparedAudio>,
    indicator: RefCell<Option<Indicator>>,
    started: Cell<Option<Instant>>,
    /// The transport stream being written, and the file it becomes.
    stream: RefCell<Option<PathBuf>>,
    destination: RefCell<Option<PathBuf>>,
}

impl Recording {
    fn new(
        app: &gtk::Application,
        options: &RecordOptions,
        mode: Mode,
        exit_code: &Rc<Cell<u8>>,
    ) -> Rc<Self> {
        let config = AppConfig::load();
        // Needed for the overlay's toolbar and the indicator.
        let _user_css = theme::install(&config);
        let record = config.shot.record.clone();
        let accent =
            theme::load_generated_settings(theme::effective_custom_css_path(&config).as_deref())
                .accent;

        Rc::new(Self {
            app: app.clone(),
            mode,
            explicit_output: options.output.clone(),
            audio_requested: options.audio.unwrap_or(record.audio),
            microphone_requested: options.microphone.unwrap_or(record.microphone),
            copy: options.copy && config.shot.copy,
            notify: options.notify && config.shot.notify,
            accent,
            config: record,
            exit_code: Rc::clone(exit_code),
            phase: Cell::new(Phase::Choosing),
            hold: RefCell::new(Some(app.hold())),
            overlay: RefCell::new(None),
            outputs: RefCell::new(Vec::new()),
            scene: RefCell::new(Scene::default()),
            process: RefCell::new(None),
            audio: RefCell::new(PreparedAudio::default()),
            indicator: RefCell::new(None),
            started: Cell::new(None),
            stream: RefCell::new(None),
            destination: RefCell::new(None),
        })
    }

    fn start(self: &Rc<Self>) {
        let Some(display) = gdk::Display::default() else {
            self.fail("no display");
            return;
        };
        let outputs = capture::outputs(&display);
        *self.outputs.borrow_mut() = outputs.clone();
        self.install_signal_handlers();

        match self.mode {
            Mode::Region => self.choose_region(&display, &outputs),
            Mode::Window => {
                let scene = compositor::query();
                let area = scene.focused_window().map(|window| window.rect);
                *self.scene.borrow_mut() = scene;
                match area {
                    Some(area) => self.begin(area),
                    None => self.fail("there is no focused window to record"),
                }
            }
            Mode::Screen => {
                let scene = compositor::query();
                let area = focused_output(&outputs, &scene).map(|output| output.rect);
                *self.scene.borrow_mut() = scene;
                match area {
                    Some(area) => self.begin(area),
                    None => self.fail("there is no screen to record"),
                }
            }
            Mode::All => self.fail("all screens cannot be recorded at once"),
        }
    }

    fn choose_region(self: &Rc<Self>, display: &gdk::Display, outputs: &[OutputInfo]) {
        let scene_worker = thread::spawn(compositor::query);
        let frozen = capture::freeze(outputs);
        let scene = scene_worker.join().unwrap_or_default();
        *self.scene.borrow_mut() = scene.clone();

        let frozen = match frozen {
            Ok(frozen) => frozen,
            Err(error) => {
                self.fail(&error);
                return;
            }
        };

        let accent = self.accent;
        let weak = Rc::downgrade(self);
        let overlay = Overlay::open(
            &self.app,
            display,
            frozen,
            scene,
            accent,
            Purpose::Recording,
            move |finish| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.overlay.borrow_mut().take();
                match finish {
                    Finish::Capture { area, .. } => this.begin(area),
                    Finish::Cancel => this.cancel(),
                }
            },
        );
        *self.overlay.borrow_mut() = Some(overlay);
    }

    /// Starts recording `area`.
    fn begin(self: &Rc<Self>, area: Rect) {
        let outputs = self.outputs.borrow().clone();
        let Some(placement) = recorder::place(area, &outputs) else {
            self.fail("the chosen area is not on any screen");
            return;
        };

        let destination = match self.destination_path() {
            Ok(path) => path,
            Err(error) => {
                self.fail(&error);
                return;
            }
        };
        let stream = stream_path(&destination);

        let audio = audio::prepare(self.audio_requested, self.microphone_requested);
        if self.notify && !audio.warnings.is_empty() {
            deliver::notify(
                "Recording without some audio",
                &audio.warnings.join("\n"),
                "audio-input-microphone",
            );
        }

        let codec = self
            .config
            .codec
            .clone()
            .filter(|codec| !codec.trim().is_empty())
            .unwrap_or_else(|| {
                recorder::default_codec(recorder::nvidia_driver_loaded()).to_string()
            });
        let settings = recorder::Settings {
            codec,
            framerate: self.config.framerate,
            audio_device: audio.device.clone(),
        };
        let arguments = recorder::arguments(&placement.target, &settings, &stream);
        tracing::info!(?placement.target, codec = %settings.codec, audio = ?settings.audio_device, "starting a recording");

        let process = match spawn("wf-recorder", &arguments, &log_path()) {
            Ok(process) => process,
            Err(error) => {
                drop(audio);
                self.fail(&error);
                return;
            }
        };

        let microphone = audio.microphone;
        *self.audio.borrow_mut() = audio;
        *self.stream.borrow_mut() = Some(stream);
        *self.destination.borrow_mut() = Some(destination);
        self.started.set(Some(Instant::now()));
        self.phase.set(Phase::Recording);

        process.wait_async(None::<&gio::Cancellable>, {
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.recorder_exited();
                }
            }
        });
        *self.process.borrow_mut() = Some(process);

        self.show_indicator(&placement, microphone);
    }

    fn show_indicator(self: &Rc<Self>, placement: &Placement, microphone: bool) {
        if !self.config.indicator {
            return;
        }
        let outputs = self.outputs.borrow();
        let scene = self.scene.borrow();
        let preferred = scene
            .cursor
            .and_then(|cursor| outputs.iter().find(|output| output.rect.contains(cursor)))
            .map(|output| output.name.clone())
            .or_else(|| scene.focused_output.clone());
        let Some(output) = indicator::choose_output(
            &outputs,
            &placement.output,
            placement.area,
            preferred.as_deref(),
        ) else {
            tracing::info!("no screen outside the recording to show the indicator on");
            return;
        };
        let Some(monitor) = monitor_named(&output.name) else {
            return;
        };

        let weak = Rc::downgrade(self);
        let indicator = Indicator::show(
            &self.app,
            &monitor,
            self.started.get().unwrap_or_else(Instant::now),
            microphone,
            move || {
                if let Some(this) = weak.upgrade() {
                    this.stop();
                }
            },
        );
        *self.indicator.borrow_mut() = Some(indicator);
    }

    /// Another invocation, or a signal: cancel a choice in progress, or stop
    /// the recording.
    fn interrupt(self: &Rc<Self>) {
        match self.phase.get() {
            Phase::Choosing => {
                let overlay = self.overlay.borrow().clone();
                match overlay {
                    Some(overlay) => overlay.cancel(),
                    None => self.cancel(),
                }
            }
            Phase::Recording => self.stop(),
            Phase::Stopping | Phase::Done => {}
        }
    }

    fn stop(self: &Rc<Self>) {
        if self.phase.get() != Phase::Recording {
            return;
        }
        self.phase.set(Phase::Stopping);
        self.close_indicator();

        let Some(process) = self.process.borrow().clone() else {
            return;
        };
        tracing::info!("stopping the recording");
        process.send_signal(SIGINT);

        // `recorder_exited` runs either way; this only makes sure it does.
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(STOP_DEADLINE, move || {
            if let Some(this) = weak.upgrade()
                && this.phase.get() == Phase::Stopping
                && let Some(process) = this.process.borrow().as_ref()
            {
                tracing::warn!("the recorder did not stop in time; killing it");
                process.force_exit();
            }
        });
    }

    fn recorder_exited(self: &Rc<Self>) {
        let expected = self.phase.get() == Phase::Stopping;
        let process = self.process.borrow_mut().take();
        self.phase.set(Phase::Done);
        self.close_indicator();
        self.audio.borrow_mut().teardown();

        let (Some(stream), Some(destination)) = (
            self.stream.borrow().clone(),
            self.destination.borrow().clone(),
        ) else {
            self.finish();
            return;
        };

        let recorded = fs::metadata(&stream).map(|meta| meta.len()).unwrap_or(0);
        if recorded == 0 {
            let _ = fs::remove_file(&stream);
            let reason = match expected {
                true => "nothing was recorded".to_string(),
                false => recorder_failure(process.as_ref()),
            };
            self.fail(&reason);
            return;
        }
        if !expected {
            // Still remuxed: whatever was captured before the failure is kept.
            tracing::warn!(reason = %recorder_failure(process.as_ref()), "the recorder stopped on its own");
        }

        self.remux(stream, destination, expected);
    }

    fn remux(self: &Rc<Self>, stream: PathBuf, destination: PathBuf, expected: bool) {
        let arguments = recorder::remux_arguments(&stream, &destination);
        let process = match spawn("ffmpeg", &arguments, &log_path_named("remux")) {
            Ok(process) => process,
            Err(error) => {
                self.fail(&format!(
                    "{error}; the raw recording is at {}",
                    stream.display()
                ));
                return;
            }
        };

        let weak = Rc::downgrade(self);
        let waited = process.clone();
        process.wait_async(None::<&gio::Cancellable>, move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let succeeded = waited.is_successful()
                && fs::metadata(&destination).is_ok_and(|meta| meta.len() > 0);
            if !succeeded {
                this.fail(&format!(
                    "the recording could not be finished; the raw capture is at {}",
                    stream.display()
                ));
                return;
            }
            let _ = fs::remove_file(&stream);

            if expected {
                this.delivered(&destination);
            } else {
                this.fail(&format!(
                    "the recorder stopped unexpectedly; what was captured is saved to {}",
                    destination.display()
                ));
            }
        });
    }

    fn delivered(&self, path: &Path) {
        let duration = self
            .started
            .get()
            .map(|started| indicator::format_elapsed(started.elapsed()))
            .unwrap_or_default();
        tracing::info!(path = %path.display(), %duration, "recording saved");

        let copied = self.copy
            && deliver::copy_file(path)
                .map_err(|error| tracing::warn!(%error, "cannot copy the recording"))
                .is_ok();
        if self.notify {
            let summary = match copied {
                true => "Recording saved and copied",
                false => "Recording saved",
            };
            deliver::notify(
                summary,
                &format!("{} · {duration}", path.display()),
                "video-x-generic",
            );
        }
        self.finish();
    }

    fn cancel(&self) {
        // A failure status, as for a cancelled screenshot.
        self.exit_code.set(1);
        self.phase.set(Phase::Done);
        self.finish();
    }

    fn fail(&self, error: &str) {
        tracing::error!(%error, "recording failed");
        eprintln!("ioexplorer-shot: {error}");
        if self.notify {
            deliver::notify("Recording failed", error, "dialog-error");
        }
        self.exit_code.set(1);
        self.phase.set(Phase::Done);
        self.close_indicator();
        self.audio.borrow_mut().teardown();
        self.finish();
    }

    fn finish(&self) {
        self.hold.borrow_mut().take();
    }

    fn close_indicator(&self) {
        if let Some(indicator) = self.indicator.borrow_mut().take() {
            indicator.close();
        }
    }

    /// Ctrl+C in a terminal, or `kill`, stops and saves rather than leaving a
    /// raw stream and a mix device behind.
    fn install_signal_handlers(self: &Rc<Self>) {
        for signal in [SIGINT, SIGTERM] {
            let weak = Rc::downgrade(self);
            glib::unix_signal_add_local(signal as _, move || {
                if let Some(this) = weak.upgrade() {
                    this.interrupt();
                }
                glib::ControlFlow::Continue
            });
        }
    }

    fn destination_path(&self) -> Result<PathBuf, String> {
        if let Some(path) = &self.explicit_output {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            return Ok(path.clone());
        }

        let directory = self.config.directory_path();
        fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        let stem = deliver::expand_file_name(
            &self.config.file_name,
            &glib::DateTime::now_local().ok(),
            "mp4",
        );
        Ok(deliver::unique_path(&directory, &stem, "mp4", |path| {
            path.exists() || stream_path(path).exists()
        }))
    }
}

/// The transport stream written while recording, beside its final file:
/// `Recording.mp4` records into `Recording.part.ts`.
fn stream_path(destination: &Path) -> PathBuf {
    destination.with_extension("part.ts")
}

fn focused_output<'a>(outputs: &'a [OutputInfo], scene: &Scene) -> Option<&'a OutputInfo> {
    let focused = scene.focused_output.as_deref();
    outputs
        .iter()
        .find(|output| Some(output.name.as_str()) == focused)
        .or_else(|| {
            let cursor = scene.cursor?;
            outputs.iter().find(|output| output.rect.contains(cursor))
        })
        .or_else(|| outputs.first())
}

fn monitor_named(name: &str) -> Option<gdk::Monitor> {
    gdk::Display::default()?
        .monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .find(|monitor| monitor.connector().as_deref() == Some(name))
}

fn log_path() -> PathBuf {
    log_path_named("record")
}

/// A log per step in the runtime directory, overwritten each time: enough to
/// explain the last failure, and never an accumulating file.
fn log_path_named(step: &str) -> PathBuf {
    glib::user_runtime_dir().join(format!("ioexplorer-shot-{step}.log"))
}

fn spawn(
    program: &str,
    arguments: &[std::ffi::OsString],
    log: &Path,
) -> Result<gio::Subprocess, String> {
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
    launcher.set_stdout_file_path(Some(log));
    launcher.set_stderr_file_path(Some(log));

    let mut argv: Vec<&std::ffi::OsStr> = vec![program.as_ref()];
    argv.extend(arguments.iter().map(|argument| argument.as_os_str()));
    launcher.spawn(&argv).map_err(|error| {
        if error.matches(gio::IOErrorEnum::NotFound)
            || error.message().contains("No such file or directory")
        {
            format!("{program} is not installed; recording needs it")
        } else {
            format!("cannot run {program}: {error}")
        }
    })
}

/// Why the recorder stopped, from its exit status and the tail of its log.
fn recorder_failure(process: Option<&gio::Subprocess>) -> String {
    let status = process
        .map(|process| match process.has_exited() {
            true => format!("wf-recorder exited with status {}", process.exit_status()),
            false => "wf-recorder was killed".to_string(),
        })
        .unwrap_or_else(|| "wf-recorder stopped".to_string());

    let log = fs::read_to_string(log_path()).unwrap_or_default();
    match log_excerpt(&log) {
        Some(excerpt) => format!("{status}: {excerpt}"),
        None => status,
    }
}

/// The most telling line of a recorder log: the last one that reads like an
/// error, or failing that the last line at all.
fn log_excerpt(log: &str) -> Option<String> {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            [
                "error",
                "failed",
                "invalid",
                "cannot",
                "could not",
                "unable",
                "no such",
            ]
            .iter()
            .any(|needle| lower.contains(needle))
        })
        .or_else(|| lines.last())
        .map(|line| line.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_sits_beside_its_final_file() {
        assert_eq!(
            stream_path(Path::new("/videos/Recording_1.mp4")),
            PathBuf::from("/videos/Recording_1.part.ts")
        );
        assert_eq!(
            stream_path(Path::new("clip.mkv")),
            PathBuf::from("clip.part.ts")
        );
    }

    #[test]
    fn the_log_excerpt_prefers_an_error_line() {
        let log = "\
Using video encoder: libx264
[libx264] frame I:1
Failed to open output: Permission denied
[libx264] kb/s: 12.0
";
        assert_eq!(
            log_excerpt(log).as_deref(),
            Some("Failed to open output: Permission denied")
        );
        assert_eq!(
            log_excerpt("just one line\n\n").as_deref(),
            Some("just one line")
        );
        assert_eq!(log_excerpt("   \n"), None);
    }
}
