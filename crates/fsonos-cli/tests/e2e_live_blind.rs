//! A daemon that cannot see the speakers says so (vg3s.7), the way one
//! macOS keeps off the LAN looks: `fsonos serve` started while every
//! virtual player is off reports `blind` in GET /health, and `fsonos doctor`
//! fails on it (exit 7) with the Local Network remedy. Once the players are
//! back the daemon finds them by itself within its houseless retry, with no
//! restart, and the doctor passes again.

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

/// `GET /health`'s `live` block (`Null` when there is none).
fn live(api: &str) -> Value {
    http(api, "GET", "/health", &[], "")
        .ok()
        .and_then(|(_, _, body)| serde_json::from_str::<Value>(&body).ok())
        .map_or(Value::Null, |health| health["live"].clone())
}

/// Poll `probe` every 200 ms until it holds or `within` has passed.
fn eventually(within: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `fsonos doctor --only daemon` against the daemon at `api`: its exit
/// code, and its `daemon.health` entry.
fn daemon_doctor(s: &mut Scenario, step: &str, api: &str) -> (Option<i32>, Value) {
    let run = s.cli(
        step,
        &["doctor", "--json", "--only", "daemon", "--http", api],
    );
    let report: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    let health = report["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|x| x["id"] == "daemon.health").cloned())
        .unwrap_or(Value::Null);
    (run.code, health)
}

/// Power every virtual player off (`true`) or back on.
fn power(s: &mut Scenario, off: bool) {
    let step = if off { "power-off" } else { "power-on" };
    let switched: Vec<String> = s.sim_handle().map_or_else(Vec::new, |sim| {
        sim.players()
            .iter()
            .filter(|p| sim.set_offline(&p.uuid, off).is_err())
            .map(|p| p.uuid.clone())
            .collect()
    });
    s.check(
        step,
        "sim",
        "every virtual player switches",
        switched.is_empty(),
        format!("failed: {switched:?}"),
    );
}

/// A loopback port free now.
fn free_port() -> String {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_or_else(|_| "127.0.0.1:0".to_owned(), |a| a.to_string())
}

#[test]
fn a_daemon_that_cannot_see_the_speakers_says_so_and_finds_them_when_they_return() {
    let mut s = Scenario::start("live-blind");
    s.sim(SimHousehold::standard());
    power(&mut s, true);
    // A fixed port, as the owner's daemon has: the doctor probes the address
    // serve recorded, and port 0 is nothing to probe.
    let http = free_port();
    let mut daemon = s.spawn(
        "serve",
        &["serve", "--http", &http, "--mcp-http", "127.0.0.1:0"],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();

    let mut seen = Value::Null;
    let blind = eventually(Duration::from_secs(20), || {
        seen = live(&api);
        seen["verdict"] == "blind"
    });
    s.check(
        "blind",
        "http",
        "GET /health says blind while no player answers, with no players counted",
        blind && seen["players"] == 0,
        &seen,
    );
    let (code, health) = daemon_doctor(&mut s, "doctor-blind", &api);
    s.check(
        "doctor-blind",
        "cli",
        "the doctor fails on a blind daemon (exit 7) and names the Local Network permission",
        code == Some(7)
            && health["status"] == "fail"
            && health["remedy"]
                .as_str()
                .is_some_and(|r| r.contains("Local Network")),
        format!("exit {code:?}: {health}"),
    );

    // The players come back: the daemon, knowing no room, retries within
    // 30 s (HOUSELESS_RETRY), finds both households and subscribes.
    power(&mut s, false);
    let found = eventually(Duration::from_secs(75), || {
        seen = live(&api);
        seen["verdict"] == "ok"
    });
    s.check(
        "found",
        "http",
        "the daemon finds the players again by itself, without a restart",
        found && seen["households"] == 2 && seen["players"].as_u64().is_some_and(|n| n >= 4),
        &seen,
    );
    let (code, health) = daemon_doctor(&mut s, "doctor-found", &api);
    s.check(
        "doctor-found",
        "cli",
        "the doctor's daemon.health passes once the daemon sees the speakers",
        health["status"] == "pass" && code != Some(7),
        format!("exit {code:?}: {health}"),
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
