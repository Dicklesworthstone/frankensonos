//! The DJ through `fsonos serve` against the virtual households: a library
//! of synthetic classical works, as a Spotify sync leaves it, then
//! `POST /dj/start` queues whole works and plays them (confirmed in the
//! sim), the live model's playback records the DJ's track in the history,
//! `POST /dj/skip` moves to another work, and `POST /dj/stop` stops. Then
//! `fsonos dj start|skip|stop` do the same through the daemon, and
//! `fsonos scene apply` of a scene saved with the DJ playing starts the
//! daemon's DJ.

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
    get_position_info(&s.lan(), s.ip("Living Room")).map_or_default(|p| p.uri)
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
    check_the_cli_through_the_daemon(&mut s, &api);
    check_a_dj_scene_through_the_daemon(&mut s, &api);

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

    // The skip reached the next work; the DJ tops the queue up after it
    // and logs that as its own action.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut rows = Value::Null;
    let topped = |rows: &Value| {
        rows.as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["client"] == "dj"
                    && r["intent"] == "dj: top up Living Room's queue"
                    && r["result"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("queued "))
            })
        })
    };
    while Instant::now() < deadline && !topped(&rows) {
        std::thread::sleep(Duration::from_millis(200));
        rows = http(api, "GET", "/actions?client=dj", &[], "")
            .ok()
            .and_then(|(_, _, b)| serde_json::from_str(&b).ok())
            .unwrap_or(Value::Null);
    }
    s.check(
        "top-up-logged",
        "http",
        "the DJ's own top-up is in the action log as client dj",
        topped(&rows),
        &rows,
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

/// `fsonos dj start|skip|stop` reach the daemon's DJ: a start from the CLI
/// is one the daemon runs (its status says so, where a CLI-run DJ would end
/// with the process), and a skip and a stop from the CLI find it.
fn check_the_cli_through_the_daemon(s: &mut Scenario, api: &str) {
    let running = || {
        http(api, "GET", "/zones/Living%20Room/dj", &[], "")
            .ok()
            .and_then(|(_, _, b)| serde_json::from_str::<Value>(&b).ok())
            .is_some_and(|st| st["running"] == true)
    };
    let start = s.cli(
        "cli-start",
        &["--daemon", "dj", "start", "Living Room", "--mood", "calm"],
    );
    let after_start = running();
    s.check(
        "cli-start",
        "cli",
        "fsonos dj start starts the daemon's DJ, steered by the mood",
        start.code == Some(0)
            && start.stdout.contains("the DJ is playing")
            && start.stdout.contains("(mood: calm)")
            && after_start,
        format!("{start:?}; daemon running: {after_start}"),
    );

    let skip = s.cli("cli-skip", &["--daemon", "dj", "skip", "Living Room"]);
    s.check(
        "cli-skip",
        "cli",
        "fsonos dj skip skips in the daemon's DJ",
        skip.code == Some(0) && skip.stdout.starts_with("skipped to"),
        format!("{skip:?}"),
    );

    // The queue the DJ feeds, as the daemon reads it.
    let queue = s.cli(
        "cli-queue",
        &[
            "--daemon",
            "--json",
            "queue",
            "Living Room",
            "--limit",
            "100",
        ],
    );
    let page: Value = serde_json::from_str(&queue.stdout).unwrap_or(Value::Null);
    let playing = page["playing"].as_u64();
    let items = page["items"].as_array().cloned().unwrap_or_default();
    s.check(
        "cli-queue",
        "cli",
        "fsonos queue shows the DJ's movements in order, and the one playing",
        queue.code == Some(0)
            && page["zone"] == "Living Room"
            && playing.is_some_and(|at| items.iter().any(|i| i["position"].as_u64() == Some(at)))
            && items
                .iter()
                .any(|i| i["uri"].as_str().is_some_and(|u| u.contains("simdj"))),
        format!("{queue:?}"),
    );

    let history = s.cli(
        "cli-history",
        &["--daemon", "history", "Living Room", "--limit", "5"],
    );
    s.check(
        "cli-history",
        "cli",
        "fsonos history names the DJ's plays under the Living Room",
        history.code == Some(0)
            && history.stdout.lines().any(|l| {
                l.contains("Living Room: ")
                    && (l.contains(": I. Allegro con brio") || l.contains(": II. Andante"))
            }),
        format!("{history:?}"),
    );

    let stop = s.cli("cli-stop", &["--daemon", "dj", "stop", "Living Room"]);
    let state = get_transport_info(&s.lan(), s.ip("Living Room"))
        .map(|t| t.state)
        .ok();
    let after_stop = running();
    s.check(
        "cli-stop",
        "cli",
        "fsonos dj stop stops the daemon's DJ and the group",
        stop.code == Some(0) && state == Some(TransportState::Stopped) && !after_stop,
        format!("{stop:?}; {state:?}; daemon running: {after_stop}"),
    );
}

/// `fsonos scene apply` reaches the daemon: a scene saved with the DJ
/// playing, applied from the CLI once the DJ stopped, starts the daemon's
/// DJ again (where a CLI-run DJ would end with the process).
fn check_a_dj_scene_through_the_daemon(s: &mut Scenario, api: &str) {
    let running = || {
        http(api, "GET", "/zones/Living%20Room/dj", &[], "")
            .ok()
            .and_then(|(_, _, b)| serde_json::from_str::<Value>(&b).ok())
            .is_some_and(|st| st["running"] == true)
    };
    let zone = json!({ "zone": "Living Room" });
    let (started, _) = post(api, "/dj/start", &zone);
    let (saved, scene) = http(api, "PUT", "/scenes/concert", &[], "")
        .map_or((0, Value::Null), |(code, _, b)| {
            (code, serde_json::from_str(&b).unwrap_or(Value::Null))
        });
    let with_the_dj = scene["groups"]
        .as_array()
        .is_some_and(|g| g.iter().any(|g| g["source"]["kind"] == "dj"));
    let (stopped, _) = post(api, "/dj/stop", &zone);
    s.check(
        "scene-save",
        "http",
        "a scene saved with the DJ playing has the DJ in it",
        started == 200 && saved == 200 && with_the_dj && stopped == 200 && !running(),
        &scene,
    );

    let apply = s.cli("cli-scene-apply", &["scene", "apply", "concert"]);
    let after = running();
    s.check(
        "cli-scene-apply",
        "cli",
        "fsonos scene apply starts the daemon's DJ, as the scene has it",
        apply.code == Some(0) && after,
        format!("{apply:?}; daemon running: {after}"),
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
        "[[programs]]\ndays = \"daily\"\nfrom = \"00:00\"\nto = \"24:00\"\nmood = \"bright\"\n";
    let wrote = std::fs::write(s.dir().join("data").join("moods.toml"), moods);
    s.check(
        "moods",
        "store",
        "an all-day bright program",
        wrote.is_ok(),
        format!("{wrote:?}"),
    );
}
