//! A status observation may inherit progress; an actual read owns its result.

use super::*;

fn status(pending: usize) -> AlbumContinuation {
    AlbumContinuation {
        owner: Some("test-owner".into()),
        generation: Some(3),
        pending,
        ..AlbumContinuation::default()
    }
}

fn progressed(offset: u32) -> AlbumContinuation {
    let uri = "spotify:album:TestAlbum".to_owned();
    let mut partial = PartialAlbum::new(uri.clone(), now());
    let body = serde_json::json!({
        "offset": 0, "total": 50,
        "items": (0..offset).map(|number| serde_json::json!({
            "uri": format!("spotify:track:Partial{number}"),
            "name": format!("Movement {number}"),
            "disc_number": 1, "track_number": number + 1,
            "duration_ms": 180_000, "explicit": false
        })).collect::<Vec<_>>()
    });
    partial
        .ingest(
            &serde_json::to_vec(&body).unwrap(),
            &crate::client::Endpoints::default(),
        )
        .unwrap();
    AlbumContinuation {
        cursor: Some(uri),
        partial: Some(partial),
        advanced: usize::try_from(offset).unwrap(),
        ..status(2)
    }
}

#[test]
fn status_only_observations_keep_the_previous_offset_and_rotation() {
    let previous = progressed(8);
    let mut observed = status(3);
    observed.inherit_progress(&previous);
    assert_eq!(observed.partial, previous.partial);
    assert_eq!(observed.cursor, previous.cursor);
    assert_eq!(observed.pending, 3);
    assert_eq!(observed.advanced, 0);
}

#[test]
fn new_pages_cannot_be_rolled_back_by_an_older_scheduler_checkpoint() {
    let previous = progressed(8);
    let mut current = progressed(16);
    let expected = current.clone();
    current.inherit_progress(&previous);
    assert_eq!(current, expected);
}

#[test]
fn completed_or_failed_attempts_cannot_resurrect_an_older_partial() {
    let previous = progressed(8);
    for (pending, completed) in [(0, 1), (1, 1), (1, 0)] {
        let mut current = AlbumContinuation {
            completed,
            cursor: Some("spotify:album:LaterAlbum".into()),
            ..status(pending)
        };
        let expected = current.clone();
        current.inherit_progress(&previous);
        assert_eq!(current, expected);
    }
    let mut complete = status(0);
    complete.inherit_progress(&previous);
    assert!(complete.partial.is_none());
    assert!(complete.cursor.is_none());
}

#[test]
fn invalidated_or_other_owner_progress_is_never_inherited() {
    let previous = progressed(8);
    let mut old_generation = previous.clone();
    old_generation.generation = Some(2);
    let mut another_owner = previous.clone();
    another_owner.owner = Some("another-owner".into());
    let mut invalidated = previous;
    invalidated.requires_refresh = true;
    for previous in [old_generation, another_owner, invalidated] {
        let mut current = status(2);
        let expected = current.clone();
        current.inherit_progress(&previous);
        assert_eq!(current, expected);
    }
}

#[test]
fn saved_album_gaps_do_not_schedule_catalog_completion() {
    use crate::library::Origin;
    use fsonos_core::store::MemStore;

    let album_uri = "spotify:album:ObservedAlbum";
    let store = MemStore::default();
    for origin in [
        Origin::SavedAlbum,
        Origin::Both,
        Origin::LikedTrack,
        Origin::Taste,
    ] {
        let item = LibraryItem {
            source_uri: "spotify:track:ObservedMovement".into(),
            title: "Cello Suite No. 1 in G Major, BWV 1007: II. Allemande".into(),
            artists: vec!["Johann Sebastian Bach".into()],
            album: Some("Bach: Cello Suites".into()),
            album_uri: Some(album_uri.into()),
            disc_number: Some(1),
            track_number: Some(2),
            origin,
            ..LibraryItem::default()
        };
        let pending = inventory_for_items(&store, &[item], 1_000).unwrap().pending;
        if matches!(origin, Origin::SavedAlbum | Origin::Both) {
            assert!(pending.is_empty(), "{origin:?}");
        } else {
            assert_eq!(pending, [album_uri], "{origin:?}");
        }
    }
}

fn liked_movement(album: &str) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:{album}Movement2"),
        title: "Cello Suite No. 1 in G Major, BWV 1007: II. Allemande".into(),
        artists: vec!["Johann Sebastian Bach".into()],
        album: Some("Bach: Cello Suites".into()),
        album_uri: Some(format!("spotify:album:{album}")),
        disc_number: Some(1),
        track_number: Some(2),
        origin: crate::library::Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

