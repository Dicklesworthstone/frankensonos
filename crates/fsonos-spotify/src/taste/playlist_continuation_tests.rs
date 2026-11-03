//! Playlist-only continuation through real HTTP and reopened SQLite caches.

use super::*;
use crate::cache::{
    LibrarySyncDetailed, continue_playlists, sync_library_detailed, works_from_store,
};
use fsonos_core::store::{CachedPlaylist, MAX_PLAYLIST_ENTRIES, SqliteStore};

const OWNER: &str = "test-owner";
const ID: &str = "BackgroundLong";
const URI: &str = "spotify:playlist:BackgroundLong";

fn populate(server: &Server, snapshot: &str, total: usize, first: usize) {
    server.set(
        "/v1/me/playlists",
        page(
            vec![json!({
                "id": ID, "name": "Jazz evening", "owner": { "id": OWNER },
                "snapshot_id": snapshot
            })],
            0,
            None,
        ),
    );
    server.set(
        &format!("/v1/playlists/{ID}"),
        json!({ "id": ID, "snapshot_id": snapshot }),
    );
    for offset in (0..total).step_by(50) {
        let end = (offset + 50).min(total);
        server.set(&format!("/v1/playlists/{ID}/items?offset={offset}"), json!({
            "items": (offset..end).map(|n| json!({ "item": track(first + n) })).collect::<Vec<_>>(),
            "offset": offset, "total": total,
            "next": (end < total).then(|| format!("{API}/playlists/{ID}/items?offset={end}"))
        }));
    }
}

async fn full(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

async fn pending(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    continue_playlists(&mut session, cx, store, OWNER)
        .await
        .unwrap()
}

fn contents(store: &dyn Store) -> CachedPlaylist {
    store.playlist_contents(URI).unwrap().unwrap()
}

#[test]
fn background_windows_complete_after_restart_without_saved_library_or_artist_requests() {
    let server = Server::new();
    populate(&server, "stable", 405, 0);
    server.set(
        "/v1/me/tracks",
        page(
            vec![json!({
                "added_at": "2026-10-10T00:00:00Z", "track": track(900)
            })],
            0,
            None,
        ),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("background-playlists.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(first.playlists.owner.as_deref(), Some(OWNER));
        assert_eq!(
            (first.playlists.pending, first.playlists.advanced),
            (1, 150)
        );
        let mut saved = store
            .library()
            .unwrap()
            .into_iter()
            .find(|row| row.track.source_uri == "spotify:track:TestTrack900")
            .unwrap();
        saved.taste_weight = Some(1111);
        saved.genres = vec!["jazz".into()];
        store.upsert_library(&[saved.clone()]).unwrap();
        store.close().unwrap();

        let calls = server.calls("/v1/").len();
        for (examined, remaining) in [(300, 1), (405, 0)] {
            let mut store = SqliteStore::open(&file).unwrap();
            let before = server.calls("/v1/playlists/").len();
            let result = pending(&server, &cx, &mut store).await;
            assert_eq!(result.playlists.pending, remaining);
            assert!(result.playlists.advanced > 0);
            assert!(server.calls("/v1/playlists/").len() - before <= 4);
            assert_eq!(contents(&store).examined, examined);
            let kept = store
                .library()
                .unwrap()
                .into_iter()
                .find(|row| row.track.source_uri == saved.track.source_uri)
                .unwrap();
            assert_eq!(
                (kept.origin, kept.taste_weight, kept.genres),
                (saved.origin, Some(1111), saved.genres.clone())
            );
            store.close().unwrap();
        }
        let mut store = SqliteStore::open(&file).unwrap();
        let complete = contents(&store);
        assert!(complete.complete);
        assert_eq!(complete.tracks.len(), 405);
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack404")
                .is_some()
        );
        let item_reads = server.item_calls().len();
        let done = pending(&server, &cx, &mut store).await;
        assert_eq!(done.playlists.pending, 0);
        assert_eq!(
            server.item_calls().len(),
            item_reads,
            "complete snapshots are not restarted"
        );
        assert_eq!(
            contents(&store),
            complete,
            "skipped contents retain their revision"
        );
        assert!(
            server.calls("/v1/")[calls..].iter().all(|call| {
                matches!(path(call), "/v1/me" | "/v1/me/playlists")
                    || path(call).starts_with("/v1/playlists/")
            }),
            "continuation must not reread likes, albums, top items or artists"
        );
        store.close().unwrap();
    });
}

