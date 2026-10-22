//! Stream an audio device to a Sonos group as live MP3.
//!
//! The capture and encoder run on the machine which can reach the speakers;
//! no speaker is modified and the stream is exposed only on that LAN interface.
//! Ctrl-C restores playback only while source and group ownership still match.
//! Cleanup does not restore volume, mute or grouping it did not change.
//!
//! Sonos buffers HTTP radio for seconds. Acoustic measurement and optional mpv
//! correction are experimental; neither implies a physical lip-sync guarantee.

use anyhow::{Context as _, bail};
use clap::Args;
use fsonos_api::PlayRequest;
use fsonos_core::snapshot::{self, ZoneSnapshot};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::GlobalArgs;
use crate::direct::Direct;

mod application;
mod broadcast;
mod calibration;
mod latency;
mod recovery;
#[cfg(unix)]
mod synchronization;
mod virtual_output;

#[derive(Debug, Clone, Args)]
pub struct StreamArgs {
    /// Target room/group, on the same LAN as this computer.
    pub room: String,
    /// Capture format used by FFmpeg: pulse for PipeWire/PulseAudio, avfoundation on macOS.
    #[arg(long, default_value = "pulse")]
    pub format: String,
    /// FFmpeg input source (Linux: @DEFAULT_MONITOR@, or a named .monitor).
    #[arg(long, default_value = "@DEFAULT_MONITOR@")]
    pub input: String,
    /// Linux Pulse: create a temporary output and capture only apps routed to it.
    #[arg(long, conflicts_with_all = ["input", "latency_wav"])]
    pub virtual_output: Option<String>,
    /// After --, launch a foreground Pulse app into --virtual-output; Ctrl-C stops it.
    #[arg(last = true, num_args = 1.., value_name = "COMMAND", requires = "virtual_output")]
    #[cfg_attr(unix, arg(conflicts_with_all = ["measure_latency", "sync_mpv"]))]
    pub application: Vec<std::ffi::OsString>,
    /// MP3 bitrate, in kbit/s. Higher rates use more bandwidth.
    #[arg(long, default_value_t = 192, value_parser = clap::value_parser!(u16).range(64..=320))]
    pub bitrate: u16,
    /// Path to the locally installed FFmpeg executable.
    #[arg(long, default_value = "ffmpeg")]
    pub ffmpeg: String,
    /// Measure a synchronized stereo WAV instead of streaming (no device I/O).
    #[arg(long)]
    pub latency_wav: Option<std::path::PathBuf>,
    /// Largest acoustic delay to measure; recording must be at least twice as long.
    #[arg(long, default_value_t = 8_000, value_parser = clap::value_parser!(u32).range(21..=14_000))]
    pub max_lag_ms: u32,
    #[cfg(unix)]
    #[command(flatten)]
    pub sync: synchronization::SyncArgs,
}

fn encoder_args(args: &StreamArgs) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-f".into(),
        args.format.clone(),
        "-i".into(),
        args.input.clone(),
        "-vn".into(),
        "-ac".into(),
        "2".into(),
        "-ar".into(),
        "44100".into(),
        "-c:a".into(),
        "libmp3lame".into(),
        "-reservoir".into(),
        "0".into(),
        "-b:a".into(),
        format!("{}k", args.bitrate),
        "-f".into(),
        "mp3".into(),
        "-write_xing".into(),
        "0".into(),
        "-id3v2_version".into(),
        "0".into(),
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]
}

fn launch_encoder(args: &StreamArgs) -> anyhow::Result<Child> {
    Process::new(&args.ffmpeg)
        .args(encoder_args(args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "start {} (install FFmpeg and an MP3 encoder, or pass --ffmpeg)",
                args.ffmpeg
            )
        })
}

