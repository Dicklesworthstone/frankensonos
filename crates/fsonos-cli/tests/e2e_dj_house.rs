//! Two rooms' DJs do not repeat each other's music, through `fsonos serve`
//! against the virtual households. The daemon's clock is pinned
//! (`--clock-file`), so the DJ in the Kitchen (S1) and the one in the
//! Living Room (S2) draw the same seed: each room's own history alone would
//! have them pick the same works. With anti-repeat house-wide, no work is
//! queued in both rooms, and a skip in the Kitchen picks none of the Living
//! Room's works either.

mod e2e;

use e2e::{Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

const COMPOSERS: [&str; 8] = [
    "Ludwig van Beethoven",
    "Johannes Brahms",
    "Joseph Haydn",
    "Wolfgang Amadeus Mozart",
    "Franz Schubert",
    "Felix Mendelssohn",
    "Robert Schumann",
    "Pyotr Ilyich Tchaikovsky",
];

/// Eight works of two movements each, one per composer; every identifier is
/// made up.
fn library() -> Vec<LibraryItem> {
    let mut items = Vec::new();
    for (w, composer) in COMPOSERS.iter().enumerate() {
        let work = format!("Sonata No. {} in C Major, Op. {}", w + 1, 10 + w);
        for (m, movement) in ["I. Allegro", "II. Andante"].iter().enumerate() {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:simhw{w}{m}00000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Sonatas")),
                album_uri: Some(format!("spotify:album:simhwalb{w}00000000000")),
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

/// The works (by index) on `room`'s queue in the sim, and the queue's URIs.
fn queued_works(s: &Scenario, room: &str) -> (BTreeSet<usize>, Vec<String>) {
    let uris: Vec<String> = browse_all(&s.lan(), s.ip(room), QUEUE)
        .unwrap_or_default()
        .iter()
        .filter_map(|o| {
            o.res
                .as_ref()
                .and_then(|r| spotify_uri_from_renderer_uri(&r.uri))
        })
        .collect();
    let works = uris
        .iter()
        .filter_map(|u| {
            u.strip_prefix("spotify:track:simhw")?
                .get(..1)?
                .parse()
                .ok()
        })
        .collect();
    (works, uris)
}

#[test]
fn two_rooms_djs_never_queue_the_same_work() {
    let mut s = Scenario::start("dj_house");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let clock = s.dir().join("clock");
    let pinned = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0)
        .unwrap_or_default()
        .to_rfc3339();
    std::fs::write(&clock, pinned).unwrap();
    let clock = clock.to_string_lossy().into_owned();

    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
            "--clock-file",
            &clock,
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

    for room in ["Kitchen", "Living Room"] {
        let (status, started) = post(&api, "/dj/start", &json!({ "zone": room }));
        s.check(
            "start",
            "http",
            &format!("POST /dj/start plays the DJ in the {room}"),
            status == 200
                && started["done"]
                    .as_str()
                    .is_some_and(|d| d.contains("the DJ is playing")),
            &started,
        );
    }
    let (kitchen, kitchen_queue) = queued_works(&s, "Kitchen");
    let (living, living_queue) = queued_works(&s, "Living Room");
    s.check(
        "queues",
        "sim",
        "each room queues DJ works, and no work is queued in both",
        !kitchen.is_empty() && !living.is_empty() && kitchen.is_disjoint(&living),
        format!("Kitchen {kitchen_queue:?}; Living Room {living_queue:?}"),
    );

    // A skip jumps to the Kitchen's next work; the top-up that follows, on
    // the playback event, picks a new one: none of the Living Room's works.
    let (status, skipped) = post(&api, "/dj/skip", &json!({ "zone": "Kitchen" }));
    let deadline = Instant::now() + Duration::from_secs(15);
    let (mut topped, mut kitchen_queue) = queued_works(&s, "Kitchen");
    while topped.is_subset(&kitchen) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        (topped, kitchen_queue) = queued_works(&s, "Kitchen");
    }
    let (living, living_queue) = queued_works(&s, "Living Room");
    s.check(
        "skip",
        "sim",
        "after a skip the Kitchen tops up with a new work, none of the Living Room's",
        status == 200 && !topped.is_subset(&kitchen) && topped.is_disjoint(&living),
        format!("{skipped}; Kitchen {kitchen_queue:?}; Living Room {living_queue:?}"),
    );

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
        "a synced library with eight classical works",
        seeded.as_ref().is_ok_and(|sync| sync.classical == 16),
        format!("{seeded:?}"),
    );
    // One all-day program, so the picks do not depend on the hour.
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
