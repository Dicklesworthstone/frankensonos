//! A call that takes long holds no one else up: while a fade asked for runs
//! on the daemon (it answers when the fade ends), other requests on the same
//! listener are answered at once.

mod e2e;

use e2e::{Scenario, http, http_waiting};
use fsonos_sim::SimHousehold;
use serde_json::json;
use std::time::{Duration, Instant};

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

#[test]
fn a_long_call_holds_no_other_request_up() {
    let mut s = Scenario::start("listener-concurrency");
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
        "the live model is up",
        live.is_some(),
        daemon.seen.join("\n"),
    );

    let fading = {
        let api = api.clone();
        std::thread::spawn(move || {
            http_waiting(
                &api,
                "POST",
                "/volume/fade",
                &[("Content-Type", "application/json")],
                &json!({ "zone": "Kitchen", "volume": 5, "fade_secs": 6 }).to_string(),
                Duration::from_secs(30),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    let asked = Instant::now();
    let health = http(&api, "GET", "/health", &[], "");
    let zones = http(&api, "GET", "/zones", &[], "");
    let took = asked.elapsed();
    let ok = |answer: &std::io::Result<e2e::HttpAnswer>| {
        answer.as_ref().is_ok_and(|(code, _, _)| *code == 200)
    };
    s.check(
        "no-stall",
        "http",
        "while a 6 s fade runs, GET /health and GET /zones answer within 2 s",
        ok(&health) && ok(&zones) && took < Duration::from_secs(2),
        format!("after {took:?}: health {health:?}; zones {zones:?}"),
    );
    let faded = fading.join().ok();
    s.check(
        "fade-done",
        "http",
        "the fade itself still answers 200 when it ends",
        faded.as_ref().is_some_and(ok),
        format!("{faded:?}"),
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    s.finish();
}
