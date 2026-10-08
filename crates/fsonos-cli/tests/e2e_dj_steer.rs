//! Steering the DJ on every surface, through `fsonos serve` against the
//! virtual households: the moods and programs, an unknown mood, a clear with
//! nothing to clear, a start in a mood, its status (what plays, why, the
//! steering) over HTTP, MCP and the CLI, a steer from MCP and its undo, a
//! daemon restart that keeps the steering, and a clear from the CLI.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use serde_json::{Value, json};
use std::time::Duration;

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// Four works of two movements each; every identifier is made up.
fn library() -> Vec<LibraryItem> {
    let works = [
        ("Ludwig van Beethoven", "Symphony No. 5 in C Minor, Op. 67"),
        ("Johannes Brahms", "Symphony No. 4 in E Minor, Op. 98"),
        ("Joseph Haydn", "String Quartet in D Major, Op. 64 No. 5"),
        (
            "Wolfgang Amadeus Mozart",
            "Piano Sonata No. 11 in A Major, K. 331",
        ),
    ];
    let mut items = Vec::new();
    for (w, (composer, work)) in works.iter().enumerate() {
        for (m, movement) in ["I. Allegro con brio", "II. Andante"].iter().enumerate() {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:simst{w}{m}00000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simstalb{w}00000000000")),
                album_artists: vec!["Sim Ensemble".into()],
                disc_number: Some(1),
                track_number: u32::try_from(m + 1).ok(),
                added_at: Some(1_790_000_000),
                genres: Vec::new(),
                label: None,
                duration_secs: Some(300),
                explicit: false,
                origin: Origin::SavedAlbum,
            });
        }
    }
    items
}

/// The library in the store, and one all-day bright program so the steering
/// does not depend on when this runs.
fn seed(s: &mut Scenario) {
    let seeded = SqliteStore::open(&s.dir().join("data").join("fsonos.db"))
        .map_err(|e| e.to_string())
        .and_then(|mut store| {
            apply_library_read(&mut store, &library()).map_err(|e| e.to_string())
        });
    s.check(
        "library",
        "store",
        "a synced library with four classical works",
        seeded.as_ref().is_ok_and(|sync| sync.classical == 8),
        format!("{seeded:?}"),
    );
    let moods =
        "[[programs]]\ndays = \"daily\"\nfrom = \"00:00\"\nto = \"23:59\"\nmood = \"bright\"\n";
    let wrote = std::fs::write(s.dir().join("data").join("moods.toml"), moods);
    s.check(
        "moods-file",
        "store",
        "an all-day bright program",
        wrote.is_ok(),
        format!("{wrote:?}"),
    );
}

/// `method path` with an optional JSON body: (status, body).
fn call(api: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let text = body.map(Value::to_string).unwrap_or_default();
    let headers: &[(&str, &str)] = if body.is_some() {
        &[("Content-Type", "application/json")]
    } else {
        &[]
    };
    http(api, method, path, headers, &text).map_or((0, Value::Null), |(code, _, b)| {
        (code, serde_json::from_str(&b).unwrap_or(Value::Null))
    })
}

/// The answer to a JSON-RPC call (a plain body, or one SSE `data:` line).
fn rpc(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|_| {
        body.lines()
            .find_map(|l| l.strip_prefix("data:"))
            .and_then(|d| serde_json::from_str(d.trim()).ok())
            .unwrap_or(Value::Null)
    })
}

/// One sessionless (2026-07-28) `tools/call` over the daemon's MCP listener:
/// the `result`, or `Null`.
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

/// `fsonos serve` up with its live model: the daemon and its (API, MCP)
/// addresses.
fn serve(s: &mut Scenario, step: &str) -> (Daemon, String, String) {
    let mut daemon = s.spawn(
        step,
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
    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        step,
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && !mcp.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api, mcp)
}

const STATUS: &str = "/zones/Living%20Room/dj";

