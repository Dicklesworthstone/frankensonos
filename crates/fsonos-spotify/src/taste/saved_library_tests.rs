//! Saved-library progress survives sessions and SQLite reopen without ever
//! publishing a prefix as a complete replacement of accepted music.

use super::*;
use crate::SpotifyError;
use crate::cache::{LibrarySyncDetailed, sync_library_pass};
use fsonos_core::store::SqliteStore;

const OWNER: &str = "test-owner";

fn held(number: usize) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:PreviouslySaved{number}"),
        title: format!("Previously saved song {number}"),
        artists: vec!["Test performer".into()],
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

fn liked_page(position: u32, total: u32, number: usize) -> Value {
    json!({
        "items": [{ "track": track(number) }],
        "offset": position, "total": total, "next": null
    })
}

fn liked_library(server: &Server, total: u32) {
    for position in 0..total {
        server.set(
            &format!("/v1/me/tracks?offset={position}"),
            liked_page(position, total, position as usize),
        );
    }
}

fn count_offset(server: &Server, offset: &str) -> usize {
    server.calls("/v1/me/tracks").iter()
        .filter(|call| parameter(call, "offset").is_ok_and(|value| value == offset))
        .count()
}

async fn pass<S: Store + ?Sized>(
    server: &Server,
    cx: &Cx,
    store: &mut S,
    budget: usize,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    // A fresh session models the daemon's next admitted attempt after restart.
    let mut session = server.session(cx, "");
    sync_library_pass(&mut session, cx, store, budget).await
}

async fn finish<S: Store + ?Sized>(
    server: &Server,
    cx: &Cx,
    store: &mut S,
) -> LibrarySyncDetailed {
    for _ in 0..30 {
        match pass(server, cx, store, 4).await {
            Ok(done) => return done,
            Err(SpotifyError::LibraryPending { .. }) => {}
            Err(error) => panic!("saved-library continuation failed: {error}"),
        }
    }
    panic!("saved-library continuation did not finish");
}

#[test]
fn bounded_liked_library_finishes_across_sqlite_and_session_restarts() {
    for adopted in [false, true] {
        let server = Server::new();
        liked_library(&server, 7);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("saved-library-restart.db");
            let mut store = SqliteStore::open(&file).unwrap();
            if adopted {
                store.adopt_spotify_library_owner(OWNER).unwrap();
            }
            let original_owner = store.spotify_library_owner().unwrap();
            apply_library_read(&mut store, &[held(0)]).unwrap();
            let accepted = store.library().unwrap();
            let mut completed = None;
            let mut previous_pages = 0;
            let mut previous_checked = 0;
            let mut pending = 0;
            for _ in 0..20 {
                match pass(&server, &cx, &mut store, 4).await {
                    Ok(done) => {
                        completed = Some(done);
                        break;
                    }
                    Err(SpotifyError::LibraryPending { pages, tracks }) => {
                        pending += 1;
                        assert!(pages >= previous_pages);
                        assert!(tracks < 7 || pages == 8);
                        previous_pages = pages;
                        assert_eq!(store.library().unwrap(), accepted);
                        assert_eq!(store.spotify_library_owner().unwrap(), original_owner);
                        let encoded = store.spotify_library_progress(OWNER, original_owner.as_ref())
                            .unwrap().unwrap();
                        let snapshot: Value = serde_json::from_str(&encoded).unwrap();
                        let checked = snapshot["checked"].as_u64().unwrap();
                        assert!(checked >= previous_checked);
                        previous_checked = checked;
                        store.close().unwrap();
                        store = SqliteStore::open(&file).unwrap();
                    }
                    Err(error) => panic!("unexpected saved-library error: {error}"),
                }
            }
            let done = completed.expect("bounded windows eventually finish");
            assert!(pending >= 3, "the fixture crossed both intake and verification windows");
            assert_eq!((done.library.tracks, done.library.retired), (7, 1));
            let owner = store.spotify_library_owner().unwrap().unwrap();
            assert_eq!(owner.owner_id, OWNER);
            assert!(store.spotify_library_progress(OWNER, Some(&owner)).unwrap().is_none());
            let pool = pool_from_store(&store).unwrap();
            assert_eq!(pool.len(), 7);
            for number in 0..7 {
                assert!(pool.get(&format!("spotify:track:TestTrack{number}")).is_some());
            }
            assert!(pool.get(&held(0).source_uri).is_none());
            assert_eq!(
                count_offset(&server, "1"), 2,
                "accepted middle pages are fetched once and verified once, not restarted each pass"
            );
            store.close().unwrap();
        });
    }
}

