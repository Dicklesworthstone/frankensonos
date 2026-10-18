//! `fsonos dj rehearse` end to end, through `fsonos serve` against the
//! virtual households with a seeded library: the set the DJ would play from
//! 21:00, alike over the CLI, HTTP and MCP; the same seed, the same set; an
//! hours span; a hypothetical mood; a bad request refused. Nothing is played,
//! queued, logged or recorded.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use serde_json::{Value, json};
use std::time::Duration;

/// The set starts here: a fixed time, so the run does not depend on when
/// the test runs.
const AT: &str = "2030-01-15T21:00:00+00:00";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// Twelve works of two movements by six composers; every identifier is
/// made up.
fn library() -> Vec<LibraryItem> {
    let composers = [
        "Ludwig van Beethoven",
        "Johannes Brahms",
        "Joseph Haydn",
        "Wolfgang Amadeus Mozart",
        "Franz Schubert",
        "Antonin Dvorak",
    ];
    let mut items = Vec::new();
    for w in 0..12 {
        let composer = composers[w % composers.len()];
        let work = format!("Symphony No. {} in C Major", w + 1);
        for (m, movement) in ["I. Allegro con brio", "II. Andante"].iter().enumerate() {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:simrh{w:02}{m}0000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![composer.into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simrhalb{w:02}0000000000")),
                album_artists: vec!["Sim Ensemble".into()],
                disc_number: Some(1),
                track_number: u32::try_from(m + 1).ok(),
                added_at: Some(1_790_000_000),
                duration_secs: Some(300),
                origin: Origin::SavedAlbum,
                ..LibraryItem::default()
            });
        }
    }
    items
}

