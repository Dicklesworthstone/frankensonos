//! Real daemon: the phone app's sign-in exchange and the DJ sync against fake
//! Spotify, followed by DJ playback and the browse routes.
#![forbid(unsafe_code)]

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::get_transport_info;
use fsonos_sim::SimHousehold;
use fsonos_spotify::fake_spotify::{CLIENT_ID, FakeSpotify, REDIRECT};
use fsonos_types::TransportState;
use serde_json::Value;
use std::time::{Duration, Instant};

#[test]
fn sign_in_and_sync_make_the_dj_work_without_a_restart() {
    let fake = FakeSpotify::start();
    let mut s = Scenario::start("spotify-dj");
    s.sim(SimHousehold::standard());
    let accounts = format!("http://{}", fake.addr);
    let api = fake.endpoints().api;
    let args = [
        "serve",
        "--http",
        "127.0.0.1:0",
        "--mcp-http",
        "127.0.0.1:0",
        "--spotify-client-id",
        CLIENT_ID,
        "--spotify-redirect-uri",
        REDIRECT,
        "--spotify-accounts-url",
        &accounts,
        "--spotify-api-url",
        &api,
    ];
    let mut daemon = s.spawn("serve", &args);
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap();
    let addr = ready
        .split_whitespace()
        .find_map(|w| w.strip_prefix("http=http://"))
        .unwrap();
    daemon
        .wait_line("fsonos serve: live", Duration::from_secs(20))
        .unwrap();
    let mut bodies = Vec::new();
    let mut request = |method, path: &str, body: &str| {
        let (code, headers, text) = http(
            addr,
            method,
            path,
            &[("Content-Type", "application/json")],
            body,
        )
        .unwrap();
        bodies.push(text.clone());
        (code, headers, text)
    };
    let before = request("POST", "/dj/start", r#"{"zone":"Living Room"}"#);
    assert_ne!(before.0, 200);
    let (code, _, text) = request("POST", "/auth/spotify/exchange", &app_sign_in(&fake));
    assert_eq!(code, 200, "{text}");
    assert_eq!(request("POST", "/dj/sync", "{}").0, 200);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (_, _, text) = request("GET", "/spotify/status", "");
        let status: Value = serde_json::from_str(&text).unwrap();
        if status["sync"]["running"] == false {
            assert_eq!(status["sync"]["error"], Value::Null);
            assert_eq!(status["signed_in"], true);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    let (code, _, text) = request("POST", "/dj/start", r#"{"zone":"Living Room"}"#);
    assert_eq!(code, 200, "{text}");
    let transport = get_transport_info(&s.lan(), s.ip("Living Room")).unwrap();
    assert_eq!(transport.state, TransportState::Playing);
    request("GET", "/actions", "");
    let stopped = daemon.interrupt(Duration::from_secs(10));
    assert_eq!(stopped, Some(0));
    for token in ["access-1", "refresh-1", "access-r1", "refresh-r1"] {
        assert!(!bodies.join("\n").contains(token));
        assert!(!daemon.seen.join("\n").contains(token));
    }
    s.check(
        "spotify-dj",
        "http+sim",
        "sign-in and sync let the existing DJ play without restarting",
        true,
        text,
    );
    s.finish();
    fake.stop();
}

/// The phone's browse tabs read the cache the daemon's sync filled, and search reaches Spotify.
#[test]
fn after_a_sync_the_app_browses_the_library_and_searches_spotify() {
    let fake = FakeSpotify::start();
    let mut s = Scenario::start("spotify-album");
    s.sim(SimHousehold::standard());
    let accounts = format!("http://{}", fake.addr);
    let api = fake.endpoints().api;
    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
            "--spotify-client-id",
            CLIENT_ID,
            "--spotify-redirect-uri",
            REDIRECT,
            "--spotify-accounts-url",
            &accounts,
            "--spotify-api-url",
            &api,
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap();
    let addr = ready
        .split_whitespace()
        .find_map(|w| w.strip_prefix("http=http://"))
        .unwrap();
    daemon
        .wait_line("fsonos serve: live", Duration::from_secs(20))
        .unwrap();
    let request = |method, path: &str, body: &str| {
        http(
            addr,
            method,
            path,
            &[("Content-Type", "application/json")],
            body,
        )
        .unwrap()
    };
    let (code, _, text) = request("POST", "/auth/spotify/exchange", &app_sign_in(&fake));
    assert_eq!(code, 200, "{text}");
    assert_eq!(request("POST", "/dj/sync", "{}").0, 200);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status: Value = serde_json::from_str(&request("GET", "/spotify/status", "").2).unwrap();
        if status["sync"]["running"] == false {
            assert_eq!(status["sync"]["error"], Value::Null);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    let albums: Value = serde_json::from_str(&request("GET", "/spotify/albums", "").2).unwrap();
    assert_eq!(albums["total"], 2, "{albums}");
    assert_eq!(
        albums["items"][0]["art_url"],
        "https://cdn.example.invalid/bach.jpg"
    );
    let (code, _, text) = request("GET", "/spotify/albums/FakeAlbum0000000000001/tracks", "");
    assert_eq!(code, 200, "{text}");
    let tracks: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(tracks.as_array().unwrap().len(), 3, "{tracks}");
    let liked: Value = serde_json::from_str(&request("GET", "/spotify/tracks", "").2).unwrap();
    assert!(liked["total"].as_u64().unwrap() > 0, "{liked}");
    let (code, _, text) = request("GET", "/spotify/search?q=gould", "");
    assert_eq!(code, 200, "{text}");
    assert_eq!(daemon.interrupt(Duration::from_secs(10)), Some(0));
    s.check(
        "spotify-browse",
        "http+sim+spotify",
        "after a sync the app browses albums, album tracks and liked tracks, and searches Spotify",
        true,
        text,
    );
    s.finish();
    fake.stop();
}

/// The body of the phone app's sign-in (its PKCE verifier and code), with the
/// fake Spotify set to accept it.
fn app_sign_in(fake: &FakeSpotify) -> String {
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const APP_REDIRECT: &str = "frankensonos://spotify-callback";
    {
        let mut state = fake.state.lock().unwrap();
        state.expected_challenge = Some(CHALLENGE.into());
        state.expected_redirect = Some(APP_REDIRECT.into());
    }
    serde_json::json!({
        "code": "good-code", "code_verifier": VERIFIER, "redirect_uri": APP_REDIRECT
    })
    .to_string()
}
