//! A sleep timer outlives a daemon restart, through `fsonos serve` against
//! the virtual households: a timer set through the first serve is the
//! daemon's (it fades the group out), serve stops and starts again on the
//! same data directory, and once the new serve knows the house it has the
//! timer again, still the daemon's and ending when it did.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const ROOM: &str = "Living Room";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// `method path` with an optional JSON body: (status, body).
fn call(api: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let text = body.map(Value::to_string).unwrap_or_default();
    let headers: &[(&str, &str)] = if body.is_some() {
        &[("Content-Type", "application/json")]
    } else {
        &[]
    };
    http(api, method, path, headers, &text).map_or((0, Value::Null), |(code, _, b)| {
        (code, serde_json::from_str(&b).unwrap_or(Value::Null))
    })
}

/// `fsonos serve` up with its live model, and the API's `host:port`.
fn serve(s: &mut Scenario, step: &str) -> (Daemon, String) {
    let mut daemon = s.spawn(step, &["serve", "--http", "127.0.0.1:0"]);
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        step,
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

fn stop(s: &mut Scenario, step: &str, daemon: &mut Daemon) {
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        step,
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
}

/// The Living Room group's timer as `GET /sleep` lists it.
fn timer(api: &str) -> Option<Value> {
    let (code, body) = call(api, "GET", "/sleep", None);
    if code != 200 {
        return None;
    }
    body.as_array()?.iter().find(|t| t["zone"] == ROOM).cloned()
}

#[test]
fn a_sleep_timer_outlives_a_restart_of_the_daemon() {
    let mut s = Scenario::start("sleep-restart");
    s.sim(SimHousehold::standard());
    let (mut first, api) = serve(&mut s, "serve-1");
    let (code, set) = call(
        &api,
        "POST",
        "/sleep",
        Some(&json!({ "zone": ROOM, "duration": "45m" })),
    );
    s.check(
        "set",
        "http",
        "POST /sleep sets the daemon's timer, which fades",
        code == 200 && set["fades"] == true,
        format!("{code} {set}"),
    );
    stop(&mut s, "stop-1", &mut first);

    // The new serve takes the timer up once its first survey has found the
    // house, not before (when every group still looks gone).
    let (mut second, api) = serve(&mut s, "serve-2");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = timer(&api);
    while !seen.as_ref().is_some_and(|t| t["fades"] == true) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        seen = timer(&api);
    }
    s.check(
        "taken-up",
        "http",
        "the restarted serve has the timer again: the daemon's, ending when it did",
        seen.as_ref()
            .is_some_and(|t| t["fades"] == true && t["ends"] == set["ends"]),
        format!("{seen:?} (set: {set})"),
    );
    stop(&mut s, "stop-2", &mut second);
    s.finish();
}