/// The library in the store, and one all-day bright program.
fn seed(s: &mut Scenario) {
    let seeded = SqliteStore::open(&s.dir().join("data").join("fsonos.db"))
        .map_err(|e| e.to_string())
        .and_then(|mut store| {
            apply_library_read(&mut store, &library()).map_err(|e| e.to_string())
        });
    s.check(
        "library",
        "store",
        "a synced library with twelve classical works",
        seeded.as_ref().is_ok_and(|sync| sync.classical == 24),
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
    let text = body.map_or_default(Value::to_string);
    let headers: &[(&str, &str)] = if body.is_some() {
        &[("Content-Type", "application/json")]
    } else {
        &[]
    };
    http(api, method, path, headers, &text).map_or((0, Value::Null), |(code, _, b)| {
        (code, serde_json::from_str(&b).unwrap_or(Value::Null))
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
    .map_or(Value::Null, |(_, _, body)| {
        let answer: Value = serde_json::from_str(&body).unwrap_or_else(|_| {
            body.lines()
                .find_map(|l| l.strip_prefix("data:"))
                .and_then(|d| serde_json::from_str(d.trim()).ok())
                .unwrap_or(Value::Null)
        });
        answer["result"].clone()
    })
}

/// `fsonos serve` up with its live model: the daemon and its (API, MCP)
/// addresses.
fn serve(s: &mut Scenario) -> (Daemon, String, String) {
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
    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "serve",
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && !mcp.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api, mcp)
}

/// What a rehearsal must leave as it was: the action log, what played, the
/// Living Room's queue and its transport, and the DJ not running.
fn untouched(api: &str) -> Value {
    json!({
        "actions": call(api, "GET", "/actions", None).1,
        "history": call(api, "GET", "/history", None).1,
        "queue": call(api, "GET", "/zones/Living%20Room/queue", None).1,
        "state": call(api, "GET", "/zones/Living%20Room/state", None).1["transport_state"],
        "dj": call(api, "GET", "/zones/Living%20Room/dj", None).1["running"],
    })
}

/// The works of a rehearsal as (start, composer: title, mood).
fn picks(set: &Value) -> Vec<(String, String, String)> {
    set["works"]
        .as_array()
        .map(|works| {
            works
                .iter()
                .map(|w| {
                    (
                        w["at"].as_str().unwrap_or("").to_owned(),
                        format!("{}: {}", w["work"]["composer"], w["work"]["title"]),
                        w["mood"].as_str().unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_rehearsal_plans_the_djs_set_alike_on_every_surface_and_writes_nothing() {
    let mut s = Scenario::start("dj-rehearse");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let (_daemon, api, mcp) = serve(&mut s);
    let before = untouched(&api);

    let args = [
        "dj",
        "rehearse",
        "Living Room",
        "--works",
        "6",
        "--at",
        AT,
        "--pick-seed",
        "7",
        "--json",
        "--daemon",
    ];
    let run = s.cli("rehearse-cli", &args);
    let set: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    let works = picks(&set);
    s.check(
        "rehearse-cli",
        "cli",
        "six works from 21:00, each later than the last, each with its reason and the program's mood",
        run.ok()
            && works.len() == 6
            && works[0].0 == "2030-01-15T21:00:00+00:00"
            && works.windows(2).all(|w| w[0].0 < w[1].0)
            && works.iter().all(|w| w.2 == "bright")
            && set["works"].as_array().is_some_and(|all| {
                all.iter().all(|w| {
                    w["work"]["reason"]["summary"]
                        .as_str()
                        .is_some_and(|r| !r.is_empty())
                        && w["work"]["minutes"] == 10
                })
            })
            && set["summary"]["works"] == 6
            && set["summary"]["minutes"] == 60
            && set["summary"]["hours"][0]["energy_target"].is_u64(),
        format!("{}{}", run.stdout, run.stderr),
    );

    let again = s.cli("rehearse-again", &args);
    s.check(
        "rehearse-again",
        "cli",
        "the same seed rehearses the same set",
        again.ok() && again.stdout == run.stdout,
        &again.stdout,
    );

    let body = json!({ "zone": "Living Room", "works": 6, "at": AT, "seed": 7 });
    let (code, over_http) = call(&api, "POST", "/dj/rehearse", Some(&body));
    s.check(
        "rehearse-http",
        "http",
        "POST /dj/rehearse answers the same set",
        code == 200 && picks(&over_http) == works,
        &over_http,
    );
    let result = mcp_call(&mcp, "dj_rehearse", &body);
    s.check(
        "rehearse-mcp",
        "mcp-http",
        "dj_rehearse answers the same set, and lists it in its text",
        picks(&result["structuredContent"]) == works
            && result["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("21:00") && t.contains("6 works, 60 min")),
        &result,
    );

    check_spans(&mut s, &api);
    s.check(
        "wrote-nothing",
        "http",
        "nothing played, queued, logged or recorded, and the DJ never started",
        untouched(&api) == before,
        format!("{before}\n{}", untouched(&api)),
    );
    s.finish();
}

/// An hours span, a hypothetical mood, and a bad request.
fn check_spans(s: &mut Scenario, api: &str) {
    let body = json!({ "zone": "Living Room", "hours": 2, "at": AT });
    let (code, set) = call(api, "POST", "/dj/rehearse", Some(&body));
    let works = picks(&set);
    s.check(
        "rehearse-hours",
        "http",
        "two hours: twelve ten-minute works, the last starting before 23:00",
        code == 200
            && works.len() == 12
            && works
                .last()
                .is_some_and(|w| w.0.as_str() < "2030-01-15T23:00:00+00:00"),
        &set,
    );
    let body = json!({ "zone": "Living Room", "works": 3, "at": AT, "mood": "Calm" });
    let (code, set) = call(api, "POST", "/dj/rehearse", Some(&body));
    s.check(
        "rehearse-mood",
        "http",
        "a mood named for the rehearsal steers every pick in place of the program",
        code == 200 && picks(&set).iter().all(|w| w.2 == "calm") && picks(&set).len() == 3,
        &set,
    );
    let body = json!({ "zone": "Living Room", "works": 3, "hours": 2 });
    let (code, refused) = call(api, "POST", "/dj/rehearse", Some(&body));
    s.check(
        "rehearse-refused",
        "http",
        "works and hours together are refused as INVALID_ARGUMENT",
        code == 422 && refused["code"] == "INVALID_ARGUMENT",
        &refused,
    );
}
