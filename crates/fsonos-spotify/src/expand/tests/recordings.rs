//! Exercise the same expansion used by library sync and cache-only startup.

use super::*;

fn sections() -> Vec<AlbumTrack> {
    [
        "I. Introitus: Requiem aeternam",
        "II. Kyrie",
        "III. Sequentia: No. 1, Dies irae",
        "III. Sequentia: No. 2, Tuba mirum",
        "III. Sequentia: No. 3, Rex tremendae",
        "IV. Offertorium: No. 1, Domine Jesu",
        "IV. Offertorium: No. 2, Hostias",
        "V. Sanctus",
    ]
    .into_iter()
    .zip(1u32..)
    .map(|(movement, number)| AlbumTrack {
        disc_number: 1,
        track_number: number,
        source_uri: format!("spotify:track:Section{number}"),
        title: format!("Requiem in D Minor, K. 626: {movement}"),
        duration_secs: Some(120),
    })
    .collect()
}

fn two_takes() -> Vec<AlbumTrack> {
    let first = sections();
    let second = first.iter().map(|track| AlbumTrack {
        disc_number: 2,
        source_uri: format!("{}Second", track.source_uri),
        ..track.clone()
    });
    first.iter().cloned().chain(second).collect()
}

fn item(track: &AlbumTrack) -> LibraryItem {
    LibraryItem {
        title: track.title.clone(),
        artists: vec!["Wolfgang Amadeus Mozart".into(), "Test Choir".into()],
        album: Some("Test Requiem Recordings".into()),
        album_artists: vec!["Wolfgang Amadeus Mozart".into()],
        disc_number: Some(track.disc_number),
        track_number: Some(track.track_number),
        duration_secs: track.duration_secs,
        ..liked(&track.source_uri, ALBUM)
    }
}

fn one(track: &AlbumTrack) -> Work {
    let mut works = works_of(&[item(track)]);
    assert_eq!(works.len(), 1);
    works.pop().unwrap()
}

fn uris(work: &Work) -> Vec<&str> {
    work.movements
        .iter()
        .map(|movement| movement.track.source_uri.as_str())
        .collect()
}

#[test]
fn a_liked_section_completes_every_section_of_the_same_recording() {
    let album = sections();
    let expected: Vec<&str> = album.iter().map(|t| t.source_uri.as_str()).collect();
    for held in &album {
        let work = one(held);
        let full = expand_work(&work, &album).unwrap();
        assert_eq!(uris(&full), expected, "{}", held.title);
        assert_eq!(full.completeness, Completeness::Complete);
        assert_eq!(full.total_secs, 960);
        assert_eq!(
            full.movements.iter().filter(|m| m.origin.is_liked()).count(),
            1
        );
        assert!(expand_work(&full, &album).is_none());
    }
    let saved: Vec<LibraryItem> = album
        .iter()
        .map(|track| LibraryItem {
            origin: Origin::SavedAlbum,
            ..item(track)
        })
        .collect();
    let grouped = works_of(&saved);
    assert_eq!(grouped.len(), 1, "saved and expanded works agree");
    assert_eq!(uris(&grouped[0]), expected);
}

#[test]
fn multipart_alternate_takes_expand_independently_even_with_scrambled_input() {
    let album = two_takes();
    let mut reversed = album.clone();
    reversed.reverse();
    for range in [0..8, 8..16] {
        let work = one(&album[range.start + 3]);
        let full = expand_work(&work, &reversed).unwrap();
        let expected: Vec<&str> = album[range].iter().map(|t| t.source_uri.as_str()).collect();
        assert_eq!(uris(&full), expected);
        assert_eq!(full.completeness, Completeness::Complete);
    }
}

#[test]
fn movements_from_different_takes_cannot_authorize_a_combined_recording() {
    let album = two_takes();
    // Both are sections of III, so grouping alone has no numeral restart.
    let work = works_of(&[item(&album[2]), item(&album[12])]).pop().unwrap();
    assert_eq!(work.movements.len(), 2);
    assert!(expand_work(&work, &album).is_none());
    let mut store = MemStore::default();
    store.save_album_tracks(ALBUM, &album, NOW).unwrap();
    assert_eq!(
        complete_from_cache(&store, vec![work.clone()]).unwrap(),
        [work]
    );
}

