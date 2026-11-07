//! Accepted artist discovery through real HTTP and reopened SQLite caches.

use super::*;
use crate::cache::sync_library_detailed;
use fsonos_core::store::SqliteStore;

const OWNER: &str = "test-owner";
const SCOPE: &str = "user-follow-read";

fn id(number: usize) -> String {
    format!("RotationArtist{number:04}")
}

fn song(number: usize) -> Value {
    let mut song = track(number);
    song["artists"] = json!([{ "id": id(number), "name": format!("Performer {number}") }]);
    song
}

fn populate(server: &Server, total: usize) {
    for start in (0..total.max(1)).step_by(50) {
        let end = (start + 50).min(total);
        let route = if start == 0 {
            "/v1/me/following".into()
        } else {
            format!(
                "/v1/me/following?type=artist&limit=50&after={}",
                id(start - 1)
            )
        };
        let next = (end < total).then(|| {
            format!(
                "{API}/me/following?type=artist&limit=50&after={}",
                id(end - 1)
            )
        });
        server.set(
            &route,
            json!({ "artists": {
                "items": (start..end).map(|number| json!({
                    "id": id(number), "name": format!("Performer {number}"), "genres": ["folk"]
                })).collect::<Vec<_>>(),
                "total": total, "next": next, "limit": 50,
                "cursors": { "after": (end > 0).then(|| id(end - 1)) }
            }}),
        );
    }
    for number in 0..total {
        server.set(
            &format!("/v1/artists/{}/top-tracks", id(number)),
            json!({ "tracks": [song(number)] }),
        );
    }
}

async fn full(
    server: &Server,
    cx: &Cx,
    store: &mut dyn Store,
) -> crate::cache::LibrarySyncDetailed {
    let mut session = server.session(cx, SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

fn receipt(store: &dyn Store) -> String {
    let owner = store.spotify_library_owner().unwrap().unwrap();
    store.spotify_artist_discovery(&owner).unwrap().unwrap()
}

fn cursor(store: &dyn Store) -> String {
    let value: Value = serde_json::from_str(&receipt(store)).unwrap();
    value["last_artist_id"].as_str().unwrap().to_owned()
}

fn samples(server: &Server) -> Vec<String> {
    server
        .calls("/v1/artists/")
        .into_iter()
        .filter(|call| path(call).ends_with("/top-tracks"))
        .collect()
}

#[test]
fn repeated_full_syncs_resume_after_restart_and_keep_other_artists_music() {
    let server = Server::new();
    populate(&server, 120);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("artist-rotation.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(first.library.candidates, 50);
        assert_eq!(cursor(&store), id(49));
        assert!(
            first
                .notes
                .iter()
                .any(|note| note.contains("70 artist samples") && note.contains("dj sync again"))
        );
        let accepted = receipt(&store);
        store.close().unwrap();

        // Reopen immediately, without advancing any clock. The next accepted
        // refresh samples the next fifty, retaining the first fifty by ID.
        let mut store = SqliteStore::open(&file).unwrap();
        let second = full(&server, &cx, &mut store).await;
        assert_eq!(
            (second.library.candidates, second.library.retired),
            (100, 0)
        );
        assert_eq!(cursor(&store), id(99));
        assert_ne!(receipt(&store), accepted);
        let calls = samples(&server);
        assert_eq!(calls.len(), 100);
        assert!(calls[50].contains(&format!("/{}/", id(50))));

        let third = full(&server, &cx, &mut store).await;
        assert_eq!((third.library.candidates, third.library.retired), (120, 0));
        assert_eq!(cursor(&store), id(29));
        let calls = samples(&server);
        assert_eq!(calls.len(), 150);
        assert!(calls[100].contains(&format!("/{}/", id(100))));
        assert!(calls[120].contains(&format!("/{}/", id(0))));
        store.close().unwrap();
    });
}

#[test]
fn losing_full_publication_does_not_advance_the_artist_cursor_or_publish_samples() {
    let server = Server::new();
    populate(&server, 60);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("artist-publication-loss.db");
        let mut store = SqliteStore::open(&file).unwrap();
        full(&server, &cx, &mut store).await;
        let accepted = receipt(&store);
        let before = store.library().unwrap();
        let other_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            format!("/v1/artists/{}/top-tracks", id(50)),
            Box::new(move || {
                let mut other = SqliteStore::open(&other_file).unwrap();
                let owner = other.spotify_library_owner().unwrap().unwrap();
                let old = other
                    .spotify_library_progress(OWNER, Some(&owner))
                    .unwrap()
                    .unwrap();
                let mut newer: Value = serde_json::from_str(&old).unwrap();
                newer["nonce"] = json!("a-newer-full-reader");
                assert!(
                    other
                        .save_spotify_library_progress(
                            OWNER,
                            Some(&owner),
                            Some(&old),
                            &newer.to_string()
                        )
                        .unwrap()
                );
                other.close().unwrap();
            }),
        ));
        let mut session = server.session(&cx, SCOPE);
        assert!(
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .is_err()
        );
        assert_eq!(receipt(&store), accepted);
        assert_eq!(store.library().unwrap(), before);
        let calls = samples(&server).len();
        full(&server, &cx, &mut store).await;
        assert!(samples(&server)[calls].contains(&format!("/{}/", id(50))));
        assert_eq!(
            store
                .library()
                .unwrap()
                .iter()
                .filter(|entry| entry.is_candidate())
                .count(),
            60
        );
        store.close().unwrap();
    });
}

