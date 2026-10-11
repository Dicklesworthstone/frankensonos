//! The DJ outlives a daemon restart, through `fsonos serve` against the
//! virtual households: the DJ starts in the Living Room, serve stops and
//! starts again on the same data directory, and the new serve still has
//! the DJ there. As the music plays on into the last work queued, it tops
//! the queue up with whole works, as the first serve would have.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control::get_position_info;
use fsonos_sim::{GenaEvent, SimHousehold};
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const ROOM: &str = "Living Room";

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
                source_uri: format!("spotify:track:simrs{w}{m}00000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simrsalb{w}00000000000")),
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

/// Whether `ready` comes to hold within 10 s.
fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    ready()
}

/// The Living Room's queue, as track URIs.
fn queue(s: &Scenario) -> Vec<String> {
    browse_all(&s.lan(), s.ip(ROOM), QUEUE)
        .map(|items| {
            items
                .into_iter()
                .filter_map(|o| o.res.map(|r| r.uri))
                .collect()
        })
        .unwrap_or_default()
}

/// (work, movement) of one of the library's tracks, by its queue URI.
fn movement(uri: &str) -> Option<(char, char)> {
    let (_, rest) = uri.split_once("simrs")?;
    let mut digits = rest.chars();
    Some((digits.next()?, digits.next()?))
}

/// The Living Room's queue position and its track's length in seconds.
fn on_now(s: &Scenario) -> (u32, u32) {
    get_position_info(&s.lan(), s.ip(ROOM))
        .map_or((0, 300), |p| (p.track, p.duration_secs.unwrap_or(300)))
}

/// How many AVTransport subscriptions the Living Room has taken.
fn subscriptions(s: &Scenario) -> usize {
    let Some(sim) = s.sim_handle() else {
        return 0;
    };
    let uuid = sim.player(ROOM).map(|p| p.uuid.clone()).unwrap_or_default();
    sim.gena_log()
        .iter()
        .filter(|e| {
            e.player == uuid
                && e.service == "AVTransport"
                && matches!(e.event, GenaEvent::Subscribed { .. })
        })
        .count()
}

/// `fsonos serve` up with its live model, and the API's `host:port`.
fn serve(s: &mut Scenario, step: &str) -> (Daemon, String) {
    let mut daemon = s.spawn(step, &["serve", "--http", "127.0.0.1:0"]);
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        step,
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

fn stop(s: &mut Scenario, step: &str, daemon: &mut Daemon) {
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        step,
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
}

#[test]
fn the_dj_feeds_on_after_serve_restarts() {
    let mut s = Scenario::start("dj-restart");
    s.sim(SimHousehold::standard());
    let data = s.dir().join("data");
    let seeded = SqliteStore::open(&data.join("fsonos.db"))
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

    let (mut first, api) = serve(&mut s, "serve");
    let zone = json!({ "zone": ROOM });
    let (started, start) = call(&api, "POST", "/dj/start", Some(&zone));
    let queued = queue(&s);
    s.check(
        "start",
        "http",
        "the DJ plays in the Living Room: two works of two movements queued",
        started == 200 && queued.len() == 4,
        format!("{start}; queue {queued:?}"),
    );
    let before = subscriptions(&s);
    stop(&mut s, "stop", &mut first);
    let saved = std::fs::read_to_string(data.join("dj-feeds.json")).unwrap_or_default();

    let (mut second, api) = serve(&mut s, "serve-again");
    let (_, status) = call(&api, "GET", "/zones/Living%20Room/dj", None);
    s.check(
        "restarted",
        "http",
        "the restarted serve has the DJ in the Living Room",
        status["running"] == true,
        format!("{status}; saved {saved}"),
    );

    // The new serve hears the Living Room's events before the music moves.
    let listening = eventually(|| subscriptions(&s) > before);
    let sim = s.sim_handle().expect("the sim");
    let (track, length) = on_now(&s);
    sim.advance(Duration::from_secs(u64::from(length) + 2));
    let (track2, length2) = on_now(&s);
    sim.advance(Duration::from_secs(u64::from(length2) + 2));
    let into_last = on_now(&s).0;
    let topped = eventually(|| queue(&s).len() == 6);
    let after = queue(&s);
    // Both movements of one work, and not a work already queued.
    let works: Vec<_> = after.iter().map(|u| movement(u)).collect();
    let whole = matches!(
        works.get(4..6),
        Some([Some((w, '0')), Some((v, '1'))]) if w == v
            && works[..4].iter().all(|m| m.is_some_and(|(earlier, _)| earlier != *w))
    );
    s.check(
        "top-up",
        "sim",
        "into the last work queued, the restarted DJ queues another whole work",
        listening && track == 1 && track2 == 2 && into_last == 3 && topped && whole,
        format!(
            "listening {listening}; tracks {track} -> {track2} -> {into_last}; queue {after:?}"
        ),
    );

    stop(&mut s, "stop-again", &mut second);
    s.finish();
}
