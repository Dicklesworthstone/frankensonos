//! Time passing reaches `fsonos serve`: as the simulated clock moves, an
//! album plays on to its next track and then to its end, and the daemon's
//! zone state follows from the players' events alone (no player is asked).

mod e2e;

use e2e::{Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::Value;
use std::time::{Duration, Instant};

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// `GET path` as JSON (`Null` when it is not).
fn get(api: &str, path: &str) -> Value {
    http(api, "GET", path, &[], "")
        .ok()
        .and_then(|(_, _, body)| serde_json::from_str(&body).ok())
        .unwrap_or(Value::Null)
}

/// Poll `probe` every 100 ms until it holds or `within` has passed.
fn eventually(within: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Reads of playback state the daemon made of any player.
fn reads(s: &Scenario) -> usize {
    s.sim_handle().map_or(0, |sim| {
        sim.soap_log()
            .iter()
            .filter(|e| {
                matches!(
                    e.action.as_str(),
                    "GetVolume" | "GetTransportInfo" | "GetPositionInfo" | "GetMediaInfo"
                )
            })
            .count()
    })
}

/// Kitchen's (transport state, queue position, track URI) in the daemon.
fn kitchen(api: &str) -> (String, u64, String) {
    let state = get(api, "/zones/Kitchen/state");
    (
        state["transport_state"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        state["track"]["queue_position"].as_u64().unwrap_or(0),
        state["track"]["uri"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

#[test]
fn serve_follows_an_album_as_sim_time_plays_it_through() {
    let mut s = Scenario::start("time");
    s.sim(SimHousehold::standard());
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
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "serve's live model is up",
        live.is_some(),
        daemon.seen.join("\n"),
    );

    // A three-track album, each track three minutes long.
    let run = s.cli("play", &["play", "Kitchen", "--favorite", "Sim Symphonies"]);
    s.check("play", "cli", "exits 0", run.ok(), &run.stderr);
    let mut now = kitchen(&api);
    let started = eventually(Duration::from_secs(5), || {
        now = kitchen(&api);
        now.0 == "playing" && now.1 == 1
    });
    s.check(
        "play",
        "http",
        "zone state shows the album playing from track 1",
        started,
        format!("{now:?}"),
    );
    let first_uri = now.2.clone();
    let polled = reads(&s);

    // Three minutes and a second later: the next track, from its event.
    if let Some(sim) = s.sim_handle() {
        sim.advance(Duration::from_secs(181));
    }
    let moved_on = eventually(Duration::from_secs(3), || {
        now = kitchen(&api);
        now.0 == "playing" && now.1 == 2 && now.2 != first_uri
    });
    s.check(
        "next-track",
        "http",
        "within 3 s zone state is on track 2, a track of its own",
        moved_on,
        format!("{now:?} (track 1 was {first_uri})"),
    );

    // Past the album's end: stopped, back on its first track.
    if let Some(sim) = s.sim_handle() {
        sim.advance(Duration::from_secs(360));
    }
    let ended = eventually(Duration::from_secs(3), || {
        now = kitchen(&api);
        now.0 == "stopped" && now.1 == 1
    });
    s.check(
        "album-end",
        "http",
        "within 3 s zone state shows the album ended (stopped, track 1)",
        ended,
        format!("{now:?}"),
    );
    let asked = reads(&s) - polled;
    s.check(
        "events-only",
        "sim",
        "no player was asked: each change came from its event",
        asked == 0,
        format!("{asked} Get* calls to players while time passed"),
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0),
        format!("exit {code:?}"),
    );
    s.finish();
}
