//! The DJ through `fsonos serve` against the virtual households: a library
//! of synthetic classical works, as a Spotify sync leaves it, then
//! `POST /dj/start` queues whole works and plays them (confirmed in the
//! sim), the live model's playback records the DJ's track in the history,
//! `POST /dj/skip` moves to another work, and `POST /dj/stop` stops.

mod e2e;

use e2e::{Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_proto::control::{get_position_info, get_transport_info};
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

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
                source_uri: format!("spotify:track:simdj{w}{m}00000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simalbum{w}000000000000")),
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

/// `POST path` with a JSON body: (status, body).
fn post(api: &str, path: &str, body: &Value) -> (u16, Value) {
    http(
        api,
        "POST",
        path,
        &[("Content-Type", "application/json")],
        &body.to_string(),
    )
    .map_or((0, Value::Null), |(code, _, b)| {
        (code, serde_json::from_str(&b).unwrap_or(Value::Null))
    })
}

/// The track URI the Living Room's transport is on.
fn on_now(s: &Scenario) -> String {
    get_position_info(&s.lan(), s.ip("Living Room"))
        .map(|p| p.uri)
        .unwrap_or_default()
}

#[test]
fn the_dj_queues_whole_works_and_skips_and_stops() {
    let mut s = Scenario::start("dj");
    s.sim(SimHousehold::standard());
    seed(&mut s);

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

    let zone = json!({ "zone": "Living Room" });
    let (status, started) = post(&api, "/dj/start", &zone);
    s.check(
        "start",
        "http",
        "POST /dj/start answers what the DJ plays, steered by the program's mood",
        status == 200
            && started["done"]
                .as_str()
                .is_some_and(|d| d.contains("the DJ is playing") && d.contains("(mood: bright)")),
        &started,
    );
    let state = get_transport_info(&s.lan(), s.ip("Living Room"))
        .map(|t| t.state)
        .ok();
    let first = on_now(&s);
    s.check(
        "start",
        "sim",
        "the Living Room plays a DJ movement from its queue",
        state == Some(TransportState::Playing) && first.contains("simdj"),
        format!("{state:?} on {first}"),
    );

    // The live model's playback events record the DJ's track.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut history = Value::Null;
    while Instant::now() < deadline {
        history = http(&api, "GET", "/history?zone=Living+Room", &[], "")
            .ok()
            .and_then(|(_, _, b)| serde_json::from_str(&b).ok())
            .unwrap_or(Value::Null);
        if history.to_string().contains("spotify:track:simdj") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    s.check(
        "history",
        "http",
        "the DJ's track is in the history",
        history.to_string().contains("spotify:track:simdj"),
        &history,
    );

    check_skip_and_stop(&mut s, &api, &first);

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

/// `POST /dj/skip` moves to another work than `first`; `POST /dj/stop` stops,
/// and a second stop has no DJ to stop.
fn check_skip_and_stop(s: &mut Scenario, api: &str, first: &str) {
    let (status, skipped) = post(api, "/dj/skip", &json!({ "zone": "Living Room" }));
    let after = on_now(s);
    s.check(
        "skip",
        "http",
        "POST /dj/skip moves to another work",
        status == 200
            && skipped["done"]
                .as_str()
                .is_some_and(|d| d.starts_with("skipped to"))
            && after.contains("simdj")
            && after != first,
        format!("{skipped}; {first} -> {after}"),
    );

    let (status, stopped) = post(api, "/dj/stop", &json!({ "zone": "Living Room" }));
    let state = get_transport_info(&s.lan(), s.ip("Living Room"))
        .map(|t| t.state)
        .ok();
    s.check(
        "stop",
        "http",
        "POST /dj/stop stops the group",
        status == 200 && state == Some(TransportState::Stopped),
        format!("{stopped}; {state:?}"),
    );
    let (status, again) = post(api, "/dj/stop", &json!({ "zone": "Living Room" }));
    s.check(
        "stop-again",
        "http",
        "stopping a DJ that isn't running is NO_DJ_SESSION",
        status == 404 && again["code"] == "NO_DJ_SESSION",
        &again,
    );
}

/// The data directory as a sync and the owner leave it: the library in the
/// store, and a moods.toml with one all-day program.
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

    // One all-day program, so the pick does not depend on when this runs
    // (a moods.toml's programs replace the built-in ones).
    let moods =
        "[[programs]]\ndays = \"daily\"\nfrom = \"00:00\"\nto = \"23:59\"\nmood = \"bright\"\n";
    let wrote = std::fs::write(s.dir().join("data").join("moods.toml"), moods);
    s.check(
        "moods",
        "store",
        "an all-day bright program",
        wrote.is_ok(),
        format!("{wrote:?}"),
    );
}
