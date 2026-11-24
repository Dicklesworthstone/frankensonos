//! Revalidation changes only fully proved catalog observations. Old music
//! remains usable through bounded reads and failures; definite empty or missing
//! observations must not leave expanded movements behind after a restart.

use super::*;
use crate::client::SimplifiedTrack;
use crate::expand::{ALBUM_CACHE_MAX_AGE_SECS, album_track};
use fsonos_core::store::AlbumTrack;

fn cached_tracks(album: &str, count: u32) -> Vec<AlbumTrack> {
    (1..=count)
        .map(|number| {
            let track: SimplifiedTrack =
                serde_json::from_value(movement(album, number)).unwrap();
            album_track(&track).unwrap()
        })
        .collect()
}

fn seed(store: &mut dyn Store, at: i64) {
    store.adopt_spotify_library_owner(OWNER).unwrap();
    apply_library_read(store, &[held(ALBUM)]).unwrap();
    store
        .save_album_tracks(
            &format!("spotify:album:{ALBUM}"),
            &cached_tracks(ALBUM, 3),
            at,
        )
        .unwrap();
}

fn positions(store: &dyn Store) -> Vec<u32> {
    works_from_store(store)
        .unwrap()
        .work_of(&held(ALBUM).source_uri)
        .unwrap()
        .movements
        .iter()
        .map(|movement| movement.track_number.unwrap())
        .collect()
}

#[test]
fn fresh_cache_stays_quiet_and_a_completed_checkpoint_can_revalidate_expired_music() {
    let server = Server::new();
    album_pages(&server, ALBUM, 3);
    let mut restricted = movement(ALBUM, 1);
    restricted["restrictions"] = json!({ "reason": "market" });
    server.set(
        &format!("{ROUTE}?offset=0"),
        json!({
            "items": [restricted], "offset": 0, "total": 3,
            "next": format!("{API}/albums/{ALBUM}/tracks?offset=1")
        }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let at = chrono::Utc::now().timestamp();
        let uri = format!("spotify:album:{ALBUM}");
        let mut store = MemStore::default();
        seed(&mut store, at);

        let fresh = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!((fresh.albums.pending, fresh.albums.completed), (0, 0));
        assert_eq!(fresh.albums.recheck_at, Some(at + ALBUM_CACHE_MAX_AGE_SECS));
        assert!(server.calls("/v1/").is_empty(), "fresh cache needs no HTTP");

        store
            .save_album_tracks(&uri, &cached_tracks(ALBUM, 3), at - ALBUM_CACHE_MAX_AGE_SECS)
            .unwrap();
        assert_eq!(positions(&store), [1, 2, 3], "old evidence remains usable");
        let resumed: AlbumContinuation =
            serde_json::from_str(&serde_json::to_string(&fresh.albums).unwrap()).unwrap();
        let refreshed = continue_with(&server, &cx, &mut store, Some(&resumed)).await;
        assert_eq!((refreshed.albums.pending, refreshed.albums.completed), (0, 1));
        assert_eq!(server.calls(ROUTE).len(), 3);
        assert_eq!(positions(&store), [2, 3], "a newly restricted movement is removed");
        let cached = store.album_tracks(&uri).unwrap().unwrap();
        assert!(cached.fetched_at >= at);
        assert_eq!(
            refreshed.albums.recheck_at,
            Some(cached.fetched_at + ALBUM_CACHE_MAX_AGE_SECS)
        );

        let before = server.calls("/v1/").len();
        continue_with(&server, &cx, &mut store, Some(&refreshed.albums)).await;
        assert_eq!(server.calls("/v1/").len(), before);
    });
}

