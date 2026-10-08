//! `fsonos serve` behind Tailscale Serve, without Tailscale: requests reach
//! the loopback listeners exactly as Serve forwards them (`Host` is the
//! host's MagicDNS name, with the port for 8443; `X-Forwarded-*` and the
//! `Tailscale-User-*` headers set), and both the HTTP API and the MCP
//! endpoint answer. The daemon runs with Tailscale detection off, as one
//! started at boot before Tailscale is up does: Serve must not depend on the
//! daemon having seen the tailnet. A foreign Host is still refused.

mod e2e;

use e2e::{Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::time::Duration;

/// A placeholder MagicDNS name: Serve forwards whatever name the client used.
const NAME: &str = "sonos-host.example-tailnet.ts.net";

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// The headers Tailscale Serve adds in front of the client's request.
fn forwarded(host: &str) -> Vec<(&str, &str)> {
    vec![
        ("Host", host),
        ("X-Forwarded-For", "100.101.102.104"),
        ("X-Forwarded-Proto", "https"),
        ("Tailscale-User-Login", "ada@example.com"),
        ("Tailscale-User-Name", "Ada"),
    ]
}

/// The JSON-RPC message in an MCP HTTP answer (plain JSON or one SSE event).
fn rpc(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|_| {
        body.lines()
            .find_map(|l| l.strip_prefix("data:"))
            .and_then(|d| serde_json::from_str(d.trim()).ok())
            .unwrap_or(Value::Null)
    })
}

/// The API behind `--https=443`: Host is the bare name.
fn check_api(s: &mut Scenario, api: &str) {
    let health = http(api, "GET", "/health", &forwarded(NAME), "");
    s.check(
        "api-health",
        "http",
        "GET /health as Serve forwards it (Host: <name>.ts.net) is ok",
        health
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("\"ok\"")),
        format!("{health:?}"),
    );
    let zones = http(api, "GET", "/zones", &forwarded(NAME), "");
    let zone_count = zones
        .as_ref()
        .ok()
        .and_then(|(_, _, b)| serde_json::from_str::<Value>(b).ok())
        .and_then(|v| v.as_array().map(Vec::len));
    s.check(
        "api-zones",
        "http",
        "GET /zones through Serve lists the four sim rooms",
        zone_count == Some(4),
        format!("{zones:?}"),
    );
    let rebound = http(api, "GET", "/health", &forwarded("evil.example"), "");
    s.check(
        "api-foreign-host",
        "http",
        "a Host outside ts.net is still refused (DNS rebinding)",
        rebound.as_ref().is_ok_and(|(code, _, _)| *code == 400),
        format!("{rebound:?}"),
    );
}

/// The browser defenses still hold behind a ts.net Host: a page served
/// anywhere else (a Funnel page on another tailnet's ts.net name, say)
/// cannot drive the speakers, and writes must be JSON.
fn check_browser_defenses(s: &mut Scenario, api: &str) {
    let mut foreign = forwarded(NAME);
    foreign.extend([
        ("Origin", "https://attacker.other-tailnet.ts.net"),
        ("Content-Type", "application/json"),
    ]);
    let body = json!({ "zone": "Kitchen" }).to_string();
    let refused = http(api, "POST", "/pause", &foreign, &body);
    s.check(
        "api-foreign-origin",
        "http",
        "a foreign Origin behind a ts.net Host is 403 UNTRUSTED_ORIGIN",
        refused
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 403 && body.contains("UNTRUSTED_ORIGIN")),
        format!("{refused:?}"),
    );
    let mut plain = forwarded(NAME);
    plain.push(("Content-Type", "text/plain"));
    let unjson = http(api, "POST", "/pause", &plain, &body);
    s.check(
        "api-json-only",
        "http",
        "a write that is not application/json is 415 behind a ts.net Host",
        unjson.as_ref().is_ok_and(|(code, _, _)| *code == 415),
        format!("{unjson:?}"),
    );
}

/// MCP behind `--https=8443`: Host carries the port.
fn check_mcp(s: &mut Scenario, mcp: &str) {
    let host = format!("{NAME}:8443");
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "list_zones",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let mut headers = forwarded(&host);
    headers.extend([
        ("Content-Type", "application/json"),
        ("Accept", "application/json"),
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "list_zones"),
    ]);
    let answer = http(mcp, "POST", "/mcp", &headers, &call.to_string());
    let zones = answer.as_ref().map_or(0, |(_, _, body)| {
        rpc(body)["result"]["structuredContent"]["zones"]
            .as_array()
            .map_or(0, Vec::len)
    });
    s.check(
        "mcp-list-zones",
        "mcp-http",
        "an MCP tools/call as Serve forwards it (Host: <name>.ts.net:8443) lists the rooms",
        zones == 4,
        format!("{answer:?}"),
    );
}

#[test]
fn requests_forwarded_by_tailscale_serve_reach_the_api_and_mcp() {
    let mut s = Scenario::start("behind_serve");
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
        .to_owned();
    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_owned();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "ready",
        "daemon",
        "serve is up on loopback and found the households",
        api.starts_with("127.0.0.1:") && mcp.starts_with("127.0.0.1:") && live.is_some(),
        daemon.seen.join("\n"),
    );

    check_api(&mut s, &api);
    check_browser_defenses(&mut s, &api);
    check_mcp(&mut s, &mcp);

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
