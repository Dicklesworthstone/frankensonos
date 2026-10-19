//! `fsonos serve` behind Tailscale Serve, without Tailscale: requests reach
//! the loopback listeners exactly as Serve forwards them (`Host` is the
//! host's MagicDNS name, with the port for 8443; `X-Forwarded-*` and the
//! `Tailscale-User-*` headers set), and both the HTTP API and the MCP
//! endpoint answer. The daemon runs with Tailscale detection off, as one
//! started at boot before Tailscale is up does: Serve must not depend on the
//! daemon having seen the tailnet. A foreign Host is still refused. The MCP
//! listener names its callers as the API does: behind `--tailscale-serve`,
//! Serve's login header names the tailnet user, whose own policy applies.

mod e2e;

use e2e::{Daemon, Scenario, http};
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

/// `fsonos serve` on loopback with `extra` flags, up and live: the daemon,
/// its API address and its MCP address.
fn serve(s: &mut Scenario, step: &str, extra: &[&str]) -> (Daemon, String, String) {
    let mut args = vec![
        "serve",
        "--http",
        "127.0.0.1:0",
        "--mcp-http",
        "127.0.0.1:0",
    ];
    args.extend_from_slice(extra);
    let mut daemon = s.spawn(step, &args);
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
        &format!("{step}-ready"),
        "daemon",
        "serve is up on loopback and found the households",
        api.starts_with("127.0.0.1:") && mcp.starts_with("127.0.0.1:") && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api, mcp)
}

#[test]
fn requests_forwarded_by_tailscale_serve_reach_the_api_and_mcp() {
    let mut s = Scenario::start("behind_serve");
    s.sim(SimHousehold::standard());
    let (mut daemon, api, mcp) = serve(&mut s, "serve", &[]);

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

/// A `set_volume` MCP call as Serve forwards it to `mcp` (Host with the 8443
/// port), from `login` (a user-owned device; `None` for a tagged one), with
/// whatever `extra` headers the caller adds: the HTTP answer.
fn set_volume_through_serve(
    mcp: &str,
    login: Option<&str>,
    extra: &[(&str, &str)],
) -> std::io::Result<e2e::HttpAnswer> {
    let host = format!("{NAME}:8443");
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "set_volume",
            "arguments": { "zone": "Kitchen", "volume": 15 },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let mut headers: Vec<(&str, &str)> = forwarded(&host)
        .into_iter()
        .filter(|(name, _)| !name.starts_with("Tailscale-User-"))
        .collect();
    if let Some(login) = login {
        headers.push(("Tailscale-User-Login", login));
    }
    headers.extend_from_slice(extra);
    headers.extend([
        ("Content-Type", "application/json"),
        ("Accept", "application/json"),
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "set_volume"),
    ]);
    http(mcp, "POST", "/mcp", &headers, &call.to_string())
}

/// Whether an MCP answer is a refusal under the house policy.
fn policy_denied(answer: &std::io::Result<e2e::HttpAnswer>) -> bool {
    answer.as_ref().is_ok_and(|(_, _, body)| {
        let reply = rpc(body);
        reply["result"]["isError"] == true && body.contains("POLICY_DENIED")
    })
}

/// Whether an MCP answer is a tool success.
fn succeeded(answer: &std::io::Result<e2e::HttpAnswer>) -> bool {
    answer.as_ref().is_ok_and(|(_, _, body)| {
        let reply = rpc(body);
        reply["result"].is_object() && reply["result"]["isError"] != true
    })
}

/// The newest logged action's id, client and decision.
fn newest_action(api: &str) -> (i64, String, String) {
    let listed = http(api, "GET", "/actions?limit=1", &[], "");
    let newest = listed
        .ok()
        .and_then(|(_, _, body)| serde_json::from_str::<Value>(&body).ok())
        .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
        .unwrap_or(Value::Null);
    (
        newest["id"].as_i64().unwrap_or_default(),
        newest["client"].as_str().unwrap_or_default().to_owned(),
        newest["decision"].as_str().unwrap_or_default().to_owned(),
    )
}