/// Address the captured coordinator by identity, not a possibly renamed room.
fn restore_if_still_streaming(
    global: &GlobalArgs,
    source: &str,
    snapshot: &ZoneSnapshot,
    started: bool,
) -> anyhow::Result<bool> {
    let direct = Direct::survey(global)?;
    let network = global.network()?;
    let restored = recovery::restore(
        &*network.transport,
        direct.households()?,
        snapshot,
        source,
        started,
    );
    let room = direct
        .households()?
        .iter()
        .find_map(|h| h.player(&snapshot.coordinator))
        .map_or_else(|| snapshot.coordinator.0.clone(), |p| p.room_name.clone());
    direct.log_done(
        format!("stream: restore {room}'s group"),
        match &restored {
            Ok(r) if r.skipped.is_empty() => "put back what was playing".into(),
            Ok(r) => format!("put back, except {:?}", r.skipped),
            Err(e) => format!("failed: {e}"),
        },
    );
    let report = restored?;
    if !report.skipped.is_empty() {
        eprintln!(
            "fsonos stream: some prior state was left unchanged: {:?}",
            report.skipped
        );
    }
    Ok(report.skipped.is_empty())
}

fn report_measurement(
    json: bool,
    room: &str,
    path: &std::path::Path,
    max_lag_ms: u32,
) -> anyhow::Result<()> {
    let measured = calibration::measure_file(path, max_lag_ms)?;
    crate::emit(
        json,
        &serde_json::json!({
            "room": room,
            "lag_seconds": measured.seconds(),
            "lag_samples": measured.lag_samples,
            "sample_rate": latency::SAMPLE_RATE,
            "correlation": measured.correlation,
            "peak_ratio": measured.peak_ratio,
            "mpv_audio_delay_delta": -measured.seconds(),
            "device_verified": false,
        }),
        |_| {
            format!(
                "Measured acoustic lag: {:.3} s (correlation {:.3}, peak ratio {:.2}).\n\
                 mpv audio-delay correction: -{:.3} s. No device was contacted.\n",
                measured.seconds(),
                measured.correlation,
                measured.peak_ratio,
                measured.seconds()
            )
        },
    )
}

/// The synchronization a stream was asked for (Unix only).
#[cfg(unix)]
type SyncPlan = Option<synchronization::Prepared>;
#[cfg(not(unix))]
type SyncPlan = ();

/// What streaming ends with: the latency synchronization measured (Unix).
#[cfg(unix)]
type Waited = Option<synchronization::Measurement>;
#[cfg(not(unix))]
type Waited = ();

/// The group a stream replaces and the stream it plays instead: what every
/// check before an irreversible step compares the speakers with.
struct Playing<'a> {
    network: &'a crate::config::Network,
    direct: &'a Direct,
    previous: &'a ZoneSnapshot,
    source: &'a str,
}

impl Playing<'_> {
    /// Whether the original group still plays the stream (and, once
    /// `settled`, plays it now).
    #[cfg(unix)]
    fn verify(&self, settled: bool) -> anyhow::Result<()> {
        recovery::verify_measurement(
            &*self.network.transport,
            self.direct.households()?,
            self.previous,
            self.source,
            settled,
        )
        .map_err(anyhow::Error::msg)
    }
}

/// The group's music, grouping and volumes before the stream replaces them.
fn snapshot_group(
    network: &crate::config::Network,
    direct: &Direct,
    coordinator: &fsonos_types::PlayerId,
) -> anyhow::Result<ZoneSnapshot> {
    snapshot::capture(
        &*network.transport,
        direct.households()?,
        coordinator,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .try_into()?,
    )
    .context("snapshot the group's music and volumes before streaming")
}

/// Replace the group's music with the stream, once capture is live and the
/// group is still the one snapshotted. `attempted` turns true just before
/// the play goes out.
fn start_playback(
    playing: &Playing<'_>,
    stream: &mut broadcast::LiveStream,
    room: &str,
    attempted: &mut bool,
) -> anyhow::Result<()> {
    stream
        .wait_ready(Duration::from_secs(10))
        .context("prepare live audio before replacing the group's music")?;
    recovery::verify_start(
        &*playing.network.transport,
        playing.direct.households()?,
        playing.previous,
    )
    .map_err(anyhow::Error::msg)?;
    stream
        .check()
        .context("confirm live capture before playback")?;
    // Record intent before dispatch: an error can follow an applied URI.
    *attempted = true;
    playing
        .direct
        .run("play", |rooms| {
            fsonos_api::plan::plan_play(
                rooms,
                &PlayRequest {
                    zone: room.to_owned(),
                    source_uri: playing.source.to_owned(),
                    title: Some("Computer audio".into()),
                },
            )
        })
        .map(|_| ())
        .map_err(anyhow::Error::from)
}

