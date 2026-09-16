//! Audio for a recording: what is playing, the microphone, or both mixed into
//! one track.
//!
//! `wf-recorder` records from exactly one PulseAudio source. Output audio alone
//! is the default sink's monitor and the microphone alone is the default
//! source; both at once need a device that carries the two mixed, which is a
//! temporary null sink with a loopback from each into it, recorded through its
//! monitor. It lives only as long as the recording and is removed afterwards —
//! and, should a crash leave one behind, before the next recording starts.
//!
//! Every source is probed before use. PipeWire clocks a mix from its inputs,
//! and a source that delivers nothing — a USB microphone in a bad state is
//! enough — stalls the whole mix. `wf-recorder` then never returns from
//! reading it, cannot stop, and leaves no playable file. Dropping a dead source
//! up front, with a warning, costs a fraction of a second and avoids all of it.

use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Name of the temporary mix sink. Fixed rather than per-process: only one
/// recording runs at a time, and a fixed name is what makes a leftover from a
/// crashed run findable.
pub const MIX_SINK: &str = "ioexplorer_shot_mix";

/// How long a source gets to produce its first samples.
const PROBE_DEADLINE: Duration = Duration::from_secs(2);

/// The device a recording reads from, plus whatever had to be created for it.
#[derive(Debug, Default)]
pub struct PreparedAudio {
    /// Handed to `wf-recorder --audio`. `None` records silence-free video.
    pub device: Option<String>,
    /// Whether the microphone actually made it into the recording.
    pub microphone: bool,
    /// Sources that were asked for but dropped, as sentences for a notification.
    pub warnings: Vec<String>,
    /// PulseAudio module ids to unload, in load order.
    modules: Vec<u32>,
}

impl PreparedAudio {
    /// Removes the mix device, if one was created. Safe to call twice.
    pub fn teardown(&mut self) {
        for module in self.modules.drain(..).rev() {
            if let Err(error) = pactl(&["unload-module", &module.to_string()]) {
                tracing::warn!(%error, module, "cannot remove the recording's audio mix");
            }
        }
    }
}