#[test]
fn repeated_uris_need_a_real_position_to_select_a_take() {
    let mut album = two_takes();
    album[10].source_uri = album[2].source_uri.clone();
    for index in [2, 10] {
        let full = expand_work(&one(&album[index]), &album).unwrap();
        let disc = album[index].disc_number;
        assert_eq!(full.movements.len(), 8);
        assert!(full.movements.iter().all(|m| m.disc_number == Some(disc)));
    }
    for position in [None, Some(0)] {
        let mut work = one(&album[2]);
        work.movements[0].disc_number = position;
        work.movements[0].track_number = position;
        assert!(expand_work(&work, &album).is_none(), "{position:?}");
    }
}

#[test]
fn missing_identity_does_not_guess_even_when_only_one_recording_is_present() {
    let album = sections();
    let mut work = one(&album[3]);
    work.movements[0].track.source_uri = "spotify:track:Absent".into();
    work.movements[0].disc_number = None;
    work.movements[0].track_number = None;
    assert!(expand_work(&work, &album).is_none());
    assert!(expand_work(&one(&album[3]), &[]).is_none());
}

#[test]
fn cache_only_startup_restores_the_complete_multipart_recording() {
    let album = two_takes();
    let work = one(&album[11]);
    let mut store = MemStore::default();
    store.save_album_tracks(ALBUM, &album, NOW).unwrap();
    let restored = complete_from_cache(&store, vec![work]).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].movements.len(), 8);
    assert!(restored[0].movements.iter().all(|m| m.disc_number == Some(2)));
    assert_eq!(restored[0].completeness, Completeness::Complete);
}

#[test]
fn a_relinked_uri_must_still_name_the_movement_at_its_position() {
    let album = sections();
    let mut work = one(&album[3]);
    work.movements[0].track.source_uri = "spotify:track:RelinkedSection".into();
    let full = expand_work(&work, &album).unwrap();
    assert_eq!(full.movements.len(), 8);
    assert_eq!(
        full.movements[3].track.source_uri,
        "spotify:track:RelinkedSection"
    );
    // A stale coordinate must not match a different section of the same work.
    work.movements[0].track_number = Some(3);
    assert!(expand_work(&work, &album).is_none());
}

#[test]
fn recovered_positions_keep_a_liked_section_in_order_and_preserve_its_metadata() {
    let album = sections();
    let expected: Vec<&str> = album.iter().map(|t| t.source_uri.as_str()).collect();
    for position in [None, Some(0)] {
        let mut work = one(&album[3]);
        let held = &mut work.movements[0];
        held.disc_number = position;
        held.track_number = position;
        held.track.duration_secs = None;
        held.explicit = true;
        held.taste_pm = 1730;
        held.genres = vec!["choral".into()];
        let mut preserved = held.clone();
        preserved.disc_number = Some(1);
        preserved.track_number = Some(4);
        preserved.track.duration_secs = Some(120);
        let full = expand_work(&work, &album).unwrap();
        assert_eq!(uris(&full), expected);
        assert_eq!(full.movements[3], preserved);
        assert_eq!(full.total_secs, 960);
        assert!(expand_work(&full, &album).is_none());
    }
}

#[test]
fn a_unique_uri_repairs_stale_positions_without_losing_other_sections() {
    let album = two_takes();
    let mut work = one(&album[11]);
    work.movements[0].disc_number = Some(1);
    work.movements[0].track_number = Some(1);
    work.movements[0].track.duration_secs = Some(99);
    let full = expand_work(&work, &album).unwrap();
    let expected: Vec<&str> = album[8..].iter().map(|t| t.source_uri.as_str()).collect();
    assert_eq!(uris(&full), expected);
    assert_eq!(full.movements[3].disc_number, Some(2));
    assert_eq!(full.movements[3].track_number, Some(4));
    assert_eq!(full.movements[3].track.duration_secs, Some(99));
    assert_eq!(full.total_secs, 939);
}

#[test]
fn an_all_liked_recording_is_confirmed_without_inventing_saves_or_expanded_tracks() {
    let album = sections();
    let items: Vec<LibraryItem> = album.iter().map(item).collect();
    let work = works_of(&items).pop().unwrap();
    assert_eq!(work.completeness, Completeness::Unknown);
    let full = expand_work(&work, &album).unwrap();
    assert_eq!(
        full.movements, work.movements,
        "all original metadata survives"
    );
    assert_eq!(full.completeness, Completeness::Complete);
    assert!(full.movements.iter().all(|m| m.origin.is_liked() && !m.expanded));
    assert!(expand_work(&full, &album).is_none());
}