/// Once the group plays the stream: launch the foreground application, if
/// one was asked for, only after the speaker reads the audio from the group
/// it had; then stream until stopped. `launched` holds the application for
/// cleanup even when streaming fails.
fn stream_until_stopped(
    playing: &Playing<'_>,
    stream: &mut broadcast::LiveStream,
    launch: Option<&application::Launch>,
    launched: &mut Option<application::Application>,
    sync: SyncPlan,
    stop: &Arc<AtomicBool>,
    room: &str,
) -> anyhow::Result<Waited> {
    if launch.is_some() {
        stream
            .wait_for_reader(Duration::from_secs(10))
            .context("wait for audio delivery before application launch")?;
        #[cfg(unix)]
        playing
            .verify(false)
            .context("confirm the original stream group before application launch")?;
        // The reader may have disconnected while the speaker reads ran.
        stream.wait_for_reader(Duration::ZERO)?;
    }
    *launched = launch
        .map(|launch| application::Application::start(launch, Arc::clone(stop)))
        .transpose()?;
    eprintln!("fsonos stream: {room} listening; press Ctrl-C to stop");
    #[cfg(unix)]
    let result = synchronization::wait(stream, sync, Arc::clone(stop), |settled| {
        playing.verify(settled)
    });
    #[cfg(not(unix))]
    let result = {
        let _ = (sync, playing);
        stream.wait_until_stopped().map_err(anyhow::Error::from)
    };
    result
}

/// Stop the foreground application before the speakers are restored or its
/// output removed. A child that was not reaped keeps its output.
fn stop_application(
    launched: &mut Option<application::Application>,
    output: Option<&mut virtual_output::VirtualOutput>,
) -> anyhow::Result<Option<()>> {
    let stopped = launched
        .as_mut()
        .map(application::Application::shutdown)
        .transpose();
    if let Err(error) = &stopped {
        eprintln!("fsonos stream: foreground application ended with an error: {error:#}");
    }
    if launched.as_ref().is_some_and(|app| !app.reaped())
        && let Some(output) = output
    {
        output.retain();
    }
    stopped
}