#[test]
fn empty_restricted_and_missing_catalogs_retire_old_movements_across_restart() {
    for kind in ["empty", "restricted", "missing"] {
        let server = Server::new();
        match kind {
            "empty" => server.set(
                ROUTE,
                json!({ "items": [], "offset": 0, "total": 0, "next": null }),
            ),
            "restricted" => {
                let tracks: Vec<_> = (1..=3)
                    .map(|number| {
                        let mut track = movement(ALBUM, number);
                        track["is_playable"] = json!(false);
                        track
                    })
                    .collect();
                server.set(
                    ROUTE,
                    json!({ "items": tracks, "offset": 0, "total": 3, "next": null }),
                );
            }
            "missing" => {
                server.state.lock().unwrap().refused.insert(ROUTE.into(), 404);
            }
            _ => unreachable!("known catalog observation"),
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("expired-album.db");
            let uri = format!("spotify:album:{ALBUM}");
            let mut store = SqliteStore::open(&file).unwrap();
            seed(&mut store, 1);
            let saved = store.library().unwrap();
            let before = store.library_catalog_revision().unwrap();

            let result = continue_with(&server, &cx, &mut store, None).await;
            assert_eq!((result.albums.completed, result.albums.pending), (1, 0), "{kind}");
            assert!(store.album_tracks(&uri).unwrap().is_none(), "{kind}");
            assert_eq!(positions(&store), [2], "{kind}: held music stays, expansion retires");
            assert_eq!(store.library().unwrap(), saved, "refresh never changes saves");
            assert_ne!(store.library_catalog_revision().unwrap(), before, "{kind}");
            let retry = store.album_misses().unwrap();
            assert_eq!(retry.len(), 1, "{kind}");
            assert_eq!(result.albums.recheck_at, Some(retry[0].1), "{kind}");
            let checkpoint = serde_json::to_string(&result.albums).unwrap();
            store.close().unwrap();

            let mut store = SqliteStore::open(&file).unwrap();
            let previous = serde_json::from_str(&checkpoint).unwrap();
            assert!(store.album_tracks(&uri).unwrap().is_none(), "{kind}");
            assert_eq!(positions(&store), [2], "{kind}: restart cannot revive the old cache");
            let calls = server.calls("/v1/").len();
            let waiting = continue_with(&server, &cx, &mut store, Some(&previous)).await;
            assert_eq!(waiting.albums.completed, 0);
            assert_eq!(waiting.albums.recheck_at, Some(retry[0].1));
            assert_eq!(server.calls("/v1/").len(), calls, "{kind}: respect known miss");
            store.close().unwrap();
        });
    }
}

#[test]
fn malformed_outage_and_cooldown_revalidation_preserve_the_last_completed_list() {
    for kind in ["malformed", "outage", "cooldown"] {
        let server = Server::new();
        match kind {
            "malformed" => server.set(
                ROUTE,
                json!({ "items": [], "offset": 0, "total": 3, "next": null }),
            ),
            "outage" | "cooldown" => {
                server
                    .state
                    .lock()
                    .unwrap()
                    .refused
                    .insert(ROUTE.into(), if kind == "outage" { 503 } else { 429 });
            }
            _ => unreachable!("known failure"),
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let uri = format!("spotify:album:{ALBUM}");
            let mut store = MemStore::default();
            seed(&mut store, 1);
            let original = store.album_tracks(&uri).unwrap();
            let revision = store.library_catalog_revision().unwrap();
            let mut session = server.session(&cx, "user-library-read");
            let result = continue_albums(&mut session, &cx, &mut store, None)
                .await
                .unwrap();
            assert_eq!((result.albums.completed, result.albums.pending), (0, 1), "{kind}");
            assert_eq!(store.album_tracks(&uri).unwrap(), original, "{kind}");
            assert_eq!(store.library_catalog_revision().unwrap(), revision, "{kind}");
            assert_eq!(positions(&store), [1, 2, 3], "{kind}");
            assert!(store.album_misses().unwrap().is_empty(), "{kind}: no false absence");
            assert!(!result.notes.is_empty(), "{kind}: failure remains visible");

            if kind == "cooldown" {
                assert!(result.albums.retry_after_secs > 0);
                let before = server.calls("/v1/").len();
                let limited =
                    continue_albums(&mut session, &cx, &mut store, Some(&result.albums)).await;
                assert!(matches!(limited, Err(SpotifyError::Api { status: 429, .. })));
                assert_eq!(server.calls("/v1/").len(), before, "no early retry");
            }

            server.state.lock().unwrap().refused.remove(ROUTE);
            album_pages(&server, ALBUM, 3);
            let recovered = continue_with(&server, &cx, &mut store, Some(&result.albums)).await;
            assert_eq!((recovered.albums.completed, recovered.albums.pending), (1, 0), "{kind}");
            assert!(store.album_tracks(&uri).unwrap().unwrap().fetched_at > 1, "{kind}");
        });
    }
}

#[test]
fn an_expired_large_album_keeps_old_evidence_until_bounded_verification_finishes() {
    let server = Server::new();
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("expired-album-continuation.db");
        let uri = format!("spotify:album:{ALBUM}");
        let mut store = SqliteStore::open(&file).unwrap();
        seed(&mut store, 1);
        let original = store.album_tracks(&uri).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(server.calls(ROUTE).len(), ALBUM_PAGE_BUDGET);
        assert_eq!((first.albums.completed, first.albums.pending), (0, 1));
        assert_eq!(store.album_tracks(&uri).unwrap(), original);
        assert_eq!(positions(&store), [1, 2, 3]);
        let checkpoint = serde_json::to_string(&first.albums).unwrap();
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let previous = serde_json::from_str(&checkpoint).unwrap();
        let checking = continue_with(&server, &cx, &mut store, Some(&previous)).await;
        assert_eq!(checking.albums.completed, 0);
        assert_eq!(server.calls(ROUTE).len(), 2 * ALBUM_PAGE_BUDGET);
        assert_eq!(store.album_tracks(&uri).unwrap(), original);
        let completed = finish_with(&server, &cx, &mut store, &checking.albums).await;
        assert_eq!((completed.albums.completed, completed.albums.pending), (1, 0));
        assert_eq!(positions(&store), (1..=10).collect::<Vec<_>>());
        assert!(store.album_tracks(&uri).unwrap().unwrap().fetched_at > 1);
        store.close().unwrap();
    });
}

