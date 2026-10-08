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
use fsonos_proto::control::{get_media_info, get_transport_info, get_volume};
use fsonos_sim::{SimClock, SimHousehold};
use fsonos_types::TransportState;
use serde_json::Value;
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
