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

mod broadcast;
mod calibration;
mod latency;
mod recovery;
#[cfg(unix)]
mod synchronization;

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
) -> anyhow::Result<()> {
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
    Ok(())
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

pub fn run(global: &GlobalArgs, args: &StreamArgs) -> anyhow::Result<()> {
    if global.daemon || global.daemon_url.is_some() {
        bail!("stream needs a direct LAN connection to Sonos; remove --daemon/--daemon-url");
    }
    if args.ffmpeg.is_empty() || args.format.is_empty() || args.input.is_empty() {
        bail!("--ffmpeg, --format and --input must not be empty");
    }
    if let Some(path) = &args.latency_wav {
        return report_measurement(global.json, &args.room, path, args.max_lag_ms);
    }
    #[cfg(unix)]
    let sync = synchronization::Prepared::from_args(args)?;
    let direct = Direct::survey(global)?;
    let target = fsonos_api::plan::resolve(direct.households()?, &args.room)?;
    let coordinator = target.coordinator;
    let network = global.network()?;
    let interface = network.lan.local_address_toward(coordinator.ip)?;
    let previous = snapshot::capture(
        &*network.transport,
        direct.households()?,
        &coordinator.id,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .try_into()?,
    )
    .context("snapshot the group's music and volumes before streaming")?;

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
    let mut stream =
        broadcast::LiveStream::start(launch_encoder(args)?, listener, path, Arc::clone(&stop))
            .context("start live capture and the MP3 listener")?;

    let mut attempted = false;
    let play = stream
        .wait_ready(Duration::from_secs(10))
        .context("prepare live audio before replacing the group's music")
        .and_then(|()| {
            recovery::verify_start(&*network.transport, direct.households()?, &previous)
                .map_err(anyhow::Error::msg)?;
            stream
                .check()
                .context("confirm live capture before playback")?;
            // Record intent before dispatch: an error can follow an applied URI.
            attempted = true;
            direct
                .run("play", |rooms| {
                    fsonos_api::plan::plan_play(
                        rooms,
                        &PlayRequest {
                            zone: args.room.clone(),
                            source_uri: source.clone(),
                            title: Some("Computer audio".into()),
                        },
                    )
                })
                .map_err(anyhow::Error::from)
        });
    let was_started = play.is_ok();
    let result = match play {
        Ok(_) => {
            eprintln!(
                "fsonos stream: {} listening; press Ctrl-C to stop",
                args.room
            );
            #[cfg(unix)]
            let result = synchronization::wait(&mut stream, sync, Arc::clone(&stop));
            #[cfg(not(unix))]
            let result = stream.wait_until_stopped().map_err(anyhow::Error::from);
            result
        }
        Err(error) => Err(error),
    };

    // Keep capture available while checking ownership and restoring playback.
    // Cancellation does not close the audio sockets before this observation.
    if attempted
        && let Err(error) = restore_if_still_streaming(global, &source, &previous, was_started)
    {
        eprintln!("fsonos stream: could not restore prior playback: {error:#}");
    }
    let stopped = stream.shutdown();
    result?;
    stopped.context("stop the live MP3 stream")
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
