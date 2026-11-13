//! Finite artist discovery through real HTTP, durable receipts and reopened SQLite.

use super::*;
use crate::cache::{ArtistContinuation, LibrarySyncDetailed, continue_artist_samples};
use fsonos_core::store::{LibraryOrigin, SpotifyArtistDiscoveryFence};

async fn pending(
    server: &Server,
    cx: &Cx,
    store: &mut dyn Store,
    previous: Option<&ArtistContinuation>,
) -> LibrarySyncDetailed {
    let mut session = server.session(cx, SCOPE);
    continue_artist_samples(&mut session, cx, store, previous)
        .await
        .unwrap()
}

fn personal_reads(server: &Server) -> [usize; 4] {
    [
        server.calls("/v1/me/albums").len(),
        server.calls("/v1/me/tracks").len(),
        server.calls("/v1/me/following").len(),
        server.calls("/v1/me/top/").len(),
    ]
}

#[test]
fn a_full_read_then_reopened_background_passes_finish_120_artists_and_go_idle() {
    let server = Server::new();
    populate(&server, 120);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("artist-background.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(first.library.candidates, 50);
        let mut state = first.artist_samples;
        assert_eq!(
            (state.completed, state.pending, state.attempted),
            (50, 70, 50)
        );
        let cycle = state.cycle.clone();
        let personal = personal_reads(&server);
        store.close().unwrap();

        for (attempts, completed, left) in [(50, 100, 20), (20, 120, 0)] {
            let mut store = SqliteStore::open(&file).unwrap();
            let before = samples(&server).len();
            let result = pending(&server, &cx, &mut store, Some(&state)).await;
            assert_eq!(result.library.candidates, completed);
            assert_eq!(result.library.retired, 0);
            assert_eq!(
                (
                    result.artist_samples.attempted,
                    result.artist_samples.completed,
                    result.artist_samples.pending,
                ),
                (attempts, completed, left),
            );
            assert_eq!(result.artist_samples.cycle, cycle);
            assert_eq!(samples(&server).len() - before, attempts);
            assert_eq!(personal_reads(&server), personal);
            assert!(!result.artist_samples.requires_refresh);
            state = serde_json::from_str(&serde_json::to_string(&result.artist_samples).unwrap())
                .unwrap();
            store.close().unwrap();
        }

        let mut store = SqliteStore::open(&file).unwrap();
        let calls = server.calls("/v1/").len();
        let accepted = receipt(&store);
        let idle = pending(&server, &cx, &mut store, Some(&state)).await;
        assert_eq!(
            (idle.artist_samples.completed, idle.artist_samples.pending),
            (120, 0)
        );
        assert_eq!(idle.artist_samples.attempted, 0);
        assert_eq!(server.calls("/v1/").len(), calls);
        assert_eq!(receipt(&store), accepted);
        let entries = store.library().unwrap();
        assert_eq!(
            entries.iter().filter(|entry| entry.is_candidate()).count(),
            120
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.taste_weight == Some(crate::taste::ARTIST_PM))
        );
        store.close().unwrap();
    });
}

#[test]
fn an_unavailable_artist_yields_until_later_artists_finish_and_remains_retryable() {
    let server = Server::new();
    populate(&server, 120);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = SqliteStore::open(&server.data.join("artist-failures.db")).unwrap();
        let first = full(&server, &cx, &mut store).await;
        let route = format!("/v1/artists/{}/top-tracks", id(50));
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(route.clone(), 503);
        let refused = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
        assert_eq!(
            (
                refused.artist_samples.completed,
                refused.artist_samples.pending
            ),
            (50, 70)
        );
        assert_eq!(refused.artist_samples.attempted, 1);
        assert_eq!(cursor(&store), id(50));

        let later = pending(&server, &cx, &mut store, Some(&refused.artist_samples)).await;
        assert_eq!(
            (later.artist_samples.completed, later.artist_samples.pending),
            (100, 20)
        );
        let almost = pending(&server, &cx, &mut store, Some(&later.artist_samples)).await;
        assert_eq!(
            (
                almost.artist_samples.completed,
                almost.artist_samples.pending
            ),
            (119, 1)
        );
        let unchanged = store.library().unwrap();
        let stalled = pending(&server, &cx, &mut store, Some(&almost.artist_samples)).await;
        assert_eq!(
            (
                stalled.artist_samples.completed,
                stalled.artist_samples.pending
            ),
            (119, 1)
        );
        assert_eq!(stalled.artist_samples.cycle, first.artist_samples.cycle);
        assert_eq!(store.library().unwrap(), unchanged);
        server.state.lock().unwrap().refused.remove(&route);
        let done = pending(&server, &cx, &mut store, Some(&stalled.artist_samples)).await;
        assert_eq!(
            (done.artist_samples.completed, done.artist_samples.pending),
            (120, 0)
        );
        store.close().unwrap();
    });
}

