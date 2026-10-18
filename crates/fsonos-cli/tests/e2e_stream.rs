//! `fsonos stream` end to end with real FFmpeg, against the virtual
//! households. A lavfi sine source stands in for computer audio (the
//! `--format`/`--input` flags hand it to FFmpeg as they would a PulseAudio
//! monitor). The Kitchen first plays a station; `fsonos stream Kitchen`
//! points its group at a live MP3 endpoint on the speaker-facing interface
//! (loopback here); the test reads that endpoint as a player would, finds
//! MP3 frames, and is refused anything but GET or HEAD on its path. SIGINT
//! puts the station back, and the action log says so.
//!
//! It needs `ffmpeg` with libmp3lame. Where there is none it reports the
//! skip and passes, unless `FSONOS_E2E_FFMPEG=require` (set in Linux CI,
//! which installs FFmpeg), where a missing FFmpeg fails it.

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::{get_position_info, get_transport_info};
use fsonos_sim::SimHousehold;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::{Duration, Instant};

const BEFORE: &str = "x-rincon-mp3radio://stream.example.invalid/before.mp3";

/// FFmpeg, with the MP3 encoder `fsonos stream` asks for.
fn ffmpeg_with_lame() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("libmp3lame")
        })
}

/// `(status line, headers, body)` from `addr`, reading the body until it
/// holds `want` bytes, the peer closes, or 10 s pass.
fn request(addr: &str, head: &str, want: usize) -> (String, String, Vec<u8>) {
    let Ok(mut peer) = TcpStream::connect(addr) else {
        return (String::new(), String::new(), Vec::new());
    };
    let _ = peer.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = peer.write_all(head.as_bytes());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    let mut buf = [0_u8; 4096];
    while Instant::now() < deadline {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
        let body = got
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| got.len() - at - 4);
        if body.is_some_and(|b| b >= want) {
            break;
        }
    }
    let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (
            String::from_utf8_lossy(&got).into_owned(),
            String::new(),
            Vec::new(),
        );
    };
    let head = String::from_utf8_lossy(&got[..at]).into_owned();
    let (status, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));
    (
        status.to_string(),
        headers.to_ascii_lowercase(),
        got[at + 4..].to_vec(),
    )
}

/// An MPEG audio frame header: 11 sync bits set.
fn has_mp3_frames(body: &[u8]) -> bool {
    body.windows(2).any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)
}

#[test]
fn a_live_stream_plays_from_real_ffmpeg_and_sigint_puts_the_music_back() {
    if !ffmpeg_with_lame() {
        assert!(
            std::env::var("FSONOS_E2E_FFMPEG").as_deref() != Ok("require"),
            "FSONOS_E2E_FFMPEG=require, but there is no ffmpeg with libmp3lame"
        );
        eprintln!("e2e_stream skipped: no ffmpeg with libmp3lame on this machine");
        return;
    }
    let mut s = Scenario::start("stream");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let on_now = |s: &Scenario| {
        get_position_info(&s.lan(), kitchen)
            .map(|p| p.uri)
            .unwrap_or_default()
    };

    let run = s.cli("play-before", &["play", "Kitchen", BEFORE]);
    s.check(
        "play-before",
        "cli",
        "the Kitchen plays a station first",
        run.ok() && on_now(&s) == BEFORE,
        format!("{}{}", run.stdout, run.stderr),
    );

    let mut stream = s.spawn(
        "stream",
        &[
            "stream",
            "Kitchen",
            "--format",
            "lavfi",
            "--input",
            "sine=frequency=440:sample_rate=44100",
        ],
    );
    let listening = stream.wait_line("listening; press Ctrl-C to stop", Duration::from_secs(30));
    let uri = on_now(&s);
    let target = uri
        .strip_prefix("x-rincon-mp3radio://")
        .unwrap_or_default()
        .to_string();
    let (addr, path) = target
        .split_once('/')
        .map(|(a, p)| (a.to_string(), format!("/{p}")))
        .unwrap_or_default();
    let id = path
        .strip_prefix("/stream/")
        .and_then(|p| p.strip_suffix(".mp3"))
        .unwrap_or_default();
    s.check(
        "stream-start",
        "sim",
        "the Kitchen's group plays the live MP3 endpoint, on the loopback interface (the speaker-facing one here), at a random path",
        listening.is_some()
            && addr.starts_with("127.")
            && id.len() >= 16
            && id.chars().all(|c| c.is_ascii_alphanumeric()),
        format!("{uri}\n{}", stream.seen.join("\n")),
    );

    check_endpoint(&mut s, &addr, &path);

    let code = stream.interrupt(Duration::from_secs(30));
    let state = get_transport_info(&s.lan(), kitchen)
        .map(|t| format!("{:?}", t.state))
        .unwrap_or_default();
    s.check(
        "stream-restore",
        "sim",
        "SIGINT ends the stream and puts the station back, playing",
        code == Some(0) && on_now(&s) == BEFORE && state == "Playing",
        format!(
            "exit {code:?}; now {} ({state})\n{}",
            on_now(&s),
            stream.seen.join("\n")
        ),
    );

    check_logged(&mut s);
    s.finish();
}

/// The endpoint at `addr` and `path` serves MP3 audio to a GET, as a player
/// fetches it, and nothing else.
fn check_endpoint(s: &mut Scenario, addr: &str, path: &str) {
    let (status, headers, body) = request(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
        8192,
    );
    s.check(
        "stream-fetch",
        "http",
        "a player's GET of the endpoint gets audio/mpeg with MP3 frames from FFmpeg",
        status.contains(" 200 ")
            && headers.contains("content-type: audio/mpeg")
            && body.len() >= 4096
            && has_mp3_frames(&body),
        format!("{status} | {headers} | {} bytes", body.len()),
    );
    let (refused, _, _) = request(
        addr,
        &format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\n\r\n"),
        0,
    );
    let (elsewhere, _, _) = request(
        addr,
        &format!("GET /stream/other.mp3 HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
        0,
    );
    s.check(
        "stream-read-only",
        "http",
        "only GET or HEAD of its own path is served: a POST and another path are 404",
        refused.contains(" 404 ") && elsewhere.contains(" 404 "),
        format!("{refused} | {elsewhere}"),
    );
}

/// The restore is in the action log.
fn check_logged(s: &mut Scenario) {
    let log = s.cli("stream-logged", &["log", "--json"]);
    let rows: Value = serde_json::from_str(&log.stdout).unwrap_or(Value::Null);
    s.check(
        "stream-logged",
        "cli",
        "the restore is in the action log as the CLI's",
        rows.as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["client"] == "cli"
                    && r["intent"]
                        .as_str()
                        .is_some_and(|i| i.starts_with("stream: restore Kitchen"))
                    && r["result"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("put back"))
            })
        }),
        &log.stdout,
    );
}
