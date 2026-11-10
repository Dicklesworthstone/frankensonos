//! A show resolves through the real CLI's read-only Spotify adapter before
//! the virtual S1/S2 queues change. Direct CLI, standalone MCP and the daemon
//! all use the same bounded selection, including setup completed after startup.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_proto::content::browse_all;
use fsonos_proto::control::{get_position_info, get_transport_info};
use fsonos_sim::SimHousehold;
use fsonos_spotify::client::{CachedToken, TEST_BASE_ENV, TokenCache, requested_scope};
use fsonos_spotify::fake_spotify::{CLIENT_ID, FakeSpotify};
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::time::Duration;

const SHOW: &str = "Show000000000000000001";

fn episode(n: u32) -> Value {
    let id = format!("Episode{n:015}");
    json!({
        "id": id, "uri": format!("spotify:episode:{id}"), "type": "episode",
        "name": format!("Catalog episode {n}"), "duration_ms": 1_800_000,
        "is_playable": true, "explicit": n == 2,
    })
}

fn catalog() -> FakeSpotify {
    let fake = FakeSpotify::start();
    {
        let mut state = fake.state.lock().unwrap();
        state.access = "show-access".into();
        state.library_pages.insert(
            format!("/v1/shows/{SHOW}"),
            json!({
                "id": SHOW, "uri": format!("spotify:show:{SHOW}"), "type": "show",
                "name": "The Sim Conversations", "total_episodes": 30,
                "episodes": {
                    "items": [episode(2), episode(1), episode(2)],
                    "offset": 0, "limit": 20, "total": 30,
                    "next": "https://example.invalid/no-next-page-read",
                },
            })
            .to_string(),
        );
    }
    fake
}

fn save_sign_in(s: &Scenario) {
    let data = s.dir().join("data");
    std::fs::write(
        data.join("spotify.toml"),
        format!("client_id = \"{CLIENT_ID}\"\n"),
    )
    .unwrap();
    TokenCache::in_data_dir(&data)
        .store(&CachedToken {
            access_token: "show-access".into(),
            refresh_token: "show-refresh".into(),
            expires_at: i64::MAX / 2,
            scope: requested_scope(),
        })
        .unwrap();
}

fn queue_uris(s: &Scenario, room: &str) -> Vec<String> {
    browse_all(&s.lan(), s.ip(room), "Q:0")
        .unwrap()
        .iter()
        .filter_map(fsonos_proto::didl::DidlObject::to_track)
        .map(|track| track.source_uri)
        .collect()
}

fn check_selection(s: &mut Scenario, step: &str, room: &str) {
    let actual = queue_uris(s, room);
    let expected: Vec<String> = [2, 1]
        .into_iter()
        .map(|n| format!("spotify:episode:Episode{n:015}"))
        .collect();
    let state = get_transport_info(&s.lan(), s.ip(room))
        .map(|transport| transport.state)
        .ok();
    let position = get_position_info(&s.lan(), s.ip(room)).ok();
    s.check(
        step,
        "sim",
        "the show's distinct episodes play in provider order, including explicit episodes",
        actual == expected
            && state == Some(TransportState::Playing)
            && position.as_ref().is_some_and(|position| {
                position.track == 1 && position.uri.contains("Episode000000000000002")
            }),
        format!("{room}: {actual:?}, {state:?}, {position:?}"),
    );
}

