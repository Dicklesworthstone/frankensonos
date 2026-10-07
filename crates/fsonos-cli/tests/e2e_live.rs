//! `fsonos serve` keeps a live model of the virtual households: it says when
//! the model is up, zone state follows a change made on a player directly
//! (delivered by a GENA event: no player is asked), and a player that goes
//! offline shows in the doctor once a failed command makes the daemon look.

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::set_volume;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
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

/// The doctor's `daemon.live` entry.
fn live_check(api: &str) -> Value {
    get(api, "/doctor")["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|x| x["id"] == "daemon.live").cloned())
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

#[test]
fn serve_keeps_a_live_model_of_the_sim() {
    let mut s = Scenario::start("live");
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
        "serve says when its live model has surveyed both households",
        live.as_deref()
            .is_some_and(|l| field(l, "households") == Some("2")),
        daemon.seen.join("\n"),
    );
    let events = live
        .as_deref()
        .and_then(|l| field(l, "events"))
        .unwrap_or("");
    s.check(
        "live",
        "daemon",
        "events arrive on loopback under the routes file",
        events.starts_with("http://127.0.0.1:"),
        events,
    );

    let zones = get(&api, "/zones");
    s.check(
        "zones",
        "http",
        "GET /zones lists both households' zones from the live model",
        zones.as_array().is_some_and(|z| z.len() == 4),
        &zones,
    );
    let subscribed = eventually(Duration::from_secs(5), || {
        live_check(&api)["evidence"]["subscriptions"]
            .as_u64()
            .is_some_and(|n| n > 0)
    });
    s.check(
        "subscribed",
        "doctor",
        "the daemon holds event subscriptions",
        subscribed,
        live_check(&api),
    );

    check_external_change(&mut s, &api);
    check_offline(&mut s, &api);

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

/// A volume set on the Kitchen player directly shows in its zone state
/// within 3 s, and nothing asked any player for it.
fn check_external_change(s: &mut Scenario, api: &str) {
    let reads = |s: &Scenario| {
        s.sim_handle().map_or(0, |sim| {
            sim.soap_log()
                .iter()
                .filter(|e| {
                    matches!(
                        e.action.as_str(),
                        "GetVolume" | "GetTransportInfo" | "GetPositionInfo"
                    )
                })
                .count()
        })
    };
    let kitchen = s.ip("Kitchen");
    let before = get(api, "/zones/Kitchen/state")["volume"].as_u64();
    let level = if before == Some(37) { 38 } else { 37 };
    let polled = reads(s);
    let set = set_volume(&s.lan(), kitchen, level);
    s.check(
        "external-change",
        "sim",
        "the Kitchen player's volume is set directly, not through fsonos",
        set.is_ok(),
        format!("{set:?}"),
    );
    let mut state = Value::Null;
    let seen = eventually(Duration::from_secs(3), || {
        state = get(api, "/zones/Kitchen/state");
        state["volume"] == json!(level)
    });
    s.check(
        "external-change",
        "http",
        "zone state shows the new volume within 3 s",
        seen,
        &state,
    );
    let asked = reads(s) - polled;
    s.check(
        "external-change",
        "sim",
        "no player was asked: the change came from its event",
        asked == 0,
        format!("{asked} Get* calls to players while reading zone state"),
    );
}

/// Kitchen goes offline; a command to it fails, the daemon looks again, and
/// the doctor names it offline.
fn check_offline(s: &mut Scenario, api: &str) {
    let uuid = s
        .sim_handle()
        .and_then(|sim| sim.player("Kitchen").map(|p| p.uuid.clone()))
        .unwrap_or_default();
    let off = s.sim_handle().map(|sim| sim.set_offline("Kitchen", true));
    s.check(
        "offline",
        "sim",
        "the Kitchen player powers off",
        matches!(off, Some(Ok(()))),
        format!("{off:?}"),
    );
    let body = json!({ "zone": "Kitchen", "volume": 20 }).to_string();
    let failed = http(
        api,
        "POST",
        "/volume",
        &[("Content-Type", "application/json")],
        &body,
    );
    s.check(
        "offline",
        "http",
        "a command to the offline player fails as unreachable",
        failed
            .as_ref()
            .is_ok_and(|(code, _, b)| *code == 503 && b.contains("PLAYER_UNREACHABLE")),
        format!("{failed:?}"),
    );
    let mut check = Value::Null;
    let named = eventually(Duration::from_secs(10), || {
        check = live_check(api);
        check["status"] == "warn"
            && check["evidence"]["offline"]
                .as_array()
                .is_some_and(|o| o.iter().any(|id| id == uuid.as_str()))
    });
    s.check(
        "offline",
        "doctor",
        "the doctor names the Kitchen player offline",
        named,
        &check,
    );
}
