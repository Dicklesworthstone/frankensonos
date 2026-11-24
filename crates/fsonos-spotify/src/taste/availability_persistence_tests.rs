//! Provider denials accepted with the full account read survive catalog caches,
//! failed reads and process restarts, without disabling allowed work completion.

use super::*;
use crate::cache::{sync_library_detailed, sync_library_pass, works_from_store};
use crate::client::{FullTrack, SimplifiedTrack};
use crate::expand::{AlbumMisses, complete_from_cache, complete_works};
use crate::works::{Completeness, group_works};
use chrono::Utc;
use fsonos_core::store::{AlbumTrack, SqliteStore};

const ALBUM: &str = "spotify:album:FakeAlbum0000000000009";

fn movements() -> Vec<Value> {
    let mut tracks = Vec::new();
    for page in [
        include_str!("../../tests/fixtures/album_tracks_page1.json"),
        include_str!("../../tests/fixtures/album_tracks_page2.json"),
    ] {
        let page: Value = serde_json::from_str(page).unwrap();
        tracks.extend(page["items"].as_array().unwrap().iter().cloned());
    }
    tracks.truncate(5);
    for track in &mut tracks {
        track["album"] = json!({
            "id": "FakeAlbum0000000000009", "uri": ALBUM,
            "name": "Mahler: Symphony No. 5",
            "artists": [{"name": "Gustav Mahler"}], "release_date": "1982"
        });
    }
    tracks
}

fn uri(number: usize) -> String {
    format!("spotify:track:FakeMahlerTrack{number:08}")
}

fn cache<S: Store + ?Sized>(store: &mut S) {
    let tracks: Vec<AlbumTrack> = movements()
        .into_iter()
        .map(|value| serde_json::from_value::<SimplifiedTrack>(value).unwrap())
        .filter_map(|track| crate::expand::album_track(&track))
        .collect();
    store
        .save_album_tracks(ALBUM, &tracks, Utc::now().timestamp())
        .unwrap();
}

fn observe(server: &Server, taste: bool, denied: Option<usize>, restored: Option<usize>) {
    let tracks = movements();
    let mut observed = vec![tracks[3].clone()];
    if let Some(number) = denied {
        let mut track = tracks[number - 1].clone();
        track["restrictions"] = json!({"reason": "market"});
        observed.push(track);
    }
    if let Some(number) = restored {
        observed.push(tracks[number - 1].clone());
    }
    let route = if taste {
        "/v1/me/top/tracks"
    } else {
        "/v1/me/tracks"
    };
    if !taste {
        observed = observed
            .into_iter()
            .map(|track| json!({"track": track}))
            .collect();
    }
    server.set(route, page(observed, 0, None));
}

async fn refresh<S: Store + ?Sized>(server: &Server, cx: &Cx, store: &mut S, taste: bool) {
    let scope = if taste { "user-top-read" } else { "" };
    let mut session = server.session(cx, scope);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap();
}

fn assert_work<S: Store + ?Sized>(store: &S, missing: Option<usize>) {
    let pool = works_from_store(store).unwrap();
    assert_eq!(pool.len(), 1);
    let work = &pool.works()[0];
    let expected: Vec<_> = (1..=5)
        .filter(|number| Some(*number) != missing)
        .map(uri)
        .collect();
    let actual: Vec<_> = work
        .movements
        .iter()
        .map(|movement| movement.track.source_uri.clone())
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(
        work.completeness,
        if missing.is_some() {
            Completeness::Partial
        } else {
            Completeness::Complete
        }
    );
}

#[test]
fn liked_and_taste_denials_survive_sqlite_reopen_until_fresh_positive_evidence() {
    for taste in [false, true] {
        for missing in [2, 5] {
            let server = Server::new();
            observe(&server, taste, Some(missing), None);
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                let file = server.data.join("durable-availability.db");
                let mut store = SqliteStore::open(&file).unwrap();
                cache(&mut store);
                refresh(&server, &cx, &mut store, taste).await;
                assert_eq!(
                    store.spotify_unavailable_tracks().unwrap(),
                    HashSet::from([uri(missing)])
                );
                assert_work(&store, Some(missing));
                assert_eq!(
                    store.album_tracks(ALBUM).unwrap().unwrap().tracks.len(),
                    5,
                    "the independent catalog cache still contains the older positive observation"
                );
                store.close().unwrap();

                let mut store = SqliteStore::open(&file).unwrap();
                assert_work(&store, Some(missing));
                observe(&server, taste, None, None);
                refresh(&server, &cx, &mut store, taste).await;
                assert_work(&store, Some(missing));

                // Generic cache writes are not fresh provider observations.
                let cached: Vec<_> = [4, missing]
                    .into_iter()
                    .map(|number| {
                        serde_json::from_value::<FullTrack>(movements()[number - 1].clone())
                            .unwrap()
                            .library_item(Origin::Taste, None)
                            .unwrap()
                    })
                    .collect();
                apply_library_read(&mut store, &cached).unwrap();
                assert!(
                    pool_from_store(&store)
                        .unwrap()
                        .get(&uri(missing))
                        .is_none()
                );
                assert_work(&store, Some(missing));

                observe(&server, taste, None, Some(missing));
                refresh(&server, &cx, &mut store, taste).await;
                assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
                assert_work(&store, None);
                store.close().unwrap();
            });
        }
    }
}

