//! The `wf-recorder` command line, and fitting a chosen area to what it can
//! record.
//!
//! Kept free of process handling so every decision about the arguments is
//! testable.

use std::{ffi::OsString, path::Path};

use crate::shot::{capture::OutputInfo, geometry::Rect};

/// What `wf-recorder` is pointed at.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A whole output, by connector name.
    Output(String),
    /// Part of one output, in global logical coordinates.
    Area(Rect),
}

/// Where a recording happens: the target, plus the output it is on.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub target: Target,
    pub output: OutputInfo,
    pub area: Rect,
}

/// Fits `area` to a single output, which is all `wf-recorder` can capture.
///
/// The output holding most of the area wins, and the area is clipped to it —
/// a window hanging off the edge of its screen records the part that is on
/// it. An area that is a whole output becomes that output by name, which lets
/// the recorder skip cropping altogether.
pub fn place(area: Rect, outputs: &[OutputInfo]) -> Option<Placement> {
    let (output, clipped) = outputs
        .iter()
        .filter_map(|output| {
            let clipped = area.snapped_out().intersection(&output.rect)?;
            Some((output, clipped))
        })
        .max_by(|(_, a), (_, b)| (a.width * a.height).total_cmp(&(b.width * b.height)))?;

    let target = if clipped == output.rect {
        Target::Output(output.name.clone())
    } else {
        Target::Area(clipped)
    };
    Some(Placement {
        target,
        output: output.clone(),
        area: clipped,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub codec: String,
    /// `0` records on damage only.
    pub framerate: u32,
    pub audio_device: Option<String>,
}

/// The encoder when none is configured: NVENC on an NVIDIA driver, whose
/// encoder is on the card and costs almost nothing, else x264.
///
/// VAAPI is not auto-picked: it needs a render device and a matching filter
/// chain chosen per machine, which a guess gets wrong as often as right.
pub fn default_codec(nvidia_driver_loaded: bool) -> &'static str {
    if nvidia_driver_loaded {
        "h264_nvenc"
    } else {
        "libx264"
    }
}

pub fn nvidia_driver_loaded() -> bool {
    Path::new("/proc/driver/nvidia/version").exists()
}

/// Quality settings for the encoders this knows. Anything else runs on its
/// own defaults, which for NVENC in particular means a 2 Mbit/s cap that
/// smears text — hence spelling out a constant-quality mode.
fn codec_parameters(codec: &str) -> &'static [&'static str] {
    match codec {
        "h264_nvenc" | "hevc_nvenc" | "av1_nvenc" => &["preset=p5", "rc=vbr", "cq=23", "b=0"],
        "libx264" | "libx265" => &["preset=veryfast", "crf=21"],
        _ => &[],
    }
}

/// The full argument list, excluding the program name.
///
/// Output is always MPEG-TS, whatever the final file is. A transport stream
/// is readable up to its last complete packet, so if the recorder has to be
/// killed the capture survives; an MP4 or MKV killed mid-write has no index and
/// is lost entirely. The finished stream is remuxed into the real container.
pub fn arguments(target: &Target, settings: &Settings, file: &Path) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec!["-y".into()];

    match target {
        Target::Output(name) => {
            arguments.push("-o".into());
            arguments.push(name.into());
        }
        Target::Area(area) => {
            let area = area.snapped_out();
            arguments.push("-g".into());
            arguments.push(
                format!(
                    "{},{} {}x{}",
                    area.x as i64, area.y as i64, area.width as i64, area.height as i64
                )
                .into(),
            );
        }
    }

    arguments.push("-c".into());
    arguments.push(settings.codec.clone().into());
    for parameter in codec_parameters(&settings.codec) {
        arguments.push("-p".into());
        arguments.push((*parameter).into());
    }
    // Widely playable, and what browsers and chat apps expect.
    arguments.push("-x".into());
    arguments.push("yuv420p".into());

    if settings.framerate > 0 {
        arguments.push("-r".into());
        arguments.push(settings.framerate.to_string().into());
    }
    if let Some(device) = &settings.audio_device {
        // `--audio DEVICE` would be read as a flag without a value followed by
        // a stray argument; the device has to be joined with `=`.
        arguments.push(format!("--audio={device}").into());
    }

    arguments.push("-m".into());
    arguments.push("mpegts".into());
    arguments.push("-f".into());
    arguments.push(file.into());
    arguments
}

