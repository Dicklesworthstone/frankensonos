//! Complete snapshot proofs reconcile bounded playlist reads into current taste.

use super::*;
use crate::cache::{LibrarySyncDetailed, continue_playlists, sync_library_detailed};
use crate::taste::PLAYLIST_PM;
use fsonos_core::store::{CachedPlaylist, SqliteStore};

const OWNER: &str = "test-owner";
const LONG: &str = "ReconcileLong";

fn id(n: usize) -> String {
    format!("Reconcile{n:03}")
}

fn uri(n: usize) -> String {
    format!("spotify:track:TestTrack{n}")
}

fn item_route(n: usize) -> String {
    format!("/v1/playlists/{}/items", id(n))
}

fn directory(server: &Server, ids: &[usize]) {
    server.set(
        "/v1/me/playlists",
        page(
            ids.iter()
                .map(|n| {
                    json!({
                        "id": id(*n), "name": format!("Playlist {n}"),
                        "owner": { "id": OWNER }, "snapshot_id": "test-version-1"
                    })
                })
                .collect(),
            0,
            None,
        ),
    );
}

fn setup_lists(server: &Server) {
    directory(server, &(0..30).collect::<Vec<_>>());
    for n in 0..30 {
        server.set(
            &item_route(n),
            page(vec![json!({ "item": track(n) })], 0, None),
        );
    }
}

fn directory_version(server: &Server, n: usize, version: Option<&str>) {
    {
        let mut state = server.state.lock().unwrap();
        let rows = state.pages.get_mut("/v1/me/playlists").unwrap()["items"]
            .as_array_mut()
            .unwrap();
        let row = rows.iter_mut().find(|row| row["id"] == id(n)).unwrap();
        row["snapshot_id"] = json!(version);
    }
    if let Some(version) = version {
        server.set(
            &format!("/v1/playlists/{}", id(n)),
            json!({ "id": id(n), "snapshot_id": version }),
        );
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

async fn fill_lists(server: &Server, cx: &Cx, store: &mut dyn Store) {
    assert_eq!(full(server, cx, store).await.playlists.pending, 5);
    assert_eq!(pending(server, cx, store).await.playlists.pending, 0);
    assert_eq!(pool_from_store(store).unwrap().len(), 30);
}

#[test]
fn complete_unvisited_snapshots_retire_removed_taste_after_background_completion_and_restart() {
    let server = Server::new();
    setup_lists(&server);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("playlist-reconciliation.db");
        let mut store = SqliteStore::open(&file).unwrap();
        fill_lists(&server, &cx, &mut store).await;
        let before = server.item_calls().len();
        let reconciled = full(&server, &cx, &mut store).await;
        assert_eq!(server.item_calls().len() - before, 25);
        assert_eq!(reconciled.library.tracks, 30);
        assert_eq!(reconciled.playlists.pending, 0);
        assert!(
            !reconciled
                .notes
                .iter()
                .any(|note| note.contains("playlist budget"))
        );

        // The next metadata rotation visits 25..29 and 1..20. The changed list 23
        // is unvisited; its old checkpoint cannot be authoritative for version 2.
        directory(&server, &(1..30).collect::<Vec<_>>());
        directory_version(&server, 23, Some("version-2"));
        server.set(
            &item_route(23),
            page(vec![json!({ "item": track(10_023) })], 0, None),
        );
        let partial = full(&server, &cx, &mut store).await;
        assert_eq!(partial.playlists.pending, 1);
        assert_eq!(partial.library.retired, 0);
        assert!(pool_from_store(&store).unwrap().get(&uri(0)).is_some());
        assert!(pool_from_store(&store).unwrap().get(&uri(23)).is_some());
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let completed = pending(&server, &cx, &mut store).await;
        assert_eq!(completed.playlists.pending, 0);
        assert!(pool_from_store(&store).unwrap().get(&uri(10_023)).is_some());
        assert!(
            pool_from_store(&store).unwrap().get(&uri(0)).is_some(),
            "playlist-only intake cannot retire unrelated taste signals"
        );
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let before = server.item_calls().len();
        let current = full(&server, &cx, &mut store).await;
        assert_eq!(server.item_calls().len() - before, 25);
        assert_eq!(current.playlists.pending, 0);
        assert_eq!(current.library.retired, 2);
        assert_eq!(current.library.tracks, 29);
        let pool = pool_from_store(&store).unwrap();
        assert!(pool.get(&uri(0)).is_none());
        assert!(pool.get(&uri(23)).is_none());
        assert!(pool.get(&uri(10_023)).is_some());
        for n in (1..30).filter(|n| *n != 23) {
            assert!(
                pool.get(&uri(n)).is_some(),
                "unchanged unvisited music survives"
            );
        }
        store.close().unwrap();
        let reopened = SqliteStore::open(&file).unwrap();
        assert_eq!(pool_from_store(&reopened).unwrap().len(), 29);
        reopened.close().unwrap();
    });
}

