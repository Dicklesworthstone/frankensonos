//! Calls that take long hold no one else up: while more fades run on the
//! daemon than its listeners have workers (each answers when its fade ends),
//! other requests on both listeners are answered at once.

mod e2e;

use e2e::{Scenario, http, http_waiting};
use fsonos_sim::{SimBuilder, SimHousehold, SimModel, SimPlayerSpec};
use serde_json::json;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// An MCP `tools/call` over streamable HTTP, as an agent sends it.
fn mcp_call(
    mcp: &str,
    tool: &str,
    arguments: &serde_json::Value,
) -> std::io::Result<e2e::HttpAnswer> {
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
    http_waiting(
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
        Duration::from_secs(30),
    )
}

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// The rooms faded over HTTP (S1) and over MCP (S2): one fade each, so none
/// supersedes another.
const HTTP_ROOMS: [&str; 5] = ["Kitchen", "Office", "Den", "Study", "Hall"];
const MCP_ROOMS: [&str; 5] = ["Living Room", "Bedroom", "Patio", "Garage", "Loft"];

/// A house with a room for each fade.
fn ten_rooms() -> SimBuilder {
    let play5 = |name| SimPlayerSpec::new(name, SimModel::Play5Gen1);
    let one = |name| SimPlayerSpec::new(name, SimModel::One);
    SimHousehold::builder()
        .s1(HTTP_ROOMS.map(play5))
        .s2(MCP_ROOMS.map(one))
}

/// Five 6 s fades over HTTP and five over MCP, each answering when it ends:
/// more long calls at once than either listener has workers.
fn start_fades(api: &str, mcp: &str) -> Vec<JoinHandle<std::io::Result<e2e::HttpAnswer>>> {
    HTTP_ROOMS
        .iter()
        .map(|room| {
            let (api, room) = (api.to_string(), (*room).to_string());
            std::thread::spawn(move || {
                http_waiting(
                    &api,
                    "POST",
                    "/volume/fade",
                    &[("Content-Type", "application/json")],
                    &json!({ "zone": room, "volume": 5, "fade_secs": 6 }).to_string(),
                    Duration::from_secs(30),
                )
            })
        })
        .chain(MCP_ROOMS.iter().map(|room| {
            let (mcp, room) = (mcp.to_string(), (*room).to_string());
            std::thread::spawn(move || {
                mcp_call(
                    &mcp,
                    "fade_volume",
                    &json!({ "zone": room, "volume": 6, "seconds": 6 }),
                )
            })
        }))
        .collect()
}

#[test]
fn a_long_call_holds_no_other_request_up() {
    let mut s = Scenario::start("listener-concurrency");
    s.sim(ten_rooms());
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

    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_string();
    let fades = start_fades(&api, &mcp);
    std::thread::sleep(Duration::from_millis(800));
    let asked = Instant::now();
    let health = http(&api, "GET", "/health", &[], "");
    let zones = http(&api, "GET", "/zones", &[], "");
    let listed = mcp_call(&mcp, "list_zones", &json!({}));
    let took = asked.elapsed();
    // An MCP tool's failure is a 200 too: it must carry a result, not an error.
    let ok = |answer: &std::io::Result<e2e::HttpAnswer>| {
        answer.as_ref().is_ok_and(|(code, _, body)| {
            *code == 200 && !body.contains("\"isError\":true") && !body.contains("\"error\"")
        })
    };
    s.check(
        "no-stall",
        "http",
        "while ten 6 s fades run, GET /health, GET /zones and an MCP list_zones answer within 2 s",
        ok(&health) && ok(&zones) && ok(&listed) && took < Duration::from_secs(2),
        format!("after {took:?}: health {health:?}; zones {zones:?}; mcp {listed:?}"),
    );
    let done: Vec<bool> = fades
        .into_iter()
        .map(|f| f.join().is_ok_and(|answer| ok(&answer)))
        .collect();
    s.check(
        "fades-done",
        "http",
        "every fade still answers 200 when it ends",
        done.iter().all(|d| *d),
        format!("{done:?}"),
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
