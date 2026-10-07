//! `fsonos serve` on IPv6 loopback: an IPv6 listener answers the Host header
//! clients send it (`[::1]:port`), as a tailnet IPv6 listener must, and still
//! refuses a foreign Host.

mod e2e;

use e2e::{Scenario, http};
use fsonos_sim::SimHousehold;
use std::time::Duration;

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

#[test]
fn an_ipv6_listener_answers_its_own_address() {
    let mut s = Scenario::start("ipv6");
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        s.pending("ipv6", "this host has no IPv6 loopback");
        s.finish();
        return;
    }
    s.sim(SimHousehold::standard());
    let mut daemon = s.spawn(
        "serve",
        &["serve", "--http", "[::1]:0", "--mcp-http", "127.0.0.1:0"],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_owned();
    s.check(
        "ready",
        "daemon",
        "the HTTP API is on [::1]",
        api.starts_with("[::1]:") && !api.ends_with(":0"),
        daemon.seen.join("\n"),
    );

    // `http` sends `Host: [::1]:<port>`, as curl and browsers do.
    let health = http(&api, "GET", "/health", &[], "");
    s.check(
        "health",
        "http",
        "GET /health with Host [::1]:<port> is ok",
        health
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("\"ok\"")),
        format!("{health:?}"),
    );
    let rebound = http(&api, "GET", "/health", &[("Host", "evil.example")], "");
    s.check(
        "foreign-host",
        "http",
        "a foreign Host is still refused",
        rebound.as_ref().is_ok_and(|(code, _, _)| *code == 400),
        format!("{rebound:?}"),
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0),
        format!("exit {code:?}; {}", daemon.seen.join(" | ")),
    );
    s.finish();
}
