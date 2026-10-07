//! `fsonos serve` end to end against the virtual households: the daemon
//! binds ephemeral loopback ports and says where, the HTTP API controls the
//! sim (confirmed from the sim's state), the MCP server answers over
//! streamable HTTP, and SIGINT stops it cleanly.

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::get_transport_info;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::time::Duration;

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
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

#[test]
fn serve_hosts_the_api_and_mcp_over_the_sim() {
    let mut s = Scenario::start("serve");
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
    let ready = daemon.wait_line("fsonos serve: ready", Duration::from_secs(20));
    s.check(
        "ready",
        "daemon",
        "serve reports the addresses it bound",
        ready.is_some(),
        daemon.seen.join("\n"),
    );
    let ready = ready.unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("");
    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("");
    s.check(
        "ready",
        "daemon",
        "both listeners are on loopback ephemeral ports",
        api.starts_with("127.0.0.1:") && mcp.starts_with("127.0.0.1:") && !api.ends_with(":8099"),
        &ready,
    );

    check_http_api(&mut s, api);
    check_http_reads(&mut s, api);
    check_browser_safety(&mut s, api, mcp);
    check_mcp_http(&mut s, mcp);

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0)
            && daemon
                .seen
                .iter()
                .any(|l| l.contains("fsonos serve: stopping")),
        format!("exit {code:?}; {}", daemon.seen.join(" | ")),
    );
    s.finish();
}

/// The HTTP API: health, zones, and play/pause confirmed from the sim.
fn check_http_api(s: &mut Scenario, api: &str) {
    let kitchen = s.ip("Kitchen");
    let health = http(api, "GET", "/health", &[], "");
    s.check(
        "health",
        "http",
        "GET /health is ok",
        health
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("\"ok\"")),
        format!("{health:?}"),
    );
    let zones = http(api, "GET", "/zones", &[], "");
    let zone_count = zones
        .as_ref()
        .ok()
        .and_then(|(_, _, b)| serde_json::from_str::<Value>(b).ok())
        .and_then(|v| v.as_array().map(Vec::len));
    s.check(
        "zones",
        "http",
        "GET /zones lists the four sim rooms",
        zone_count == Some(4),
        format!("{zones:?}"),
    );

    let json_body = [("Content-Type", "application/json")];
    let play =
        json!({ "zone": "Kitchen", "source_uri": "x-rincon-mp3radio://stream.example.org/a.mp3" });
    let played = http(api, "POST", "/play", &json_body, &play.to_string());
    s.check(
        "play",
        "http",
        "POST /play is 200",
        played.as_ref().is_ok_and(|(code, _, _)| *code == 200),
        format!("{played:?}"),
    );
    let paused = http(
        api,
        "POST",
        "/pause",
        &json_body,
        &json!({ "zone": "kitchen" }).to_string(),
    );
    s.check(
        "pause",
        "http",
        "POST /pause is 200",
        paused.as_ref().is_ok_and(|(code, _, _)| *code == 200),
        format!("{paused:?}"),
    );
    let state = get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
    s.check(
        "pause",
        "sim",
        "Kitchen is paused in the sim",
        state == Some(TransportState::Paused),
        format!("{state:?}"),
    );
}

/// One modern-era (2026-07-28, sessionless) `tools/call` over streamable
/// HTTP: the `result`, or `Null`, plus the raw answer for the log.
fn mcp_call(mcp: &str, tool: &str, arguments: &Value) -> (Value, String) {
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let answer = http(
        mcp,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", tool),
        ],
        &call.to_string(),
    );
    let result = answer
        .as_ref()
        .map_or(Value::Null, |(_, _, body)| rpc(body)["result"].clone());
    (result, format!("{answer:?}"))
}

/// The MCP server's tools over streamable HTTP, through the daemon's shared
/// surface to the sim.
fn check_mcp_http(s: &mut Scenario, mcp: &str) {
    let (result, raw) = mcp_call(mcp, "list_zones", &json!({}));
    let zones = result["structuredContent"]["zones"]
        .as_array()
        .map_or(0, Vec::len);
    s.check(
        "mcp-list-zones",
        "mcp-http",
        "list_zones over MCP HTTP sees the four sim rooms",
        result["isError"] != true && zones == 4,
        raw,
    );
    let (result, raw) = mcp_call(mcp, "list_favorites", &json!({ "zone": "Bedroom" }));
    s.check(
        "mcp-list-favorites",
        "mcp-http",
        "list_favorites over MCP HTTP lists the household's favorites",
        result["structuredContent"]["favorites"]
            .as_array()
            .is_some_and(|f| f.len() == 5),
        raw,
    );
    let (result, raw) = mcp_call(mcp, "doctor", &json!({}));
    s.check(
        "mcp-doctor",
        "mcp-http",
        "doctor over MCP HTTP returns the report",
        result["structuredContent"]["schema"] == 1
            && result["structuredContent"]["checks"]
                .as_array()
                .is_some_and(|c| !c.is_empty()),
        raw,
    );
    let (result, raw) = mcp_call(mcp, "get_zone_state", &json!({ "zone": "Living Room" }));
    s.check(
        "mcp-zone-state",
        "mcp-http",
        "get_zone_state over MCP HTTP matches GET /zones/{room}/state",
        result["structuredContent"]["transport_state"] == "playing",
        raw,
    );
}