impl Drop for PreparedAudio {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Resolves, probes and — for both sources at once — mixes the requested audio.
pub fn prepare(system: bool, microphone: bool) -> PreparedAudio {
    let mut prepared = PreparedAudio::default();
    if !system && !microphone {
        return prepared;
    }

    remove_stale_mix();

    let sink = system
        .then(|| pactl(&["get-default-sink"]))
        .and_then(|result| {
            result
                .map_err(|error| tracing::warn!(%error, "cannot find the default output"))
                .ok()
        });
    let source = microphone
        .then(|| pactl(&["get-default-source"]))
        .and_then(|result| {
            result
                .map_err(|error| tracing::warn!(%error, "cannot find the default microphone"))
                .ok()
        });
    let monitor = sink.map(|sink| format!("{sink}.monitor"));

    // Both probes at once: each can take up to the deadline.
    let monitor_probe = monitor
        .clone()
        .map(|device| thread::spawn(move || probe(&device)));
    let source_probe = source
        .clone()
        .map(|device| thread::spawn(move || probe(&device)));
    let monitor = monitor.filter(|_| joined(monitor_probe));
    let source = source.filter(|_| joined(source_probe));

    if system && monitor.is_none() {
        prepared
            .warnings
            .push("Output audio is not available, so this recording has none.".to_string());
    }
    if microphone && source.is_none() {
        prepared.warnings.push(
            "The microphone is not delivering any audio, so it is left out of this recording."
                .to_string(),
        );
    }

    prepared.device = match (monitor, source) {
        (Some(monitor), Some(source)) => match create_mix(&monitor, &source) {
            Ok(modules) => {
                prepared.modules = modules;
                prepared.microphone = true;
                Some(format!("{MIX_SINK}.monitor"))
            }
            Err(error) => {
                tracing::warn!(%error, "cannot mix the microphone in; recording output audio only");
                prepared.warnings.push(
                    "The microphone could not be mixed in, so only output audio is recorded."
                        .to_string(),
                );
                Some(monitor)
            }
        },
        (Some(monitor), None) => Some(monitor),
        (None, Some(source)) => {
            prepared.microphone = true;
            Some(source)
        }
        (None, None) => None,
    };

    prepared
}

fn joined(probe: Option<thread::JoinHandle<bool>>) -> bool {
    probe.is_some_and(|probe| probe.join().unwrap_or(false))
}

/// Creates the mix sink and a loopback from each source into it. On failure
/// anything already loaded is unloaded again.
fn create_mix(monitor: &str, source: &str) -> Result<Vec<u32>, String> {
    let mut modules = Vec::new();
    let result = (|| {
        modules.push(load_module(&[
            "module-null-sink",
            &format!("sink_name={MIX_SINK}"),
            "sink_properties=device.description=ioexplorer-shot-mix",
        ])?);
        for input in [monitor, source] {
            modules.push(load_module(&loopback_arguments(input))?);
        }
        Ok(())
    })();

    match result {
        Ok(()) => Ok(modules),
        Err(error) => {
            for module in modules.into_iter().rev() {
                let _ = pactl(&["unload-module", &module.to_string()]);
            }
            Err(error)
        }
    }
}

/// A loopback pinned to its endpoints: without `*_dont_move`, PipeWire is free
/// to re-route the stream when the default device changes mid-recording.
fn loopback_arguments(source: &str) -> Vec<String> {
    vec![
        "module-loopback".to_string(),
        format!("source={source}"),
        format!("sink={MIX_SINK}"),
        "latency_msec=30".to_string(),
        "source_dont_move=true".to_string(),
        "sink_dont_move=true".to_string(),
    ]
}

fn load_module<S: AsRef<str>>(arguments: &[S]) -> Result<u32, String> {
    let mut command = vec!["load-module"];
    command.extend(arguments.iter().map(AsRef::as_ref));
    let reply = pactl(&command)?;
    reply
        .trim()
        .parse()
        .map_err(|_| format!("pactl returned an unexpected module id: {reply}"))
}

/// Unloads any mix left behind by a recording that did not clean up.
fn remove_stale_mix() {
    let Ok(listing) = pactl(&["list", "short", "modules"]) else {
        return;
    };
    for module in stale_modules(&listing) {
        tracing::info!(module, "removing a leftover recording mix");
        let _ = pactl(&["unload-module", &module.to_string()]);
    }
}

/// Module ids in `pactl list short modules` that belong to the mix, loopbacks
/// first so nothing is left pointing at a sink that has gone.
pub fn stale_modules(listing: &str) -> Vec<u32> {
    let sink = format!("sink_name={MIX_SINK}");
    let loopback = format!("sink={MIX_SINK}");
    let mut sinks = Vec::new();
    let mut loopbacks = Vec::new();

    for line in listing.lines() {
        let mut fields = line.split('\t');
        let (Some(id), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(id) = id.trim().parse::<u32>() else {
            continue;
        };
        let arguments = fields.next().unwrap_or_default();
        let mentions = |needle: &str| arguments.split_whitespace().any(|word| word == needle);

        match name {
            "module-null-sink" if mentions(&sink) => sinks.push(id),
            "module-loopback" if mentions(&loopback) => loopbacks.push(id),
            _ => {}
        }
    }

    loopbacks.extend(sinks);
    loopbacks
}

/// Whether `device` produces samples within the deadline.
fn probe(device: &str) -> bool {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "pulse", "-i", device, "-t", "0.15", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "cannot run ffmpeg to check an audio source");
            return false;
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < PROBE_DEADLINE => {
                thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(device, "audio source delivered nothing in time");
                return false;
            }
        }
    }
}

fn pactl<S: AsRef<str>>(arguments: &[S]) -> Result<String, String> {
    let output = Command::new("pactl")
        .args(arguments.iter().map(AsRef::as_ref))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run pactl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pactl {} failed: {}",
            arguments.first().map(AsRef::as_ref).unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftover_mix_modules_are_found_loopbacks_first() {
        let listing = "\
536870913\tmodule-always-sink\t\t
536870916\tmodule-null-sink\tsink_name=ioexplorer_shot_mix sink_properties=device.description=ioexplorer-shot-mix\t
536870917\tmodule-loopback\tsource=alsa_output.x.monitor sink=ioexplorer_shot_mix latency_msec=30\t
536870918\tmodule-loopback\tsource=alsa_input.y sink=ioexplorer_shot_mix latency_msec=30\t
536870919\tmodule-loopback\tsource=alsa_input.y sink=some_other_sink\t
536870920\tmodule-null-sink\tsink_name=ioexplorer_shot_mix_backup\t";

        assert_eq!(
            stale_modules(listing),
            vec![536870917, 536870918, 536870916]
        );
    }

    #[test]
    fn an_empty_or_odd_listing_finds_nothing() {
        assert!(stale_modules("").is_empty());
        assert!(stale_modules("garbage\nmore garbage").is_empty());
    }

    #[test]
    fn loopbacks_are_pinned_to_the_mix() {
        let arguments = loopback_arguments("alsa_input.mic");
        assert!(arguments.contains(&"source=alsa_input.mic".to_string()));
        assert!(arguments.contains(&format!("sink={MIX_SINK}")));
        assert!(arguments.contains(&"source_dont_move=true".to_string()));
    }

    #[test]
    fn no_audio_requested_needs_no_device() {
        let prepared = prepare(false, false);
        assert!(prepared.device.is_none());
        assert!(!prepared.microphone);
        assert!(prepared.warnings.is_empty());
    }
}
