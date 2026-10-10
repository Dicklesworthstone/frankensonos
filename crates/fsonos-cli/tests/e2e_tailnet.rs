//! `fsonos serve` reached over this host's real tailnet (ts-autobind,
//! ts-doctor). Off by default (and in CI); run on the daemon host, with
//! Tailscale up, with:
//!
//! ```text
//! cargo test -p fsonos-cli --features tailscale-live --test e2e_tailnet -- --nocapture
//! ```
//!
//! Only the listener is real: the daemon serves the virtual households, so no
//! speaker is touched. For each address family the tailnet gives this host,
//! serve binds the detected address (on an ephemeral port, so a daemon
//! already running here is not in the way), answers `/health` over it, and
//! stops promptly. (A host that cannot reach its own tailnet IPv6 address,
//! as with Tailscale on macOS, leaves that probe pending.)
//! Over IPv4 it also answers by MagicDNS name, names the tailnet caller by
//! Tailscale's WhoIs (this host's own node: the owner's login, or its tag),
//! whose policy then lets it control, and `fsonos doctor` finds it on the
//! tailnet. The bind set the default plan would choose, and what serve
//! logged, are printed.
#![cfg(feature = "tailscale-live")]

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// Start `fsonos serve` with its HTTP API on `ip`, port chosen by the OS;
/// the daemon and the API's `host:port`.
fn serve_on(s: &mut Scenario, step: &str, ip: IpAddr) -> (Daemon, String) {
    let bind = SocketAddr::new(ip, 0).to_string();
    let mut daemon = s.spawn(
        step,
        &["serve", "--http", &bind, "--mcp-http", "127.0.0.1:0"],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_owned();
    s.check(
        step,
        "daemon",
        &format!("the HTTP API is on the tailnet address {ip}"),
        api.parse::<SocketAddr>()
            .is_ok_and(|a| a.ip() == ip && a.port() != 0),
        daemon.seen.join("\n"),
    );
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        step,
        "daemon",
        "serve's live model finds the households",
        live.is_some(),
        daemon.seen.join("\n"),
    );
    for line in daemon
        .seen
        .iter()
        .filter(|l| l.starts_with("fsonos serve:"))
    {
        eprintln!("tailscale-live: {line}");
    }
    (daemon, api)
}

/// `GET /health` over the tailnet address, and by `name` when given.
fn check_health(s: &mut Scenario, step: &str, api: &str, name: Option<&str>) {
    let health = http(api, "GET", "/health", &[], "");
    s.check(
        step,
        "http",
        &format!("GET /health over {api} is ok"),
        health
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("\"ok\"")),
        format!("{health:?}"),
    );
    if let Some(name) = name {
        let port = api.rsplit(':').next().unwrap_or_default();
        let host = format!("{name}:{port}");
        let by_name = http(api, "GET", "/health", &[("Host", &host)], "");
        s.check(
            step,
            "http",
            "the listener answers for its MagicDNS name",
            by_name.as_ref().is_ok_and(|(code, _, _)| *code == 200),
            format!("{by_name:?}"),
        );
    }
}

/// A tailnet caller is named by Tailscale's WhoIs (here this host's own
/// node), not `unknown`, and its own policy applies: it may read and write.
fn check_policy(s: &mut Scenario, api: &str) {
    let zones = http(api, "GET", "/zones", &[], "");
    let zone_count = zones
        .as_ref()
        .ok()
        .and_then(|(_, _, b)| serde_json::from_str::<Value>(b).ok())
        .and_then(|v| v.as_array().map(Vec::len));
    s.check(
        "read",
        "http",
        "a tailnet caller may read: GET /zones lists the four sim rooms",
        zone_count == Some(4),
        format!("{zones:?}"),
    );
    let policy = http(api, "GET", "/policy", &[], "");
    let you = policy
        .as_ref()
        .ok()
        .and_then(|(_, _, b)| serde_json::from_str::<Value>(b).ok())
        .and_then(|v| v["you"].as_str().map(str::to_owned));
    s.check(
        "named",
        "http",
        "WhoIs names the tailnet caller: GET /policy says who, not `unknown`",
        you.as_deref()
            .is_some_and(|who| !who.is_empty() && who != "unknown"),
        format!("{policy:?}"),
    );
    let paused = http(
        api,
        "POST",
        "/pause",
        &[("Content-Type", "application/json")],
        &json!({ "zone": "Kitchen" }).to_string(),
    );
    s.check(
        "write",
        "http",
        "the named caller's policy lets it control: POST /pause is 200",
        paused.as_ref().is_ok_and(|(code, _, _)| *code == 200),
        format!("{paused:?}"),
    );
}