#[test]
fn the_dj_is_steered_alike_on_every_surface_and_keeps_its_steering() {
    let mut s = Scenario::start("dj-steer");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let (mut daemon, api, mcp) = serve(&mut s, "serve");

    let (code, moods) = call(&api, "GET", "/dj/moods", None);
    let names = moods["moods"].to_string();
    s.check(
        "moods",
        "http",
        "GET /dj/moods lists the built-in moods, moods.toml's program, and that it plays now",
        code == 200
            && names.contains("\"focus\"")
            && names.contains("\"sunday-morning\"")
            && moods["programs"].as_array().is_some_and(|p| p.len() == 1)
            && moods["now"]["source"] == "program"
            && moods["now"]["mood"] == "bright",
        &moods,
    );

    let zone = json!({ "zone": "Living Room", "mood": "disco" });
    let (code, unknown) = call(&api, "POST", "/dj/steer", Some(&zone));
    s.check(
        "unknown-mood",
        "http",
        "an unknown mood is UNKNOWN_MOOD (404), suggesting the moods there are",
        code == 404
            && unknown["code"] == "UNKNOWN_MOOD"
            && unknown["suggestions"].to_string().contains("focus"),
        &unknown,
    );

    let clear = json!({ "zone": "Living Room", "clear": true });
    let (code, nothing) = call(&api, "POST", "/dj/steer", Some(&clear));
    s.check(
        "clear-nothing",
        "http",
        "clearing steering that isn't there succeeds and changes nothing",
        code == 200 && nothing["changed"] == false,
        &nothing,
    );

    let start = json!({ "zone": "Living Room", "mood": "focus", "for_secs": 7200 });
    let (code, started) = call(&api, "POST", "/dj/start", Some(&start));
    s.check(
        "start-in-a-mood",
        "http",
        "POST /dj/start with a mood steers the group and starts the DJ in it",
        code == 200
            && started["done"].as_str().is_some_and(|d| {
                d.contains("steered Living Room's group: focus mood, for 2 hours")
                    && d.contains("the DJ is playing")
                    && d.contains("(mood: focus)")
            }),
        &started,
    );

    check_status(&mut s, &api, &mcp);
    check_steer_and_undo(&mut s, &api, &mcp);

    // The steering is in the store: a new daemon still has it.
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop-serve",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    let (mut daemon, api, _) = serve(&mut s, "serve-again");
    let (code, status) = call(&api, "GET", STATUS, None);
    s.check(
        "restart-keeps-steering",
        "http",
        "after a restart the DJ is no longer running, but its steering is still the zone's",
        code == 200
            && status["running"] == false
            && status["steering"]["source"] == "session"
            && status["steering"]["mood"] == "focus",
        &status,
    );

    check_cli(&mut s, &api);

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "exit",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    s.finish();
}