#[test]
fn cache_reconstruction_recovers_an_all_liked_work_with_unknown_coordinates() {
    let album = sections();
    let items: Vec<LibraryItem> = album.iter().map(item).collect();
    let mut work = works_of(&items).pop().unwrap();
    for movement in &mut work.movements {
        movement.disc_number = None;
        movement.track_number = None;
    }
    let mut store = MemStore::default();
    store.save_album_tracks(ALBUM, &album, NOW).unwrap();
    let restored = complete_from_cache(&store, vec![work]).unwrap();
    assert_eq!(restored[0].completeness, Completeness::Complete);
    let positions: Vec<Option<u32>> = restored[0]
        .movements
        .iter()
        .map(|m| m.track_number)
        .collect();
    assert_eq!(positions, (1..=8).map(Some).collect::<Vec<_>>());
    assert_eq!(complete_from_cache(&store, restored.clone()).unwrap(), restored);
}

#[test]
fn missing_or_duplicate_catalogue_positions_cannot_authorize_reconstruction() {
    let original = sections();
    let work = one(&original[3]);
    for (index, number) in [(0, 0), (1, 1)] {
        let mut album = original.clone();
        album[index].track_number = number;
        assert!(expand_work(&work, &album).is_none());
    }
}

#[test]
fn two_held_aliases_of_one_album_slot_are_not_silently_discarded() {
    let album = sections();
    let mut work = one(&album[3]);
    let mut alias = work.movements[0].clone();
    alias.track.source_uri = "spotify:track:AnotherRelink".into();
    alias.explicit = true;
    work.movements.push(alias);
    assert!(expand_work(&work, &album).is_none());
    let mut store = MemStore::default();
    store.save_album_tracks(ALBUM, &album, NOW).unwrap();
    assert_eq!(
        complete_from_cache(&store, vec![work.clone()]).unwrap(),
        [work]
    );
}

#[test]
fn a_numbering_gap_remains_partial_even_after_reading_the_album() {
    let sections = sections();
    let album = [sections[0].clone(), sections[7].clone()];
    let full = expand_work(&one(&album[0]), &album).unwrap();
    assert_eq!(full.movements.len(), 2);
    assert_eq!(full.completeness, Completeness::Partial);
    assert!(full.needs_expansion());
}

#[test]
fn a_complete_all_liked_recording_is_confirmed_through_the_real_album_reader() {
    let (spotify, cache, data_dir) = authorized("expand-confirm-all-liked");
    spotify.state.lock().unwrap().rate_limit_albums = 0;
    let endpoints = spotify.endpoints();
    let items: Vec<LibraryItem> = fixture_album()
        .into_iter()
        .take(5)
        .map(|track| LibraryItem {
            title: track.title,
            disc_number: Some(track.disc_number),
            track_number: Some(track.track_number),
            duration_secs: track.duration_secs,
            ..liked(&track.source_uri, ALBUM)
        })
        .collect();
    let original = works_of(&items);
    assert_eq!(original[0].completeness, Completeness::Unknown);
    let (confirmed, report, restored) = runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints);
        let mut store = MemStore::default();
        let (confirmed, report) = complete_works(
            &mut session,
            &cx,
            &mut store,
            original,
            &DjConfig::default(),
            NOW,
            &mut AlbumMisses::default(),
        )
        .await;
        let restored = complete_from_cache(&store, works_of(&items)).unwrap();
        (confirmed, report, restored)
    });
    assert_eq!((report.completed, report.fetched), (1, 1));
    assert!(report.failed.is_empty());
    assert_eq!(report.halted, None);
    assert_eq!(confirmed[0].completeness, Completeness::Complete);
    assert!(
        confirmed[0]
            .movements
            .iter()
            .all(|m| m.origin.is_liked() && !m.expanded)
    );
    assert_eq!(restored, confirmed, "restart has the same complete work");
    assert_eq!(requests(&spotify.state, "/v1/albums/"), 2);
    spotify.stop();
    std::fs::remove_dir_all(data_dir).unwrap();
}