fn cache_movement(store: &mut impl Store, item: &LibraryItem, at: i64) {
    let track = fsonos_core::store::AlbumTrack {
        disc_number: 1,
        track_number: 2,
        source_uri: item.source_uri.clone(),
        title: item.title.clone(),
        duration_secs: Some(120),
        explicit: Some(false),
    };
    store
        .save_album_tracks(item.album_uri.as_deref().unwrap(), &[track], at)
        .unwrap();
}

#[test]
fn expiry_boundary_and_invalid_timestamps_cannot_hide_due_album_reads() {
    use crate::expand::ALBUM_CACHE_MAX_AGE_SECS;
    use fsonos_core::store::MemStore;

    let at = 2 * ALBUM_CACHE_MAX_AGE_SECS + 100;
    let item = liked_movement("ExpiryAlbum");
    let uri = item.album_uri.as_deref().unwrap();
    for (fetched_at, next) in [
        (at - ALBUM_CACHE_MAX_AGE_SECS - 1, None),
        (at - ALBUM_CACHE_MAX_AGE_SECS, None),
        (at - ALBUM_CACHE_MAX_AGE_SECS + 1, Some(at + 1)),
        (at, Some(at + ALBUM_CACHE_MAX_AGE_SECS)),
        (-1, None),
        (0, None),
        (at + 1, None),
        (i64::MAX, None),
    ] {
        let mut store = MemStore::default();
        cache_movement(&mut store, &item, fetched_at);
        let observed = inventory_for_items(&store, std::slice::from_ref(&item), at).unwrap();
        assert_eq!(observed.recheck_at, next, "fetched_at={fetched_at}");
        if next.is_some() {
            assert!(observed.pending.is_empty(), "fetched_at={fetched_at}");
        } else {
            assert_eq!(observed.pending, [uri], "fetched_at={fetched_at}");
        }
    }
}

#[test]
fn inventory_retains_the_earliest_future_expiry_or_miss_alongside_due_albums() {
    use crate::expand::ALBUM_CACHE_MAX_AGE_SECS;
    use fsonos_core::store::MemStore;

    let at = 2 * ALBUM_CACHE_MAX_AGE_SECS;
    let due = liked_movement("ADueAlbum");
    let cached = liked_movement("BCachedAlbum");
    let missed = liked_movement("CMissingAlbum");
    let cached_uri = cached.album_uri.clone().unwrap();
    let missed_uri = missed.album_uri.clone().unwrap();
    let mut store = MemStore::default();
    cache_movement(&mut store, &cached, at - ALBUM_CACHE_MAX_AGE_SECS + 500);
    store
        .save_album_misses(&[(missed_uri.clone(), at + 100)])
        .unwrap();
    let items = [due.clone(), cached, missed, due.clone()];
    let first = inventory_for_items(&store, &items, at).unwrap();
    assert_eq!(first.pending, [due.album_uri.clone().unwrap()]);
    assert_eq!(first.recheck_at, Some(at + 100));

    let retry = inventory_for_items(&store, &items, at + 100).unwrap();
    assert_eq!(retry.pending, [due.album_uri.unwrap(), missed_uri]);
    assert_eq!(retry.recheck_at, Some(at + 500));
    let expired = inventory_for_items(&store, &items, at + 500).unwrap();
    assert!(expired.pending.contains(&cached_uri));
    assert_eq!(expired.pending.len(), 3);
    assert_eq!(expired.recheck_at, None);
}

#[test]
fn legacy_flags_need_a_refresh_even_when_the_timestamp_is_current() {
    use fsonos_core::store::MemStore;

    let at = 1_000_000;
    let item = liked_movement("LegacyFlags");
    let uri = item.album_uri.as_deref().unwrap();
    let mut store = MemStore::default();
    cache_movement(&mut store, &item, at);
    let mut tracks = store.album_tracks(uri).unwrap().unwrap().tracks;
    tracks[0].explicit = None;
    store.save_album_tracks(uri, &tracks, at).unwrap();
    let observed = inventory_for_items(&store, &[item], at).unwrap();
    assert_eq!(observed.pending, ["spotify:album:LegacyFlags"]);
    assert_eq!(observed.recheck_at, None);
}