#[test]
fn a_disappearance_from_an_old_account_cannot_clear_the_current_accounts_album() {
    for missing in [false, true] {
        let server = Server::new();
        if missing {
            server.state.lock().unwrap().refused.insert(ROUTE.into(), 404);
        } else {
            server.set(
                ROUTE,
                json!({ "items": [], "offset": 0, "total": 0, "next": null }),
            );
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("expired-album-owner.db");
            let uri = format!("spotify:album:{ALBUM}");
            let mut store = SqliteStore::open(&file).unwrap();
            seed(&mut store, 1);
            let replacement = cached_tracks(ALBUM, 5);
            let new_tracks = replacement.clone();
            let other_file = file.clone();
            let other_uri = uri.clone();
            server.state.lock().unwrap().before_response = Some((
                ROUTE.into(),
                Box::new(move || {
                    let mut other = SqliteStore::open(&other_file).unwrap();
                    other.adopt_spotify_library_owner("another-test-owner").unwrap();
                    apply_library_read(&mut other, &[held(ALBUM)]).unwrap();
                    other
                        .save_album_tracks(&other_uri, &new_tracks, chrono::Utc::now().timestamp())
                        .unwrap();
                    other.close().unwrap();
                }),
            ));
            let result = continue_with(&server, &cx, &mut store, None).await;
            assert!(result.albums.requires_refresh);
            assert_eq!(result.albums.completed, 0);
            assert_eq!(result.albums.recheck_at, None);
            assert_eq!(store.album_tracks(&uri).unwrap().unwrap().tracks, replacement);
            assert!(store.album_misses().unwrap().is_empty());
            store.close().unwrap();
        });
    }
}

#[test]
fn direct_completion_cannot_replace_or_retire_an_intervening_accounts_album() {
    for (kind, adopted) in [("empty", false), ("missing", true), ("positive", true)] {
        let server = Server::new();
        match kind {
            "empty" => server.set(
                ROUTE,
                json!({ "items": [], "offset": 0, "total": 0, "next": null }),
            ),
            "missing" => {
                server.state.lock().unwrap().refused.insert(ROUTE.into(), 404);
            }
            "positive" => server.set(
                ROUTE,
                json!({
                    "items": (1..=3).map(|number| movement(ALBUM, number)).collect::<Vec<_>>(),
                    "offset": 0, "total": 3, "next": null
                }),
            ),
            _ => unreachable!("known catalog observation"),
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("direct-album-owner.db");
            let uri = format!("spotify:album:{ALBUM}");
            let mut store = SqliteStore::open(&file).unwrap();
            if adopted {
                store.adopt_spotify_library_owner(OWNER).unwrap();
            }
            apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
            store
                .save_album_tracks(&uri, &cached_tracks(ALBUM, 3), 1)
                .unwrap();
            let replacement = cached_tracks(ALBUM, 5);
            let new_tracks = replacement.clone();
            let other_file = file.clone();
            let other_uri = uri.clone();
            server.state.lock().unwrap().before_response = Some((
                ROUTE.into(),
                Box::new(move || {
                    let mut other = SqliteStore::open(&other_file).unwrap();
                    other.adopt_spotify_library_owner("another-test-owner").unwrap();
                    // Returning to the same ID must still invalidate the old
                    // generation. An unowned cache must notice adoption too.
                    other.adopt_spotify_library_owner(OWNER).unwrap();
                    other
                        .save_album_tracks(&other_uri, &new_tracks, chrono::Utc::now().timestamp())
                        .unwrap();
                    other.close().unwrap();
                }),
            ));
            let pool = crate::classical::CandidatePool::build(&[held(ALBUM)]);
            let original = crate::works::group_works(pool.tracks());
            let mut session = server.session(&cx, "user-library-read");
            let mut misses = crate::expand::AlbumMisses::default();
            let (works, report) = crate::expand::complete_works(
                &mut session,
                &cx,
                &mut store,
                original.clone(),
                &crate::dj::DjConfig::default(),
                chrono::Utc::now().timestamp(),
                &mut misses,
            )
            .await;
            assert_eq!(works, original, "{kind}");
            assert!(report.halted.is_some(), "{kind}");
            assert_eq!((report.completed, report.fetched), (0, 0), "{kind}");
            assert!(misses.is_empty(), "{kind}: no stale negative evidence");
            assert_eq!(store.album_tracks(&uri).unwrap().unwrap().tracks, replacement);
            store.close().unwrap();
        });
    }
}
