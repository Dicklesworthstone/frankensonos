//! The owner's feedback to the DJ, end to end against the virtual
//! households, with `fsonos serve` on a clock the test moves
//! (`--clock-file`) in step with the sim's own:
//!
//! * likes from the CLI (`fsonos dj like`, which asks the daemon: with a
//!   room, and without one, the group its DJ plays in) and an agent (MCP
//!   `dj_feedback`), and a dislike over HTTP (`POST /dj/feedback`, no room),
//!   each recorded about the work playing;
//! * an early skip: `POST /dj/skip` ten seconds into a work;
//! * `fsonos dj why` shows the feedback factor in the next pick (a skip plays
//!   the work already waiting, picked before the feedback; the one queued
//!   behind it weighs it);
//! * a full listen: that work heard to the end of its last movement;
//! * a like from the CLI reaches the daemon's DJ, not one of its own: the
//!   pick it makes as the work playing ends weighs it.

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
        "[[programs]]\ndays = \"daily\"\nfrom = \"00:00\"\nto = \"24:00\"\nmood = \"bright\"\n";
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

/// In order: the rows' times come from different clocks (an agent's real
/// time, the daemon's test clock).
fn sorted(mut signals: Vec<i64>) -> Vec<i64> {
    signals.sort_unstable();
    signals
}

fn set_clock(path: &Path, at: DateTime<FixedOffset>) {
    // Whole or not at all: the daemon reads the file on every event.
    let next = path.with_extension("next");
    std::fs::write(&next, at.to_rfc3339()).expect("write the clock file");
    std::fs::rename(&next, path).expect("replace the clock file");
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

    // The agent (its own process) stamps its like with the real time: the
    // daemon's clock moves on past it before it picks again.
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
    let mark = last_event(&api);
    let t3 = t2 + TimeDelta::seconds(i64::from(length) + 2);
    set_clock(&clock, t3);
    sim.advance(Duration::from_secs(u64::from(length) + 2));
    let moved = eventually(Duration::from_secs(10), || on_now(&s).0 == first + 1);
    // The DJ takes in the second movement before the daemon's time moves on,
    // so the time that follows is heard as the second's, not the first's.
    let taken = dj_reached(&api, mark, 2);
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
    // Heard by the daemon's clock, as it played: never an early skip on the way.
    let false_skip = feedback(&s)
        .iter()
        .any(|r| r.signal == -1 && r.work_key != liked && r.work_key != skipped);
    s.check(
        "full-listen",
        "store",
        "a work heard to the end of its last movement is a full listen (+1) of that work, not an early skip",
        moved && taken && heard && !false_skip,
        format!(
            "moved {moved} ({first}/{length}s -> {second}/{length2}s, then {:?}), DJ took it in {taken}; zone then {state}, now {}; rows {:?}",
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

/// Two likes from the CLI (one without a room) and one from an agent, then
/// a dislike over HTTP without a room: four rows about the work playing.
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
        "without a room, the CLI's like is about the group the daemon's DJ plays in",
        run.code == Some(0) && run.stdout.starts_with("noted that you like "),
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
        "four rows about one work, its composer and its performer: +3, +3, +3, -3",
        sorted(signals(&rows)) == [-3, 3, 3, 3]
            && rows.iter().all(|r| r.work_key == rows[0].work_key)
            && rows
                .iter()
                .all(|r| r.performer.as_deref() == Some("sim ensemble")),
        format!("{rows:?}"),
    );
}

/// A like from the CLI with serve's DJ playing is the daemon's to weigh, not
/// a DJ's of the CLI's own: the pick the daemon's DJ makes as the work
/// playing ends (before that full listen reloads its feedback from the
/// store) weighs it.
#[test]
fn a_like_from_the_cli_moves_the_daemons_next_pick() {
    let mut s = Scenario::start("dj_feedback_cli");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let clock = s.dir().join("clock");
    let t0 = now_plus(0);
    set_clock(&clock, t0);
    let (mut daemon, api) = serve_with_the_dj(&mut s, &clock);
    let before = dj_status(&api);
    s.check(
        "unweighed",
        "http",
        "the works the DJ queued as it started weigh no feedback",
        !picks(&before).is_empty() && !picks(&before).iter().any(|w| weighs_feedback(w)),
        &before,
    );

    let run = s.cli("cli-like", &["dj", "like", ROOM]);
    s.check(
        "cli-like",
        "cli",
        "fsonos dj like records a like of the work playing",
        run.code == Some(0) && run.stdout.starts_with("noted that you like "),
        format!("{run:?}"),
    );

    // The work playing is heard to its end: the DJ queues one more.
    let sim = s.sim_handle().expect("the sim");
    let mut at = t0;
    for _ in 0..2 {
        let (track, length) = on_now(&s);
        at += TimeDelta::seconds(i64::from(length) + 2);
        set_clock(&clock, at);
        sim.advance(Duration::from_secs(u64::from(length) + 2));
        eventually(Duration::from_secs(10), || on_now(&s).0 == track + 1);
    }
    let mut after = Value::Null;
    let weighed = eventually(Duration::from_secs(10), || {
        after = dj_status(&api);
        picks(&after).iter().any(|w| weighs_feedback(w))
    });
    s.check(
        "next-pick",
        "http",
        "the daemon's DJ's next pick, made as that work ended, weighs the CLI's like",
        weighed,
        format!("{after}; rows {:?}", feedback(&s)),
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

/// The Living Room's DJ as serve reports it.
/// The newest event's id on the daemon's event bus.
fn last_event(api: &str) -> u64 {
    http(api, "GET", "/events?wait=0", &[], "")
        .ok()
        .and_then(|(_, _, body)| serde_json::from_str::<Value>(&body).ok())
        .and_then(|events| events["last_id"].as_u64())
        .unwrap_or(0)
}

/// Whether, after event `since`, the DJ said (`dj.status`, which it
/// publishes once it has handled a playback change) that movement
/// `movement` of its work plays.
fn dj_reached(api: &str, since: u64, movement: u64) -> bool {
    let mut since = since;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Some(events) = http(
            api,
            "GET",
            &format!("/events?since={since}&wait=2"),
            &[],
            "",
        )
        .ok()
        .and_then(|(_, _, body)| serde_json::from_str::<Value>(&body).ok()) else {
            continue;
        };
        if events["events"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["event"] == "dj.status" && e["data"]["now"]["movement"] == movement)
        {
            return true;
        }
        since = events["last_id"].as_u64().unwrap_or(since);
    }
    false
}

fn dj_status(api: &str) -> Value {
    http(api, "GET", "/zones/Living%20Room/dj", &[], "")
        .ok()
        .and_then(|(_, _, b)| serde_json::from_str(&b).ok())
        .unwrap_or(Value::Null)
}

/// The DJ's work playing and the works it queued.
fn picks(status: &Value) -> Vec<&Value> {
    std::iter::once(&status["now"])
        .chain(status["next"].as_array().into_iter().flatten())
        .filter(|w| w.is_object())
        .collect()
}

/// Whether the owner's feedback moved a pick's odds.
fn weighs_feedback(work: &Value) -> bool {
    work["reason"]["factors"]
        .as_array()
        .is_some_and(|f| f.iter().any(|f| f["factor"] == "feedback"))
}
