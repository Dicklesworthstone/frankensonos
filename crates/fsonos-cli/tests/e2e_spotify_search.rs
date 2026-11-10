//! Search the live catalog through the real CLI, standalone MCP and daemon,
//! then explicitly play a returned source through the existing guarded path.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_proto::content::browse_all;
use fsonos_sim::SimHousehold;
use fsonos_spotify::client::{CachedToken, TEST_BASE_ENV, TokenCache, requested_scope};
use fsonos_spotify::fake_spotify::{CLIENT_ID, FakeSpotify};
use serde_json::{Value, json};
use std::time::Duration;

const ARTIST: &str = "Artist0000000000000001";
const QUERY: &str = "Lantern & Coast";

fn catalog() -> FakeSpotify {
    let fake = FakeSpotify::start();
    let path = fake
        .endpoints()
        .search(QUERY, "artist", 10)
        .strip_prefix(&format!("http://{}", fake.addr))
        .unwrap()
        .to_owned();
    let track = |n: u32| {
        let id = format!("Track{n:017}");
        json!({
            "id": id, "uri": format!("spotify:track:{id}"), "name": format!("Catalog song {n}"),
            "artists": [{ "id": ARTIST, "name": "Lantern Coast" }],
            "album": {
                "id": "Album00000000000000001",
                "uri": "spotify:album:Album00000000000001",
                "name": "Catalog album",
            },
            "duration_ms": 210_000, "is_playable": true, "explicit": false,
        })
    };
    {
        let mut state = fake.state.lock().unwrap();
        state.access = "catalog-access".into();
        state.library_pages.insert(
            path,
            json!({
                "artists": {
                    "items": [{
                        "id": ARTIST, "uri": format!("spotify:artist:{ARTIST}"),
                        "type": "artist", "name": "Lantern Coast",
                    }],
                    "offset": 0, "limit": 10, "total": 1, "next": null,
                },
            })
            .to_string(),
        );
        state.library_pages.insert(
            format!("/v1/artists/{ARTIST}/top-tracks?market=from_token"),
            json!({ "tracks": [track(2), track(1)] }).to_string(),
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
            access_token: "catalog-access".into(),
            refresh_token: "catalog-refresh".into(),
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

fn checked_source(s: &mut Scenario, step: &str, body: &str) -> String {
    let found: Value = serde_json::from_str(body).unwrap_or_default();
    let source = found["items"][0]["source_uri"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    s.check(
        step,
        "catalog",
        "one canonical artist source is returned for the exact query",
        found["query"] == QUERY
            && found["kind"] == "artist"
            && found["items"]
                .as_array()
                .is_some_and(|items| items.len() == 1)
            && source == format!("spotify:artist:{ARTIST}")
            && found["more"] == false,
        &found,
    );
    source
}

fn check_selected_queue(s: &mut Scenario, step: &str, room: &str) {
    let actual = queue_uris(s, room);
    let expected: Vec<String> = [2, 1]
        .into_iter()
        .map(|n| format!("spotify:track:Track{n:017}"))
        .collect();
    s.check(
        step,
        "sim",
        "the explicitly chosen search source resolves to its ordered artist tracks",
        actual == expected,
        format!("{actual:?}"),
    );
}

#[test]
fn direct_cli_and_standalone_mcp_search_before_a_separate_play_choice() {
    let fake = catalog();
    let base = format!("http://{}", fake.addr);
    let mut s = Scenario::start("catalog-search-direct-mcp");
    s.sim(SimHousehold::standard());
    let before = queue_uris(&s, "Kitchen");
    let refused = s.cli_env(
        "search-before-setup",
        &["--direct", "spotify-search", QUERY, "--kind", "artist"],
        &[(TEST_BASE_ENV, &base)],
        &[],
    );
    s.check(
        "search-before-setup",
        "cli",
        "missing sign-in has the shared error contract and performs no catalog read",
        !refused.ok()
            && refused.stderr.contains("SPOTIFY_AUTH_REQUIRED")
            && fake.state.lock().unwrap().log.is_empty(),
        &refused.stderr,
    );
    save_sign_in(&s);
    let found = s.cli_env(
        "direct-search",
        &[
            "--direct",
            "--json",
            "spotify-search",
            QUERY,
            "--kind",
            "artist",
        ],
        &[(TEST_BASE_ENV, &base)],
        &[],
    );
    s.check(
        "direct-search",
        "cli",
        "search exits successfully",
        found.ok(),
        &found.stderr,
    );
    let source = checked_source(&mut s, "direct-search", &found.stdout);

    let mut mcp = s.mcp_env(&[], &[(TEST_BASE_ENV, &base)]);
    mcp.initialize();
    let found = mcp.request(
        "tools/call",
        &json!({
            "name": "search_spotify",
            "arguments": { "query": QUERY, "kind": "artist" },
        }),
    );
    s.check(
        "mcp-search",
        "mcp",
        "standalone MCP offers the same canonical source",
        found["result"].is_object()
            && found["result"]["isError"] != true
            && found["result"].to_string().contains(&source),
        &found,
    );
    drop(mcp);
    let after = queue_uris(&s, "Kitchen");
    s.check(
        "search-is-read-only",
        "sim",
        "catalog reads leave the speaker queue intact",
        after == before,
        format!("{after:?}"),
    );

    let played = s.cli_env(
        "play-choice",
        &["--direct", "play", "Kitchen", &source],
        &[(TEST_BASE_ENV, &base)],
        &[],
    );
    s.check(
        "play-choice",
        "cli",
        "the explicit choice plays",
        played.ok(),
        &played.stderr,
    );
    check_selected_queue(&mut s, "play-choice", "Kitchen");
    let calls = fake.stop().log;
    s.check(
        "catalog-reads",
        "spotify",
        "two searches and one explicit artist resolution use only read requests",
        calls.len() == 3
            && calls
                .iter()
                .filter(|call| call.starts_with("GET /v1/search?"))
                .count()
                == 2
            && calls
                .iter()
                .filter(|call| call.contains("/top-tracks?"))
                .count()
                == 1,
        format!("{calls:?}"),
    );
    s.finish();
}

fn serve(s: &mut Scenario, base: &str) -> (Daemon, String) {
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
        "the daemon starts before Spotify setup",
        live.is_some() && !api.is_empty(),
        daemon.seen.join("\n"),
    );
    (daemon, api)
}

#[test]
fn http_and_remote_cli_search_use_the_daemons_account_then_play_the_choice() {
    let fake = catalog();
    let base = format!("http://{}", fake.addr);
    let mut s = Scenario::start("catalog-search-daemon");
    s.sim(SimHousehold::standard());
    let (mut daemon, api) = serve(&mut s, &base);
    save_sign_in(&s);
    let before = queue_uris(&s, "Living Room");
    let (status, _, body) = http(
        &api,
        "GET",
        "/spotify/search?q=Lantern%20%26%20Coast&kind=artist",
        &[],
        "",
    )
    .unwrap();
    s.check(
        "http-search",
        "http",
        "HTTP search succeeds",
        status == 200,
        &body,
    );
    let source = checked_source(&mut s, "http-search", &body);

    let daemon_url = format!("http://{api}");
    let found = s.cli(
        "remote-search",
        &[
            "--daemon-url",
            &daemon_url,
            "--json",
            "spotify-search",
            QUERY,
            "--kind",
            "artist",
        ],
    );
    s.check(
        "remote-search",
        "cli",
        "remote search succeeds",
        found.ok(),
        &found.stderr,
    );
    assert_eq!(
        checked_source(&mut s, "remote-search", &found.stdout),
        source
    );
    let after = queue_uris(&s, "Living Room");
    s.check(
        "search-is-read-only",
        "sim",
        "HTTP and remote CLI search preserve the current queue",
        after == before,
        format!("{after:?}"),
    );
    let played = s.cli(
        "play-choice",
        &["--daemon-url", &daemon_url, "play", "Living Room", &source],
    );
    s.check(
        "play-choice",
        "cli",
        "the remote choice plays",
        played.ok(),
        &played.stderr,
    );
    check_selected_queue(&mut s, "play-choice", "Living Room");
    let stopped = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "shutdown",
        "daemon",
        "the daemon exits cleanly",
        stopped == Some(0),
        daemon.seen.join("\n"),
    );
    let calls = fake.stop().log;
    s.check(
        "catalog-reads",
        "spotify",
        "the signed-in daemon performs both searches and the chosen artist resolution",
        calls
            .iter()
            .filter(|call| call.starts_with("GET /v1/search?"))
            .count()
            == 2
            && calls
                .iter()
                .filter(|call| call.contains("/top-tracks?"))
                .count()
                == 1,
        format!("{calls:?}"),
    );
    s.finish();
}
