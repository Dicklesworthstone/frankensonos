//! Provider restrictions never become playable music or incomplete coverage.

use super::*;
use fsonos_core::store::SqliteStore;

fn restricted(number: usize, reason: &str) -> Value {
    let mut value = track(number);
    value["restrictions"] = json!({ "reason": reason });
    value
}

#[test]
fn saved_hydration_and_taste_skip_restrictions_without_losing_later_music() {
    let server = Server::new();
    server.set(
        "/v1/me/albums",
        page(
            vec![json!({ "album": {
                "id": "AvailabilityAlbum", "uri": "spotify:album:AvailabilityAlbum",
                "name": "Test album", "total_tracks": 4,
                "tracks": {
                    "items": [track(0), restricted(1, "market")],
                    "offset": 0, "total": 4,
                    "next": format!("{API}/albums/AvailabilityAlbum/tracks?offset=2")
                }
            }})],
            0,
            None,
        ),
    );
    server.set(
        "/v1/albums/AvailabilityAlbum/tracks?offset=2",
        json!({
            "items": [restricted(2, "product"), track(3)],
            "offset": 2, "total": 4, "next": null
        }),
    );
    server.set(
        "/v1/me/tracks",
        page(
            vec![
                json!({ "track": restricted(4, "explicit") }),
                json!({ "track": track(5) }),
            ],
            0,
            None,
        ),
    );
    server.set("/v1/me/playlists", page(vec![playlist(0, true)], 0, None));
    server.set(
        "/v1/playlists/TestList0/items",
        json!({
            "items": [
                { "item": track(7) },
                { "item": { "restrictions": { "reason": "future-reason" } } },
                { "item": track(7) }
            ], "offset": 0, "total": 3, "next": null
        }),
    );
    server.set(
        "/v1/me/top/tracks",
        page(vec![restricted(8, "market"), track(9)], 0, None),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "playlist-read-private user-top-read");
        let mut store = MemStore::default();
        let result = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!((result.tracks, result.candidates), (5, 5));
        let pool = pool_from_store(&store).unwrap();
        for number in [0, 3, 5, 7, 9] {
            assert!(
                pool.get(&format!("spotify:track:TestTrack{number}"))
                    .is_some()
            );
        }
        for number in [1, 2, 4, 8] {
            assert!(
                pool.get(&format!("spotify:track:TestTrack{number}"))
                    .is_none()
            );
        }
        let playlist = store
            .playlist_contents("spotify:playlist:TestList0")
            .unwrap()
            .unwrap();
        assert!(playlist.complete);
        assert_eq!((playlist.examined, playlist.source_total), (3, Some(3)));
        assert_eq!(
            playlist
                .tracks
                .iter()
                .map(|track| track.position)
                .collect::<Vec<_>>(),
            [1, 3]
        );
    });
    assert_eq!(server.calls("/v1/albums/AvailabilityAlbum/tracks").len(), 1);
}

#[test]
fn malformed_restriction_metadata_cannot_replace_a_saved_library() {
    let server = Server::new();
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": track(0) })], 0, None),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("availability.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let mut session = server.session(&cx, "");
        sync_library(&mut session, &cx, &mut store).await.unwrap();
        let before = store.library().unwrap();
        let owner = store.spotify_library_owner().unwrap();
        store.close().unwrap();
        server.set(
            "/v1/me/tracks",
            page(
                vec![json!({ "track": track(1) })],
                0,
                Some(format!("{API}/me/tracks?offset=1")),
            ),
        );
        for restriction in [json!("market"), json!([]), json!(false)] {
            let mut broken = track(2);
            broken["restrictions"] = restriction;
            server.set(
                "/v1/me/tracks?offset=1",
                page(vec![json!({ "track": broken })], 1, None),
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
            page(vec![json!({ "track": restricted(2, "market") })], 1, None),
        );
        let mut store = SqliteStore::open(&file).unwrap();
        let result = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!((result.tracks, result.retired), (1, 1));
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack1")
                .is_some()
        );
        store.close().unwrap();
    });
}