fn directory(server: &Server, ids: &[usize]) {
    for (index, chunk) in ids.chunks(50).enumerate() {
        let offset = index * 50;
        let next = offset + chunk.len();
        server.set(
            &format!("/v1/me/playlists?offset={offset}"),
            json!({
                "items": chunk.iter().map(|n| json!({
                    "id": format!("Background{n:03}"), "name": format!("List {n}"),
                    "owner": { "id": OWNER }, "snapshot_id": "test-version-1"
                })).collect::<Vec<_>>(),
                "offset": offset, "total": ids.len(),
                "next": (next < ids.len()).then(|| format!("{API}/me/playlists?offset={next}"))
            }),
        );
        // The first endpoint's URL also includes offset=0.
    }
    for n in ids {
        server.set(
            &format!("/v1/playlists/Background{n:03}/items"),
            page(vec![json!({ "item": track(*n) })], 0, None),
        );
    }
}

#[test]
fn continuation_gives_later_lists_their_turn_and_skips_completed_lists_after_reordering() {
    let server = Server::new();
    directory(&server, &(0..60).collect::<Vec<_>>());
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        assert_eq!(full(&server, &cx, &mut store).await.playlists.pending, 35);
        assert_eq!(server.item_calls().len(), 25);
        // Remove the cursor's ID, reverse the directory, and add a later ID.
        directory(
            &server,
            &(0..60)
                .filter(|&n| n != 24)
                .chain([99])
                .rev()
                .collect::<Vec<_>>(),
        );
        let second = pending(&server, &cx, &mut store).await;
        assert_eq!(second.playlists.pending, 11);
        assert_eq!(server.item_calls().len(), 50);
        let third = pending(&server, &cx, &mut store).await;
        assert_eq!(third.playlists.pending, 0);
        assert_eq!(server.item_calls().len(), 61);
        let calls = server.item_calls();
        assert!(path(&calls[25]).contains("Background025"));
        assert!(path(calls.last().unwrap()).contains("Background099"));
        let expected = store
            .playlist_contents("spotify:playlist:Background000")
            .unwrap();
        pending(&server, &cx, &mut store).await;
        assert_eq!(server.item_calls().len(), 61);
        assert_eq!(
            store
                .playlist_contents("spotify:playlist:Background000")
                .unwrap(),
            expected
        );
    });
}

#[test]
fn background_replacement_keeps_last_complete_contents_and_restarts_changed_snapshots() {
    let server = Server::new();
    populate(&server, "original", 3, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        full(&server, &cx, &mut store).await;
        let original = contents(&store);
        populate(&server, "first-edit", 350, 1_000);
        full(&server, &cx, &mut store).await;
        pending(&server, &cx, &mut store).await;
        assert_eq!(contents(&store), original);
        populate(&server, "second-edit", 350, 2_000);
        let restarted = pending(&server, &cx, &mut store).await;
        assert_eq!(restarted.playlists.advanced, 150);
        assert_eq!(contents(&store), original);
        pending(&server, &cx, &mut store).await;
        let finished = pending(&server, &cx, &mut store).await;
        assert_eq!(finished.playlists.pending, 0);
        let replaced = contents(&store);
        assert!(replaced.complete);
        assert_eq!(replaced.tracks.len(), 350);
        assert_eq!(replaced.tracks[0].source_uri, "spotify:track:TestTrack2000");
        assert_eq!(
            replaced.tracks[349].source_uri,
            "spotify:track:TestTrack2349"
        );
    });
}

#[test]
fn a_server_cooldown_survives_the_continuation_result_without_accepting_unproved_pages() {
    let server = Server::new();
    populate(&server, "stable", 405, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        full(&server, &cx, &mut store).await;
        let original = contents(&store);
        let checkpoint = store.playlist_progress(OWNER, URI).unwrap();
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(format!("/v1/playlists/{ID}/items"), 429);
        let before = server.item_calls().len();
        let limited = pending(&server, &cx, &mut store).await;
        assert_eq!(limited.playlists.pending, 1);
        assert_eq!(limited.playlists.advanced, 0);
        assert!((3500..=3600).contains(&limited.playlists.retry_after_secs));
        assert_eq!(server.item_calls().len() - before, 1);
        assert_eq!(store.playlist_progress(OWNER, URI).unwrap(), checkpoint);
        assert_eq!(contents(&store), original);
    });
}