fn long_playlist(server: &Server, changed_metadata: bool) {
    server.set(
        "/v1/me/playlists",
        page(
            vec![json!({
                "id": LONG, "name": "Long playlist",
                "owner": { "id": OWNER }, "snapshot_id": "test-version-1"
            })],
            0,
            None,
        ),
    );
    for offset in (0..405).step_by(50) {
        let end = (offset + 50).min(405);
        let items = (offset..end)
            .map(|n| {
                if changed_metadata && n == 1 {
                    json!({ "item": null })
                } else {
                    let mut item = track(n);
                    if changed_metadata {
                        item["name"] = json!(format!("Refreshed song {n}"));
                    }
                    json!({ "item": item })
                }
            })
            .collect::<Vec<_>>();
        server.set(
            &format!("/v1/playlists/{LONG}/items?offset={offset}"),
            json!({
                "items": items, "offset": offset, "total": 405,
                "next": (end < 405).then(|| format!("{API}/playlists/{LONG}/items?offset={end}"))
            }),
        );
    }
}

fn long_contents(store: &dyn Store) -> CachedPlaylist {
    store
        .playlist_contents(&format!("spotify:playlist:{LONG}"))
        .unwrap()
        .unwrap()
}

async fn fill_long(server: &Server, cx: &Cx, store: &mut dyn Store) {
    assert_eq!(full(server, cx, store).await.playlists.pending, 1);
    assert_eq!(pending(server, cx, store).await.playlists.pending, 1);
    assert_eq!(pending(server, cx, store).await.playlists.pending, 0);
    assert_eq!(long_contents(store).tracks.len(), 405);
}

#[test]
fn bounded_metadata_windows_preserve_complete_long_snapshots_and_resume_after_restart() {
    let server = Server::new();
    long_playlist(&server, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("long-playlist-metadata.db");
        let mut store = SqliteStore::open(&file).unwrap();
        fill_long(&server, &cx, &mut store).await;
        store.close().unwrap();
        long_playlist(&server, true);
        for (start, next, retired) in [(0, 150, 1), (150, 300, 0), (300, 0, 0)] {
            let mut store = SqliteStore::open(&file).unwrap();
            let reads = server.item_calls().len();
            let result = full(&server, &cx, &mut store).await;
            let calls = server.item_calls();
            assert_eq!(
                parameter(&calls[reads], "offset").unwrap(),
                start.to_string()
            );
            assert!(calls.len() - reads <= 3);
            assert_eq!(
                result.playlists.pending, 0,
                "same-version metadata creates no intake loop"
            );
            assert_eq!(result.library.retired, retired);
            assert_eq!(result.library.tracks, 404);
            assert!(
                !result
                    .notes
                    .iter()
                    .any(|note| note.contains("playlist items"))
            );
            let contents = long_contents(&store);
            assert!(contents.complete);
            assert_eq!(contents.examined, 405);
            assert_eq!(contents.source_total, Some(405));
            assert_eq!(contents.tracks.len(), 404);
            assert_eq!(contents.tracks[0].title, "Refreshed song 0");
            assert_eq!(contents.tracks.last().unwrap().position, 405);
            assert!(contents.tracks.iter().all(|track| track.position != 2));
            let checkpoint = store
                .playlist_progress(OWNER, &format!("spotify:playlist:{LONG}"))
                .unwrap()
                .unwrap();
            let checkpoint: Value = serde_json::from_str(&checkpoint).unwrap();
            assert_eq!(checkpoint["refresh_offset"], next);
            assert_eq!(checkpoint["complete"], true);
            assert_eq!(checkpoint["applied"], true);
            assert!(pool_from_store(&store).unwrap().get(&uri(404)).is_some());
            assert!(pool_from_store(&store).unwrap().get(&uri(1)).is_none());
            store.close().unwrap();
        }
        let store = SqliteStore::open(&file).unwrap();
        assert!(
            long_contents(&store)
                .tracks
                .iter()
                .all(|track| track.title.starts_with("Refreshed song"))
        );
        store.close().unwrap();
    });
}

