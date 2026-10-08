//! Sleep timers and schedules end to end, against the virtual households:
//! `fsonos serve` with a clock the test moves (`--clock-file`) and a short
//! fade (`--sleep-fade`).
//!
//! * A sleep timer set through serve (`POST /sleep`) fades the group out,
//!   pauses it and puts its volume back when the daemon's time reaches it;
//!   the speaker's own timer backs it up five minutes later until then.
//!   On its own, the CLI sets only the speaker's own timer (`fsonos sleep`),
//!   and extends and cancels it.
//! * A daily DJ start in a mood, added with `fsonos schedule add` (in the
//!   store, while serve runs), fires once at its time, as the CLI that added
//!   it, not again that day, and again the next day; it pauses, resumes and
//!   is removed.

mod e2e;

use chrono::{DateTime, FixedOffset, Local, NaiveTime, TimeDelta, TimeZone, Timelike};
use e2e::{Daemon, Scenario, http};
use fsonos_core::store::SqliteStore;
use fsonos_proto::control::{
    get_position_info, get_remaining_sleep_timer, get_transport_info, get_volume,
};
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ROOM: &str = "Living Room";
const RADIO: &str = "x-rincon-mp3radio://stream.example.invalid/evening.mp3";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// `METHOD path` with an optional JSON body: (status, body).
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