#[test]
fn an_endpoint_failure_yields_its_turn_on_the_next_accepted_refresh() {
    let server = Server::new();
    populate(&server, 60);
    server
        .state
        .lock()
        .unwrap()
        .refused
        .insert(format!("/v1/artists/{}/top-tracks", id(0)), 503);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(cursor(&store), id(0));
        assert_eq!(samples(&server).len(), 1);
        assert!(
            first
                .notes
                .iter()
                .any(|note| note.contains("60 artist samples"))
        );
        let second = full(&server, &cx, &mut store).await;
        assert_eq!(second.library.candidates, 50);
        assert_eq!(cursor(&store), id(50));
        assert!(samples(&server)[1].contains(&format!("/{}/", id(1))));
    });
}

#[test]
fn identical_artist_names_do_not_hide_a_followed_artist_with_a_different_id() {
    let server = Server::new();
    populate(&server, 1);
    server.set(
        "/v1/me/following",
        json!({ "artists": {
            "items": [{ "id": id(0), "name": "Shared Name", "genres": ["folk"] }], "next": null
        }}),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, SCOPE);
        let saved = [LibraryItem {
            artists: vec!["Shared Name".into()],
            artist_id: Some("DifferentArtist".into()),
            ..LibraryItem::default()
        }];
        let (taste, read) =
            read_taste(&mut session, &cx, &mut MemStore::default(), &saved, 0).await;
        assert_eq!(
            (
                read.artist_reads,
                read.artist_reads_pending,
                read.artist_tracks
            ),
            (1, 0, 1)
        );
        assert_eq!(taste.tracks[0].artist_id.as_deref(), Some(id(0).as_str()));
    });
    assert_eq!(samples(&server).len(), 1);
}

#[test]
fn a_failed_refresh_keeps_a_renamed_artists_previous_music_by_id() {
    let server = Server::new();
    populate(&server, 1);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        full(&server, &cx, &mut store).await;
        let before = store.library().unwrap();
        server.set("/v1/me/following", json!({ "artists": {
            "items": [{ "id": id(0), "name": "New Profile Name", "genres": ["folk"] }], "next": null
        }}));
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(format!("/v1/artists/{}/top-tracks", id(0)), 503);
        let result = full(&server, &cx, &mut store).await;
        assert_eq!(result.library.retired, 0);
        assert_eq!(store.library().unwrap(), before);
    });
}

#[test]
fn changing_accounts_resets_discovery_without_inheriting_the_previous_cursor() {
    let server = Server::new();
    populate(&server, 60);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        full(&server, &cx, &mut store).await;
        let first = store.spotify_library_owner().unwrap().unwrap();
        assert_eq!(cursor(&store), id(49));
        server.set("/v1/me", json!({ "id": "second-owner" }));
        full(&server, &cx, &mut store).await;
        let second = store.spotify_library_owner().unwrap().unwrap();
        assert_ne!(first, second);
        assert!(store.spotify_artist_discovery(&first).unwrap().is_none());
        assert_eq!(cursor(&store), id(49));
        assert!(samples(&server)[50].contains(&format!("/{}/", id(0))));
    });
}

#[test]
fn a_rate_limit_advances_only_the_attempted_artist_and_carries_its_cooldown() {
    let server = Server::new();
    populate(&server, 60);
    server
        .state
        .lock()
        .unwrap()
        .refused
        .insert(format!("/v1/artists/{}/top-tracks", id(0)), 429);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(samples(&server).len(), 1);
        assert_eq!(cursor(&store), id(0));
        assert!(first.playlists.retry_after_secs > 0);
        assert!(
            first
                .notes
                .iter()
                .any(|note| note.contains("60 artist samples"))
        );
        // A fresh Session represents the next scheduler admission after the
        // shared deadline, not permission for the old Session to ignore it.
        server.state.lock().unwrap().refused.clear();
        let second = full(&server, &cx, &mut store).await;
        assert_eq!(second.library.candidates, 50);
        assert!(samples(&server)[1].contains(&format!("/{}/", id(1))));
        assert_eq!(cursor(&store), id(50));
    });
}

#[test]
fn empty_or_unplayable_top_samples_retain_accepted_music_and_continue_rotation() {
    for unplayable in [false, true] {
        let server = Server::new();
        populate(&server, 60);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            full(&server, &cx, &mut store).await;
            let accepted = store
                .library()
                .unwrap()
                .into_iter()
                .find(|entry| entry.track.source_uri == "spotify:track:TestTrack1")
                .unwrap();
            let mut unavailable = song(1);
            unavailable["is_playable"] = false.into();
            let tracks = if unplayable {
                vec![unavailable]
            } else {
                Vec::new()
            };
            server.set(
                &format!("/v1/artists/{}/top-tracks", id(1)),
                json!({ "tracks": tracks }),
            );
            let second = full(&server, &cx, &mut store).await;
            assert_eq!((second.library.candidates, second.library.retired), (60, 0));
            assert_eq!(
                store
                    .library()
                    .unwrap()
                    .into_iter()
                    .find(|entry| entry.track.source_uri == "spotify:track:TestTrack1")
                    .unwrap(),
                accepted
            );
            assert!(
                second
                    .notes
                    .iter()
                    .any(|note| note.contains("no playable tracks in the artist top-track sample"))
            );
            assert_eq!(
                samples(&server).len(),
                100,
                "an empty artist must not stop the window"
            );
            assert_eq!(cursor(&store), id(39));
            full(&server, &cx, &mut store).await;
            assert!(samples(&server)[100].contains(&format!("/{}/", id(40))));
        });
    }
}