#[test]
fn a_late_malformed_page_keeps_validated_prefix_and_the_previous_public_library() {
    let server = Server::new();
    liked_library(&server, 6);
    let mut malformed = liked_page(3, 6, 3);
    malformed["items"][0]["track"].as_object_mut().unwrap().remove("explicit");
    server.set("/v1/me/tracks?offset=3", malformed);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("saved-library-decode.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(0)]).unwrap();
        let accepted = store.library().unwrap();
        let error = pass(&server, &cx, &mut store, 512).await.unwrap_err();
        assert!(matches!(error, SpotifyError::Decode(_)));
        let progress = store.spotify_library_progress(OWNER, Some(&owner)).unwrap().unwrap();
        let snapshot: Value = serde_json::from_str(&progress).unwrap();
        assert_eq!(snapshot["read"]["pages"], 4);
        assert_eq!(snapshot["read"]["items"].as_array().unwrap().len(), 3);
        assert_eq!(store.library().unwrap(), accepted);
        store.close().unwrap();

        server.set("/v1/me/tracks?offset=3", liked_page(3, 6, 3));
        let mut store = SqliteStore::open(&file).unwrap();
        assert!(matches!(pass(&server, &cx, &mut store, 4).await,
            Err(SpotifyError::LibraryPending { pages: 6, tracks: 5 })));
        assert_eq!(store.library().unwrap(), accepted);
        let done = finish(&server, &cx, &mut store).await;
        assert_eq!((done.library.tracks, done.library.retired), (6, 1));
        assert_eq!(count_offset(&server, "1"), 2);
        store.close().unwrap();
    });
}

fn saved_album(total: u32) -> Value {
    json!({
        "items": [{ "added_at": "2025-01-15T20:31:02Z", "album": {
            "id": "SavedRecording", "uri": "spotify:album:SavedRecording",
            "name": "Saved recording", "artists": [{ "name": "Album artist" }],
            "release_date": "1982", "label": "Test label", "total_tracks": total
        }}], "offset": 0, "total": 1, "next": null
    })
}

fn album_page(position: u32, total: u32) -> Value {
    let mut item = track(position as usize);
    item["track_number"] = json!(position + 1);
    item["disc_number"] = json!(1);
    item["explicit"] = json!(position == 1);
    json!({ "items": [item], "offset": position, "total": total, "next": null })
}

#[test]
fn rate_limited_album_hydration_resumes_with_save_and_explicit_metadata() {
    let server = Server::new();
    server.set("/v1/me/albums", saved_album(2));
    let album = "/v1/albums/SavedRecording/tracks";
    server.set(&format!("{album}?offset=0"), album_page(0, 2));
    server.set(&format!("{album}?offset=1"), album_page(1, 2));
    server.state.lock().unwrap().refused.insert(album.into(), 429);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("saved-album-rate-limit.db");
        let mut store = SqliteStore::open(&file).unwrap();
        apply_library_read(&mut store, &[held(0)]).unwrap();
        let accepted = store.library().unwrap();
        let mut session = server.session(&cx, "");
        let error = sync_library_pass(&mut session, &cx, &mut store, 512).await.unwrap_err();
        assert!(matches!(error, SpotifyError::Api { status: 429, .. }));
        assert!(session.retry_after_secs() > 0);
        assert_eq!(store.library().unwrap(), accepted);
        assert!(store.spotify_library_owner().unwrap().is_none());
        let progress = store.spotify_library_progress(OWNER, None).unwrap().unwrap();
        let snapshot: Value = serde_json::from_str(&progress).unwrap();
        assert_eq!(snapshot["read"]["pages"], 2);
        store.close().unwrap();

        server.state.lock().unwrap().refused.remove(album);
        let mut store = SqliteStore::open(&file).unwrap();
        let done = finish(&server, &cx, &mut store).await;
        assert_eq!((done.library.tracks, done.library.candidates, done.library.retired), (2, 1, 1));
        let rows = store.library().unwrap();
        let last = rows.iter().find(|row| row.track.source_uri == "spotify:track:TestTrack1").unwrap();
        assert!(last.explicit);
        assert_eq!(last.added, 1_736_973_062);
        assert_eq!(last.release_year, Some(1982));
        assert_eq!((last.disc_number, last.track_number), (Some(1), Some(2)));
        assert_eq!(last.track.album.as_deref(), Some("Saved recording"));
        assert_eq!(last.album_artists.as_deref(), Some("Album artist"));
        store.close().unwrap();
    });
}

