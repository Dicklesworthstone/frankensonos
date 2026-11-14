//! Definite provider denials outrank fallback retention of unobserved taste.

use super::*;
use crate::cache::sync_library_detailed;
use crate::client::FullTrack;
use chrono::{SecondsFormat, Utc};

const ARTIST: &str = "AvailabilityArtist";

fn uri(number: usize) -> String {
    format!("spotify:track:TestTrack{number}")
}

fn retained_store() -> MemStore {
    let mut store = MemStore::default();
    store.adopt_spotify_library_owner("test-owner").unwrap();
    let items: Vec<_> = [0, 1, 2, 3, 9]
        .into_iter()
        .map(|number| {
            serde_json::from_value::<FullTrack>(track(number))
                .unwrap()
                .library_item(Origin::Taste, None)
                .unwrap()
        })
        .collect();
    apply_library_read(&mut store, &items).unwrap();
    store
}

fn denial_rows(catalog: bool) -> Vec<Value> {
    let mut rows: Vec<_> = [0, 1, 2, 9, 5].into_iter().map(track).collect();
    rows[0]["restrictions"] = json!({ "reason": "market" });
    if catalog {
        rows[1]["restrictions"] = json!({ "reason": "product" });
    } else {
        rows[1]["album"]["restrictions"] = json!({ "reason": "product" });
    }
    rows[2]["is_playable"] = json!(false);
    rows[3]["restrictions"] = json!({ "reason": "future-provider-reason" });
    if catalog {
        for row in &mut rows {
            row["artists"][0]["id"] = json!(ARTIST);
        }
    }
    rows
}

fn source(server: &Server, source: &str) -> &'static str {
    let mut rows = denial_rows(source == "artist catalog");
    // A new successful saved-library observation overrides optional taste
    // availability; it must not be discarded with the cached fallback rows.
    let mut saved = track(9);
    saved["name"] = json!("Fresh saved metadata");
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": saved })], 0, None),
    );
    match source {
        "top" => {
            server.set("/v1/me/top/tracks", page(rows, 0, None));
            server.set(
                "/v1/me/player/recently-played",
                json!({ "items": "invalid" }),
            );
            "user-top-read user-read-recently-played"
        }
        "recent" => {
            let at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            server.set(
                "/v1/me/player/recently-played",
                json!({ "items": rows.into_iter().map(|track| {
                    json!({ "track": track, "played_at": at })
                }).collect::<Vec<_>>() }),
            );
            server.set("/v1/me/playlists", json!({ "items": "invalid" }));
            "user-read-recently-played playlist-read-private"
        }
        "playlist" => {
            rows[0] = json!({
                "uri": uri(0), "restrictions": { "reason": "market" }
            });
            server.set("/v1/me/top/tracks", json!({ "items": "invalid" }));
            server.set("/v1/me/playlists", page(vec![playlist(0, true)], 0, None));
            server.set(
                "/v1/playlists/TestList0/items",
                page(
                    rows.into_iter()
                        .map(|item| json!({ "item": item }))
                        .collect(),
                    0,
                    None,
                ),
            );
            "user-top-read playlist-read-private"
        }
        "artist top" | "artist catalog" => {
            server.set("/v1/me/top/tracks", json!({ "items": "invalid" }));
            server.set(
                "/v1/me/top/artists",
                page(
                    vec![json!({ "id": ARTIST, "name": "Test artist" })],
                    0,
                    None,
                ),
            );
            if source == "artist top" {
                server.set(
                    &format!("/v1/artists/{ARTIST}/top-tracks"),
                    json!({ "tracks": rows }),
                );
            } else {
                server
                    .state
                    .lock()
                    .unwrap()
                    .refused
                    .insert(format!("/v1/artists/{ARTIST}/top-tracks"), 403);
                server.set(
                    &format!("/v1/artists/{ARTIST}/albums"),
                    page(
                        vec![json!({
                            "id": "AvailabilityAlbum", "uri": "spotify:album:AvailabilityAlbum",
                            "name": "Test album", "album_type": "album",
                            "artists": [{ "id": ARTIST, "name": "Test artist" }]
                        })],
                        0,
                        None,
                    ),
                );
                server.set("/v1/albums/AvailabilityAlbum/tracks", page(rows, 0, None));
            }
            "user-top-read"
        }
        _ => unreachable!("known test source"),
    }
}

#[test]
fn fresh_denials_cannot_be_revived_by_an_unrelated_optional_source_failure() {
    for kind in ["top", "recent", "playlist", "artist top", "artist catalog"] {
        let server = Server::new();
        let scope = source(&server, kind);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, scope);
            let mut store = retained_store();
            let result = sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            assert!(
                result
                    .notes
                    .iter()
                    .any(|note| note.contains("taste signals not read")),
                "{kind}: {result:?}"
            );
            let pool = pool_from_store(&store).unwrap();
            for number in [0, 1, 2] {
                assert!(pool.get(&uri(number)).is_none(), "{kind}: denied {number}");
            }
            assert!(
                pool.get(&uri(3)).is_some(),
                "{kind}: unknown prior taste survives"
            );
            assert!(
                pool.get(&uri(5)).is_some(),
                "{kind}: fresh playable taste survives"
            );
            let saved = store
                .library()
                .unwrap()
                .into_iter()
                .find(|entry| entry.track.source_uri == uri(9))
                .unwrap();
            assert!(saved.is_candidate(), "{kind}: fresh saved music wins");
            assert_eq!(saved.origin, fsonos_core::store::LibraryOrigin::LikedTrack);
            assert_eq!(saved.track.title, "Fresh saved metadata");
        });
    }
}