/// `fsonos doctor` finds Tailscale up and the daemon on the tailnet at `api`.
fn check_doctor(s: &mut Scenario, api: &str) {
    let run = s.cli(
        "doctor",
        &["doctor", "--json", "--only", "tailscale", "--http", api],
    );
    let report: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    let status = |id: &str| {
        report["checks"]
            .as_array()
            .and_then(|c| c.iter().find(|x| x["id"] == id))
            .and_then(|x| x["status"].as_str())
            .unwrap_or("missing")
            .to_owned()
    };
    eprintln!(
        "tailscale-live: doctor running={} magicdns={} reach={}",
        status("tailscale.running"),
        status("tailscale.magicdns"),
        status("tailscale.reach")
    );
    s.check(
        "doctor",
        "cli",
        "tailscale.running and tailscale.reach pass against the tailnet listener",
        status("tailscale.running") == "pass" && status("tailscale.reach") == "pass",
        &run.stdout,
    );
}

/// Whether this host reaches its own address `ip`: Tailscale on macOS drops
/// traffic from the host to its own tailnet IPv6 address.
fn reaches_itself(ip: IpAddr) -> bool {
    let Ok(listener) = std::net::TcpListener::bind((ip, 0)) else {
        return false;
    };
    listener.local_addr().is_ok_and(|addr| {
        std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
    })
}

fn stop(s: &mut Scenario, step: &str, mut daemon: Daemon) {
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        step,
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0)
            && daemon
                .seen
                .iter()
                .any(|l| l.contains("fsonos serve: stopping")),
        format!("exit {code:?}; {}", daemon.seen.join(" | ")),
    );
}

#[test]
fn serve_answers_over_the_detected_tailnet_address() {
    let mut s = Scenario::start("tailnet");
    s.sim(SimHousehold::standard());
    s.on_the_tailnet();
    let status = fsonos_tailscale::detect();
    let plan = fsonos_tailscale::bind_plan(&status, 8099, None);
    eprintln!("tailscale-live: default HTTP bind plan {plan:#?}");
    let Some(tailnet) = status.running().cloned() else {
        s.pending(
            "tailnet",
            &format!("Tailscale is not running here: {status:?}"),
        );
        s.finish();
        return;
    };
    if let Some(v4) = tailnet.ipv4.first() {
        let (daemon, api) = serve_on(&mut s, "serve-v4", IpAddr::V4(*v4));
        check_health(&mut s, "health-v4", &api, tailnet.magic_dns_name.as_deref());
        check_policy(&mut s, &api);
        check_doctor(&mut s, &api);
        stop(&mut s, "stop-v4", daemon);
    }
    if let Some(v6) = tailnet.ipv6.first() {
        let ip = IpAddr::V6(*v6);
        let (daemon, api) = serve_on(&mut s, "serve-v6", ip);
        if reaches_itself(ip) {
            check_health(&mut s, "health-v6", &api, None);
        } else {
            s.pending(
                "health-v6",
                "this host cannot reach its own tailnet IPv6 address (Tailscale on macOS); \
                 probe it from another tailnet device",
            );
        }
        // Shutdown stays prompt even when the wake connect cannot land.
        stop(&mut s, "stop-v6", daemon);
    }
    s.finish();
}