pub fn run(global: &GlobalArgs, args: &StreamArgs) -> anyhow::Result<()> {
    if global.daemon || global.daemon_url.is_some() {
        bail!("stream needs a direct LAN connection to Sonos; remove --daemon/--daemon-url");
    }
    if args.ffmpeg.is_empty() || args.format.is_empty() || args.input.is_empty() {
        bail!("--ffmpeg, --format and --input must not be empty");
    }
    let prepared_args = virtual_output::prepare(args)?;
    let args = &prepared_args;
    let launch = application::Launch::prepare(args)?;
    if let Some(path) = &args.latency_wav {
        return report_measurement(global.json, &args.room, path, args.max_lag_ms);
    }
    #[cfg(unix)]
    let sync = synchronization::Prepared::from_args(args)?;
    #[cfg(not(unix))]
    let sync = ();
    let direct = Direct::survey(global)?;
    let target = fsonos_api::plan::resolve(direct.households()?, &args.room)?;
    let coordinator = target.coordinator;
    let network = global.network()?;
    let interface = network.lan.local_address_toward(coordinator.ip)?;
    let previous = snapshot_group(&network, &direct, &coordinator.id)?;

    // Bind before touching the speaker; no partial playback on bind failure.
    let listener = TcpListener::bind(SocketAddr::new(interface, 0))
        .context("bind a stream listener on the speaker-facing LAN interface")?;
    let id = fsonos_core::announce::clip::clip_id()?;
    let path = format!("/stream/{id}.mp3");
    let source = format!("x-rincon-mp3radio://{}{path}", listener.local_addr()?);
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    let mut output = virtual_output::VirtualOutput::start(args.virtual_output.as_deref(), &stop)?;
    let mut stream =
        broadcast::LiveStream::start(launch_encoder(args)?, listener, path, Arc::clone(&stop))
            .context("start live capture and the MP3 listener")?;

    let playing = Playing {
        network: &network,
        direct: &direct,
        previous: &previous,
        source: &source,
    };
    let mut attempted = false;
    let play = start_playback(&playing, &mut stream, &args.room, &mut attempted);
    let was_started = play.is_ok();
    let mut launched = None;
    let result = play.and_then(|()| {
        stream_until_stopped(
            &playing,
            &mut stream,
            launch.as_ref(),
            &mut launched,
            sync,
            &stop,
            &args.room,
        )
    });

    // Stop our foreground application before restoring speakers or removing
    // its output. Child failure must not skip the existing cleanup sequence.
    let application_stopped = stop_application(&mut launched, output.as_mut());

    // Keep capture available while checking ownership and restoring playback.
    // Cancellation does not close the audio sockets before this observation.
    let playback_restored = attempted
        && match restore_if_still_streaming(global, &source, &previous, was_started) {
            Ok(restored) => restored,
            Err(error) => {
                eprintln!("fsonos stream: could not restore prior playback: {error:#}");
                false
            }
        };
    let stopped = stream.shutdown();
    // Capture and probe processes are stopped before their temporary output.
    let output_stopped = output
        .as_mut()
        .map(virtual_output::VirtualOutput::shutdown)
        .transpose();
    if let Err(error) = &output_stopped {
        eprintln!("fsonos stream: temporary output cleanup was not confirmed: {error:#}");
    }
    #[cfg(unix)]
    let measured = result?;
    #[cfg(not(unix))]
    {
        result?;
        let _ = playback_restored;
    }
    stopped.context("stop the live MP3 stream")?;
    application_stopped.context("finish the foreground application")?;
    output_stopped.context("remove the temporary output")?;
    #[cfg(unix)]
    if let Some(measured) = measured {
        measured.emit(global.json, &args.room, playback_restored)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_arguments_are_explicit_and_bounded() {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: StreamArgs,
        }
        let parsed = Cli::try_parse_from([
            "stream",
            "Room",
            "--latency-wav",
            "aligned.wav",
            "--max-lag-ms",
            "4000",
        ])
        .unwrap();
        assert_eq!(parsed.args.max_lag_ms, 4_000);
        assert!(parsed.args.latency_wav.is_some());
        assert!(Cli::try_parse_from(["stream", "Room", "--max-lag-ms", "0"]).is_err());
        assert!(Cli::try_parse_from(["stream", "Room", "--max-lag-ms", "15000"]).is_err());
        #[cfg(unix)]
        {
            assert!(Cli::try_parse_from(["stream", "Room", "--sync-mpv", "/tmp/mpv"]).is_err());
            assert!(
                Cli::try_parse_from([
                    "stream",
                    "Room",
                    "--sync-input",
                    "paired",
                    "--sync-mpv",
                    "/tmp/mpv",
                    "--latency-wav",
                    "aligned.wav",
                ])
                .is_err()
            );
            let live = Cli::try_parse_from([
                "stream",
                "Room",
                "--sync-input",
                "paired",
                "--sync-mpv",
                "/tmp/mpv",
                "--sync-base-delay",
                "-0.2",
            ])
            .unwrap();
            assert!((live.args.sync.sync_base_delay + 0.2).abs() < 1e-9);
        }
    }

    #[test]
    fn encoder_is_a_local_argument_array_without_a_shell() {
        let args = StreamArgs {
            room: "Office".into(),
            input: "sink.monitor".into(),
            virtual_output: None,
            application: Vec::new(),
            format: "pulse".into(),
            bitrate: 256,
            ffmpeg: "ffmpeg".into(),
            latency_wav: None,
            max_lag_ms: 8_000,
            #[cfg(unix)]
            sync: synchronization::SyncArgs::default(),
        };
        let command = encoder_args(&args);
        assert_eq!(
            command.windows(2).find(|v| v[0] == "-i"),
            Some(&command[6..8])
        );
        assert!(command.contains(&"sink.monitor".to_string()));
        assert!(command.contains(&"256k".to_string()));
        assert_eq!(command.last().unwrap(), "pipe:1");
        for (option, value) in [
            ("-reservoir", "0"),
            ("-write_xing", "0"),
            ("-id3v2_version", "0"),
            ("-flush_packets", "1"),
        ] {
            assert!(
                command
                    .windows(2)
                    .any(|pair| pair[0] == option && pair[1] == value)
            );
        }
    }
}