#[test]
fn direct_cli_and_standalone_mcp_resolve_shows_on_both_households() {
    let fake = catalog();
    let base = format!("http://{}", fake.addr);
    let mut s = Scenario::start("show-direct-mcp");
    s.sim(SimHousehold::standard());
    save_sign_in(&s);
    let link = format!("https://open.spotify.com/show/{SHOW}?si=sim");
    let played = s.cli_env(
        "show-direct",
        &["--direct", "play", "Kitchen", &link],
        &[(TEST_BASE_ENV, &base)],
        &[],
    );
    s.check(
        "show-direct",
        "cli",
        "a show share link plays",
        played.ok(),
        &played.stderr,
    );
    check_selection(&mut s, "show-direct", "Kitchen");

    let mut mcp = s.mcp_env(&[], &[(TEST_BASE_ENV, &base)]);
    mcp.initialize();
    let played = mcp.request(
        "tools/call",
        &json!({
            "name": "play",
            "arguments": { "zone": "Living Room", "source_uri": format!("spotify:show:{SHOW}") },
        }),
    );
    s.check(
        "show-mcp",
        "mcp",
        "standalone MCP resolves a show with the saved app and grant",
        played["result"].is_object() && played["result"]["isError"] != true,
        &played,
    );
    check_selection(&mut s, "show-mcp", "Living Room");
    drop(mcp);
    let requests = fake.stop().log;
    let expected_read = format!("GET /v1/shows/{SHOW}");
    s.check(
        "catalog",
        "spotify",
        "both surfaces read exactly one show page, with no library writes",
        requests.len() == 2 && requests.iter().all(|line| line == &expected_read),
        format!("{requests:?}"),
    );
    s.finish();
}

fn serve_without_spotify(s: &mut Scenario, base: &str) -> (Daemon, String) {
    let mut daemon = s.spawn_env(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
        ],
        &[(TEST_BASE_ENV, base)],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = ready
        .split_whitespace()
        .find_map(|word| word.strip_prefix("http=http://"))
        .unwrap_or("")
        .to_owned();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "the daemon starts before Spotify is configured",
        live.is_some() && !api.is_empty(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

fn play_show(api: &str, room: &str) -> (u16, String) {
    let request = json!({ "zone": room, "source_uri": format!("spotify:show:{SHOW}") });
    let (code, _, body) = http(
        api,
        "POST",
        "/play",
        &[("Content-Type", "application/json")],
        &request.to_string(),
    )
    .unwrap();
    (code, body)
}

#[test]
fn a_running_daemon_picks_up_show_setup_and_serves_http_and_remote_cli() {
    let fake = catalog();
    let base = format!("http://{}", fake.addr);
    let mut s = Scenario::start("show-daemon-lazy-setup");
    s.sim(SimHousehold::standard());
    let (mut daemon, api) = serve_without_spotify(&mut s, &base);
    let before = queue_uris(&s, "Living Room");
    let (code, refused) = play_show(&api, "Living Room");
    let after = queue_uris(&s, "Living Room");
    s.check(
        "show-before-setup",
        "http",
        "missing setup returns the authentication contract and leaves the queue intact",
        code == 409
            && refused.contains("SPOTIFY_AUTH_REQUIRED")
            && after == before
            && fake.state.lock().unwrap().log.is_empty(),
        format!("HTTP {code}: {refused}; queue {after:?}"),
    );

    save_sign_in(&s);
    let (code, played) = play_show(&api, "Living Room");
    s.check(
        "show-after-setup",
        "http",
        "setup is picked up without restarting the daemon",
        code == 200,
        format!("HTTP {code}: {played}"),
    );
    check_selection(&mut s, "show-after-setup", "Living Room");

    let daemon_url = format!("http://{api}");
    let played = s.cli(
        "show-remote-cli",
        &[
            "--daemon-url",
            &daemon_url,
            "play",
            "Kitchen",
            &format!("spotify:show:{SHOW}"),
        ],
    );
    s.check(
        "show-remote-cli",
        "cli",
        "the remote CLI resolves on the signed-in daemon",
        played.ok(),
        &played.stderr,
    );
    check_selection(&mut s, "show-remote-cli", "Kitchen");
    let stopped = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "shutdown",
        "daemon",
        "the daemon exits cleanly",
        stopped == Some(0),
        daemon.seen.join("\n"),
    );
    let requests = fake.stop().log;
    let expected_read = format!("GET /v1/shows/{SHOW}");
    s.check(
        "catalog",
        "spotify",
        "only the two successful selections read catalog pages",
        requests.len() == 2 && requests.iter().all(|line| line == &expected_read),
        format!("{requests:?}"),
    );
    s.finish();
}