#[test]
fn changed_accounts_cannot_merge_into_the_prior_full_library() {
    let server = Server::new();
    populate(&server, "stable", 405, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        full(&server, &cx, &mut store).await;
        let library = store.library().unwrap();
        let progress = store.playlist_progress(OWNER, URI).unwrap();
        let observed = contents(&store);
        let reads = server.item_calls().len();
        server.set("/v1/me", json!({ "id": "another-owner" }));
        let changed = pending(&server, &cx, &mut store).await;
        assert!(changed.playlists.requires_refresh);
        assert_eq!(changed.playlists.owner.as_deref(), Some("another-owner"));
        assert_eq!(server.item_calls().len(), reads);
        assert_eq!(store.library().unwrap(), library);
        assert_eq!(store.playlist_progress(OWNER, URI).unwrap(), progress);
        assert_eq!(contents(&store), observed);
    });
}

#[test]
fn unchanged_entry_and_metadata_caps_are_terminal_for_background_reads() {
    let server = Server::new();
    populate(&server, "stable", 3, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        full(&server, &cx, &mut store).await;
        let observed = contents(&store);
        for metadata in [false, true] {
            store.save_playlist_progress(OWNER, URI, &json!({
                "version": 1, "owner_id": OWNER, "playlist_uri": URI, "snapshot_id": "stable",
                "examined": if metadata { 150 } else { MAX_PLAYLIST_ENTRIES },
                "source_total": MAX_PLAYLIST_ENTRIES + 1, "complete": false,
                "applied": true, "limited": metadata, "tracks": []
            }).to_string()).unwrap();
            let reads = server.calls("/v1/playlists/").len();
            let capped = pending(&server, &cx, &mut store).await;
            assert_eq!((capped.playlists.pending, capped.playlists.limited), (0, 1));
            assert_eq!(server.calls("/v1/playlists/").len(), reads);
            assert_eq!(contents(&store), observed);
        }
    });
}

#[test]
fn proved_terminal_checkpoints_recover_failed_cache_projection_without_rereading_items() {
    let server = Server::new();
    populate(&server, "stable", 3, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut source = MemStore::default();
        full(&server, &cx, &mut source).await;
        let original = contents(&source);
        let mut proof: Value =
            serde_json::from_str(&source.playlist_progress(OWNER, URI).unwrap().unwrap()).unwrap();
        for capped in [false, true] {
            // Simulate a crash after a fenced checkpoint was durable, before
            // either the public observation or candidate library was written.
            proof["applied"] = json!(false);
            proof["complete"] = json!(!capped);
            proof["examined"] = json!(if capped { MAX_PLAYLIST_ENTRIES } else { 3 });
            proof["source_total"] = json!(if capped { MAX_PLAYLIST_ENTRIES + 1 } else { 3 });
            let mut restarted = MemStore::default();
            restarted
                .save_playlist_progress(OWNER, URI, &proof.to_string())
                .unwrap();
            let requests = server.calls("/v1/playlists/").len();
            let recovered = pending(&server, &cx, &mut restarted).await;
            assert_eq!(server.calls("/v1/playlists/").len(), requests);
            assert_eq!(recovered.playlists.pending, 0);
            assert_eq!(recovered.playlists.limited, usize::from(capped));
            let observed = contents(&restarted);
            assert_eq!(observed.fetched_at, original.fetched_at);
            assert_eq!(observed.tracks, original.tracks);
            assert_eq!(observed.complete, !capped);
            assert_eq!(
                observed.examined,
                if capped { MAX_PLAYLIST_ENTRIES } else { 3 }
            );
            assert_eq!(pool_from_store(&restarted).unwrap().tracks().len(), 3);
            let saved: Value =
                serde_json::from_str(&restarted.playlist_progress(OWNER, URI).unwrap().unwrap())
                    .unwrap();
            assert_eq!(saved["applied"], true);
            pending(&server, &cx, &mut restarted).await;
            assert_eq!(server.calls("/v1/playlists/").len(), requests);
            assert_eq!(contents(&restarted), observed);
        }
    });
}

const WORK_ALBUM: &str = "spotify:album:LatePlaylistSuite";
const WORK_TRACK: &str = "spotify:track:LateMovement2";
const WORK_ROUTE: &str = "/v1/albums/LatePlaylistSuite/tracks";

fn movement(number: u32) -> Value {
    let name = match number {
        1 => "I. Allegro",
        2 => "II. Adagio",
        _ => "III. Presto",
    };
    json!({
        "id": format!("LateMovement{number}"),
        "uri": format!("spotify:track:LateMovement{number}"),
        "name": format!("Piano Sonata No. 14 in C-Sharp Minor, Op. 27 No. 2: {name}"),
        "artists": [{ "name": "Ludwig van Beethoven" }, { "name": "Test pianist" }],
        "album": {
            "name": "Test sonata recording", "uri": WORK_ALBUM,
            "artists": [{ "name": "Ludwig van Beethoven" }]
        },
        "duration_ms": 180_000, "is_playable": true,
        "disc_number": 1, "track_number": number
    })
}