#[test]
fn changed_root_counts_or_music_restart_only_private_progress() {
    for change_total in [false, true] {
        let server = Server::new();
        liked_library(&server, 5);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(0)]).unwrap();
            let accepted = store.library().unwrap();
            assert!(matches!(pass(&server, &cx, &mut store, 4).await,
                Err(SpotifyError::LibraryPending { pages: 4, tracks: 3 })));
            let total = if change_total { 6 } else { 5 };
            liked_library(&server, total);
            server.set("/v1/me/tracks?offset=0", liked_page(0, total, 99));
            assert!(matches!(pass(&server, &cx, &mut store, 4).await,
                Err(SpotifyError::LibraryPending { pages: 2, tracks: 1 })));
            assert_eq!(store.library().unwrap(), accepted);
            assert_eq!(store.spotify_library_owner().unwrap(), Some(owner));
            let done = finish(&server, &cx, &mut store).await;
            assert_eq!(done.library.tracks, total as usize);
            let pool = pool_from_store(&store).unwrap();
            assert!(pool.get("spotify:track:TestTrack99").is_some());
            assert!(pool.get("spotify:track:TestTrack0").is_none());
        });
    }
}

#[test]
fn unchanged_roots_cannot_publish_a_changed_middle_page_from_an_older_pass() {
    let server = Server::new();
    liked_library(&server, 5);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(0)]).unwrap();
        let accepted = store.library().unwrap();
        assert!(matches!(pass(&server, &cx, &mut store, 4).await,
            Err(SpotifyError::LibraryPending { pages: 4, tracks: 3 })));
        server.set("/v1/me/tracks?offset=1", liked_page(1, 5, 99));
        // Intake finishes, then verification gets past the stable root.
        for _ in 0..2 {
            assert!(matches!(pass(&server, &cx, &mut store, 4).await,
                Err(SpotifyError::LibraryPending { .. })));
            assert_eq!(store.library().unwrap(), accepted);
        }
        assert!(matches!(pass(&server, &cx, &mut store, 4).await,
            Err(SpotifyError::LibraryPending { pages: 1, tracks: 0 })));
        assert_eq!(store.library().unwrap(), accepted);
        let done = finish(&server, &cx, &mut store).await;
        assert_eq!((done.library.tracks, done.library.retired), (5, 1));
        let pool = pool_from_store(&store).unwrap();
        assert!(pool.get("spotify:track:TestTrack99").is_some());
        assert!(pool.get("spotify:track:TestTrack1").is_none());
    });
}

#[test]
fn an_account_switch_during_essential_http_cannot_adopt_or_publish_stale_work() {
    for return_to_same_owner in [false, true] {
        let server = Server::new();
        liked_library(&server, 2);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("saved-library-owner-race.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let (initial, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(0)]).unwrap();
            let target = file.clone();
            server.state.lock().unwrap().before_response = Some((
                "/v1/me/tracks".into(),
                Box::new(move || {
                    let mut other = SqliteStore::open(&target).unwrap();
                    other.adopt_spotify_library_owner("other-test-owner").unwrap();
                    if return_to_same_owner {
                        other.adopt_spotify_library_owner(OWNER).unwrap();
                    }
                    apply_library_read(&mut other, &[held(9)]).unwrap();
                    other.close().unwrap();
                }),
            ));
            assert!(matches!(pass(&server, &cx, &mut store, 512).await,
                Err(SpotifyError::Store(_))));
            let current = store.spotify_library_owner().unwrap().unwrap();
            assert_ne!(current.generation, initial.generation);
            assert_eq!(current.owner_id, if return_to_same_owner { OWNER } else { "other-test-owner" });
            let pool = pool_from_store(&store).unwrap();
            assert_eq!(pool.len(), 1);
            assert!(pool.get(&held(9).source_uri).is_some());
            assert!(store.spotify_library_progress(OWNER, Some(&initial)).unwrap().is_none());
            store.close().unwrap();
        });
    }
}