#[test]
fn incomplete_versions_limits_and_other_signal_failures_preserve_prior_taste() {
    for failure in [
        "missing version",
        "limited",
        "metadata failure",
        "top tracks",
    ] {
        let server = Server::new();
        setup_lists(&server);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            fill_lists(&server, &cx, &mut store).await;
            directory(&server, &(1..30).collect::<Vec<_>>());
            match failure {
                "missing version" => directory_version(&server, 26, None),
                "limited" => {
                    let key = format!("spotify:playlist:{}", id(26));
                    let previous = store.playlist_progress(OWNER, &key).unwrap().unwrap();
                    let mut marker: Value = serde_json::from_str(&previous).unwrap();
                    marker["complete"] = json!(false);
                    marker["source_total"] = json!(10_001);
                    marker["examined"] = json!(10_000);
                    marker["tracks"] = json!([]);
                    store
                        .save_playlist_progress(OWNER, &key, &marker.to_string())
                        .unwrap();
                }
                "metadata failure" => server.set(&item_route(1), json!({ "not_a_page": true })),
                "top tracks" => server.set("/v1/me/top/tracks", json!({ "not_a_page": true })),
                _ => unreachable!(),
            }
            let scopes = if failure == "top tracks" {
                "user-library-read playlist-read-private user-top-read"
            } else {
                PLAYLIST_SCOPE
            };
            let mut session = server.session(&cx, scopes);
            let result = sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            assert_eq!(result.library.retired, 0, "{failure}");
            assert_eq!(pool_from_store(&store).unwrap().len(), 30, "{failure}");
            assert!(
                pool_from_store(&store).unwrap().get(&uri(0)).is_some(),
                "{failure}"
            );
            assert!(!result.notes.is_empty());
        });
    }
}

#[test]
fn unchanged_version_metadata_cannot_replace_a_complete_snapshot_after_coverage_or_proof_failure() {
    let server = Server::new();
    long_playlist(&server, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        fill_long(&server, &cx, &mut store).await;
        let before = long_contents(&store);
        let key = format!("spotify:playlist:{LONG}");
        let proof = store.playlist_progress(OWNER, &key).unwrap();
        {
            let mut state = server.state.lock().unwrap();
            let page = state
                .pages
                .get_mut(&format!("/v1/playlists/{LONG}/items?offset=0"))
                .unwrap();
            page["total"] = json!(406);
        }
        let failed = full(&server, &cx, &mut store).await;
        assert_eq!(failed.library.retired, 0);
        assert_eq!(long_contents(&store), before);
        assert_eq!(store.playlist_progress(OWNER, &key).unwrap(), proof);

        long_playlist(&server, true);
        server.set(
            &format!("/v1/playlists/{LONG}"),
            json!({ "id": LONG, "snapshot_id": "changed-during-read" }),
        );
        let failed = full(&server, &cx, &mut store).await;
        assert_eq!(failed.library.retired, 0);
        assert_eq!(long_contents(&store), before);
        assert_eq!(store.playlist_progress(OWNER, &key).unwrap(), proof);
        assert_eq!(pool_from_store(&store).unwrap().len(), 405);
    });
}

#[test]
fn saved_music_survives_playlist_removal_but_loses_its_old_playlist_weight() {
    let server = Server::new();
    setup_lists(&server);
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": track(0) })], 0, None),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        fill_lists(&server, &cx, &mut store).await;
        let before = store
            .library()
            .unwrap()
            .into_iter()
            .find(|entry| entry.track.source_uri == uri(0))
            .unwrap();
        assert_eq!(before.taste_weight, Some(PLAYLIST_PM));
        directory(&server, &(1..30).collect::<Vec<_>>());
        let reconciled = full(&server, &cx, &mut store).await;
        assert_eq!(reconciled.playlists.pending, 0);
        assert_eq!(reconciled.library.retired, 0);
        assert_eq!(reconciled.library.tracks, 30);
        let entry = store
            .library()
            .unwrap()
            .into_iter()
            .find(|entry| entry.track.source_uri == uri(0))
            .unwrap();
        assert!(entry.is_candidate());
        assert_eq!(entry.origin, fsonos_core::store::LibraryOrigin::LikedTrack);
        assert_eq!(
            entry.taste_weight, None,
            "an authoritative read drops the vanished signal"
        );
    });
}