/// The same status over HTTP, MCP (one process: the DJ runs there) and the
/// CLI (asking the daemon).
fn check_status(s: &mut Scenario, api: &str, mcp: &str) {
    let (code, status) = call(api, "GET", STATUS, None);
    let left = status["steering"]["expires_in_secs"].as_u64().unwrap_or(0);
    s.check(
        "status",
        "http",
        "GET /zones/{room}/dj: the DJ runs, plays a work with its reason, steered by focus for about 2 hours",
        code == 200
            && status["running"] == true
            && status["now"]["composer"].as_str().is_some_and(|c| !c.is_empty())
            && status["now"]["movements"] == 2
            && status["now"]["reason"]["summary"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
            && status["steering"]["source"] == "session"
            && status["steering"]["mood"] == "focus"
            && status["steering"]["constraints"]["energy_bias"] == -1
            && (7000..=7200).contains(&left),
        &status,
    );

    let result = mcp_call(mcp, "dj_status", &json!({ "zone": "Living Room" }));
    let shared = &result["structuredContent"];
    s.check(
        "status-mcp",
        "mcp-http",
        "dj_status answers the HTTP route's shape, and says it in its text",
        shared["running"] == true
            && shared["now"]["title"] == status["now"]["title"]
            && shared["steering"]["mood"] == "focus"
            && result["content"][0]["text"].as_str().is_some_and(|t| {
                t.contains("plays") && t.contains("Steering: steered: focus mood")
            }),
        &result,
    );

    let run = s.cli(
        "status-cli",
        &["dj", "status", "Living Room", "--daemon", api],
    );
    let title = status["now"]["title"].as_str().unwrap_or("?").to_string();
    s.check(
        "status-cli",
        "cli",
        "fsonos dj status asks the daemon: the same work and steering",
        run.code == Some(0)
            && run.stdout.contains("The DJ in Living Room's group plays")
            && run.stdout.contains(&title)
            && run.stdout.contains("Steering: steered: focus mood"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli("why-cli", &["dj", "why", "Living Room", "--daemon", api]);
    s.check(
        "why-cli",
        "cli",
        "fsonos dj why gives the reason and its weighed factors",
        run.code == Some(0) && run.stdout.contains("Why: ") && run.stdout.contains('×'),
        format!("{}{}", run.stdout, run.stderr),
    );
}

/// A steer from MCP replaces the session (seen over HTTP); undo puts the
/// previous one back.
fn check_steer_and_undo(s: &mut Scenario, api: &str, mcp: &str) {
    let result = mcp_call(
        mcp,
        "dj_steer",
        &json!({
            "zone": "Living Room",
            "include_keywords": ["piano"],
            "energy_bias": 1,
            "for_minutes": 30
        }),
    );
    let (_, status) = call(api, "GET", STATUS, None);
    // A session that names no mood takes the program's (bright), with its
    // own constraints on top.
    s.check(
        "steer-mcp",
        "mcp-http",
        "dj_steer replaces the steering: piano works, brighter, for 30 minutes, under the program's mood",
        result["content"][0]["text"].as_str().is_some_and(|t| {
            t.contains("with piano, brighter, for 30 minutes") && t.contains("from the next piece")
        }) && status["steering"]["source"] == "session"
            && status["steering"]["mood"] == "bright"
            && status["steering"]["constraints"]["include_keywords"] == json!(["piano"])
            && status["steering"]["constraints"]["energy_bias"] == 1,
        format!("{result}\n{status}"),
    );

    let (code, undone) = call(api, "POST", "/undo", Some(&json!({ "own_only": false })));
    let (_, status) = call(api, "GET", STATUS, None);
    s.check(
        "undo-steer",
        "http",
        "undo puts the previous steering back: focus, no piano filter",
        code == 200
            && undone["summary"]
                .as_str()
                .is_some_and(|t| t.contains("steering for Living Room put back"))
            && status["steering"]["mood"] == "focus"
            && status["steering"]["constraints"]["include_keywords"].is_null(),
        format!("{undone}\n{status}"),
    );
}

/// The CLI on its own: the steering read directly, a clear, and the moods.
fn check_cli(s: &mut Scenario, api: &str) {
    // FSONOS_HTTP_ADDR is port 0 here: no daemon to ask.
    let run = s.cli("status-direct", &["dj", "status", "Living Room"]);
    s.check(
        "status-direct",
        "cli",
        "without a daemon, fsonos dj status shows the zone's steering, read directly",
        run.code == Some(0)
            && run.stdout.contains("isn't running")
            && run.stdout.contains("Steering: steered: focus mood"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli("clear-cli", &["dj", "steer", "Living Room", "--clear"]);
    let (_, status) = call(api, "GET", STATUS, None);
    s.check(
        "clear-cli",
        "cli",
        "fsonos dj steer --clear: the daemon's next pick follows the program again",
        run.code == Some(0)
            && run.stdout.contains("cleared the DJ steering")
            && status["steering"]["source"] == "program"
            && status["steering"]["mood"] == "bright",
        format!("{}{}\n{status}", run.stdout, run.stderr),
    );
    let run = s.cli("moods-cli", &["dj", "moods", "--daemon", api]);
    s.check(
        "moods-cli",
        "cli",
        "fsonos dj moods marks the mood playing now and lists the program",
        run.code == Some(0)
            && run.stdout.contains("* bright")
            && run.stdout.contains("00:00–23:59  bright")
            && run
                .stdout
                .contains("Now: the time-of-day program: bright mood"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli(
        "steer-bad-cli",
        &["dj", "steer", "Living Room", "--mood", "disco"],
    );
    s.check(
        "steer-bad-cli",
        "cli",
        "an unknown mood from the CLI is UNKNOWN_MOOD, exit 3",
        run.code == Some(3) && run.stderr.contains("UNKNOWN_MOOD"),
        format!("{}{}", run.stdout, run.stderr),
    );
}
