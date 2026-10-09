//! The owner's standing DJ preferences on every surface, against the virtual
//! households. The CLI sets and refuses them in preferences.toml. A daemon
//! started after a ban never picks the banned composer's works. HTTP and
//! MCP over HTTP read and change the same file.

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
                source_uri: format!("spotify:track:simpf{w}{m}00000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simpfalb{w}00000000000")),
                album_artists: vec!["Sim Ensemble".into()],
                disc_number: Some(1),
                track_number: u32::try_from(m + 1).ok(),
                added_at: Some(1_790_000_000),
                genres: Vec::new(),
                label: None,
                duration_secs: Some(300),
                explicit: false,
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

/// One sessionless (2026-07-28) `tools/call` over the daemon's MCP listener.
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

/// `fsonos serve` up with its live model: the daemon, its API and MCP.
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

#[test]
fn standing_preferences_hold_on_every_surface() {
    let mut s = Scenario::start("dj-prefs");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    check_cli(&mut s);

    let (mut daemon, api, mcp) = serve(&mut s);
    let (code, shown) = call(&api, "GET", "/dj/preferences", None);
    s.check(
        "http-show",
        "http",
        "the daemon reads the same preferences.toml: Brahms is banned",
        code == 200 && shown["ban"]["artists"] == json!(["Johannes Brahms"]),
        &shown,
    );

    let (code, started) = call(
        &api,
        "POST",
        "/dj/start",
        Some(&json!({ "zone": "Living Room" })),
    );
    let (_, status) = call(&api, "GET", "/zones/Living%20Room/dj", None);
    let composers: Vec<String> = std::iter::once(&status["now"])
        .chain(status["next"].as_array().into_iter().flatten())
        .filter_map(|w| w["composer"].as_str().map(str::to_owned))
        .collect();
    s.check(
        "banned-never-picked",
        "http",
        "the DJ started after the ban picks none of Brahms's works",
        code == 200 && !composers.is_empty() && composers.iter().all(|c| c != "Johannes Brahms"),
        format!("{started}\n{status}"),
    );

    let result = mcp_call(&mcp, "dj_preferences", &json!({}));
    s.check(
        "mcp-show",
        "mcp-http",
        "dj_preferences says the same in lines",
        result["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("Ban: artists Johannes Brahms.")),
        &result,
    );
    let result = mcp_call(
        &mcp,
        "dj_prefer",
        &json!({ "key": "favor.artists", "value": "Joseph Haydn" }),
    );
    s.check(
        "mcp-prefer",
        "mcp-http",
        "dj_prefer favors Haydn, from the next pick",
        result["content"][0]["text"].as_str().is_some_and(|t| {
            t.starts_with("favor.artists: Joseph Haydn") && t.contains("next pick")
        }),
        &result,
    );

    let unban = json!({ "key": "ban.artists", "value": "Johannes Brahms", "unset": true });
    let (code, done) = call(&api, "POST", "/dj/preferences", Some(&unban));
    s.check(
        "http-unset",
        "http",
        "POST /dj/preferences unsets the ban; Haydn stays favored",
        code == 200
            && done["changed"] == true
            && done["preferences"]["ban"]["artists"] == json!([])
            && done["preferences"]["favor"]["artists"] == json!(["Joseph Haydn"]),
        &done,
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "exit",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    let run = s.cli("show-after", &["dj", "prefs", "show"]);
    s.check(
        "show-after",
        "cli",
        "the CLI reads back what the daemon wrote",
        run.code == Some(0) && run.stdout.contains("Favor: artists Joseph Haydn."),
        format!("{}{}", run.stdout, run.stderr),
    );
    s.finish();
}

/// The CLI on its own: nothing stated, a ban, and the refusals.
fn check_cli(s: &mut Scenario) {
    let run = s.cli("show-empty", &["dj", "prefs", "show", "--json"]);
    let shown: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    s.check(
        "show-empty",
        "cli",
        "with no preferences.toml nothing is stated",
        run.code == Some(0) && shown["explicit"] == false && shown["energy"].is_null(),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli(
        "ban",
        &["dj", "prefs", "set", "ban.artists", "Johannes Brahms"],
    );
    s.check(
        "ban",
        "cli",
        "fsonos dj prefs set bans Brahms and writes preferences.toml",
        run.code == Some(0)
            && run.stdout.contains("ban.artists: Johannes Brahms")
            && s.dir().join("data").join("preferences.toml").exists(),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli("energy-bad", &["dj", "prefs", "set", "energy", "150"]);
    s.check(
        "energy-bad",
        "cli",
        "an energy past 100 is INVALID_ARGUMENT, exit 2",
        run.code == Some(2) && run.stderr.contains("INVALID_ARGUMENT"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli("key-bad", &["dj", "prefs", "set", "favour.genres", "jazz"]);
    s.check(
        "key-bad",
        "cli",
        "an unknown key is INVALID_ARGUMENT, suggesting the real ones",
        run.code == Some(2) && run.stderr.contains("favor.genres"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli(
        "unset-nothing",
        &["dj", "prefs", "unset", "favor.genres", "jazz"],
    );
    s.check(
        "unset-nothing",
        "cli",
        "unsetting what is not there changes nothing, and says so",
        run.code == Some(0) && run.stdout.contains("nothing to remove"),
        format!("{}{}", run.stdout, run.stderr),
    );
}
