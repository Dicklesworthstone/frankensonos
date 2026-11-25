//! Availability travels with accepted playlist windows through publication.

use super::*;
use crate::taste::read_pending_playlists;
use fsonos_core::store::SpotifyPlaylistPublication;

fn denied_last_page(server: &Server, malformed: bool) {
    let mut denied = track(0);
    denied["is_playable"] = if malformed {
        json!("false")
    } else {
        json!(false)
    };
    let mut entries = vec![json!({ "item": denied })];
    entries.extend((151..160).map(|number| json!({ "item": track(number) })));
    server.set(
        &format!("/v1/playlists/{ID}/items?offset=150"),
        json!({ "items": entries, "offset": 150, "total": 160, "next": null }),
    );
}

#[test]
fn playlist_denials_survive_interrupted_publication_without_replaying_after_acknowledgement() {
    for interrupted in [false, true] {
        let server = Server::new();
        populate(&server, "stable", 160, 0);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("playlist-background-availability.db");
            let mut store = SqliteStore::open(&file).unwrap();
            full(&server, &cx, &mut store).await;
            let uri = "spotify:track:TestTrack0";
            assert!(pool_from_store(&store).unwrap().get(uri).is_some());
            let revision = store.library_catalog_revision().unwrap();
            denied_last_page(&server, false);
            if interrupted {
                let owner = store.spotify_library_owner().unwrap().unwrap();
                let mut session = server.session(&cx, PLAYLIST_SCOPE);
                let (taste, read) = read_pending_playlists(
                    &mut session,
                    &cx,
                    &mut store,
                    &owner,
                    chrono::Utc::now().timestamp(),
                )
                .await;
                assert_eq!(
                    read.unavailable,
                    std::collections::HashSet::from([uri.into()])
                );
                assert_eq!(
                    taste
                        .pending_playlist_denials(store.library_catalog_revision().unwrap())
                        .unwrap(),
                    [uri]
                );
                assert_eq!(store.library_catalog_revision().unwrap(), revision);
                assert!(pool_from_store(&store).unwrap().get(uri).is_some());
                assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
                // End the process after the private proof is saved, before
                // the candidate/denial/acknowledgement transaction starts.
                store.close().unwrap();
                store = SqliteStore::open(&file).unwrap();
            }
            let item_reads = server.item_calls().len();
            let result = pending(&server, &cx, &mut store).await;
            assert_eq!(result.playlists.pending, 0);
            if interrupted {
                assert_eq!(
                    server.item_calls().len(),
                    item_reads,
                    "reuse the retained complete proof"
                );
            }
            assert_eq!(
                store.spotify_unavailable_tracks().unwrap(),
                std::collections::HashSet::from([uri.into()])
            );
            assert_ne!(store.library_catalog_revision().unwrap(), revision);
            assert!(pool_from_store(&store).unwrap().get(uri).is_none());
            assert!(
                pool_from_store(&store)
                    .unwrap()
                    .get("spotify:track:TestTrack151")
                    .is_some()
            );
            let proof: Value =
                serde_json::from_str(&store.playlist_progress(OWNER, URI).unwrap().unwrap())
                    .unwrap();
            assert_eq!(proof["applied"], true);
            assert!(
                proof.get("unavailable").is_none(),
                "consumed evidence is not replayable"
            );
            store.close().unwrap();

            let mut store = SqliteStore::open(&file).unwrap();
            assert!(pool_from_store(&store).unwrap().get(uri).is_none());
            // Fresh saved music in a full refresh can clear the old denial.
            // Reusing an acknowledged playlist must not apply it again.
            server.set(
                "/v1/me/tracks",
                page(vec![json!({ "track": track(0) })], 0, None),
            );
            full(&server, &cx, &mut store).await;
            assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
            pending(&server, &cx, &mut store).await;
            assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
            assert!(pool_from_store(&store).unwrap().get(uri).is_some());
            store.close().unwrap();
        });
    }
}

#[test]
fn playlist_denials_cannot_outlive_their_exact_publication_proof() {
    for changed_account in [false, true] {
        let server = Server::new();
        populate(&server, "stable", 160, 0);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("playlist-denial-proof.db");
            let mut store = SqliteStore::open(&file).unwrap();
            full(&server, &cx, &mut store).await;
            denied_last_page(&server, false);
            let owner = store.spotify_library_owner().unwrap().unwrap();
            let mut session = server.session(&cx, PLAYLIST_SCOPE);
            let (taste, _) = read_pending_playlists(
                &mut session,
                &cx,
                &mut store,
                &owner,
                chrono::Utc::now().timestamp(),
            )
            .await;
            let unavailable_revision = store.library_catalog_revision().unwrap();
            let unavailable = taste
                .pending_playlist_denials(unavailable_revision)
                .unwrap();
            assert_eq!(unavailable, ["spotify:track:TestTrack0"]);
            let (fence, _) = crate::taste::prepare_playlist_publication(
                &owner,
                taste.directory.as_ref(),
                None,
                &taste.playlist_contents,
            )
            .unwrap();
            let publication = SpotifyPlaylistPublication {
                expected_directory: fence.expected_directory,
                directory: fence.directory,
                observations: fence.observations,
                unavailable,
                unavailable_revision,
                ..SpotifyPlaylistPublication::default()
            };
            if changed_account {
                store.adopt_spotify_library_owner("another-owner").unwrap();
                store.adopt_spotify_library_owner(OWNER).unwrap();
            } else {
                let previous = store.playlist_progress(OWNER, URI).unwrap().unwrap();
                let mut newer: Value = serde_json::from_str(&previous).unwrap();
                newer["fetched_at"] = json!(newer["fetched_at"].as_i64().unwrap() + 1);
                assert!(
                    store
                        .save_spotify_playlist_progress(
                            &owner,
                            URI,
                            Some(&previous),
                            &newer.to_string(),
                        )
                        .unwrap()
                );
            }
            let revision = store.library_catalog_revision().unwrap();
            assert!(
                !store
                    .publish_spotify_playlist_continuation(&owner, &publication)
                    .unwrap()
            );
            assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
            assert_eq!(store.library_catalog_revision().unwrap(), revision);
            if !changed_account {
                assert!(
                    pool_from_store(&store)
                        .unwrap()
                        .get("spotify:track:TestTrack0")
                        .is_some()
                );
            }
            store.close().unwrap();
        });
    }
}

