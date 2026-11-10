use super::*;
use fsonos_spotify::client::CachedToken;
use fsonos_spotify::fake_spotify::{CLIENT_ID, FakeSpotify, REDIRECT};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};

const ARTIST: &str = "Artist0000000000000001";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn sources() -> Sources {
    Sources {
        data_dir: std::env::temp_dir().join(format!(
            "fsonos-artist-adapter-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        )),
        client_id: Some(CLIENT_ID.into()),
        redirect_uri: Some(REDIRECT.into()),
        endpoints: Endpoints::default(),
        state: Mutex::new(State::default()),
    }
}

fn token(access: &str) -> CachedToken {
    CachedToken {
        access_token: access.into(),
        refresh_token: "artist-refresh".into(),
        expires_at: i64::MAX / 2,
        scope: "user-library-read".into(),
    }
}

fn selection(fake: &FakeSpotify) {
    let mut state = fake.state.lock().unwrap();
    state.access = "artist-access".into();
    state.library_pages.insert(
        format!("/v1/artists/{ARTIST}/top-tracks?market=from_token"),
        json!({ "tracks": [{
            "id": "Track00000000000000001", "uri": "spotify:track:Track00000000000000001",
            "name": "Catalog song", "artists": [{ "id": ARTIST, "name": "Lantern Coast" }],
            "album": {
                "id": "Album00000000000000001", "uri": "spotify:album:Album00000000000000001",
                "name": "Catalog album",
            },
            "duration_ms": 210_000, "explicit": true, "is_playable": true,
        }] })
        .to_string(),
    );
}

fn configured(fake: &FakeSpotify) -> Sources {
    let mut sources = sources();
    sources.endpoints = fake.endpoints();
    TokenCache::in_data_dir(&sources.data_dir)
        .store(&token("artist-access"))
        .unwrap();
    sources
}

#[test]
fn saved_app_settings_are_loaded_after_installation_and_explicit_options_win() {
    let mut sources = sources();
    // An empty explicit value prevents the test depending on an ambient
    // environment and retains the production saved-file fallback.
    sources.client_id = Some(String::new());
    assert_eq!(
        sources.configuration().unwrap_err().code,
        ErrorCode::SpotifyAuthRequired
    );
    crate::spotify_app::save(&sources.data_dir, CLIENT_ID).unwrap();
    let config = sources.configuration().unwrap();
    assert_eq!(config.client_id, CLIENT_ID);
    assert_eq!(config.redirect_uri, REDIRECT);
    sources.client_id = Some("abcdef0123456789abcdef0123456789".into());
    sources.redirect_uri = Some("http://127.0.0.1:9888/spotify/callback".into());
    let config = sources.configuration().unwrap();
    assert_eq!(config.client_id, "abcdef0123456789abcdef0123456789");
    assert_eq!(
        config.redirect_uri,
        "http://127.0.0.1:9888/spotify/callback"
    );
}

#[test]
fn real_worker_resolves_tracks_and_reloads_a_later_sign_in() {
    let fake = FakeSpotify::start();
    selection(&fake);
    let sources = configured(&fake);
    for access in ["artist-access", "new-sign-in-access"] {
        TokenCache::in_data_dir(&sources.data_dir)
            .store(&token(access))
            .unwrap();
        access.clone_into(&mut fake.state.lock().unwrap().access);
        let found = sources
            .artist_tracks(&format!("spotify:artist:{ARTIST}"))
            .unwrap();
        assert_eq!(found.kind, SourceSelectionKind::TopTracks);
        assert_eq!(
            found.tracks,
            [(
                "spotify:track:Track00000000000000001".into(),
                "Catalog song".into()
            )]
        );
    }
    let seen = fake.stop();
    assert_eq!(seen.log.len(), 2);
    assert_eq!(
        seen.refreshes, 0,
        "fresh sessions see the replacement grant"
    );
}

#[test]
fn a_missing_grant_returns_the_authentication_contract_without_network_io() {
    let fake = FakeSpotify::start();
    let mut sources = sources();
    sources.endpoints = fake.endpoints();
    assert_eq!(
        sources.artist_tracks(ARTIST).unwrap_err().code,
        ErrorCode::SpotifyAuthRequired
    );
    assert!(fake.stop().log.is_empty());
}

#[test]
fn a_busy_lookup_is_rejected_instead_of_queueing_another_worker() {
    let sources = sources();
    let _running = sources.state.lock().unwrap();
    let failed = sources.artist_tracks(ARTIST).unwrap_err();
    assert_eq!(failed.code, ErrorCode::NotReady);
    assert!(failed.detail.contains("already being read"));
}

#[test]
fn the_adapter_carries_server_cooldowns_across_fresh_sessions() {
    let fake = FakeSpotify::start();
    let sources = configured(&fake);
    {
        let mut state = fake.state.lock().unwrap();
        state.access = "artist-access".into();
        state.rate_limit_artists = 1;
        state.artist_retry_after = Some(3600);
    }
    for _ in 0..2 {
        let failed = sources.artist_tracks(ARTIST).unwrap_err();
        assert_eq!(failed.code, ErrorCode::NotReady);
        assert!(failed.detail.contains("rate-limited"));
    }
    assert_eq!(fake.stop().log.len(), 1, "the second call sends no request");
}

#[test]
fn a_concurrent_sign_in_during_refresh_keeps_the_session_credential_fence() {
    let fake = FakeSpotify::start();
    selection(&fake);
    let sources = configured(&fake);
    let cache = TokenCache::in_data_dir(&sources.data_dir);
    let mut stale = token("expired-access");
    stale.expires_at = 1;
    cache.store(&stale).unwrap();
    let changed = cache.clone();
    let replacement = token("new-owner-access");
    let concurrent = replacement.clone();
    {
        let mut state = fake.state.lock().unwrap();
        state.refresh = "artist-refresh".into();
        state.before_refresh_response = Some(Box::new(move || changed.store(&concurrent).unwrap()));
    }
    let failure = sources.artist_tracks(ARTIST).unwrap_err();
    assert_eq!(failure.code, ErrorCode::SpotifyAuthRequired);
    assert_eq!(cache.load().unwrap(), Some(replacement));
    let seen = fake.stop();
    assert_eq!(seen.log.len(), 1);
    assert!(seen.log[0].contains("/api/token"));
}

#[test]
fn cooldown_durations_do_not_overflow_and_expired_cooldowns_allow_reading() {
    let mut state = State::default();
    state.record(u64::MAX);
    assert!(state.remaining() > 0);
    state.cooldown = Some((
        Instant::now() - Duration::from_secs(2),
        Duration::from_secs(1),
    ));
    assert_eq!(state.remaining(), 0);
    state.record(0);
    assert_eq!(state.remaining(), 0);
}

const SHOW: &str = "Show000000000000000001";

fn show_selection(fake: &FakeSpotify) {
    let episode = |number: u32| {
        let id = format!("Episode{number:015}");
        json!({
            "id": id, "uri": format!("spotify:episode:{id}"), "type": "episode",
            "name": format!("Catalog episode {number}"), "duration_ms": 1_800_000,
            "explicit": number == 2, "is_playable": true,
        })
    };
    let mut state = fake.state.lock().unwrap();
    state.access = "artist-access".into();
    state.library_pages.insert(
        format!("/v1/shows/{SHOW}"),
        json!({
            "id": SHOW, "uri": format!("spotify:show:{SHOW}"), "type": "show",
            "name": "The Sim Conversations", "total_episodes": 30,
            "episodes": {
                "items": [episode(2), episode(1), episode(2)],
                "offset": 0, "limit": 20, "total": 30,
                "next": "https://example.invalid/episodes-do-not-follow",
            },
        })
        .to_string(),
    );
}

#[test]
fn real_worker_resolves_show_episodes_and_reloads_a_later_sign_in() {
    let fake = FakeSpotify::start();
    show_selection(&fake);
    let sources = configured(&fake);
    for access in ["artist-access", "new-show-sign-in-access"] {
        TokenCache::in_data_dir(&sources.data_dir)
            .store(&token(access))
            .unwrap();
        access.clone_into(&mut fake.state.lock().unwrap().access);
        let found = sources
            .show_episodes(&format!("spotify:show:{SHOW}"))
            .unwrap();
        assert_eq!(found.kind, SourceSelectionKind::ShowEpisodes);
        assert_eq!(
            found.tracks,
            [
                (
                    "spotify:episode:Episode000000000000002".into(),
                    "Catalog episode 2".into()
                ),
                (
                    "spotify:episode:Episode000000000000001".into(),
                    "Catalog episode 1".into()
                ),
            ]
        );
    }
    let seen = fake.stop();
    assert_eq!(seen.log, vec![format!("GET /v1/shows/{SHOW}"); 2]);
    assert_eq!(seen.refreshes, 0, "the replacement grant is read from disk");
}

#[test]
fn show_lookups_share_missing_grant_and_busy_admission_contracts() {
    let fake = FakeSpotify::start();
    let mut sources = sources();
    sources.endpoints = fake.endpoints();
    assert_eq!(
        sources.show_episodes(SHOW).unwrap_err().code,
        ErrorCode::SpotifyAuthRequired
    );
    let running = sources.state.lock().unwrap();
    let failed = sources.show_episodes(SHOW).unwrap_err();
    assert_eq!(failed.code, ErrorCode::NotReady);
    assert!(failed.detail.contains("already being read"));
    drop(running);
    assert!(fake.stop().log.is_empty());
}

#[test]
fn artist_and_show_reads_share_the_same_cooldown_across_workers() {
    for show_first in [false, true] {
        let fake = FakeSpotify::start();
        selection(&fake);
        show_selection(&fake);
        let sources = configured(&fake);
        {
            let mut state = fake.state.lock().unwrap();
            if show_first {
                state
                    .api_errors
                    .insert(format!("/v1/shows/{SHOW}"), (429, Some(3600)));
            } else {
                state.api_errors.insert(
                    format!("/v1/artists/{ARTIST}/top-tracks?market=from_token"),
                    (429, Some(3600)),
                );
            }
        }
        for show in [show_first, !show_first, show_first] {
            let failed = if show {
                sources.show_episodes(SHOW).unwrap_err()
            } else {
                sources.artist_tracks(ARTIST).unwrap_err()
            };
            assert_eq!(failed.code, ErrorCode::NotReady);
            assert!(failed.detail.contains("rate-limited"));
        }
        let seen = fake.stop();
        assert_eq!(seen.log.len(), 1, "other catalog kinds retain the cooldown");
        assert_eq!(
            seen.log[0].contains("/v1/shows/"),
            show_first,
            "only the first admitted lookup reaches Spotify"
        );
    }
}

#[test]
fn show_catalog_refusals_are_reported_without_artist_fallback_requests() {
    let fake = FakeSpotify::start();
    show_selection(&fake);
    fake.state
        .lock()
        .unwrap()
        .api_errors
        .insert(format!("/v1/shows/{SHOW}"), (403, None));
    let sources = configured(&fake);
    let failure = sources.show_episodes(SHOW).unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    assert!(failure.detail.contains("403"));
    assert_eq!(fake.stop().log, [format!("GET /v1/shows/{SHOW}")]);
}

fn catalog_search_fixture(fake: &FakeSpotify) -> String {
    let path = fake
        .endpoints()
        .search("Lantern & Coast", "artist", 10)
        .strip_prefix(&format!("http://{}", fake.addr))
        .unwrap()
        .to_owned();
    let mut state = fake.state.lock().unwrap();
    state.access = "artist-access".into();
    state.library_pages.insert(
        path.clone(),
        json!({
            "artists": {
                "items": [{
                    "id": ARTIST,
                    "uri": format!("spotify:artist:{ARTIST}"),
                    "type": "artist",
                    "name": "Lantern Coast",
                }],
                "offset": 0, "limit": 10, "total": 20,
                "next": "https://example.invalid/catalog-page-not-followed",
            },
        })
        .to_string(),
    );
    path
}

#[test]
fn real_worker_searches_with_the_current_sign_in_and_returns_explicit_sources() {
    let fake = FakeSpotify::start();
    let path = catalog_search_fixture(&fake);
    let sources = configured(&fake);
    for access in ["artist-access", "replacement-search-access"] {
        TokenCache::in_data_dir(&sources.data_dir)
            .store(&token(access))
            .unwrap();
        access.clone_into(&mut fake.state.lock().unwrap().access);
        let found = sources
            .search_catalog("Lantern & Coast", CatalogKind::Artist, 10)
            .unwrap();
        assert_eq!(
            found.items,
            [CatalogItem {
                source_uri: format!("spotify:artist:{ARTIST}"),
                title: "Lantern Coast".into(),
                subtitle: None,
            }]
        );
        assert!(found.more);
    }
    let seen = fake.stop();
    assert_eq!(seen.log, vec![format!("GET {path}"); 2]);
    assert_eq!(seen.refreshes, 0);
}

#[test]
fn search_shares_missing_grant_and_busy_admission_contracts_without_network_io() {
    let fake = FakeSpotify::start();
    let mut sources = sources();
    sources.endpoints = fake.endpoints();
    assert_eq!(
        sources
            .search_catalog("music", CatalogKind::Track, 10)
            .unwrap_err()
            .code,
        ErrorCode::SpotifyAuthRequired
    );
    let running = sources.state.lock().unwrap();
    let failed = sources
        .search_catalog("music", CatalogKind::Track, 10)
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::NotReady);
    assert!(failed.detail.contains("already being read"));
    drop(running);
    assert!(fake.stop().log.is_empty());
}

#[test]
fn search_and_playback_share_the_same_spotify_cooldown() {
    for search_first in [true, false] {
        let fake = FakeSpotify::start();
        selection(&fake);
        let path = catalog_search_fixture(&fake);
        let sources = configured(&fake);
        {
            let mut state = fake.state.lock().unwrap();
            if search_first {
                state.api_errors.insert(path.clone(), (429, Some(3600)));
            } else {
                state.api_errors.insert(
                    format!("/v1/artists/{ARTIST}/top-tracks?market=from_token"),
                    (429, Some(3600)),
                );
            }
        }
        for search in [search_first, !search_first, search_first] {
            let failed = if search {
                sources
                    .search_catalog("Lantern & Coast", CatalogKind::Artist, 10)
                    .unwrap_err()
            } else {
                sources
                    .artist_tracks(&format!("spotify:artist:{ARTIST}"))
                    .unwrap_err()
            };
            assert_eq!(failed.code, ErrorCode::NotReady);
            assert!(failed.detail.contains("rate-limited"));
        }
        let seen = fake.stop();
        assert_eq!(seen.log.len(), 1);
        assert_eq!(
            seen.log[0].starts_with("GET /v1/search?"),
            search_first,
            "only the first admitted catalog read reaches Spotify"
        );
    }
}
