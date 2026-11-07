//! Malformed content flags cannot publish clean tracks or replace accepted caches.

use super::*;
use crate::cache::{sync_library_detailed, works_from_store};
use crate::dj::{DjConfig, PickContext, Rng, pick_next};
use crate::prefs::Preferences;
use crate::steer::Moods;
use fsonos_core::store::SqliteStore;

fn with_flag(number: usize, flag: Option<Value>) -> Value {
    let mut item = track(number);
    match flag {
        Some(flag) => item["explicit"] = flag,
        None => {
            item.as_object_mut().unwrap().remove("explicit");
        }
    }
    item
}

#[test]
fn invalid_liked_track_flags_leave_the_last_good_library_intact() {
    let server = Server::new();
    server.set(
        "/v1/me/tracks",
        page(
            vec![
                json!({ "track": track(0) }),
                json!({ "track": with_flag(1, Some(json!(true))) }),
            ],
            0,
            None,
        ),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("liked-content-flags.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let mut session = server.session(&cx, "");
        let initial = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!((initial.tracks, initial.candidates), (2, 1));
        let before = store.library().unwrap();
        let owner = store.spotify_library_owner().unwrap();
        store.close().unwrap();

        // A valid first page must not leak into the cache if a later page
        // cannot say whether its song is explicit.
        server.set(
            "/v1/me/tracks",
            page(
                vec![json!({ "track": track(2) })],
                0,
                Some(format!("{API}/me/tracks?offset=1")),
            ),
        );
        for flag in [
            None,
            Some(Value::Null),
            Some(json!("false")),
            Some(json!(0)),
        ] {
            server.set(
                "/v1/me/tracks?offset=1",
                page(vec![json!({ "track": with_flag(3, flag) })], 1, None),
            );
            let mut store = SqliteStore::open(&file).unwrap();
            let error = sync_library(&mut session, &cx, &mut store)
                .await
                .unwrap_err();
            assert!(matches!(error, crate::SpotifyError::Decode(_)), "{error}");
            assert_eq!(store.library().unwrap(), before);
            assert_eq!(store.spotify_library_owner().unwrap(), owner);
            store.close().unwrap();
        }

        server.set(
            "/v1/me/tracks?offset=1",
            page(
                vec![json!({ "track": with_flag(3, Some(json!(true))) })],
                1,
                None,
            ),
        );
        let mut store = SqliteStore::open(&file).unwrap();
        let refreshed = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!(
            (refreshed.tracks, refreshed.candidates, refreshed.retired),
            (2, 1, 2)
        );
        let pool = works_from_store(&store).unwrap();
        let selected = pick_next(
            &pool,
            &PickContext::default(),
            &DjConfig::default(),
            &mut Rng::new(1),
        )
        .unwrap();
        assert_eq!(
            selected.movements[0].track.source_uri,
            "spotify:track:TestTrack2"
        );
        assert!(pool.work_of("spotify:track:TestTrack3").unwrap().movements[0].explicit);
        store.close().unwrap();
    });
}

#[test]
fn invalid_playlist_flags_preserve_complete_metadata_and_replacement_snapshots() {
    let server = Server::new();
    server.set("/v1/me/playlists", page(vec![playlist(0, true)], 0, None));
    server.set(
        "/v1/playlists/TestList0/items",
        page(
            vec![json!({ "item": track(0) }), json!({ "item": track(1) })],
            0,
            None,
        ),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        sync_library(&mut session, &cx, &mut store).await.unwrap();
        let before = store.library().unwrap();
        let contents = store
            .playlist_contents("spotify:playlist:TestList0")
            .unwrap();
        let progress = store
            .playlist_progress("test-owner", "spotify:playlist:TestList0")
            .unwrap();
        for version in ["test-version-1", "replacement"] {
            let mut list = playlist(0, true);
            list["snapshot_id"] = json!(version);
            server.set("/v1/me/playlists", page(vec![list], 0, None));
            server.set(
                "/v1/playlists/TestList0",
                json!({ "id": "TestList0", "snapshot_id": version }),
            );
            for flag in [
                None,
                Some(Value::Null),
                Some(json!("false")),
                Some(json!(0)),
            ] {
                server.set(
                    "/v1/playlists/TestList0/items",
                    page(
                        vec![
                            json!({ "item": track(2) }),
                            json!({ "item": with_flag(3, flag) }),
                        ],
                        0,
                        None,
                    ),
                );
                let result = sync_library_detailed(&mut session, &cx, &mut store)
                    .await
                    .unwrap();
                assert_eq!(result.library.retired, 0);
                assert!(
                    result
                        .notes
                        .iter()
                        .any(|note| note.contains("playlist track")),
                    "{:?}",
                    result.notes
                );
                assert_eq!(store.library().unwrap(), before);
                assert_eq!(
                    store
                        .playlist_contents("spotify:playlist:TestList0")
                        .unwrap(),
                    contents
                );
                assert_eq!(
                    store
                        .playlist_progress("test-owner", "spotify:playlist:TestList0")
                        .unwrap(),
                    progress
                );
            }
        }

        server.set(
            "/v1/playlists/TestList0/items",
            page(
                vec![json!({ "item": with_flag(3, Some(json!(true))) })],
                0,
                None,
            ),
        );
        let result = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(
            (
                result.library.tracks,
                result.library.candidates,
                result.library.retired
            ),
            (1, 0, 2)
        );
        let observed = store
            .playlist_contents("spotify:playlist:TestList0")
            .unwrap()
            .unwrap();
        assert!(observed.complete);
        assert_eq!(observed.tracks.len(), 1);
        assert!(observed.tracks[0].explicit);
        let pool = works_from_store(&store).unwrap();
        assert!(
            pick_next(
                &pool,
                &PickContext::default(),
                &DjConfig::default(),
                &mut Rng::new(1)
            )
            .is_none()
        );
        let allowed = pool.with_preferences(
            Preferences {
                explicit: true,
                ..Preferences::default()
            },
            &Moods::default(),
        );
        assert!(
            pick_next(
                &allowed,
                &PickContext::default(),
                &DjConfig::default(),
                &mut Rng::new(1)
            )
            .is_some()
        );
    });
}
