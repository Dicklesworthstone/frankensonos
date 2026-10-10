//! The DJ follows the music when it moves, through `fsonos serve` against
//! the virtual households: the DJ starts in the Kitchen and is steered
//! there, the music moves to the Office (which leads the group from then
//! on), and the Office has the DJ, its steering and a skip; the Kitchen has
//! neither. Moved back unsteered, the DJ leaves the Kitchen's stale
//! steering behind rather than taking it on.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
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
        for (m, movement) in ["I. Allegro", "II. Andante"].iter().enumerate() {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:simmv{w}{m}00000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simmvalb{w}00000000000")),
                album_artists: vec!["Sim Ensemble".into()],
                disc_number: Some(1),
                track_number: u32::try_from(m + 1).ok(),
                duration_secs: Some(300),
                origin: Origin::SavedAlbum,
                ..LibraryItem::default()
            });
        }
    }
    items
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

/// A room's DJ status, once `ready` holds (the live model hears the new
/// topology from the players' events), or the last one read.
fn status_when(api: &str, room: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, status) = call(api, "GET", &format!("/zones/{room}/dj"), None);
        if ready(&status) || Instant::now() > deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The library in the store, and `fsonos serve` up with its live model:
/// the daemon and its API's `host:port`.
fn serve_seeded(s: &mut Scenario) -> (Daemon, String) {
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
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "serve",
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

/// Unsteered now, the DJ moves back to a Kitchen steered while idle: the
/// Kitchen's old steering was for music it no longer plays.
fn check_moving_back_unsteered(s: &mut Scenario, api: &str) {
    let clear = json!({ "zone": "Office", "clear": true });
    let (cleared, _) = call(api, "POST", "/dj/steer", Some(&clear));
    let focus = json!({ "zone": "Kitchen", "mood": "focus" });
    let (stale, _) = call(api, "POST", "/dj/steer", Some(&focus));
    let back = json!({ "zone": "Office", "to": "Kitchen" });
    let (moved_back, outcome) = call(api, "POST", "/move", Some(&back));
    s.check(
        "move-back",
        "http",
        "the unsteered DJ moves back to the Kitchen, steered focus while idle",
        cleared == 200 && stale == 200 && moved_back == 200,
        &outcome,
    );
    let kitchen_back = status_when(api, "Kitchen", |st| st["running"] == true);
    s.check(
        "stale",
        "http",
        "the Kitchen has the DJ without its stale focus steering",
        kitchen_back["running"] == true && kitchen_back["steering"]["source"] != "session",
        &kitchen_back,
    );
}

#[test]
fn the_dj_and_its_steering_follow_the_music_to_another_room() {
    let mut s = Scenario::start("dj-move");
    s.sim(SimHousehold::standard());
    let (mut daemon, api) = serve_seeded(&mut s);

    let kitchen = json!({ "zone": "Kitchen" });
    let (started, start) = call(&api, "POST", "/dj/start", Some(&kitchen));
    let steer = json!({ "zone": "Kitchen", "mood": "calm" });
    let (steered, _) = call(&api, "POST", "/dj/steer", Some(&steer));
    s.check(
        "start",
        "http",
        "the DJ plays in the Kitchen, steered calm",
        started == 200 && steered == 200,
        format!("{start}"),
    );

    let to_office = json!({ "zone": "Kitchen", "to": "Office" });
    let (moved, outcome) = call(&api, "POST", "/move", Some(&to_office));
    s.check(
        "move",
        "http",
        "the music moves to the Office",
        moved == 200,
        &outcome,
    );

    let office = status_when(&api, "Office", |st| st["running"] == true);
    s.check(
        "office",
        "http",
        "the Office has the DJ and its calm steering",
        office["running"] == true
            && office["steering"]["source"] == "session"
            && office["steering"]["mood"] == "calm",
        &office,
    );
    let (_, kitchen_now) = call(&api, "GET", "/zones/Kitchen/dj", None);
    s.check(
        "kitchen",
        "http",
        "the Kitchen has neither the DJ nor the steering",
        kitchen_now["running"] == false && kitchen_now["steering"]["source"] != "session",
        &kitchen_now,
    );
    let (skipped, skip) = call(&api, "POST", "/dj/skip", Some(&json!({ "zone": "Office" })));
    s.check(
        "skip",
        "http",
        "a skip in the Office reaches the DJ there",
        skipped == 200,
        &skip,
    );

    check_moving_back_unsteered(&mut s, &api);

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
