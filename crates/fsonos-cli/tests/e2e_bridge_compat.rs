//! `fsonos serve --bridge-compat`: the real daemon answers the legacy Sonos
//! bridge's API on loopback, as the `bridge-compat` client under the house
//! policy (vg3s.9). Confirmed from the sim's own state.

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::get_volume;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::time::Duration;

fn parse(answer: &std::io::Result<e2e::HttpAnswer>) -> (u16, Value) {
    answer
        .as_ref()
        .map_or((0, Value::Null), |(status, _, body)| {
            (*status, serde_json::from_str(body).unwrap_or(Value::Null))
        })
}

#[test]
fn serve_answers_the_legacy_bridge_api_under_the_house_policy() {
    let mut s = Scenario::start("bridge-compat");
    s.sim(SimHousehold::standard());
    let policy = s.dir().join("data").join("policy.toml");
    let written = std::fs::write(&policy, "[defaults]\nmax_volume = 30\n");
    s.check(
        "policy",
        "store",
        "a policy.toml capping every room at 30",
        written.is_ok(),
        format!("{written:?}"),
    );
    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
            "--bridge-compat",
            "127.0.0.1:0",
        ],
    );
    let line = daemon
        .wait_line("bridge-compatible API on", Duration::from_secs(20))
        .unwrap_or_default();
    let bridge = line
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    let surveyed = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "listen",
        "daemon",
        "serve says where the bridge-compatible API listens, on loopback, and surveys",
        bridge.starts_with("127.0.0.1:") && surveyed.is_some(),
        daemon.seen.join("\n"),
    );

    let (status, health) = parse(&http(&bridge, "GET", "/health", &[], ""));
    s.check(
        "health",
        "bridge",
        "GET /health answers {ok, devices} as the bridge did",
        status == 200 && health["ok"] == true && health["devices"].as_u64().is_some_and(|n| n > 0),
        &health,
    );

    let kitchen = s.ip("Kitchen");
    let body = json!({ "zone": "Kitchen", "level": 80 }).to_string();
    let (status, set) = parse(&http(
        &bridge,
        "POST",
        "/volume",
        &[("Content-Type", "application/json")],
        &body,
    ));
    let level = get_volume(&s.lan(), kitchen).ok();
    s.check(
        "volume",
        "bridge",
        "POST /volume asking 80 is held to the cap: 30, and the sim agrees",
        status == 200 && set["ok"] == true && set["volume"] == 30 && level == Some(30),
        format!("{set} | sim {level:?}"),
    );

    let (status, refused) = parse(&http(
        &bridge,
        "POST",
        "/volume",
        &[
            ("Origin", "https://page.example"),
            ("Content-Type", "text/plain"),
        ],
        &json!({ "zone": "Kitchen", "level": 5 }).to_string(),
    ));
    s.check(
        "browser",
        "bridge",
        "a web page's POST is refused and changes nothing",
        status == 403 && get_volume(&s.lan(), kitchen).ok() == Some(30),
        &refused,
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0), the bridge listener with it",
        code == Some(0),
        format!("exit {code:?}"),
    );
    s.finish();
}
