//! Steering the DJ by genre and decade on every surface, against the virtual
//! households: HTTP keeps the classical works out and then lets only jazz in,
//! MCP lets only the classical works in, and the CLI takes decades as people
//! write them. Every song and work here is made up; the songs carry the
//! genre tags and release years a library read gives, and the cache keeps.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use serde_json::{Value, json};
use std::time::Duration;

const STATUS: &str = "/zones/Living%20Room/dj";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

fn item(uri: String, title: String, artists: Vec<String>, album: String) -> LibraryItem {
    LibraryItem {
        source_uri: uri,
        title,
        artists,
        album: Some(album),
        disc_number: Some(1),
        added_at: Some(1_790_000_000),
        duration_secs: Some(240),
        origin: Origin::SavedAlbum,
        ..LibraryItem::default()
    }
}

/// Two classical works of two movements each, and four songs.
fn library() -> Vec<LibraryItem> {
    let mut items = Vec::new();
    let works = [
        ("Ludwig van Beethoven", "Symphony No. 5 in C Minor, Op. 67"),
        ("Johannes Brahms", "Symphony No. 4 in E Minor, Op. 98"),
    ];
    for (w, (composer, work)) in works.iter().enumerate() {
        for (m, movement) in ["I. Allegro con brio", "II. Andante"].iter().enumerate() {
            let mut track = item(
                format!("spotify:track:simgn{w}{m}00000000000000"),
                format!("{work}: {movement}"),
                vec![(*composer).into(), "Sim Ensemble".into()],
                format!("{composer}: Works"),
            );
            track.album_uri = Some(format!("spotify:album:simgnalb{w}00000000000"));
            track.album_artists = vec!["Sim Ensemble".into()];
            track.track_number = u32::try_from(m + 1).ok();
            items.push(track);
        }
    }
    let songs = [
        ("Juniper Vale", "Paper Lanterns", "pop", 1964),
        ("Juniper Vale", "Night Bus Home", "pop", 1966),
        ("Nina Marsh", "Copper Skyline", "cool jazz", 1962),
        ("Nina Marsh", "Slow Tide Rising", "cool jazz", 1975),
    ];
    for (n, (artist, title, genre, year)) in songs.iter().enumerate() {
        let mut song = item(
            format!("spotify:track:simgnsong{n}000000000000"),
            (*title).into(),
            vec![(*artist).into()],
            format!("{artist}: Singles"),
        );
        song.genres = vec![(*genre).into()];
        song.release_year = Some(*year);
        items.push(song);
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
        "a synced library: two classical works and four songs, all DJ candidates",
        seeded
            .as_ref()
            .is_ok_and(|sync| sync.candidates == 8 && sync.classical == 4),
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

/// The work playing now and the next ones.
fn works(status: &Value) -> Vec<&Value> {
    std::iter::once(&status["now"])
        .chain(status["next"].as_array().into_iter().flatten())
        .filter(|w| w.is_object())
        .collect()
}

/// How many movements each of them has.
fn movements(status: &Value) -> Vec<u64> {
    works(status)
        .iter()
        .filter_map(|w| w["movements"].as_u64())
        .collect()
}

/// Stop the DJ and start it again, so every pick follows the steering now.
fn restart(api: &str) -> (u16, Value) {
    let zone = json!({ "zone": "Living Room" });
    call(api, "POST", "/dj/stop", Some(&zone));
    let (code, _) = call(api, "POST", "/dj/start", Some(&zone));
    let (_, status) = call(api, "GET", STATUS, None);
    (code, status)
}

#[test]
fn the_dj_is_steered_by_genre_and_decade_on_every_surface() {
    let mut s = Scenario::start("dj-genres");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let (mut daemon, api, mcp) = serve(&mut s);

    let steer = json!({
        "zone": "Living Room",
        "constraints": { "exclude_genres": ["classical"], "decades": [1960] }
    });
    let (code, steered) = call(&api, "POST", "/dj/steer", Some(&steer));
    s.check(
        "steer-http",
        "http",
        "POST /dj/steer: no classical, from the 1960s",
        code == 200
            && steered["done"]
                .as_str()
                .is_some_and(|d| d.contains("no classical, from the 1960s")),
        &steered,
    );
    let (code, status) = restart(&api);
    let played = movements(&status);
    s.check(
        "songs-only",
        "http",
        "the DJ keeps the classical works out: only songs, one movement each",
        code == 200
            && !played.is_empty()
            && played.iter().all(|&m| m == 1)
            && status["steering"]["constraints"]["exclude_genres"] == json!(["classical"])
            && status["steering"]["constraints"]["decades"] == json!([1960]),
        &status,
    );

    // "jazz" finds "cool jazz", a tag only Nina Marsh's songs carry.
    let jazz = json!({ "zone": "Living Room", "constraints": { "include_genres": ["jazz"] } });
    let (code, steered) = call(&api, "POST", "/dj/steer", Some(&jazz));
    let (_, status) = restart(&api);
    let artists: Vec<&str> = works(&status)
        .iter()
        .filter_map(|w| w["composer"].as_str())
        .collect();
    s.check(
        "jazz-only",
        "http",
        "include_genres jazz: only the songs tagged cool jazz",
        code == 200
            && steered["done"]
                .as_str()
                .is_some_and(|d| d.contains("jazz only"))
            && !artists.is_empty()
            && artists.iter().all(|&a| a == "Nina Marsh"),
        format!("{steered}\n{status}"),
    );

    let result = mcp_call(
        &mcp,
        "dj_steer",
        &json!({ "zone": "Living Room", "include_genres": ["classical"] }),
    );
    let (code, status) = restart(&api);
    let played = movements(&status);
    s.check(
        "classical-only",
        "mcp-http",
        "dj_steer include_genres classical: only the classical works, whole",
        result["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("classical only"))
            && code == 200
            && !played.is_empty()
            && played.iter().all(|&m| m == 2)
            && status["steering"]["constraints"]["include_genres"] == json!(["classical"]),
        format!("{result}\n{status}"),
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

/// The CLI through the daemon: decades as people write them, and refused
/// when they aren't one.
fn check_cli(s: &mut Scenario, api: &str) {
    let run = s.cli(
        "decade-bad",
        &["dj", "steer", "Living Room", "--decade", "1965"],
    );
    s.check(
        "decade-bad",
        "cli",
        "a year that starts no decade is refused before anything is sent",
        run.code == Some(2) && run.stderr.contains("is not a decade"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli(
        "steer-cli",
        &[
            "dj",
            "steer",
            "Living Room",
            "--not-genre",
            "classical",
            "--decade",
            "70s",
            "--decade",
            "1980s",
        ],
    );
    let (_, status) = call(api, "GET", STATUS, None);
    s.check(
        "steer-cli",
        "cli",
        "fsonos dj steer --not-genre --decade 70s --decade 1980s: the daemon's steering",
        run.code == Some(0)
            && run
                .stdout
                .contains("no classical, from the 1970s or the 1980s")
            && status["steering"]["constraints"]["decades"] == json!([1970, 1980]),
        format!("{}{}\n{status}", run.stdout, run.stderr),
    );
}
