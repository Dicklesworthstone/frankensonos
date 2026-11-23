//! Fresh saved-album contents must outrank an older completion cache.

use super::*;
use crate::cache::{apply_library_read, works_from_store};
use crate::library::LibraryRead;
use serde_json::json;

fn saved_items_without(missing: u32) -> Vec<LibraryItem> {
    let tracks: Vec<_> = fixture_album()
        .into_iter()
        .take(5)
        .map(|track| {
            json!({
                "id": track.source_uri.strip_prefix("spotify:track:").unwrap(),
                "uri": track.source_uri,
                "name": track.title,
                "artists": [{"name": "Gustav Mahler"}, {"name": "Test Philharmonic"}],
                "disc_number": track.disc_number,
                "track_number": track.track_number,
                "duration_ms": track.duration_secs.map(|seconds| seconds * 1000),
                "explicit": false,
                "is_playable": track.track_number != missing
            })
        })
        .collect();
    let albums = json!({
        "items": [{
            "added_at": "2025-01-15T20:31:02Z",
            "album": {
                "id": album_id(ALBUM).unwrap(),
                "uri": ALBUM,
                "name": "Mahler: Symphony No. 5",
                "artists": [{"name": "Gustav Mahler"}],
                "genres": ["classical"],
                "total_tracks": tracks.len(),
                "tracks": {
                    "items": tracks,
                    "offset": 0,
                    "limit": 50,
                    "total": 5,
                    "next": null
                }
            }
        }],
        "offset": 0,
        "limit": 50,
        "total": 1,
        "next": null
    });
    let mut read = LibraryRead::default();
    read.ingest(&serde_json::to_vec(&albums).unwrap()).unwrap();
    read.ingest(br#"{"items":[],"offset":0,"limit":50,"total":0,"next":null}"#)
        .unwrap();
    assert!(
        read.next_url().is_none(),
        "the saved album was read completely"
    );
    read.into_items()
}

fn track_numbers(work: &Work) -> Vec<u32> {
    work.movements
        .iter()
        .map(|movement| movement.track_number.unwrap())
        .collect()
}

#[test]
fn unavailable_saved_movements_are_not_resurrected_from_an_older_album_cache() {
    let album = fixture_album();
    for missing in [2, 5] {
        for origin in [Origin::SavedAlbum, Origin::Both] {
            let mut items = saved_items_without(missing);
            for item in &mut items {
                item.origin = origin;
            }
            let original = works_of(&items);
            assert_eq!(original.len(), 1);
            assert_eq!(original[0].movements.len(), 4);
            if missing == 2 {
                assert_eq!(original[0].completeness, Completeness::Partial);
            }
            assert!(!original[0].needs_expansion());
            assert!(albums_needing_completion(&original).is_empty());
            assert!(expand_work(&original[0], &album).is_none());

            let mut store = MemStore::default();
            store.save_album_tracks(ALBUM, &album, NOW - 1).unwrap();
            assert_eq!(
                complete_from_cache(&store, original.clone()).unwrap(),
                original,
                "saved provenance {origin:?}, unavailable movement {missing}"
            );
        }
    }
}

#[test]
fn mixed_saved_and_liked_provenance_keeps_the_observed_album_gap() {
    let mut items = saved_items_without(2);
    for item in &mut items {
        item.origin = Origin::LikedTrack;
    }
    items[0].origin = Origin::SavedAlbum;
    let work = works_of(&items).pop().unwrap();
    assert_eq!(work.completeness, Completeness::Partial);
    assert_eq!(track_numbers(&work), [1, 3, 4, 5]);
    assert!(!work.needs_expansion());
    assert!(expand_work(&work, &fixture_album()).is_none());
}

#[test]
fn a_full_library_refresh_keeps_a_retired_movement_out_of_restart_reconstruction() {
    let mut store = MemStore::default();
    apply_library_read(&mut store, &saved_items_without(0)).unwrap();
    store
        .save_album_tracks(ALBUM, &fixture_album(), NOW - 1)
        .unwrap();

    let report = apply_library_read(&mut store, &saved_items_without(2)).unwrap();
    assert_eq!(report.retired, 1);
    let pool = works_from_store(&store).unwrap();
    assert_eq!(pool.len(), 1);
    assert_eq!(track_numbers(&pool.works()[0]), [1, 3, 4, 5]);
    assert_eq!(pool.works()[0].completeness, Completeness::Partial);
}

#[test]
fn liked_and_taste_only_movements_still_complete_their_unsaved_recording() {
    for origin in [Origin::LikedTrack, Origin::Taste] {
        let mut item = liked(ADAGIETTO, ALBUM);
        item.origin = origin;
        let partial = works_of(&[item]).pop().unwrap();
        assert!(partial.needs_expansion());
        let full = expand_work(&partial, &fixture_album()).unwrap();
        assert_eq!(track_numbers(&full), [1, 2, 3, 4, 5]);
        assert_eq!(full.completeness, Completeness::Complete);
        assert_eq!(
            full.movements
                .iter()
                .filter(|movement| movement.expanded)
                .count(),
            4
        );
    }
}

#[test]
fn filled_in_tracks_do_not_falsely_claim_a_saved_album_read() {
    let album = fixture_album();
    let partial = works_of(&[liked(ADAGIETTO, ALBUM)]).pop().unwrap();
    let fragment = [album[0].clone(), album[3].clone()];
    let expanded = expand_work(&partial, &fragment).unwrap();
    assert_eq!(expanded.completeness, Completeness::Partial);
    assert!(expanded.movements[0].expanded);
    assert_eq!(expanded.movements[0].origin, Origin::SavedAlbum);
    assert!(expanded.needs_expansion());
    let complete = expand_work(&expanded, &album).unwrap();
    assert_eq!(track_numbers(&complete), [1, 2, 3, 4, 5]);
}

#[test]
fn saved_album_gaps_need_no_async_completion_requests_with_or_without_a_cache() {
    let (spotify, cache, data_dir) = authorized("expand-saved-availability");
    let endpoints = spotify.endpoints();
    let original = works_of(&saved_items_without(2));
    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints);
        for cached in [false, true] {
            let mut store = MemStore::default();
            if cached {
                store
                    .save_album_tracks(ALBUM, &fixture_album(), NOW - 1)
                    .unwrap();
            }
            let (works, report) = complete_works(
                &mut session,
                &cx,
                &mut store,
                original.clone(),
                &DjConfig::default(),
                NOW,
                &mut AlbumMisses::default(),
            )
            .await;
            assert_eq!(works, original);
            assert_eq!(report, Expansion::default());
        }
    });
    assert_eq!(requests(&spotify.state, "/v1/"), 0);
    spotify.stop();
    std::fs::remove_dir_all(data_dir).unwrap();
}
