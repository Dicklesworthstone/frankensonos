//! Ordered playlist observations have identical memory and durable semantics.

use fsonos_core::store::{CachedPlaylist, MemStore, PlaylistTrack, SqliteStore, Store};

const PLAYLIST: &str = "spotify:playlist:ObservedList";

fn track(position: u32, id: &str) -> PlaylistTrack {
    PlaylistTrack {
        position,
        source_uri: format!("spotify:track:{id}"),
        title: format!("Track {id}"),
        artist: Some("Observed artist".into()),
        album: Some("Observed album".into()),
        album_uri: Some("spotify:album:ObservedAlbum".into()),
        disc_number: Some(2),
        track_number: Some(3),
        duration_secs: Some(181),
        explicit: true,
    }
}

fn observed(at: i64, complete: bool) -> CachedPlaylist {
    CachedPlaylist::new(
        PLAYLIST.into(),
        at,
        Some(if complete { 5 } else { 8 }),
        5,
        complete,
        vec![track(1, "Repeated"), track(5, "Repeated")],
    )
}

fn check(store: &mut dyn Store) {
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), None);
    let first = observed(100, true);
    store.save_playlist_contents(&first).unwrap();
    assert_eq!(
        store.playlist_contents(PLAYLIST).unwrap(),
        Some(first.clone())
    );
    assert!(
        store.library().unwrap().is_empty(),
        "metadata stands on its own"
    );
    assert!(
        store.playlists().unwrap().is_empty(),
        "listing is independent"
    );
    store
        .save_playlists(&[(PLAYLIST.into(), "Renamed".into())])
        .unwrap();
    assert_eq!(
        store.playlist_contents(PLAYLIST).unwrap(),
        Some(first.clone())
    );

    let partial = observed(200, false);
    store.save_playlist_contents(&partial).unwrap();
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(partial));

    let empty = CachedPlaylist::new(PLAYLIST.into(), 300, Some(0), 0, true, Vec::new());
    store.save_playlist_contents(&empty).unwrap();
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(empty));
    assert_eq!(
        store.playlist_contents("spotify:playlist:Unknown").unwrap(),
        None
    );

    store.save_playlist_contents(&first).unwrap();
    let bad_positions = CachedPlaylist::new(
        PLAYLIST.into(),
        400,
        Some(5),
        5,
        true,
        vec![track(2, "One"), track(2, "Another")],
    );
    let bad_total = CachedPlaylist::new(PLAYLIST.into(), 400, Some(6), 5, true, Vec::new());
    let outside = CachedPlaylist::new(
        PLAYLIST.into(),
        400,
        Some(5),
        5,
        true,
        vec![track(6, "Outside")],
    );
    let mut changed = first.clone();
    changed.tracks[0].title = "Unrevisioned change".into();
    for bad in [bad_positions, bad_total, outside, changed] {
        assert!(store.save_playlist_contents(&bad).is_err());
        assert_eq!(
            store.playlist_contents(PLAYLIST).unwrap(),
            Some(first.clone())
        );
    }
}

#[test]
fn memory_preserves_occurrences_metadata_coverage_and_empty_evidence() {
    check(&mut MemStore::default());
}

#[test]
fn sqlite_preserves_occurrences_metadata_coverage_and_empty_evidence() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    check(&mut store);
    store.close().unwrap();
}

#[test]
fn observation_survives_database_reopen_with_its_exact_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("playlist.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let contents = observed(100, false);
    store.save_playlist_contents(&contents).unwrap();
    store.close().unwrap();
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(contents));
    store.close().unwrap();
}

#[test]
fn revision_identifies_the_entire_observation_including_order_and_coverage() {
    let first = observed(100, true);
    assert_eq!(first.revision, observed(100, true).revision);
    assert_ne!(first.revision, observed(101, true).revision);
    assert_ne!(first.revision, observed(100, false).revision);
    let variants = [
        vec![track(1, "Repeated"), track(4, "Repeated")],
        vec![track(1, "Different"), track(5, "Repeated")],
        vec![track(1, "Repeated")],
    ];
    for tracks in variants {
        let changed = CachedPlaylist::new(PLAYLIST.into(), 100, Some(5), 5, true, tracks);
        assert_ne!(first.revision, changed.revision);
    }
    let roundtrip: CachedPlaylist =
        serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
    assert_eq!(roundtrip, first);
}

#[test]
fn long_observations_are_bounded_by_raw_entries_and_aggregate_metadata() {
    let mut store = MemStore::default();
    let long = CachedPlaylist::new(
        PLAYLIST.into(),
        100,
        Some(250),
        250,
        true,
        (1..=250)
            .map(|position| track(position, "Repeated"))
            .collect(),
    );
    store.save_playlist_contents(&long).unwrap();
    assert_eq!(
        store.playlist_contents(PLAYLIST).unwrap(),
        Some(long.clone())
    );
    let beyond = fsonos_core::store::MAX_PLAYLIST_ENTRIES + 1;
    let unavailable =
        CachedPlaylist::new(PLAYLIST.into(), 200, Some(beyond), beyond, true, Vec::new());
    assert!(store.save_playlist_contents(&unavailable).is_err());
    let large = CachedPlaylist::new(
        PLAYLIST.into(),
        200,
        Some(350),
        350,
        true,
        (1..=350)
            .map(|position| {
                let mut item = track(position, "Repeated");
                item.title = "t".repeat(16 * 1024);
                item.artist = Some("a".repeat(16 * 1024));
                item.album = Some("b".repeat(16 * 1024));
                item
            })
            .collect(),
    );
    assert!(store.save_playlist_contents(&large).is_err());
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(long));
}