/// The read routes and play-a-favorite over HTTP.
fn check_http_reads(s: &mut Scenario, api: &str) {
    let parse = |r: &std::io::Result<e2e::HttpAnswer>| {
        r.as_ref()
            .ok()
            .and_then(|(_, _, b)| serde_json::from_str::<Value>(b).ok())
            .unwrap_or(Value::Null)
    };
    let doctor = http(api, "GET", "/doctor", &[], "");
    let report = parse(&doctor);
    s.check(
        "doctor",
        "http",
        "GET /doctor returns the report with the daemon's own checks",
        report["schema"] == 1
            && report["checks"]
                .as_array()
                .is_some_and(|c| c.iter().any(|x| x["id"] == "daemon.bind")),
        format!("{doctor:?}"),
    );
    let favorites = http(api, "GET", "/favorites?zone=Living+Room", &[], "");
    let titles = parse(&favorites);
    s.check(
        "favorites",
        "http",
        "GET /favorites lists the household's favorites",
        titles
            .as_array()
            .is_some_and(|f| f.iter().any(|x| x["title"] == "Sim Radio")),
        format!("{favorites:?}"),
    );
    let json_body = [("Content-Type", "application/json")];
    let body = json!({ "zone": "Living Room", "favorite": "sim radio" }).to_string();
    let played = http(api, "POST", "/play/favorite", &json_body, &body);
    s.check(
        "play-favorite",
        "http",
        "POST /play/favorite is 200",
        played.as_ref().is_ok_and(|(code, _, _)| *code == 200),
        format!("{played:?}"),
    );
    let state = http(api, "GET", "/zones/living%20room/state", &[], "");
    let state_json = parse(&state);
    s.check(
        "state",
        "http",
        "GET /zones/{room}/state shows the station playing",
        state_json["transport_state"] == "playing"
            && state_json["track"]["uri"]
                .as_str()
                .is_some_and(|u| u.starts_with("x-rincon-mp3radio:")),
        format!("{state:?}"),
    );
}

/// The daemon's listeners refuse what a hostile web page would send.
fn check_browser_safety(s: &mut Scenario, api: &str, mcp: &str) {
    let rebound = http(api, "GET", "/zones", &[("Host", "evil.example")], "");
    s.check(
        "foreign-host",
        "http",
        "a foreign Host (DNS rebinding) is refused",
        rebound.as_ref().is_ok_and(|(code, _, _)| *code == 400),
        format!("{rebound:?}"),
    );
    let plain = http(
        api,
        "POST",
        "/pause",
        &[("Content-Type", "text/plain")],
        r#"{"zone":"Kitchen"}"#,
    );
    s.check(
        "text-plain-write",
        "http",
        "a no-preflight text/plain POST is 415",
        plain.as_ref().is_ok_and(|(code, _, _)| *code == 415),
        format!("{plain:?}"),
    );
    let foreign = http(
        api,
        "GET",
        "/zones",
        &[("Origin", "https://evil.example")],
        "",
    );
    s.check(
        "foreign-origin",
        "http",
        "a foreign Origin is 403 and gets no CORS grant",
        foreign.as_ref().is_ok_and(|(code, headers, _)| {
            *code == 403
                && !headers
                    .iter()
                    .any(|(k, _)| k == "access-control-allow-origin")
        }),
        format!("{foreign:?}"),
    );
    let mcp_foreign = http(
        mcp,
        "POST",
        "/mcp",
        &[
            ("Origin", "https://evil.example"),
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "list_zones"),
        ],
        &json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "list_zones", "arguments": {}, "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        })
        .to_string(),
    );
    s.check(
        "mcp-foreign-origin",
        "mcp-http",
        "the MCP endpoint refuses a foreign Origin (fastmcp answers 400, no result)",
        mcp_foreign
            .as_ref()
            .is_ok_and(|(code, _, body)| (400..500).contains(code) && !body.contains("\"result\"")),
        format!("{mcp_foreign:?}"),
    );
}