/// Arguments for `ffmpeg` to turn the finished transport stream into `output`
/// without re-encoding.
pub fn remux_arguments(input: &Path, output: &Path) -> Vec<OsString> {
    let mut arguments: Vec<OsString> =
        ["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]
            .into_iter()
            .map(OsString::from)
            .collect();
    arguments.push(input.into());
    arguments.push("-c".into());
    arguments.push("copy".into());

    // The index at the front, so a video starts playing before it has fully
    // downloaded — which is how it will be viewed once shared.
    let is_mp4 = output
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp4" | "m4v" | "mov"
            )
        });
    if is_mp4 {
        arguments.push("-movflags".into());
        arguments.push("+faststart".into());
    }

    arguments.push(output.into());
    arguments
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn outputs() -> Vec<OutputInfo> {
        vec![
            OutputInfo {
                name: "DP-1".to_string(),
                rect: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            },
            OutputInfo {
                name: "DP-2".to_string(),
                rect: Rect::new(1920.0, 0.0, 1920.0, 1080.0),
            },
        ]
    }

    fn strings(arguments: &[OsString]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_whole_screen_is_recorded_by_name() {
        let placement = place(Rect::new(1920.0, 0.0, 1920.0, 1080.0), &outputs()).unwrap();
        assert_eq!(placement.target, Target::Output("DP-2".to_string()));
    }

    #[test]
    fn an_area_goes_to_the_screen_holding_most_of_it_and_is_clipped() {
        // 120px on DP-1, 400px on DP-2.
        let placement = place(Rect::new(1800.0, 100.0, 520.0, 300.0), &outputs()).unwrap();

        assert_eq!(placement.output.name, "DP-2");
        assert_eq!(
            placement.target,
            Target::Area(Rect::new(1920.0, 100.0, 400.0, 300.0))
        );
    }

    #[test]
    fn an_area_off_every_screen_cannot_be_placed() {
        assert!(place(Rect::new(5000.0, 5000.0, 10.0, 10.0), &outputs()).is_none());
    }

    #[test]
    fn area_arguments_use_the_geometry_flag_and_mpegts() {
        let settings = Settings {
            codec: "libx264".to_string(),
            framerate: 60,
            audio_device: Some("mix.monitor".to_string()),
        };
        let arguments = strings(&arguments(
            &Target::Area(Rect::new(10.4, 20.0, 300.2, 200.0)),
            &settings,
            &PathBuf::from("/tmp/a.part.ts"),
        ));

        let joined = arguments.join(" ");
        assert!(joined.contains("-g 10,20 301x200"), "{joined}");
        assert!(
            joined.contains("-c libx264 -p preset=veryfast -p crf=21"),
            "{joined}"
        );
        assert!(joined.contains("-r 60"), "{joined}");
        assert!(arguments.contains(&"--audio=mix.monitor".to_string()));
        assert!(joined.ends_with("-m mpegts -f /tmp/a.part.ts"), "{joined}");
    }

    #[test]
    fn a_variable_frame_rate_and_silent_recording_leave_their_flags_out() {
        let settings = Settings {
            codec: "h264_nvenc".to_string(),
            framerate: 0,
            audio_device: None,
        };
        let arguments = strings(&arguments(
            &Target::Output("DP-1".to_string()),
            &settings,
            &PathBuf::from("out.ts"),
        ));

        assert!(arguments.windows(2).any(|pair| pair == ["-o", "DP-1"]));
        assert!(!arguments.contains(&"-r".to_string()));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.starts_with("--audio"))
        );
        assert!(arguments.contains(&"cq=23".to_string()));
    }

    #[test]
    fn nvidia_gets_nvenc_and_everyone_else_x264() {
        assert_eq!(default_codec(true), "h264_nvenc");
        assert_eq!(default_codec(false), "libx264");
    }

    #[test]
    fn remuxing_to_mp4_moves_the_index_to_the_front() {
        let mp4 = strings(&remux_arguments(Path::new("a.ts"), Path::new("a.MP4")));
        assert!(mp4.windows(2).any(|pair| pair == ["-c", "copy"]));
        assert!(mp4.contains(&"+faststart".to_string()));

        let mkv = strings(&remux_arguments(Path::new("a.ts"), Path::new("a.mkv")));
        assert!(!mkv.contains(&"+faststart".to_string()));
        assert_eq!(mkv.last().map(String::as_str), Some("a.mkv"));
    }
}
