//! Announcements end to end against the virtual households: `fsonos chime`
//! in direct mode (the CLI serves the clip itself) and `POST /announce`
//! through `fsonos serve` (the daemon serves it from its event listener).
//! Each time a virtual player fetches the clip, plays it, and the room is put
//! back as it was: volume, what played, and playing.
//!
//! The simulator's clock only moves when advanced, so a ticker runs it
//! about ten times faster than real time while an announcement plays.

mod e2e;

use e2e::{Scenario, http};
use fsonos_api::surface::announce::AnnounceRequest;
use fsonos_core::announce::clip::Chime;
use fsonos_proto::control::{get_media_info, get_transport_info, get_volume};
use fsonos_sim::{SimClock, SimHousehold};
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const STREAM: &str = "x-rincon-mp3radio://stream.example.org/before.mp3";

/// Runs the sim's clock while it lives.
struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Ticker {
    fn start(clock: SimClock) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                clock.advance(Duration::from_millis(10));
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What a room plays and how loud.
#[derive(Debug, PartialEq)]
struct Room {
    volume: Option<u8>,
    uri: Option<String>,
    state: Option<TransportState>,
}

fn room(s: &Scenario, ip: IpAddr) -> Room {
    let lan = s.lan();
    Room {
        volume: get_volume(&lan, ip).ok(),
        uri: get_media_info(&lan, ip).ok().map(|m| m.uri),
        state: get_transport_info(&lan, ip).ok().map(|t| t.state),
    }
}

/// Kitchen plays a stream at 20, Office sits at 15.
fn set_the_scene(s: &mut Scenario) -> (Room, Room) {
    for (step, args) in [
        ("kitchen-volume", &["volume", "Kitchen", "20"][..]),
        ("office-volume", &["volume", "Office", "15"][..]),
        ("kitchen-play", &["play", "Kitchen", STREAM][..]),
    ] {
        let run = s.cli(step, args);
        s.check(step, "cli", "the house is set up", run.ok(), &run.stderr);
    }
    let kitchen = room(s, s.ip("Kitchen"));
    let office = room(s, s.ip("Office"));
    s.check(
        "before",
        "sim",
        "Kitchen plays the stream at 20",
        kitchen.volume == Some(20)
            && kitchen.uri.as_deref() == Some(STREAM)
            && kitchen.state == Some(TransportState::Playing),
        format!("{kitchen:?}"),
    );
    (kitchen, office)
}

/// The fetches of announcement clips the sim saw, as `(room, url, status,
/// has a WAV length)`.
fn clip_fetches(s: &Scenario) -> Vec<(String, String, Result<u16, String>, bool)> {
    s.sim_handle()
        .map(|sim| {
            sim.fetch_log()
                .into_iter()
                .filter(|f| f.url.contains("/media/"))
                .map(|f| (f.room, f.url, f.result, f.wav_duration_ms.is_some()))
                .collect()
        })
        .unwrap_or_default()
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[test]
fn a_chime_plays_in_kitchen_and_the_music_comes_back() {
    let mut s = Scenario::start("announce-chime");
    let clock = s.sim(SimHousehold::standard()).clock().clone();
    let (kitchen_before, office_before) = set_the_scene(&mut s);

    let run = {
        let _ticking = Ticker::start(clock);
        s.cli(
            "chime",
            &[
                "--json", "chime", "bell", "--rooms", "Kitchen", "--volume", "40",
            ],
        )
    };
    let answer = json(&run.stdout);
    s.check(
        "chime",
        "cli",
        "the chime finishes and everything is put back",
        run.ok()
            && answer["clean"] == true
            && answer["households"][0]["outcome"] == "finished"
            && answer["households"][0]["levels"][0]["volume"] == 40,
        format!("{}\n{}", run.stdout, run.stderr),
    );
    let fetches = clip_fetches(&s);
    s.check(
        "fetch",
        "sim",
        "Kitchen fetched the clip from the CLI's listener",
        fetches.len() == 1
            && fetches[0].0 == "Kitchen"
            && fetches[0].1.starts_with("http://127.0.0.1:")
            && fetches[0].2 == Ok(200)
            && fetches[0].3,
        format!("{fetches:?}"),
    );
    let kitchen = room(&s, s.ip("Kitchen"));
    s.check(
        "restored",
        "sim",
        "Kitchen plays the stream at 20 again",
        kitchen == kitchen_before,
        format!("{kitchen:?}"),
    );
    let office = room(&s, s.ip("Office"));
    s.check(
        "untouched",
        "sim",
        "Office was not announced to and did not change",
        office == office_before,
        format!("{office:?}"),
    );

    let run = s.cli("unknown-chime", &["chime", "gong", "--rooms", "Kitchen"]);
    s.check(
        "unknown-chime",
        "cli",
        "an unknown chime is INVALID_ARGUMENT (exit 2) and names the chimes",
        run.code == Some(2) && run.stderr.contains("bell"),
        &run.stderr,
    );
    s.finish();
}

#[test]
fn serve_announces_over_http_from_its_event_listener() {
    let mut s = Scenario::start("announce-serve");
    let clock = s.sim(SimHousehold::standard()).clock().clone();
    let (kitchen_before, _) = set_the_scene(&mut s);
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = ready
        .split_whitespace()
        .find_map(|w| w.strip_prefix("http=http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );

    let answer = {
        let _ticking = Ticker::start(clock);
        http(
            &api,
            "POST",
            "/announce",
            &[("Content-Type", "application/json")],
            r#"{"chime":"rise","rooms":["Kitchen"],"volume":30}"#,
        )
    };
    let (status, body) = answer.map_or((0, String::new()), |(status, _, body)| (status, body));
    let reply = json(&body);
    s.check(
        "announce",
        "http",
        "POST /announce plays the chime and puts everything back",
        status == 200 && reply["clean"] == true,
        &body,
    );
    let fetches = clip_fetches(&s);
    let api_port = api.rsplit(':').next().unwrap_or_default().to_string();
    s.check(
        "fetch",
        "sim",
        "Kitchen fetched the clip from the daemon's event listener, not the API's",
        fetches.len() == 1
            && fetches[0].2 == Ok(200)
            && fetches[0].1.starts_with("http://127.0.0.1:")
            && !fetches[0].1.contains(&format!(":{api_port}/")),
        format!("{fetches:?}"),
    );
    let kitchen = room(&s, s.ip("Kitchen"));
    s.check(
        "restored",
        "sim",
        "Kitchen plays the stream at 20 again",
        kitchen == kitchen_before,
        format!("{kitchen:?}"),
    );

    let (status, _, body) = http(
        &api,
        "POST",
        "/announce",
        &[("Content-Type", "application/json")],
        r#"{"rooms":["Kitchen"]}"#,
    )
    .unwrap_or((0, Vec::new(), String::new()));
    s.check(
        "nothing-to-say",
        "http",
        "neither text nor chime is a 422 INVALID_ARGUMENT",
        status == 422 && json(&body)["code"] == "INVALID_ARGUMENT",
        &body,
    );
    drop(daemon);
    s.finish();
}

#[test]
fn a_local_wav_plays_in_direct_mode_and_restores_the_music() {
    let mut s = Scenario::start("announce-wav-direct");
    let clock = s.sim(SimHousehold::standard()).clock().clone();
    let (kitchen_before, office_before) = set_the_scene(&mut s);
    let path = s.dir().join("local announcement.wav");
    std::fs::write(&path, Chime::Bell.wav()).unwrap();
    let run = {
        let _ticking = Ticker::start(clock);
        s.cli(
            "wav",
            &[
                "--direct",
                "--json",
                "announce",
                "--file",
                path.to_str().unwrap(),
                "--rooms",
                "Kitchen",
                "--volume",
                "30",
            ],
        )
    };
    let reply = json(&run.stdout);
    s.check(
        "wav",
        "cli",
        "a WAV file plays to completion in direct mode",
        run.ok() && reply["clean"] == true && reply["households"][0]["outcome"] == "finished",
        format!("{}\n{}", run.stdout, run.stderr),
    );
    let fetches = clip_fetches(&s);
    s.check(
        "fetch",
        "sim",
        "the player fetched valid WAV from the CLI",
        fetches.len() == 1 && fetches[0].2 == Ok(200) && fetches[0].3,
        format!("{fetches:?}"),
    );
    s.check(
        "restored",
        "sim",
        "the music, volume, and other room are unchanged after the WAV",
        room(&s, s.ip("Kitchen")) == kitchen_before && room(&s, s.ip("Office")) == office_before,
        "",
    );

    std::fs::write(&path, b"not a PCM WAV").unwrap();
    let run = s.cli(
        "malformed-wav",
        &["--direct", "announce", "--file", path.to_str().unwrap()],
    );
    s.check(
        "malformed-wav",
        "cli",
        "malformed local audio fails without another announcement",
        run.code == Some(2) && clip_fetches(&s).len() == 1,
        &run.stderr,
    );
    s.finish();
}

/// Keep the audio short while exercising upload limits beyond the previous
/// one-MiB HTTP and ten-MiB MCP defaults. RIFF allows unknown chunks.
fn wav_with_metadata(bytes: usize) -> Vec<u8> {
    let mut wav = Chime::Bell.wav();
    wav.extend_from_slice(b"JUNK");
    wav.extend_from_slice(&u32::try_from(bytes).unwrap().to_le_bytes());
    wav.resize(wav.len() + bytes, 0);
    let riff_size = u32::try_from(wav.len() - 8).unwrap();
    wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
    wav
}

fn mcp_announce(mcp: &str, arguments: &Value) -> Value {
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "announce", "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    http(
        mcp,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "announce"),
        ],
        &call.to_string(),
    )
    .map_or(Value::Null, |(_, _, body)| {
        let rpc: Value = serde_json::from_str(&body).unwrap_or_else(|_| {
            body.lines()
                .find_map(|line| line.strip_prefix("data:"))
                .and_then(|data| serde_json::from_str(data.trim()).ok())
                .unwrap_or(Value::Null)
        });
        rpc["result"].clone()
    })
}

#[test]
fn wav_uploads_share_playback_and_policy_on_http_mcp_and_daemon_cli() {
    let mut s = Scenario::start("announce-wav-daemon");
    let clock = s.sim(SimHousehold::standard()).clock().clone();
    let (kitchen_before, office_before) = set_the_scene(&mut s);
    std::fs::write(
        s.dir().join("data/policy.toml"),
        "[defaults]\nmax_volume = 25\n",
    )
    .unwrap();
    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = ready
        .split_whitespace()
        .find_map(|w| w.strip_prefix("http=http://"))
        .unwrap_or("")
        .to_string();
    let mcp = ready
        .split_whitespace()
        .find_map(|w| w.strip_prefix("mcp=http://"))
        .and_then(|url| url.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "both listeners and the live model are ready",
        !api.is_empty() && !mcp.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );

    let request = AnnounceRequest {
        rooms: vec!["Kitchen".into()],
        volume: Some(90),
        ..AnnounceRequest::from_wav(&wav_with_metadata(2 * 1024 * 1024)).unwrap()
    };
    let (status, _, body) = {
        let _ticking = Ticker::start(clock.clone());
        http(
            &api,
            "POST",
            "/announce",
            &[("Content-Type", "application/json")],
            &serde_json::to_string(&request).unwrap(),
        )
        .unwrap_or((0, Vec::new(), String::new()))
    };
    let reply = json(&body);
    s.check(
        "http-wav",
        "http",
        "HTTP accepts a WAV upload above one MiB, caps volume, and restores",
        status == 200
            && reply["clean"] == true
            && reply["households"][0]["levels"][0]["volume"] == 25,
        &body,
    );

    let request = AnnounceRequest {
        rooms: vec!["Kitchen".into()],
        volume: Some(90),
        ..AnnounceRequest::from_wav(&wav_with_metadata(8 * 1024 * 1024)).unwrap()
    };
    let result = {
        let _ticking = Ticker::start(clock.clone());
        mcp_announce(&mcp, &serde_json::to_value(request).unwrap())
    };
    s.check(
        "mcp-wav",
        "mcp-http",
        "MCP accepts more than ten MiB of base64 and shares WAV playback and caps",
        result["isError"] != true
            && result["structuredContent"]["clean"] == true
            && result["structuredContent"]["households"][0]["levels"][0]["volume"] == 25,
        &result,
    );

    let path = s.dir().join("client announcement.wav");
    std::fs::write(&path, Chime::Bell.wav()).unwrap();
    let run = {
        let _ticking = Ticker::start(clock);
        s.cli(
            "daemon-wav",
            &[
                "--daemon",
                "--json",
                "announce",
                "--file",
                path.to_str().unwrap(),
                "--rooms",
                "Kitchen",
            ],
        )
    };
    s.check(
        "daemon-wav",
        "cli",
        "the CLI uploads its local WAV through the daemon",
        run.ok() && json(&run.stdout)["clean"] == true,
        format!("{}\n{}", run.stdout, run.stderr),
    );
    let fetches = clip_fetches(&s);
    let bases: Vec<_> = fetches
        .iter()
        .map(|(_, url, _, _)| url.split("/media/").next())
        .collect();
    s.check(
        "shared-media",
        "sim",
        "all three surfaces serve validated WAVs from the daemon's one media listener",
        fetches.len() == 3
            && fetches
                .iter()
                .all(|(_, _, status, wav)| *status == Ok(200) && *wav)
            && bases.windows(2).all(|pair| pair[0] == pair[1]),
        format!("{fetches:?}"),
    );
    s.check(
        "restored",
        "sim",
        "all surfaces restore the music and leave the other room alone",
        room(&s, s.ip("Kitchen")) == kitchen_before && room(&s, s.ip("Office")) == office_before,
        "",
    );

    for (step, request) in [
        ("bad-wav", json!({"wav_base64": "bm90IFdBVg=="})),
        (
            "ambiguous-source",
            json!({"text": "hello", "wav_base64": "bm90IFdBVg=="}),
        ),
        ("host-path", json!({"file": "/private/clip.wav"})),
    ] {
        let (status, _, body) = http(
            &api,
            "POST",
            "/announce",
            &[("Content-Type", "application/json")],
            &request.to_string(),
        )
        .unwrap();
        s.check(
            step,
            "http",
            "invalid upload input is rejected before playback",
            status == 422
                && json(&body)["code"] == "INVALID_ARGUMENT"
                && clip_fetches(&s).len() == 3,
            &body,
        );
    }
    let result = mcp_announce(&mcp, &json!({"wav_base64": "%%%"}));
    s.check(
        "bad-mcp-wav",
        "mcp-http",
        "MCP reports the shared validation failure without playback",
        result["isError"] == true
            && result["content"][0]["text"]
                .as_str()
                .is_some_and(|s| s.contains("INVALID_ARGUMENT"))
            && clip_fetches(&s).len() == 3,
        &result,
    );
    drop(daemon);
    s.finish();
}

#[cfg(unix)]
#[test]
fn daemon_speech_preserves_text_and_voice_and_uses_the_announcement_pipeline() {
    let mut s = Scenario::start("announce-speech-daemon");
    let clock = s.sim(SimHousehold::standard()).clock().clone();
    let (kitchen_before, office_before) = set_the_scene(&mut s);
    let fixture = s.dir().join("voice.wav");
    let script = s.dir().join("speech-engine.sh");
    let heard_text = s.dir().join("speech-input.txt");
    let heard_voice = s.dir().join("speech-voice.txt");
    std::fs::write(&fixture, Chime::Bell.wav()).unwrap();
    std::fs::write(
        &script,
        "cat > \"$5\"\nprintf '%s' \"$4\" > \"$3\"\ncp \"$1\" \"$2\"\n",
    )
    .unwrap();
    let config = json!({
        "backend": "command",
        "command": ["/bin/sh", script, fixture, "{output}", heard_voice, "{voice}", heard_text],
        "timeout_secs": 5
    });
    std::fs::write(
        s.dir().join("data/speech.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    let ready = daemon.wait_line("fsonos serve: ready", Duration::from_secs(20));
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "the daemon is ready for local speech",
        ready.is_some() && live.is_some(),
        daemon.seen.join("\n"),
    );

    let text = "Dinner is ready; $(ignored) 'quoted'";
    let voice = "voice with spaces $(ignored)";
    let run = {
        let _ticking = Ticker::start(clock.clone());
        s.cli(
            "say",
            &[
                "--daemon", "--json", "say", text, "--voice", voice, "--rooms", "Kitchen",
            ],
        )
    };
    let reply = json(&run.stdout);
    s.check(
        "say",
        "cli",
        "configured speech plays and restores through the daemon",
        run.ok() && reply["clean"] == true && reply["households"][0]["outcome"] == "finished",
        format!("{}\n{}", run.stdout, run.stderr),
    );
    s.check(
        "speech-input",
        "engine",
        "text reaches stdin and the full voice stays one literal argument",
        std::fs::read_to_string(&heard_text).ok().as_deref() == Some(text)
            && std::fs::read_to_string(&heard_voice).ok().as_deref() == Some(voice),
        "",
    );

    let run = {
        let _ticking = Ticker::start(clock);
        s.cli(
            "chime",
            &["--daemon", "--json", "chime", "bell", "--rooms", "Kitchen"],
        )
    };
    s.check(
        "chime",
        "cli",
        "chimes use the same daemon route",
        run.ok() && json(&run.stdout)["clean"] == true,
        format!("{}\n{}", run.stdout, run.stderr),
    );
    let fetches = clip_fetches(&s);
    s.check(
        "same-listener",
        "sim",
        "speech and chime both came from the daemon media listener",
        fetches.len() == 2
            && fetches[0].1.split("/media/").next() == fetches[1].1.split("/media/").next()
            && fetches
                .iter()
                .all(|(_, _, status, wav)| *status == Ok(200) && *wav),
        format!("{fetches:?}"),
    );
    s.check(
        "restored",
        "sim",
        "speech and chime restore the previous music, volume and other room",
        room(&s, s.ip("Kitchen")) == kitchen_before && room(&s, s.ip("Office")) == office_before,
        "",
    );
    drop(daemon);
    s.finish();
}