#[test]
fn a_fresh_catalog_response_cannot_clear_an_account_denial_or_revive_a_denied_seed() {
    let server = Server::new();
    observe(&server, false, Some(2), None);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        cache(&mut store);
        refresh(&server, &cx, &mut store, false).await;
        let old = store.album_tracks(ALBUM).unwrap().unwrap();
        store
            .save_album_tracks(
                ALBUM,
                &old.tracks,
                Utc::now().timestamp() - crate::expand::ALBUM_CACHE_MAX_AGE_SECS - 1,
            )
            .unwrap();
        server.set(
            "/v1/albums/FakeAlbum0000000000009/tracks",
            page(movements(), 0, None),
        );
        let items: Vec<_> = [2, 4]
            .into_iter()
            .map(|number| {
                serde_json::from_value::<FullTrack>(movements()[number - 1].clone())
                    .unwrap()
                    .library_item(Origin::LikedTrack, None)
                    .unwrap()
            })
            .collect();
        let stale = group_works(crate::classical::CandidatePool::build(&items).tracks());
        let mut session = server.session(&cx, "");
        let (completed, _) = complete_works(
            &mut session,
            &cx,
            &mut store,
            stale.clone(),
            &crate::dj::DjConfig::default(),
            Utc::now().timestamp(),
            &mut AlbumMisses::default(),
        )
        .await;
        assert_eq!(completed.len(), 1);
        assert!(
            !completed[0]
                .movements
                .iter()
                .any(|movement| movement.track.source_uri == uri(2))
        );
        assert_eq!(completed[0].completeness, Completeness::Partial);
        assert_eq!(
            server
                .calls("/v1/albums/FakeAlbum0000000000009/tracks")
                .len(),
            1
        );
        let completed = complete_from_cache(&store, stale).unwrap();
        assert!(
            !completed[0]
                .movements
                .iter()
                .any(|movement| movement.track.source_uri == uri(2))
        );
        let denied_only = group_works(crate::classical::CandidatePool::build(&items[..1]).tracks());
        assert!(complete_from_cache(&store, denied_only).unwrap().is_empty());
        assert_eq!(
            store.spotify_unavailable_tracks().unwrap(),
            HashSet::from([uri(2)])
        );
    });
}

#[test]
fn a_failed_saved_read_cannot_clear_or_add_public_denials() {
    let server = Server::new();
    observe(&server, false, Some(2), None);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        cache(&mut store);
        refresh(&server, &cx, &mut store, false).await;
        let previous = store.library().unwrap();
        let revision = store.library_catalog_revision().unwrap();
        let mut malformed = movements()[2].clone();
        malformed["restrictions"] = json!("invalid restriction object");
        server.set(
            "/v1/me/tracks",
            page(
                vec![
                    json!({"track": movements()[1]}),
                    json!({"track": malformed}),
                ],
                0,
                None,
            ),
        );
        let mut session = server.session(&cx, "");
        assert!(
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .is_err()
        );
        assert_eq!(
            store.spotify_unavailable_tracks().unwrap(),
            HashSet::from([uri(2)])
        );
        assert_eq!(store.library().unwrap(), previous);
        assert_eq!(store.library_catalog_revision().unwrap(), revision);
        assert_work(&store, Some(2));
    });
}

#[test]
fn denied_liked_rows_stay_private_until_resumed_saved_read_is_fully_verified() {
    let server = Server::new();
    let tracks = movements();
    for (position, track) in tracks.iter().enumerate() {
        let mut track = track.clone();
        if position == 1 {
            track["is_playable"] = json!(false);
        }
        server.set(
            &format!("/v1/me/tracks?offset={position}"),
            json!({"items": [{"track": track}], "offset": position, "total": 5, "next": null}),
        );
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("availability-progress.db");
        let mut store = SqliteStore::open(&file).unwrap();
        cache(&mut store);
        let mut session = server.session(&cx, "");
        assert!(matches!(
            sync_library_pass(&mut session, &cx, &mut store, 3).await,
            Err(crate::SpotifyError::LibraryPending { .. })
        ));
        assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
        assert!(store.library().unwrap().is_empty());
        store.close().unwrap();
        let mut store = SqliteStore::open(&file).unwrap();
        let mut complete = false;
        for _ in 0..12 {
            let mut session = server.session(&cx, "");
            match sync_library_pass(&mut session, &cx, &mut store, 4).await {
                Ok(_) => {
                    complete = true;
                    break;
                }
                Err(crate::SpotifyError::LibraryPending { .. }) => {
                    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
                }
                Err(error) => panic!("resumed availability read failed: {error}"),
            }
        }
        assert!(complete);
        assert_eq!(
            store.spotify_unavailable_tracks().unwrap(),
            HashSet::from([uri(2)])
        );
        assert_work(&store, Some(2));
        store.close().unwrap();
    });
}