#[test]
fn an_empty_artist_sample_keeps_previous_music_and_does_not_starve_its_neighbor() {
    let server = Server::new();
    populate(&server, 52);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = SqliteStore::open(&server.data.join("artist-empty.db")).unwrap();
        let first = full(&server, &cx, &mut store).await;
        let mut prior = store.library().unwrap()[0].clone();
        prior.track.source_uri = "spotify:track:PreviousArtistSample".into();
        prior.artist_id = Some(id(50));
        store.upsert_library(std::slice::from_ref(&prior)).unwrap();
        server.set(
            &format!("/v1/artists/{}/top-tracks", id(50)),
            json!({ "tracks": [] }),
        );
        let next = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
        assert_eq!(
            (next.artist_samples.completed, next.artist_samples.pending),
            (51, 1)
        );
        assert_eq!(next.artist_samples.attempted, 2);
        assert_eq!(
            store
                .library()
                .unwrap()
                .iter()
                .find(|entry| entry.track.source_uri == prior.track.source_uri),
            Some(&prior),
        );
        let again = pending(&server, &cx, &mut store, Some(&next.artist_samples)).await;
        assert_eq!(
            (again.artist_samples.completed, again.artist_samples.pending),
            (51, 1)
        );
        assert_eq!(again.artist_samples.attempted, 1);
        store.close().unwrap();
    });
}

#[test]
fn a_long_rate_limit_retains_unattempted_and_unpublished_samples_without_extra_reads() {
    let server = Server::new();
    populate(&server, 120);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = SqliteStore::open(&server.data.join("artist-cooldown.db")).unwrap();
        let first = full(&server, &cx, &mut store).await;
        let before = receipt(&store);
        let library = store.library().unwrap();
        let route = format!("/v1/artists/{}/top-tracks", id(52));
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(route.clone(), 429);
        let mut session = server.session(&cx, SCOPE);
        let limited =
            continue_artist_samples(&mut session, &cx, &mut store, Some(&first.artist_samples))
                .await
                .unwrap();
        assert_eq!(limited.artist_samples.attempted, 3);
        assert_eq!(
            (
                limited.artist_samples.completed,
                limited.artist_samples.pending
            ),
            (50, 70)
        );
        assert!(limited.artist_samples.retry_after_secs > 0);
        assert_eq!(receipt(&store), before);
        assert_eq!(store.library().unwrap(), library);
        let calls = server.calls("/v1/");
        assert_eq!(path(calls.last().unwrap()), route);
        let waiting =
            continue_artist_samples(&mut session, &cx, &mut store, Some(&limited.artist_samples))
                .await
                .unwrap();
        assert_eq!(waiting.artist_samples.attempted, 0);
        assert_eq!(waiting.artist_samples.pending, 70);
        assert!(waiting.artist_samples.retry_after_secs > 0);
        assert_eq!(server.calls("/v1/"), calls);
        assert_eq!(receipt(&store), before);
        store.close().unwrap();
    });
}

#[test]
fn an_account_change_during_sampling_cannot_publish_into_a_returning_account() {
    for return_to_owner in [false, true] {
        let server = Server::new();
        populate(&server, 60);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("artist-account-change.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let first = full(&server, &cx, &mut store).await;
            let other_file = file.clone();
            server.state.lock().unwrap().before_response = Some((
                format!("/v1/artists/{}/top-tracks", id(50)),
                Box::new(move || {
                    let mut other = SqliteStore::open(&other_file).unwrap();
                    other.adopt_spotify_library_owner("another-owner").unwrap();
                    if return_to_owner {
                        other.adopt_spotify_library_owner(OWNER).unwrap();
                    }
                    other.close().unwrap();
                }),
            ));
            let result = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
            assert!(result.artist_samples.requires_refresh);
            assert!(
                store
                    .library()
                    .unwrap()
                    .iter()
                    .all(|entry| !entry.is_candidate())
            );
            let owner = store.spotify_library_owner().unwrap().unwrap();
            assert_ne!(Some(owner.generation), first.artist_samples.generation);
            assert_eq!(store.spotify_artist_discovery(&owner).unwrap(), None);
            let calls = server.calls("/v1/").len();
            let stale = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
            assert!(stale.artist_samples.requires_refresh);
            assert_eq!(server.calls("/v1/").len(), calls);
            store.close().unwrap();
        });
    }
}