fn late_work(server: &Server) {
    populate(server, "classical-snapshot", 151, 0);
    server.set(
        &format!("/v1/playlists/{ID}/items?offset=150"),
        json!({ "items": [{ "item": movement(2) }], "offset": 150, "total": 151,
            "next": null }),
    );
    server.set(
        WORK_ROUTE,
        json!({ "items": (1..=3).map(movement).collect::<Vec<_>>(),
            "offset": 0, "total": 3, "next": null }),
    );
}

#[test]
fn a_late_playlist_movement_reaches_the_dj_as_a_whole_work_after_restart() {
    let server = Server::new();
    late_work(&server);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("playlist-works.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(first.playlists.pending, 1);
        assert!(server.calls(WORK_ROUTE).is_empty());
        assert!(
            works_from_store(&store)
                .unwrap()
                .work_of(WORK_TRACK)
                .is_none()
        );
        store.close().unwrap();

        let before = server.calls("/v1/").len();
        let mut store = SqliteStore::open(&file).unwrap();
        let continued = pending(&server, &cx, &mut store).await;
        assert_eq!(continued.playlists.pending, 0);
        assert_eq!(continued.library.tracks, 151);
        assert_eq!(server.calls(WORK_ROUTE).len(), 1);
        assert!(contents(&store).complete);
        assert_eq!(
            contents(&store).tracks.last().unwrap().source_uri,
            WORK_TRACK
        );
        store.close().unwrap();

        // This is the same cache-only pool construction used by the daemon.
        let mut store = SqliteStore::open(&file).unwrap();
        let pool = works_from_store(&store).unwrap();
        let work = pool.work_of(WORK_TRACK).unwrap();
        let uris: Vec<_> = work
            .movements
            .iter()
            .map(|movement| movement.track.source_uri.as_str())
            .collect();
        assert_eq!(
            uris,
            [
                "spotify:track:LateMovement1",
                WORK_TRACK,
                "spotify:track:LateMovement3"
            ]
        );
        assert_eq!(work.completeness, crate::works::Completeness::Complete);
        assert_eq!(
            work.movements.iter().filter(|track| track.expanded).count(),
            2
        );
        assert_eq!(
            store
                .album_tracks(WORK_ALBUM)
                .unwrap()
                .unwrap()
                .tracks
                .len(),
            3
        );
        assert_eq!(
            store.library().unwrap().len(),
            151,
            "expanded movements are not invented library saves"
        );
        assert!(
            server.calls("/v1/")[before..].iter().all(|call| {
                matches!(path(call), "/v1/me" | "/v1/me/playlists" | WORK_ROUTE)
                    || path(call).starts_with("/v1/playlists/")
            }),
            "continuation reads only its playlists and the newly discovered work's album"
        );
        let reads = server.calls("/v1/").len();
        pending(&server, &cx, &mut store).await;
        assert!(
            server.calls("/v1/")[reads..]
                .iter()
                .all(|call| { matches!(path(call), "/v1/me" | "/v1/me/playlists") }),
            "completed intake does not repeat album or item requests"
        );
        store.close().unwrap();
    });
}

#[test]
fn a_failed_late_album_keeps_accepted_playlist_music_and_reports_the_missing_work() {
    for status in [404, 429] {
        let server = Server::new();
        late_work(&server);
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(WORK_ROUTE.into(), status);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            full(&server, &cx, &mut store).await;
            let continued = pending(&server, &cx, &mut store).await;
            assert_eq!(continued.playlists.pending, 0);
            assert_eq!(continued.library.tracks, 151);
            assert!(contents(&store).complete);
            let pool = works_from_store(&store).unwrap();
            assert_eq!(pool.work_of(WORK_TRACK).unwrap().movements.len(), 1);
            assert!(store.album_tracks(WORK_ALBUM).unwrap().is_none());
            assert_eq!(server.calls(WORK_ROUTE).len(), 1);
            if status == 404 {
                assert!(
                    continued
                        .notes
                        .iter()
                        .any(|note| note.contains("1 album couldn't be read"))
                );
                assert_eq!(store.album_misses().unwrap().len(), 1);
            } else {
                assert!(
                    continued
                        .notes
                        .iter()
                        .any(|note| note.contains("album read stopped early"))
                );
                assert!(continued.playlists.retry_after_secs > 0);
                assert!(store.album_misses().unwrap().is_empty());
            }
        });
    }
}
