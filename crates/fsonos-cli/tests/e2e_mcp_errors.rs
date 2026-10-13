//! A failed MCP tool call carries its error as data, not only as text: over
//! `fsonos serve`'s MCP listener (2026-07-28), a tool that refuses answers
//! `isError` with the `CODE: detail. Hint: ...` text, and its structured
//! content is the very body the HTTP API answers for the same request (code,
//! detail, hint, suggestions, retryable). A call that succeeds is unchanged.

mod e2e;

use e2e::{Scenario, http};
use fsonos_sim::SimHousehold;
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

/// One sessionless (2026-07-28) `tools/call`: the `result`, or `Null`.
fn mcp_call(mcp: &str, tool: &str, arguments: &Value) -> Value {
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
    http(
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
    )
    .map_or(Value::Null, |(_, _, body)| rpc(&body)["result"].clone())
}

#[test]
fn a_failed_tool_call_carries_the_error_body_http_answers() {
    let mut s = Scenario::start("mcp_errors");
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
        !api.is_empty() && !mcp.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );

    // The same refusal on both surfaces: a room nobody has.
    let request = json!({"zone": "Kichen"});
    let refused = http(
        &api,
        "POST",
        "/pause",
        &[("Content-Type", "application/json")],
        &request.to_string(),
    );
    let body = refused.as_ref().map_or(Value::Null, |(_, _, b)| {
        serde_json::from_str(b).unwrap_or(Value::Null)
    });
    s.check(
        "http-refuses",
        "http",
        "POST /pause for an unknown room answers 404 UNKNOWN_ROOM",
        refused.as_ref().is_ok_and(|(code, _, _)| *code == 404) && body["code"] == "UNKNOWN_ROOM",
        format!("{refused:?}"),
    );

    let failed = mcp_call(&mcp, "pause", &request);
    let text = failed["content"][0]["text"].as_str().unwrap_or("");
    s.check(
        "mcp-tool-error",
        "mcp-http",
        "the pause tool answers isError with the coded text",
        failed["isError"] == true && text.starts_with("UNKNOWN_ROOM: "),
        &failed,
    );
    s.check(
        "mcp-structured-error",
        "mcp-http",
        "its structured content is the body HTTP answers (code, detail, hint, suggestions, \
         retryable)",
        failed["structuredContent"] == body
            && body["retryable"] == false
            && body["hint"].as_str().is_some_and(|h| !h.is_empty()),
        format!("mcp {failed}\nhttp {body}"),
    );

    let zones = mcp_call(&mcp, "list_zones", &json!({}));
    s.check(
        "mcp-success",
        "mcp-http",
        "a call that succeeds is no error and carries its JSON",
        zones.get("isError").is_none_or(|e| e == false)
            && zones["structuredContent"]["zones"]
                .as_array()
                .is_some_and(|z| !z.is_empty()),
        &zones,
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
