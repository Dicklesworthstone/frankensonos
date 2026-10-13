//! Stream an audio device to a Sonos group as live MP3.
//!
//! The capture and encoder run on the machine which can reach the speakers;
//! no speaker is modified and the stream is exposed only on that LAN interface.
//! Ctrl-C terminates the encoder and restores the group only while the group
//! still plays *our* stream, so manual changes are never overwritten.
//!
//! Sonos buffers HTTP radio for seconds. This is live audio for listening,
//! NOT synchronized playback with a computer's video (issue #2's separate
//! calibration and drift-correction work).

use anyhow::{Context as _, bail};
use clap::Args;
use fsonos_api::PlayRequest;
use fsonos_core::snapshot::{self, ZoneSnapshot};
use std::io::{self, BufRead as _, BufReader, Read, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::GlobalArgs;
use crate::direct::Direct;

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
        "-b:a".into(),
        format!("{}k", args.bitrate),
        "-f".into(),
        "mp3".into(),
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

/// Serve the *one* unguessable URL to Sonos. The capture is a continuous MP3
/// bitstream; a reconnecting player can start at the next MP3 frame. The
/// listener never hosts the control API or accepts writes.
fn serve(
    listener: &TcpListener,
    path: &str,
    mut audio: impl Read,
    stop: &AtomicBool,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut peer, _)) => {
                peer.set_read_timeout(Some(Duration::from_secs(3)))?;
                peer.set_write_timeout(Some(Duration::from_secs(5)))?;
                let mut first = String::new();
                let parsed = BufReader::new((&mut peer).take(2048)).read_line(&mut first);
                if parsed.is_err()
                    || first.trim_end() != format!("GET {path} HTTP/1.1")
                        && first.trim_end() != format!("GET {path} HTTP/1.0")
                {
                    let _ = peer.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                peer.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                )?;
                if let Err(e) = io::copy(&mut audio, &mut peer)
                    && !matches!(
                        e.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::TimedOut
                    )
                {
                    return Err(e);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn stream_finished(
    encoder: &mut Child,
    server: &thread::JoinHandle<io::Result<()>>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    while !stop.load(Ordering::Acquire) {
        if server.is_finished() {
            bail!("the local MP3 streaming listener stopped unexpectedly");
        }
        if let Some(status) = encoder.try_wait()? {
            bail!("FFmpeg stopped while streaming ({status}); check --format and --input");
        }
        thread::sleep(Duration::from_millis(150));
    }
    Ok(())
}

/// Restore only if the room still names this source; a later manual play,
/// regroup or DJ action belongs to its caller, not this CLI process.
fn restore_if_still_streaming(
    global: &GlobalArgs,
    room: &str,
    source: &str,
    snapshot: &ZoneSnapshot,
) -> anyhow::Result<()> {
    let direct = Direct::survey(global)?;
    let now = direct.status(room)?;
    let matches = now.track.as_ref().is_some_and(|track| {
        track.uri == source
            || source
                .strip_prefix("x-rincon-mp3radio://")
                .is_some_and(|tail| track.uri == format!("http://{tail}"))
    });
    if !matches {
        eprintln!("fsonos stream: playback changed elsewhere; leaving it alone");
        return Ok(());
    }
    let network = global.network()?;
    let report = snapshot::restore(&*network.transport, direct.households()?, snapshot)?;
    if !report.skipped.is_empty() {
        eprintln!(
            "fsonos stream: some prior state could not be restored: {:?}",
            report.skipped
        );
    }
    Ok(())
}

pub fn run(global: &GlobalArgs, args: &StreamArgs) -> anyhow::Result<()> {
    if global.daemon || global.daemon_url.is_some() {
        bail!("stream needs a direct LAN connection to Sonos; remove --daemon/--daemon-url");
    }
    if args.ffmpeg.is_empty() || args.format.is_empty() || args.input.is_empty() {
        bail!("--ffmpeg, --format and --input must not be empty");
    }
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
    let mut encoder = launch_encoder(args)?;
    let stdout = encoder
        .stdout
        .take()
        .context("FFmpeg did not open stdout")?;
    let stop = Arc::new(AtomicBool::new(false));
    let started = Arc::clone(&stop);
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    let server = thread::Builder::new()
        .name("fsonos-live-mp3".into())
        .spawn(move || serve(&listener, &path, stdout, &started))?;

    let play = direct.run("play", |rooms| {
        fsonos_api::plan::plan_play(
            rooms,
            &PlayRequest {
                zone: args.room.clone(),
                source_uri: source.clone(),
                title: Some("Computer audio".into()),
            },
        )
    });
    let was_started = play.is_ok();
    let result = match play {
        Ok(_) => {
            eprintln!(
                "fsonos stream: {} listening; press Ctrl-C to stop",
                args.room
            );
            stream_finished(&mut encoder, &server, &stop)
        }
        Err(e) => Err(e.into()),
    };

    stop.store(true, Ordering::Release);
    // On Ctrl-C the child may be blocked writing into a disconnected speaker;
    // terminate it so the HTTP thread can leave io::copy and join.
    let _ = encoder.kill();
    let _ = encoder.wait();
    let server_result = server
        .join()
        .map_err(|_| anyhow::anyhow!("MP3 listener panicked"))?;
    if was_started
        && let Err(e) = restore_if_still_streaming(global, &args.room, &source, &previous)
    {
        eprintln!("fsonos stream: could not restore prior playback: {e:#}");
    }
    result?;
    server_result.context("serve the MP3 stream")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpStream;

    #[test]
    fn encoder_is_a_local_argument_array_without_a_shell() {
        let args = StreamArgs {
            room: "Office".into(),
            input: "sink.monitor".into(),
            format: "pulse".into(),
            bitrate: 256,
            ffmpeg: "ffmpeg".into(),
        };
        let command = encoder_args(&args);
        assert_eq!(
            command.windows(2).find(|v| v[0] == "-i"),
            Some(&command[6..8])
        );
        assert!(command.contains(&"sink.monitor".to_string()));
        assert!(command.contains(&"256k".to_string()));
        assert_eq!(command.last().unwrap(), "pipe:1");
    }

    #[test]
    fn stream_serves_only_its_unguessable_path() {
        fn fetch(address: SocketAddr, path: &str) -> String {
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            write!(
                socket,
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            response
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            serve(
                &listener,
                "/stream/example.mp3",
                Cursor::new(b"mp3-frames".to_vec()),
                &stopping,
            )
        });
        assert!(fetch(address, "/stream/guess.mp3").starts_with("HTTP/1.1 404"));
        let correct = fetch(address, "/stream/example.mp3");
        assert!(correct.starts_with("HTTP/1.1 200"));
        assert!(correct.contains("Content-Type: audio/mpeg"));
        assert!(correct.ends_with("mp3-frames"));
        stop.store(true, Ordering::Release);
        handle.join().unwrap().unwrap();
    }
}