/// The CLI's token, as the running daemon wrote it to `daemon.json`.
fn cli_token(s: &Scenario) -> String {
    std::fs::read_to_string(s.dir().join("data").join("daemon.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|file| file["cli_token"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// p7vq.12: the MCP listener names each caller as the HTTP API does, so the
/// owner's `[clients."<login>"]` rules reach MCP agents and the log names
/// them. Without `--tailscale-serve` the login header is ignored; behind it,
/// the login is the caller, and a call without one (a tagged device) is
/// `loopback-http`. An `Authorization` header a caller sends is refused, and
/// the CLI's own token over MCP is never the CLI.
#[test]
fn the_mcp_listener_names_each_caller_as_the_api_does() {
    let mut s = Scenario::start("behind_serve_mcp_identity");
    s.sim(SimHousehold::standard());
    let written = std::fs::write(
        s.dir().join("data").join("policy.toml"),
        "[clients.\"ada@example.com\"]\ndeny = [\"set_volume\"]\n",
    );
    s.check(
        "policy",
        "store",
        "a policy.toml denying set_volume to ada@example.com",
        written.is_ok(),
        format!("{written:?}"),
    );

    let (mut daemon, api, mcp) = serve(&mut s, "plain", &[]);
    let answer = set_volume_through_serve(&mcp, Some("ada@example.com"), &[]);
    let logged = newest_action(&api);
    s.check(
        "plain-ignores-login",
        "mcp-http",
        "without --tailscale-serve, a login header names no one: set_volume goes through as loopback-http",
        succeeded(&answer) && logged.1 == "loopback-http",
        format!("{answer:?} logged {logged:?}"),
    );
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "plain-stop",
        "daemon",
        "SIGINT stops serve (exit 0)",
        code == Some(0),
        format!("exit {code:?}"),
    );

    let (mut daemon, api, mcp) = serve(&mut s, "behind", &["--tailscale-serve"]);
    let answer = set_volume_through_serve(&mcp, Some("ada@example.com"), &[]);
    let logged = newest_action(&api);
    s.check(
        "login-denied",
        "mcp-http",
        "behind --tailscale-serve, Serve's login is the caller: its own policy refuses set_volume, and the log names it",
        policy_denied(&answer)
            && logged.1 == "ada@example.com"
            && logged.2.starts_with("deny"),
        format!("{answer:?} logged {logged:?}"),
    );
    let answer = set_volume_through_serve(
        &mcp,
        Some("ada@example.com"),
        &[("Authorization", "Bearer fsonos.guess.636c69")],
    );
    let after = newest_action(&api);
    s.check(
        "authorization-refused",
        "mcp-http",
        "an Authorization header the caller sends is refused before any tool runs: nothing is logged",
        !succeeded(&answer) && after.0 == logged.0,
        format!("{answer:?} logged {after:?} (before {logged:?})"),
    );
    // The CLI's own token, sent over MCP, never makes a caller the CLI: a
    // login stays that login, and no login stays loopback-http.
    let token = cli_token(&s);
    let as_cli = [
        ("X-Fsonos-Cli-Token", token.as_str()),
        ("X-Fsonos-Client", "cli"),
    ];
    let answer = set_volume_through_serve(&mcp, Some("ada@example.com"), &as_cli);
    let logged = newest_action(&api);
    s.check(
        "cli-token-is-not-the-cli",
        "mcp-http",
        "the CLI's real token over MCP leaves a Serve login that login: still refused, and logged by name",
        !token.is_empty() && policy_denied(&answer) && logged.1 == "ada@example.com",
        format!("{answer:?} logged {logged:?}"),
    );
    let answer = set_volume_through_serve(&mcp, None, &as_cli);
    let logged = newest_action(&api);
    s.check(
        "tagged-is-loopback",
        "mcp-http",
        "a call with no login (a tagged device), even with the CLI's token, is loopback-http: set_volume goes through and is logged so",
        succeeded(&answer) && logged.1 == "loopback-http",
        format!("{answer:?} logged {logged:?}"),
    );
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "behind-stop",
        "daemon",
        "SIGINT stops serve (exit 0)",
        code == Some(0),
        format!("exit {code:?}"),
    );
    s.finish();
}