#[test]
fn the_closing_identity_read_rejects_a_different_spotify_account_without_store_adoption() {
    let server = Server::new();
    populate(&server, 60);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = SqliteStore::open(&server.data.join("artist-closing-identity.db")).unwrap();
        let first = full(&server, &cx, &mut store).await;
        let accepted = receipt(&store);
        let library = store.library().unwrap();
        server.state.lock().unwrap().replace_page_on_request = Some((
            format!("/v1/artists/{}/top-tracks", id(50)),
            "/v1/me".into(),
            json!({ "id": "another-owner" }),
        ));
        let result = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
        assert!(result.artist_samples.requires_refresh);
        assert_eq!(receipt(&store), accepted);
        assert_eq!(store.library().unwrap(), library);
        assert_eq!(
            store.spotify_library_owner().unwrap().unwrap().owner_id,
            OWNER
        );
        store.close().unwrap();
    });
}

#[test]
fn a_competing_receipt_keeps_the_backlog_and_candidates_until_a_fresh_pass() {
    let server = Server::new();
    populate(&server, 60);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("artist-competing-reader.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let first = full(&server, &cx, &mut store).await;
        let accepted = receipt(&store);
        let library = store.library().unwrap();
        let other_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            format!("/v1/artists/{}/top-tracks", id(50)),
            Box::new(move || {
                let mut other = SqliteStore::open(&other_file).unwrap();
                let owner = other.spotify_library_owner().unwrap().unwrap();
                let raw = receipt(&other);
                let mut body: Value = serde_json::from_str(&raw).unwrap();
                body["publication"] = json!("newer-artist-reader");
                assert!(
                    other
                        .publish_spotify_artist_continuation(
                            &owner,
                            &SpotifyArtistDiscoveryFence {
                                expected: Some(raw),
                                progress: body.to_string(),
                            },
                            &[],
                        )
                        .unwrap()
                );
                other.close().unwrap();
            }),
        ));
        let lost = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
        assert!(lost.artist_samples.requires_refresh);
        assert_ne!(receipt(&store), accepted);
        assert_eq!(store.library().unwrap(), library);
        let winner = pending(&server, &cx, &mut store, None).await;
        assert_eq!(
            (
                winner.artist_samples.completed,
                winner.artist_samples.pending
            ),
            (60, 0)
        );
        assert_eq!(winner.library.candidates, 60);
        store.close().unwrap();
    });
}

#[test]
fn legacy_cursors_wait_for_a_full_refresh_without_guessing_a_backlog() {
    let server = Server::new();
    populate(&server, 60);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = SqliteStore::open(&server.data.join("artist-legacy.db")).unwrap();
        let first = full(&server, &cx, &mut store).await;
        let owner = store.spotify_library_owner().unwrap().unwrap();
        let before = receipt(&store);
        assert!(
            store
                .publish_spotify_artist_continuation(
                    &owner,
                    &SpotifyArtistDiscoveryFence {
                        expected: Some(before),
                        progress: json!({
                            "version": 1, "last_artist_id": id(49), "publication": "legacy-cursor",
                        })
                        .to_string(),
                    },
                    &[],
                )
                .unwrap()
        );
        let calls = server.calls("/v1/").len();
        let legacy = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
        assert!(legacy.artist_samples.requires_refresh);
        assert_eq!(server.calls("/v1/").len(), calls);
        let refreshed = full(&server, &cx, &mut store).await;
        assert!(!refreshed.artist_samples.requires_refresh);
        assert_eq!(
            (
                refreshed.artist_samples.completed,
                refreshed.artist_samples.pending,
            ),
            (50, 10)
        );
        let finished = pending(&server, &cx, &mut store, Some(&refreshed.artist_samples)).await;
        assert_eq!(
            (
                finished.artist_samples.completed,
                finished.artist_samples.pending
            ),
            (60, 0)
        );
        store.close().unwrap();
    });
}

fn catalog_song(number: usize, artist: usize) -> Value {
    let mut value = track(number);
    value["artists"] = json!([{ "id": id(artist), "name": format!("Performer {artist}") }]);
    value
}

fn catalog(server: &Server, artist: usize) {
    let album_id = format!("PendingAlbum{artist}");
    server.set(
        &format!("/v1/artists/{}/albums", id(artist)),
        page(
            vec![json!({
                "id": album_id,
                "uri": format!("spotify:album:PendingAlbum{artist}"),
                "name": "A catalog sample",
                "album_type": "album",
                "artists": [{ "id": id(artist), "name": format!("Performer {artist}") }],
                "release_date": "1994",
                "total_tracks": 4,
            })],
            0,
            None,
        ),
    );
    server.set(
        &format!("/v1/albums/PendingAlbum{artist}/tracks"),
        page(
            (0..4)
                .map(|number| catalog_song(artist * 10 + number, artist))
                .collect(),
            0,
            None,
        ),
    );
}