/// Set the daemon's time.
fn set_clock(path: &Path, at: DateTime<FixedOffset>) {
    std::fs::write(path, at.to_rfc3339()).expect("write the clock file");
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

/// Whether `probe` holds throughout `for_how_long`.
fn throughout(for_how_long: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + for_how_long;
    while Instant::now() < deadline {
        if !probe() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    probe()
}

fn transport(s: &Scenario) -> Option<TransportState> {
    get_transport_info(&s.lan(), s.ip(ROOM))
        .map(|t| t.state)
        .ok()
}

fn speaker_timer(s: &Scenario) -> Option<u32> {
    get_remaining_sleep_timer(&s.lan(), s.ip(ROOM))
        .ok()
        .flatten()
}

/// Start serve on the scenario's clock file, with a two-second fade; its
/// API address once it is live.
fn serve(s: &mut Scenario, clock: &Path) -> (Daemon, String) {
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
            "--sleep-fade",
            "2s",
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
        "serve is up with its live model",
        !api.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

fn now() -> DateTime<FixedOffset> {
    let t = Local::now().fixed_offset();
    t.with_nanosecond(0).unwrap_or(t)
}

fn clock_file(s: &Scenario) -> PathBuf {
    s.dir().join("clock")
}

#[test]
fn a_sleep_timer_fades_out_in_serve_and_is_the_speakers_own_from_the_cli() {
    let mut s = Scenario::start("sleep");
    s.sim(SimHousehold::standard());
    let clock = clock_file(&s);
    let t0 = now();
    set_clock(&clock, t0);
    let (mut daemon, api) = serve(&mut s, &clock);

    let (play, _) = call(
        &api,
        "POST",
        "/play",
        Some(&json!({ "zone": ROOM, "source_uri": RADIO })),
    );
    let (volume, _) = call(
        &api,
        "POST",
        "/volume",
        Some(&json!({ "zone": ROOM, "volume": 30 })),
    );
    s.check(
        "play",
        "http",
        "the Living Room plays a station at volume 30",
        play == 200
            && volume == 200
            && transport(&s) == Some(TransportState::Playing)
            && get_volume(&s.lan(), s.ip(ROOM)).ok() == Some(30),
        format!("play {play}, volume {volume}, {:?}", transport(&s)),
    );

    let (status, set) = call(
        &api,
        "POST",
        "/sleep",
        Some(&json!({ "zone": ROOM, "duration": "10m" })),
    );
    s.check(
        "sleep",
        "http",
        "POST /sleep arms a fading timer for ten minutes",
        status == 200 && set["fades"] == true && set["remaining_secs"] == 600,
        &set,
    );
    let backstop = speaker_timer(&s);
    s.check(
        "backstop",
        "sim",
        "the speaker's own timer is set five minutes later, in case serve stops",
        backstop.is_some_and(|left| (890..=900).contains(&left)),
        format!("{backstop:?}"),
    );
    let (_, listed) = call(&api, "GET", "/sleep", None);
    s.check(
        "list",
        "http",
        "GET /sleep lists the timer",
        listed.as_array().is_some_and(|a| a.len() == 1) && listed[0]["fades"] == true,
        &listed,
    );

    // Ten minutes on, the daemon fades the group out and pauses it.
    set_clock(&clock, t0 + TimeDelta::minutes(10));
    let paused = eventually(Duration::from_secs(15), || {
        transport(&s) == Some(TransportState::Paused)
    });
    s.check(
        "fade",
        "sim",
        "at its time the group fades out and pauses",
        paused,
        format!("{:?}", transport(&s)),
    );
    let level = get_volume(&s.lan(), s.ip(ROOM)).ok();
    let left = speaker_timer(&s);
    s.check(
        "after",
        "sim",
        "its volume is put back and the speaker's own timer cleared",
        level == Some(30) && left.is_none(),
        format!("volume {level:?}, speaker timer {left:?}"),
    );
    let (_, listed) = call(&api, "GET", "/sleep", None);
    s.check(
        "gone",
        "http",
        "no timer is listed any more",
        listed.as_array().is_some_and(Vec::is_empty),
        &listed,
    );

    check_cli_sleep(&mut s);

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

/// Two works of two movements; every identifier is made up.
fn library() -> Vec<LibraryItem> {
    let works = [
        ("Joseph Haydn", "Symphony No. 94 in G Major"),
        ("Franz Schubert", "Piano Trio No. 2 in E-flat Major"),
    ];
    let mut items = Vec::new();
    for (w, (composer, work)) in works.iter().enumerate() {
        for (m, movement) in ["I. Allegro", "II. Andante"].iter().enumerate() {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:simsched{w}{m}0000000000000"),
                title: format!("{work}: {movement}"),
                artists: vec![(*composer).into(), "Sim Ensemble".into()],
                album: Some(format!("{composer}: Works")),
                album_uri: Some(format!("spotify:album:simschedalbum{w}000000000")),
                album_artists: vec!["Sim Ensemble".into()],
                disc_number: Some(1),
                track_number: u32::try_from(m + 1).ok(),
                added_at: Some(1_790_000_000),
                genres: Vec::new(),
                label: None,
                duration_secs: Some(300),
                explicit: false,
                origin: Origin::SavedAlbum,
            });
        }
    }
    items
}

/// The library a Spotify sync leaves, and an all-day program, so the DJ's
/// pick does not depend on when this runs.
fn seed(s: &mut Scenario) {
    let data = s.dir().join("data");
    let seeded = SqliteStore::open(&data.join("fsonos.db"))
        .map_err(|e| e.to_string())
        .and_then(|mut store| {
            apply_library_read(&mut store, &library()).map_err(|e| e.to_string())
        });
    let moods =
        "[[programs]]\ndays = \"daily\"\nfrom = \"00:00\"\nto = \"23:59\"\nmood = \"bright\"\n";
    let wrote = std::fs::write(data.join("moods.toml"), moods);
    s.check(
        "library",
        "store",
        "a synced library of classical works and an all-day program",
        seeded.is_ok() && wrote.is_ok(),
        format!("{seeded:?} {wrote:?}"),
    );
}

/// The DJ's track in the Living Room, if it plays one.
fn dj_playing(s: &Scenario) -> bool {
    let uri = get_position_info(&s.lan(), s.ip(ROOM))
        .map(|p| p.uri)
        .unwrap_or_default();
    transport(s) == Some(TransportState::Playing) && uri.contains("simsched")
}

/// The CLI's logged DJ starts.
fn cli_dj_starts(api: &str) -> usize {
    let (_, actions) = call(api, "GET", "/actions?client=cli&limit=50", None);
    actions.as_array().map_or(0, |a| {
        a.iter()
            .filter(|x| {
                x["intent"]
                    .as_str()
                    .is_some_and(|i| i.starts_with("dj_start:"))
            })
            .count()
    })
}

/// `at` on the day after.
fn next_day(at: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
    let date = at.with_timezone(&Local).date_naive() + TimeDelta::days(1);
    let wall = date.and_time(NaiveTime::from_hms_opt(at.hour(), at.minute(), 0).unwrap());
    Local
        .from_local_datetime(&wall)
        .earliest()
        .map_or(at + TimeDelta::days(1), |t| t.fixed_offset())
}

#[test]
fn a_daily_dj_start_fires_once_a_day_as_the_client_that_added_it() {
    let mut s = Scenario::start("schedules");
    s.sim(SimHousehold::standard());
    seed(&mut s);
    let clock = clock_file(&s);
    let t0 = now();
    set_clock(&clock, t0);
    let (mut daemon, api) = serve(&mut s, &clock);

    // Two hours from now, on the minute.
    let first = (t0 + TimeDelta::hours(2)).with_second(0).unwrap();
    let when = format!("daily {:02}:{:02}", first.hour(), first.minute());
    let run = s.cli(
        "add",
        &[
            "schedule", "add", &when, "dj", "start", ROOM, "--mood", "bright",
        ],
    );
    s.check(
        "add",
        "cli",
        "fsonos schedule add stores a daily DJ start in a mood",
        run.code == Some(0)
            && run.stdout.starts_with(&format!(
                "added #1 {when}: start the DJ in {ROOM} (mood bright) (next "
            )),
        format!("{run:?}"),
    );

    let mut mcp = s.mcp();
    mcp.initialize();
    let listed = mcp.request(
        "tools/call",
        &json!({ "name": "list_schedules", "arguments": {} }),
    );
    let text = listed["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    s.check(
        "mcp-list",
        "mcp",
        "list_schedules shows it to an agent",
        text.contains(&format!("#1 {when}: start the DJ in {ROOM}")),
        &listed,
    );
    drop(mcp);

    set_clock(&clock, first + TimeDelta::seconds(5));
    let started = eventually(Duration::from_secs(15), || dj_playing(&s));
    s.check(
        "fire",
        "sim",
        "at its time serve starts the DJ in the Living Room",
        started,
        format!("{:?}", transport(&s)),
    );
    // Logged once the start returns, a moment after the music does.
    let logged = eventually(Duration::from_secs(5), || cli_dj_starts(&api) == 1);
    s.check(
        "logged",
        "http",
        "the run is logged as the CLI's, which added it",
        logged,
        format!("{} dj_start actions by cli", cli_dj_starts(&api)),
    );

    let (status, _) = call(&api, "POST", "/dj/stop", Some(&json!({ "zone": ROOM })));
    let stays = throughout(Duration::from_secs(3), || {
        set_clock(&clock, first + TimeDelta::seconds(40));
        transport(&s) == Some(TransportState::Stopped)
    });
    s.check(
        "once",
        "sim",
        "stopped, it does not start again that day",
        status == 200 && stays && cli_dj_starts(&api) == 1,
        format!("{status}; {:?}", transport(&s)),
    );

    let second = next_day(first);
    set_clock(&clock, second + TimeDelta::seconds(5));
    let again = eventually(Duration::from_secs(15), || {
        dj_playing(&s) && cli_dj_starts(&api) == 2
    });
    s.check(
        "next-day",
        "sim",
        "the next day it starts the DJ again",
        again,
        format!("{:?}; {} starts", transport(&s), cli_dj_starts(&api)),
    );

    check_schedule_edits(&mut s, &api);

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

/// On its own the CLI sets the speaker's own timer: set, shown, extended,
/// cancelled, and with none left, nothing to extend.
fn check_cli_sleep(s: &mut Scenario) {
    // On its own the CLI sets the speaker's own timer.
    let run = s.cli("cli-sleep", &["sleep", ROOM, "30m"]);
    s.check(
        "cli-sleep",
        "cli",
        "fsonos sleep sets the speaker's own timer, without a fade",
        run.code == Some(0)
            && run.stdout.contains("by the speaker's own timer")
            && speaker_timer(s) == Some(1800),
        format!("{run:?}; {:?}", speaker_timer(s)),
    );
    let run = s.cli("cli-show", &["sleep", ROOM]);
    s.check(
        "cli-show",
        "cli",
        "fsonos sleep <room> shows the timer",
        run.code == Some(0) && run.stdout.contains("pauses in 30m"),
        format!("{run:?}"),
    );
    let run = s.cli("cli-extend", &["sleep", ROOM, "--extend", "15m"]);
    s.check(
        "cli-extend",
        "cli",
        "--extend pushes it 15 minutes later",
        run.code == Some(0) && speaker_timer(s) == Some(2700),
        format!("{run:?}; {:?}", speaker_timer(s)),
    );
    let run = s.cli("cli-cancel", &["sleep", ROOM, "--cancel"]);
    s.check(
        "cli-cancel",
        "cli",
        "--cancel clears it",
        run.code == Some(0) && speaker_timer(s).is_none(),
        format!("{run:?}; {:?}", speaker_timer(s)),
    );
    let run = s.cli("cli-extend-none", &["sleep", ROOM, "--extend", "5m"]);
    s.check(
        "cli-extend-none",
        "cli",
        "with no timer, --extend is a usage error",
        run.code == Some(2) && run.stderr.contains("INVALID_ARGUMENT"),
        format!("{run:?}"),
    );
}

/// Pause, resume, list over HTTP, and remove schedule #1.
fn check_schedule_edits(s: &mut Scenario, api: &str) {
    let run = s.cli("pause", &["schedule", "pause", "1"]);
    s.check(
        "pause",
        "cli",
        "fsonos schedule pause pauses it",
        run.code == Some(0) && run.stdout.contains("(paused)"),
        format!("{run:?}"),
    );
    let run = s.cli("resume", &["schedule", "resume", "1"]);
    s.check(
        "resume",
        "cli",
        "fsonos schedule resume resumes it",
        run.code == Some(0) && run.stdout.contains("(next "),
        format!("{run:?}"),
    );
    let (_, schedules) = call(api, "GET", "/schedules", None);
    s.check(
        "http-list",
        "http",
        "GET /schedules shows it enabled, added by cli, with its last run",
        schedules[0]["enabled"] == true
            && schedules[0]["creator"] == "cli"
            && schedules[0]["action"] == "dj_start"
            && schedules[0]["last_fired"].is_string(),
        &schedules,
    );
    let run = s.cli("rm", &["schedule", "rm", "1"]);
    let gone = s.cli("rm-again", &["schedule", "rm", "1"]);
    s.check(
        "rm",
        "cli",
        "fsonos schedule rm removes it; a second rm is UNKNOWN_SCHEDULE (exit 3)",
        run.code == Some(0) && gone.code == Some(3) && gone.stderr.contains("UNKNOWN_SCHEDULE"),
        format!("{run:?} {gone:?}"),
    );
}
