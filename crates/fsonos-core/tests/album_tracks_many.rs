//! `Store::album_tracks_many` reads several albums in one pass (sp4f.5): the
//! pool build completes partly-held works from the cache without a query per
//! work. The SQLite store overrides the default with a single `IN` query; this
//! checks it returns exactly what reading each album on its own does.

use fsonos_core::store::{AlbumTrack, SqliteStore, Store};

fn track(n: u32) -> AlbumTrack {
    AlbumTrack {
        disc_number: 1,
        track_number: n,
        source_uri: format!("spotify:track:FakeTrack{n:013}"),
        title: format!("Movement {n}"),
        duration_secs: Some(100 + n),
    }
}

#[test]
fn album_tracks_many_matches_reading_each_album_on_its_own() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let a = "spotify:album:FakeAlbumAAAAAAAAAAAA1";
    let b = "spotify:album:FakeAlbumBBBBBBBBBBBB2";
    let c = "spotify:album:FakeAlbumCCCCCCCCCCCC3";
    store.save_album_tracks(a, &[track(1), track(2)], 111).unwrap();
    store.save_album_tracks(b, &[track(3)], 222).unwrap();
    store
        .save_album_tracks(c, &[track(4), track(5), track(6)], 333)
        .unwrap();

    // One call returns every cached album asked for; an absent URI is omitted,
    // not an error, and order within an album is disc then track.
    let many = store
        .album_tracks_many(&[a, b, "spotify:album:FakeAlbumMissing00001", c])
        .unwrap();
    let by: std::collections::HashMap<_, _> = many.into_iter().collect();
    assert_eq!(by.len(), 3, "the missing album is omitted: {by:?}");
    for uri in [a, b, c] {
        assert_eq!(
            by.get(uri).cloned(),
            store.album_tracks(uri).unwrap(),
            "batch read differs from the per-album read for {uri}"
        );
    }
    assert_eq!(by[c].tracks.len(), 3);
    assert_eq!(by[c].fetched_at, 333);

    // Nothing asked, nothing read (no malformed empty `IN` query).
    assert!(store.album_tracks_many(&[]).unwrap().is_empty());
}