#[test]
fn malformed_or_unconfirmed_reads_do_not_authorize_retiring_cached_tracks() {
    for kind in ["top", "playlist", "changed playlist"] {
        let server = Server::new();
        let scope = source(&server, if kind == "top" { "top" } else { "playlist" });
        let mut malformed = track(1);
        malformed["restrictions"] = json!("invalid restriction shape");
        let rows = vec![denial_rows(false).remove(0), malformed];
        match kind {
            "top" => server.set("/v1/me/top/tracks", page(rows, 0, None)),
            "playlist" => server.set(
                "/v1/playlists/TestList0/items",
                page(
                    rows.into_iter()
                        .map(|item| json!({ "item": item }))
                        .collect(),
                    0,
                    None,
                ),
            ),
            "changed playlist" => {
                server.state.lock().unwrap().replace_page_on_request = Some((
                    "/v1/playlists/TestList0/items".into(),
                    "/v1/playlists/TestList0".into(),
                    json!({ "id": "TestList0", "snapshot_id": "changed-version" }),
                ));
            }
            _ => unreachable!("known test source"),
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, scope);
            let mut store = retained_store();
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            let pool = pool_from_store(&store).unwrap();
            for number in [0, 1, 2, 3, 9] {
                assert!(
                    pool.get(&uri(number)).is_some(),
                    "{kind}: retained {number}"
                );
            }
        });
    }
}

#[test]
fn malformed_availability_evidence_retains_cached_tracks() {
    for kind in [
        "top",
        "playlist",
        "artist catalog",
        "playlist malformed fields",
    ] {
        let server = Server::new();
        let source_kind = if kind == "playlist malformed fields" {
            "playlist"
        } else {
            kind
        };
        let scope = source(&server, source_kind);
        let mut rows = denial_rows(kind == "artist catalog");
        rows[0]["restrictions"] = json!({ "reason": 42 });
        if kind == "artist catalog" {
            rows[1]["restrictions"] = json!({ "reason": 42 });
        } else {
            rows[1]["album"]["restrictions"] = json!({ "reason": 42 });
        }
        rows[2]["restrictions"] = json!({ "reason": 42 });
        if kind == "playlist" {
            rows[0] = json!({ "uri": uri(0), "restrictions": { "reason": 42 } });
            rows[1] = json!({
                "uri": uri(1), "album": { "restrictions": { "reason": 42 } }
            });
        } else if kind == "playlist malformed fields" {
            rows[0] = json!({
                "uri": uri(0), "is_playable": "false", "restrictions": { "reason": "market" }
            });
            rows[1] = json!({
                "uri": uri(1), "is_playable": false, "album": { "restrictions": "invalid" }
            });
            rows[2] = json!({
                "uri": uri(2), "is_playable": false, "restrictions": 42
            });
        }
        match source_kind {
            "top" => server.set("/v1/me/top/tracks", page(rows, 0, None)),
            "playlist" => server.set(
                "/v1/playlists/TestList0/items",
                page(
                    rows.into_iter()
                        .map(|item| json!({ "item": item }))
                        .collect(),
                    0,
                    None,
                ),
            ),
            "artist catalog" => {
                server.set("/v1/albums/AvailabilityAlbum/tracks", page(rows, 0, None));
            }
            _ => unreachable!("known test source"),
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, scope);
            let mut store = retained_store();
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            let pool = pool_from_store(&store).unwrap();
            for number in [0, 1, 2, 3, 9] {
                assert!(
                    pool.get(&uri(number)).is_some(),
                    "{kind}: malformed evidence cannot retire {number}"
                );
            }
            assert!(
                pool.get(&uri(5)).is_some(),
                "{kind}: the read still succeeds"
            );
        });
    }
}

#[test]
fn availability_evidence_overflow_does_not_publish_a_partial_library() {
    let server = Server::new();
    let scope = source(&server, "top");
    // Keep each unavailable object small: the response exercises the identity
    // bound independently of metadata size, transport failures or paging.
    let rows = (0..=crate::taste::MAX_UNAVAILABLE_TRACKS)
        .map(|number| {
            json!({
                "uri": uri(number), "name": "Unavailable",
                "album": { "name": "Unavailable" },
                "explicit": false, "is_playable": false
            })
        })
        .collect();
    server.set("/v1/me/top/tracks", page(rows, 0, None));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, scope);
        let mut store = retained_store();
        let prior_heard = vec![(uri(3), Utc::now().timestamp() - 60)];
        store.save_spotify_heard(&prior_heard).unwrap();
        let prior_library = store.library().unwrap();
        let prior_owner = store.spotify_library_owner().unwrap();
        let error = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, crate::SpotifyError::Decode(reason)
                if reason.contains("unavailable-track evidence exceeded")),
            "{error}"
        );
        assert_eq!(store.library().unwrap(), prior_library);
        assert_eq!(store.spotify_heard().unwrap(), prior_heard);
        assert_eq!(store.spotify_library_owner().unwrap(), prior_owner);
    });
}