#[test]
fn malformed_or_changed_playlist_windows_cannot_publish_denials() {
    for malformed in [false, true] {
        let server = Server::new();
        populate(&server, "stable", 160, 0);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            full(&server, &cx, &mut store).await;
            denied_last_page(&server, malformed);
            if !malformed {
                server.state.lock().unwrap().replace_page_on_request = Some((
                    format!("/v1/playlists/{ID}/items"),
                    format!("/v1/playlists/{ID}"),
                    json!({ "id": ID, "snapshot_id": "changed-during-read" }),
                ));
            }
            let result = pending(&server, &cx, &mut store).await;
            assert!(!result.notes.is_empty());
            assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
            assert!(
                pool_from_store(&store)
                    .unwrap()
                    .get("spotify:track:TestTrack0")
                    .is_some()
            );
        });
    }
}

#[test]
fn a_newer_full_positive_forces_unpublished_playlist_denials_to_be_read_again() {
    let server = Server::new();
    populate(&server, "stable", 160, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("playlist-denial-superseded.db");
        let mut store = SqliteStore::open(&file).unwrap();
        full(&server, &cx, &mut store).await;
        denied_last_page(&server, false);
        let owner = store.spotify_library_owner().unwrap().unwrap();
        let revision = store.library_catalog_revision().unwrap();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let (taste, _) = read_pending_playlists(
            &mut session,
            &cx,
            &mut store,
            &owner,
            chrono::Utc::now().timestamp(),
        )
        .await;
        assert_eq!(
            taste.pending_playlist_denials(revision).unwrap(),
            ["spotify:track:TestTrack0"]
        );
        let pending_proof = store.playlist_progress(OWNER, URI).unwrap().unwrap();

        // The newer full read sees fresh positive saved music, but cannot
        // observe the playlist and therefore leaves its old private proof.
        server.set(
            "/v1/me/tracks",
            page(vec![json!({ "track": track(0) })], 0, None),
        );
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert("/v1/me/playlists".into(), 503);
        full(&server, &cx, &mut store).await;
        assert_eq!(
            store.playlist_progress(OWNER, URI).unwrap().as_deref(),
            Some(pending_proof.as_str())
        );
        assert_ne!(store.library_catalog_revision().unwrap(), revision);
        assert!(
            taste
                .pending_playlist_denials(store.library_catalog_revision().unwrap())
                .is_err(),
            "a full refresh cannot consume old pending negative evidence either"
        );
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        server
            .state
            .lock()
            .unwrap()
            .refused
            .remove("/v1/me/playlists");
        populate(&server, "stable", 160, 0);
        let item_reads = server.item_calls().len();
        let result = pending(&server, &cx, &mut store).await;
        assert_eq!(result.playlists.pending, 1);
        let calls = server.item_calls();
        assert!(
            calls[item_reads..]
                .iter()
                .any(|call| call.contains("offset=0")),
            "superseded negative proof must restart provider observation"
        );
        assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack0")
                .is_some()
        );
        let result = pending(&server, &cx, &mut store).await;
        assert_eq!(result.playlists.pending, 0);
        assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
        store.close().unwrap();
    });
}

#[test]
fn playlist_denials_are_bound_to_the_catalog_before_the_provider_read() {
    let server = Server::new();
    populate(&server, "stable", 160, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("playlist-denial-revision-race.db");
        let mut store = SqliteStore::open(&file).unwrap();
        full(&server, &cx, &mut store).await;
        denied_last_page(&server, false);
        let other_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            format!("/v1/playlists/{ID}/items"),
            Box::new(move || {
                let mut other = SqliteStore::open(&other_file).unwrap();
                let mut current = other.library().unwrap()[0].clone();
                current.track.title = "Newer accepted catalog metadata".into();
                other.upsert_library(&[current]).unwrap();
                other.close().unwrap();
            }),
        ));
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        assert!(
            continue_playlists(&mut session, &cx, &mut store, OWNER)
                .await
                .is_err()
        );
        assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack0")
                .is_some()
        );
        // Retained progress is retryable, not a permanently rejected proof.
        populate(&server, "stable", 160, 0);
        pending(&server, &cx, &mut store).await;
        assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
        store.close().unwrap();
    });
}