#[test]
fn same_owner_completed_read_is_fenced_if_another_reader_claims_during_enrichment() {
    let server = Server::new();
    liked_library(&server, 1);
    server.set("/v1/me/top/tracks", page(Vec::new(), 0, None));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("saved-library-reader-race.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(0)]).unwrap();
        let accepted = store.library().unwrap();
        let target = file.clone();
        server.state.lock().unwrap().before_response = Some((
            "/v1/me/top/tracks".into(),
            Box::new(move || {
                let mut other = SqliteStore::open(&target).unwrap();
                let owner = other.spotify_library_owner().unwrap().unwrap();
                let expected = other.spotify_library_progress(OWNER, Some(&owner)).unwrap().unwrap();
                let mut claimed: Value = serde_json::from_str(&expected).unwrap();
                claimed["nonce"] = json!("another-active-reader");
                assert!(other.save_spotify_library_progress(
                    OWNER, Some(&owner), Some(&expected), &claimed.to_string()
                ).unwrap());
                other.close().unwrap();
            }),
        ));
        let mut session = server.session(&cx, "user-top-read");
        assert!(matches!(sync_library_pass(&mut session, &cx, &mut store, 512).await,
            Err(SpotifyError::Store(_))));
        assert_eq!(store.library().unwrap(), accepted);
        let progress = store.spotify_library_progress(OWNER, Some(&owner)).unwrap().unwrap();
        let claimed: Value = serde_json::from_str(&progress).unwrap();
        assert_eq!(claimed["nonce"], "another-active-reader");
        let done = finish(&server, &cx, &mut store).await;
        assert_eq!((done.library.tracks, done.library.retired), (1, 1));
        assert!(store.spotify_library_progress(OWNER, Some(&owner)).unwrap().is_none());
        store.close().unwrap();
    });
}

#[test]
fn an_undersized_pass_budget_fails_before_creating_or_changing_progress() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        for budget in 0..3 {
            assert!(matches!(pass(&server, &cx, &mut store, budget).await,
                Err(SpotifyError::Decode(_))));
            assert!(store.spotify_library_progress(OWNER, None).unwrap().is_none());
        }
        assert!(server.calls("/v1/me/albums").is_empty());
        assert!(server.calls("/v1/me/tracks").is_empty());
    });
}

#[test]
fn a_new_account_keeps_the_old_owner_and_personal_caches_until_complete() {
    let server = Server::new();
    liked_library(&server, 5);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let (old, _) = store.adopt_spotify_library_owner("previous-test-owner").unwrap();
        apply_library_read(&mut store, &[held(0)]).unwrap();
        let old_library = store.library().unwrap();
        let playlists = vec![("spotify:playlist:OldList".into(), "Old playlist".into())];
        let heard = vec![(held(0).source_uri, 123)];
        store.save_playlists(&playlists).unwrap();
        store.save_spotify_heard(&heard).unwrap();
        assert!(matches!(pass(&server, &cx, &mut store, 4).await,
            Err(SpotifyError::LibraryPending { .. })));
        assert_eq!(store.spotify_library_owner().unwrap(), Some(old.clone()));
        assert_eq!(store.library().unwrap(), old_library);
        assert_eq!(store.playlists().unwrap(), playlists);
        assert_eq!(store.spotify_heard().unwrap(), heard);
        assert!(store.spotify_library_progress(OWNER, Some(&old)).unwrap().is_some());
        let done = finish(&server, &cx, &mut store).await;
        assert_eq!((done.library.tracks, done.library.retired), (5, 1));
        let current = store.spotify_library_owner().unwrap().unwrap();
        assert_eq!(current.owner_id, OWNER);
        assert_ne!(current.generation, old.generation);
        assert!(store.playlists().unwrap().is_empty());
        assert!(store.spotify_heard().unwrap().is_empty());
    });
}
