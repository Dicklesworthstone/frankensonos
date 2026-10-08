//! The owner's feedback to the DJ, end to end against the virtual
//! households, with `fsonos serve` on a clock the test moves
//! (`--clock-file`) in step with the sim's own:
//!
//! * likes from the CLI (`fsonos dj like`) and an agent (MCP `dj_feedback`),
//!   and a dislike over HTTP (`POST /dj/feedback`, no room: the group the DJ
//!   plays in), each recorded about the work playing;
//! * an early skip: `POST /dj/skip` ten seconds into a work;
//! * `fsonos dj why` shows the feedback factor in the next pick (a skip plays
//!   the work already waiting, picked before the feedback; the one queued
//!   behind it weighs it);
//! * a full listen: that work heard to the end of its last movement.

mod e2e;

use chrono::{DateTime, FixedOffset, Local, TimeDelta, Timelike};
use e2e::{Daemon, Scenario, http};
use fsonos_core::store::{Feedback, SqliteStore, Store as _};
use fsonos_proto::control::get_position_info;
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ROOM: &str = "Living Room";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
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

/// Poll `probe` until it holds or `within` passes.
fn eventually(within: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Four works of two movements each; every identifier is made up.
fn library() -> Vec<LibraryItem> {
    let works = [
        ("Ludwig van Beethoven", "Symphony No. 7 in A Major, Op. 92"),
        ("Johannes Brahms", "Symphony No. 3 in F Major, Op. 90"),
        ("Joseph Haydn", "String Quartet in C Major, Op. 76 No. 3"),
        ("Franz Schubert", "Piano Sonata in B-flat Major, D. 960"),
    ];
    let mut items = Vec::new();
    for (w, (composer, work)) in works.iter().enumerate() {
        for (m, movement) in ["I. Allegro", "II. Andante"].iter().enumerate() {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:simfb{w}{m}000000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simfbalbum{w}00000000000")),
                album_artists: vec!["Sim Ensemble".into()],
                disc_number: Some(1),
                track_number: u32::try_from(m + 1).ok(),
                added_at: Some(1_790_000_000),
                genres: Vec::new(),
                label: None,
                duration_secs: Some(180),
                explicit: false,
                origin: Origin::SavedAlbum,
                ..LibraryItem::default()
            });
        }
    }
    items
}

fn db(s: &Scenario) -> PathBuf {
    s.dir().join("data").join("fsonos.db")
}

/// The library a Spotify sync leaves, and an all-day program, so the DJ's
/// picks do not depend on when this runs.
fn seed(s: &mut Scenario) {
    let seeded = SqliteStore::open(&db(s))
        .map_err(|e| e.to_string())
        .and_then(|mut store| {
            apply_library_read(&mut store, &library()).map_err(|e| e.to_string())
        });
    let moods =
        "[[programs]]\ndays = \"daily\"\nfrom = \"00:00\"\nto = \"23:59\"\nmood = \"bright\"\n";
    let wrote = std::fs::write(s.dir().join("data").join("moods.toml"), moods);
    s.check(
        "library",
        "store",
        "a synced library of four classical works and an all-day program",
        seeded.is_ok() && wrote.is_ok(),
        format!("{seeded:?} {wrote:?}"),
    );
}

/// Every feedback row in the store, oldest first.
fn feedback(s: &Scenario) -> Vec<Feedback> {
    SqliteStore::open(&db(s))
        .and_then(|store| store.feedback_between(0..i64::MAX))
        .unwrap_or_default()
}

fn signals(rows: &[Feedback]) -> Vec<i64> {
    rows.iter().map(|f| f.signal).collect()
}

/// In order: the rows' times come from different clocks (the CLI's and an
/// agent's real time, the daemon's test clock).
fn sorted(mut signals: Vec<i64>) -> Vec<i64> {
    signals.sort_unstable();
    signals
}

fn set_clock(path: &Path, at: DateTime<FixedOffset>) {
    std::fs::write(path, at.to_rfc3339()).expect("write the clock file");
}

/// The Living Room's queue position and the length of its track.
fn on_now(s: &Scenario) -> (u32, u32) {
    get_position_info(&s.lan(), s.ip(ROOM))
        .map_or((0, 180), |p| (p.track, p.duration_secs.unwrap_or(180)))
}

/// The Living Room's zone state as serve reports it.
fn zone_state(api: &str) -> String {
    http(api, "GET", "/zones/Living%20Room/state", &[], "")
        .map_or_else(|e| e.to_string(), |(_, _, b)| b)
}

/// The local time now, on the second, plus `secs`.
fn now_plus(secs: i64) -> DateTime<FixedOffset> {
    let now = Local::now().fixed_offset();
    now.with_nanosecond(0).unwrap_or(now) + TimeDelta::seconds(secs)
}

/// Serve on `clock` with the DJ started in the Living Room; its API address.
fn serve_with_the_dj(s: &mut Scenario, clock: &Path) -> (Daemon, String) {
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
    let (status, started) = post(&api, "/dj/start", &json!({ "zone": ROOM }));
    s.check(
        "start",
        "http",
        "serve is live and the DJ plays in the Living Room",
        live.is_some() && status == 200,
        &started,
    );
    (daemon, api)
}

