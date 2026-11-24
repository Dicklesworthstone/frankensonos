//! A signed-in daemon refreshes the DJ's library from Spotify (scenario
//! `dj_sync_fake`), against fsonos-spotify's fake Spotify on loopback (a
//! debug build's `FSONOS_SPOTIFY_TEST_BASE`) and the virtual households.
//! The cached grant has lapsed, so the daemon's own refresh at start first
//! renews it; `GET /dj/sync` then reports what it read, `POST /dj/start`
//! plays one of those tracks in the sim, and a track the owner likes
//! afterwards reaches the library on the next `POST /dj/sync`. A day later
//! on the daemon's clock (`--clock-file`), it refreshes on its own.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_proto::control::get_position_info;
use fsonos_sim::SimHousehold;
use fsonos_spotify::client::{CachedToken, TEST_BASE_ENV, TokenCache, requested_scope};
use fsonos_spotify::fake_spotify::{CLIENT_ID, FakeSpotify, REDIRECT, query_param};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// `GET /dj/sync` until `done` holds for it, for up to `wait`.
fn sync_until(api: &str, wait: Duration, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + wait;
    loop {
        let state = http(api, "GET", "/dj/sync", &[], "")
            .map_or(Value::Null, |(_, _, body)| json_of(&body));
        if done(&state) || Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
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
    .map_or((0, Value::Null), |(code, _, b)| (code, json_of(&b)))
}

/// The liked-tracks page the fake serves, with one more made-up track.
fn liked_with_one_more() -> String {
    let mut page: Value = serde_json::from_str(include_str!(
        "../../fsonos-spotify/tests/fixtures/saved_tracks_page.json"
    ))
    .unwrap();
    let mut added = page["items"][0].clone();
    added["added_at"] = "2026-10-15T08:00:00Z".into();
    added["track"]["id"] = "FakeTrack0000000000099".into();
    added["track"]["uri"] = "spotify:track:FakeTrack0000000000099".into();
    added["track"]["name"] = "Arabesque No. 1".into();
    added["track"]["track_number"] = 99.into();
    let items = page["items"].as_array_mut().unwrap();
    items.push(added);
    let total = items.len();
    page["total"] = total.into();
    page.to_string()
}

/// A grant that lapsed a minute ago, cached in the scenario's data
/// directory: the daemon must renew it before reading.
fn cache_a_lapsed_grant(s: &Scenario) {
    let data = s.dir().join("data");
    std::fs::create_dir_all(&data).unwrap();
    TokenCache::in_data_dir(&data)
        .store(&CachedToken {
            access_token: "lapsed".into(),
            refresh_token: "refresh-e2e".into(),
            expires_at: chrono::Utc::now().timestamp() - 60,
            scope: requested_scope(),
        })
        .unwrap();
}

/// `fsonos serve` with the Spotify app and the fake at `base`, on the
/// clock in `clock`, live: the daemon and its API.
fn serve(s: &mut Scenario, base: &str, clock: &Path) -> (Daemon, String) {
    serve_with_app(s, base, clock, Some(CLIENT_ID))
}

fn serve_with_app(
    s: &mut Scenario,
    base: &str,
    clock: &Path,
    client_id: Option<&str>,
) -> (Daemon, String) {
    let clock = clock.to_string_lossy().into_owned();
    let mut args = vec![
        "serve",
        "--http",
        "127.0.0.1:0",
        "--mcp-http",
        "127.0.0.1:0",
        "--clock-file",
        &clock,
    ];
    if let Some(client_id) = client_id {
        args.extend(["--spotify-client-id", client_id]);
    }
    let mut daemon = s.spawn_env("serve", &args, &[(TEST_BASE_ENV, base)]);
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
    (daemon, api)
}

/// `POST /dj/start` plays a track the refresh read; then the DJ stops.
fn the_dj_plays_what_was_read(s: &mut Scenario, api: &str) {
    let zone = json!({ "zone": "Living Room" });
    let started = post(api, "/dj/start", &zone);
    let on_now = get_position_info(&s.lan(), s.ip("Living Room")).map_or_default(|p| p.uri);
    s.check(
        "dj start",
        "sim",
        "the DJ plays a track the refresh read from Spotify",
        started.0 == 200 && on_now.contains("FakeTrack"),
        format!("{started:?} on {on_now}"),
    );
    let _ = post(api, "/dj/stop", &zone);
}

/// The owner likes another track; `POST /dj/sync` reads it.
fn a_new_like_reaches_the_library(s: &mut Scenario, api: &str, fake: &FakeSpotify, tracks: u64) {
    fake.state.lock().unwrap().liked_tracks = Some(liked_with_one_more());
    let (asked, _) = post(api, "/dj/sync", &json!({}));
    let second = sync_until(api, Duration::from_secs(30), |v| {
        v["running"] == false && v["last"]["tracks"].as_u64() == Some(tracks + 1)
    });
    s.check(
        "second sync",
        "http",
        "POST /dj/sync reads the newly liked track into the library",
        (200..300).contains(&asked)
            && second["last"]["tracks"].as_u64() == Some(tracks + 1)
            && second.get("error").is_none(),
        format!("POST {asked}; {second}"),
    );
}

/// A day and an hour later on the daemon's clock, the daemon refreshes the
/// library on its own (no request), stamped with that time.
fn a_day_later_the_daemon_refreshes_on_its_own(s: &mut Scenario, api: &str, clock: &Path, t0: i64) {
    let later = t0 + 25 * 3_600;
    let at = chrono::DateTime::from_timestamp(later, 0).unwrap_or_default();
    std::fs::write(clock, at.to_rfc3339()).unwrap();
    let daily = sync_until(api, Duration::from_secs(30), |v| {
        v["running"] == false && v["last"]["at"].as_i64() == Some(later)
    });
    s.check(
        "daily",
        "http",
        "a day later on the daemon's clock, it refreshes the library on its own",
        daily["last"]["at"].as_i64() == Some(later) && daily.get("error").is_none(),
        format!("expected at={later}: {daily}"),
    );
}

#[test]
fn a_signed_in_daemon_reads_spotify_and_the_dj_plays_what_it_read() {
    let fake = FakeSpotify::start();
    {
        let mut state = fake.state.lock().unwrap();
        "not-yet-issued".clone_into(&mut state.access);
        "refresh-e2e".clone_into(&mut state.refresh);
    }
    let mut s = Scenario::start("dj_sync_fake");
    s.sim(SimHousehold::standard());
    cache_a_lapsed_grant(&s);
    let clock = s.dir().join("clock");
    let t0 = chrono::Utc::now().timestamp();
    let start = chrono::DateTime::from_timestamp(t0, 0).unwrap_or_default();
    std::fs::write(&clock, start.to_rfc3339()).unwrap();
    let (mut daemon, api) = serve(&mut s, &format!("http://{}", fake.addr), &clock);

    // Never refreshed: the daemon refreshes on its own as it starts.
    let first = sync_until(&api, Duration::from_secs(30), |v| {
        v["running"] == false && v.get("last").is_some()
    });
    let tracks = first["last"]["tracks"].as_u64().unwrap_or(0);
    s.check(
        "first sync",
        "http",
        "the daemon's own refresh read the library: tracks and DJ candidates, no error",
        tracks >= 4
            && first["last"]["candidates"].as_u64().unwrap_or(0) >= 1
            && first.get("error").is_none(),
        &first,
    );
    let renewed = fake.state.lock().unwrap().refreshes;
    s.check(
        "renewed",
        "fake",
        "the lapsed grant was renewed with its refresh token before reading",
        renewed >= 1,
        format!("refreshes={renewed}"),
    );

    the_dj_plays_what_was_read(&mut s, &api);
    a_new_like_reaches_the_library(&mut s, &api, &fake, tracks);
    a_day_later_the_daemon_refreshes_on_its_own(&mut s, &api, &clock, t0);

    let log = fake.state.lock().unwrap().log.join("\n");
    s.check(
        "read only",
        "fake",
        "the daemon only read Spotify: token renewals and GETs, nothing else",
        log.lines()
            .all(|l| l.starts_with("GET ") || l == "POST /api/token")
            && log.contains("/v1/me/tracks"),
        &log,
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
    let _ = fake.stop();
}

/// The loopback CLI identity can initialize the app; ordinary HTTP control
/// rights do not grant the owner-only Spotify sign-in capability.
fn owner_call(api: &str, token: &str, method: &str, path: &str, body: &Value) -> (u16, Value) {
    http(
        api,
        method,
        path,
        &[
            ("Content-Type", "application/json"),
            (fsonos_api::identity::CLI_TOKEN, token),
        ],
        &body.to_string(),
    )
    .map_or((0, Value::Null), |(status, _, body)| {
        (status, json_of(&body))
    })
}

#[test]
fn a_running_unconfigured_daemon_completes_setup_and_starts_the_dj_without_restart() {
    let fake = FakeSpotify::start();
    let mut s = Scenario::start("spotify_first_setup");
    s.sim(SimHousehold::standard());
    let clock = s.dir().join("clock");
    std::fs::write(&clock, chrono::Utc::now().to_rfc3339()).unwrap();
    let (mut daemon, api) = serve_with_app(&mut s, &format!("http://{}", fake.addr), &clock, None);
    let daemon_file: Value =
        serde_json::from_slice(&std::fs::read(s.dir().join("data/daemon.json")).unwrap()).unwrap();
    let token = daemon_file["cli_token"].as_str().unwrap();
    let before = owner_call(&api, token, "GET", "/auth/spotify", &json!({}));
    s.check(
        "unconfigured",
        "http",
        "the existing daemon reports first-run configuration honestly",
        before.0 == 200 && before.1["configured"] == false && before.1["signed_in"] == false,
        format!("{before:?}"),
    );

    let configured = owner_call(
        &api,
        token,
        "POST",
        "/auth/spotify/configure",
        &json!({ "client_id": CLIENT_ID }),
    );
    s.check(
        "configure",
        "http",
        "the app is persisted and immediately available without restarting",
        configured.0 == 200 && configured.1["configured"] == true,
        format!("{configured:?}"),
    );
    let started = owner_call(&api, token, "POST", "/auth/spotify/begin", &json!({}));
    let url = started.1["url"].as_str().unwrap_or("");
    fake.state.lock().unwrap().expected_challenge = Some(query_param(url, "code_challenge"));
    let state = query_param(url, "state");
    s.check(
        "begin",
        "http",
        "the existing sign-in engine starts PKCE with the configured app",
        started.0 == 200 && query_param(url, "client_id") == CLIENT_ID,
        format!("status={}", started.0),
    );
    let completed = owner_call(
        &api,
        token,
        "POST",
        "/auth/spotify/complete",
        &json!({ "redirect": format!("{REDIRECT}?code=good-code&state={state}") }),
    );
    s.check(
        "complete",
        "http",
        "the fake Spotify verifies PKCE and the daemon caches the grant",
        completed.0 == 200 && completed.1["signed_in"] == true,
        format!("{completed:?}"),
    );

    // No POST /dj/sync: the worker installed before configuration notices
    // first sign-in and fills the shared DJ cache itself.
    let synced = sync_until(&api, Duration::from_secs(30), |value| {
        value["running"] == false && value["last"]["tracks"].as_u64().unwrap_or(0) >= 4
    });
    s.check(
        "activated watcher",
        "http",
        "the original background worker publishes the first library automatically",
        synced["last"]["tracks"].as_u64().unwrap_or(0) >= 4
            && synced.get("error").is_none()
            && daemon.running(),
        &synced,
    );
    the_dj_plays_what_was_read(&mut s, &api);
    let repeated = owner_call(
        &api,
        token,
        "POST",
        "/auth/spotify/configure",
        &json!({ "client_id": CLIENT_ID }),
    );
    let replacement = owner_call(
        &api,
        token,
        "POST",
        "/auth/spotify/configure",
        &json!({ "client_id": "fedcba9876543210fedcba9876543210" }),
    );
    s.check(
        "initial app retained",
        "http",
        "same-id retries work while changing the configured app is refused",
        repeated.0 == 200 && (400..500).contains(&replacement.0),
        format!("same={} different={}", repeated.0, replacement.0),
    );
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "exit",
        "daemon",
        "serve stops cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    s.finish();
    let _ = fake.stop();
}