#[test]
fn background_samples_reuse_the_bounded_catalog_fallback_and_keep_three_tracks_per_artist() {
    let server = Server::new();
    populate(&server, 52);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store =
            SqliteStore::open(&server.data.join("artist-background-catalog.db")).unwrap();
        let first = full(&server, &cx, &mut store).await;
        for artist in [50, 51] {
            catalog(&server, artist);
        }
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(format!("/v1/artists/{}/top-tracks", id(50)), 403);
        let before = server.calls("/v1/artists/").len();
        let personal = personal_reads(&server);
        let result = pending(&server, &cx, &mut store, Some(&first.artist_samples)).await;
        assert_eq!(
            (
                result.artist_samples.completed,
                result.artist_samples.pending
            ),
            (52, 0)
        );
        assert_eq!(result.artist_samples.attempted, 2);
        assert_eq!(result.library.candidates, 56);
        let calls = server.calls("/v1/artists/");
        let calls = &calls[before..];
        assert_eq!(
            calls
                .iter()
                .filter(|call| path(call).ends_with("/top-tracks"))
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| path(call).ends_with("/albums"))
                .count(),
            2
        );
        assert_eq!(server.calls("/v1/albums/").len(), 2);
        assert_eq!(personal_reads(&server), personal);
        store.close().unwrap();
    });
}

#[test]
fn captured_top_and_followed_weights_handle_guest_credits_without_rewriting_saved_metadata() {
    let server = Server::new();
    populate(&server, 52);
    server.set(
        "/v1/me/top/artists",
        page(
            vec![json!({ "id": id(51), "name": "Performer 51", "genres": ["folk"] })],
            0,
            None,
        ),
    );
    server.set("/v1/me/top/tracks", page(Vec::new(), 0, None));
    let mut guest = catalog_song(5000, 51);
    guest["artists"] = json!([
        { "id": id(51), "name": "Performer 51" },
        { "id": id(50), "name": "Performer 50" },
    ]);
    server.set(
        &format!("/v1/artists/{}/top-tracks", id(50)),
        json!({ "tracks": [guest.clone()] }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let scope = "user-follow-read user-top-read";
        let mut store =
            SqliteStore::open(&server.data.join("artist-background-weights.db")).unwrap();
        let mut session = server.session(&cx, scope);
        let first = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(
            (first.artist_samples.completed, first.artist_samples.pending),
            (50, 2)
        );
        let mut saved = store.library().unwrap()[0].clone();
        saved.track.source_uri = guest["uri"].as_str().unwrap().to_owned();
        saved.artist_id = Some(id(51));
        saved.track.artist = Some("Owner saved metadata".into());
        saved.origin = LibraryOrigin::LikedTrack;
        saved.added = 123;
        saved.genres = vec!["owner genre".into()];
        saved.taste_weight = Some(crate::taste::MAX_TASTE_PM);
        store.upsert_library(std::slice::from_ref(&saved)).unwrap();
        let result =
            continue_artist_samples(&mut session, &cx, &mut store, Some(&first.artist_samples))
                .await
                .unwrap();
        assert_eq!(
            (
                result.artist_samples.completed,
                result.artist_samples.pending
            ),
            (52, 0)
        );
        assert_eq!(result.artist_samples.attempted, 2);
        let entries = store.library().unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.track.source_uri == saved.track.source_uri),
            Some(&saved)
        );
        let own_uri = song(51)["uri"].as_str().unwrap().to_owned();
        let own = entries
            .iter()
            .find(|entry| entry.track.source_uri == own_uri)
            .unwrap();
        assert_eq!(own.origin, LibraryOrigin::Taste);
        assert_eq!(
            own.taste_weight,
            Some(crate::taste::ARTIST_PM * crate::taste::ARTIST_PM / 1000)
        );
        assert_eq!(own.artist_id.as_deref(), Some(id(51).as_str()));
        let calls = server.calls("/v1/").len();
        let idle =
            continue_artist_samples(&mut session, &cx, &mut store, Some(&result.artist_samples))
                .await
                .unwrap();
        assert_eq!(idle.artist_samples.pending, 0);
        assert_eq!(server.calls("/v1/").len(), calls);
        store.close().unwrap();
    });
}