#[test]
fn likes_dislikes_skips_and_full_listens_are_recorded_and_steer_the_dj() {
    let mut s = Scenario::start("dj_feedback");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let clock = s.dir().join("clock");
    let t0 = Local::now().fixed_offset();
    let t0 = t0.with_nanosecond(0).unwrap_or(t0);
    set_clock(&clock, t0);
    let (mut daemon, api) = serve_with_the_dj(&mut s, &clock);

    explicit_feedback(&mut s, &api);

    // The CLI and the agent stamp their likes with the real time: the
    // daemon's clock moves on past them before it picks again.
    let t1 = now_plus(1);
    set_clock(&clock, t1);
    let (status, _) = post(&api, "/dj/skip", &json!({ "zone": ROOM }));
    s.check(
        "skip",
        "http",
        "POST /dj/skip plays the work queued next",
        status == 200,
        status,
    );

    // Ten seconds into that work, the owner skips it: an early skip.
    let liked = feedback(&s).first().and_then(|r| r.work_key.clone());
    let t2 = t1 + TimeDelta::seconds(10);
    set_clock(&clock, t2);
    let (status, _) = post(&api, "/dj/skip", &json!({ "zone": ROOM }));
    let skipped_key = || {
        feedback(&s)
            .iter()
            .find(|r| r.signal == -1 && r.work_key != liked)
            .and_then(|r| r.work_key.clone())
    };
    let found = eventually(Duration::from_secs(10), || skipped_key().is_some());
    let skipped = skipped_key();
    s.check(
        "early-skip",
        "store",
        "a skip ten seconds into a work is an early skip (-1) of it",
        status == 200 && found,
        format!("{:?}", feedback(&s)),
    );

    // The work playing now was queued after the likes and the dislike: its
    // pick weighed them, and fsonos dj why says so.
    let run = s.cli("why", &["dj", "why", ROOM, "--daemon"]);
    s.check(
        "why",
        "cli",
        "fsonos dj why shows the feedback factor in a pick made after the feedback",
        run.code == Some(0) && run.stdout.contains("feedback"),
        format!("{run:?}"),
    );

    // That work is heard to the end: both its movements play out.
    let sim = s.sim_handle().expect("the sim");
    let (first, length) = on_now(&s);
    let t3 = t2 + TimeDelta::seconds(i64::from(length) + 2);
    set_clock(&clock, t3);
    sim.advance(Duration::from_secs(u64::from(length) + 2));
    let moved = eventually(Duration::from_secs(10), || on_now(&s).0 == first + 1);
    let (second, length2) = on_now(&s);
    let state = zone_state(&api);
    set_clock(&clock, t3 + TimeDelta::seconds(i64::from(length2) + 2));
    sim.advance(Duration::from_secs(u64::from(length2) + 2));
    let full = || {
        feedback(&s)
            .iter()
            .any(|r| r.signal == 1 && r.work_key != liked && r.work_key != skipped)
    };
    let heard = eventually(Duration::from_secs(10), full);
    s.check(
        "full-listen",
        "store",
        "a work heard to the end of its last movement is a full listen (+1) of that work",
        moved && heard,
        format!(
            "moved {moved} ({first}/{length}s -> {second}/{length2}s, then {:?}); zone then {state}, now {}; rows {:?}",
            on_now(&s),
            zone_state(&api),
            feedback(&s)
        ),
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

/// A like from the CLI and one from an agent, then a dislike over HTTP
/// without a room: three rows about the work playing.
fn explicit_feedback(s: &mut Scenario, api: &str) {
    let run = s.cli("cli-like", &["dj", "like", ROOM]);
    s.check(
        "cli-like",
        "cli",
        "fsonos dj like records a like of the work playing",
        run.code == Some(0) && run.stdout.starts_with("noted that you like "),
        format!("{run:?}"),
    );
    let run = s.cli("cli-like-nowhere", &["dj", "like"]);
    s.check(
        "cli-like-nowhere",
        "cli",
        "without a room, the CLI on its own (no DJ running in it) asks for one",
        run.code == Some(2) && run.stderr.contains("name the room"),
        format!("{run:?}"),
    );

    let mut mcp = s.mcp();
    mcp.initialize();
    let liked = mcp.request(
        "tools/call",
        &json!({ "name": "dj_feedback", "arguments": { "zone": ROOM, "signal": "like" } }),
    );
    let text = liked["result"]["content"][0]["text"].as_str().unwrap_or("");
    s.check(
        "mcp-like",
        "mcp",
        "dj_feedback records an agent's like",
        text.starts_with("noted that you like "),
        &liked,
    );
    drop(mcp);

    let (status, disliked) = post(api, "/dj/feedback", &json!({ "signal": "dislike" }));
    s.check(
        "http-dislike",
        "http",
        "POST /dj/feedback without a room is about the group the DJ plays in",
        status == 200 && disliked["signal"] == "dislike" && disliked["zone"] == ROOM,
        &disliked,
    );
    let rows = feedback(s);
    s.check(
        "recorded",
        "store",
        "three rows about one work, its composer and its performer: +3, +3, -3",
        sorted(signals(&rows)) == [-3, 3, 3]
            && rows.iter().all(|r| r.work_key == rows[0].work_key)
            && rows
                .iter()
                .all(|r| r.performer.as_deref() == Some("sim ensemble")),
        format!("{rows:?}"),
    );
}
