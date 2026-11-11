//! Background artist samples publish additively with the exact durable cohort
//! receipt. Memory and SQLite must agree, including across competing readers.

use fsonos_core::store::{
    LibraryEntry, LibraryOrigin, MAX_SPOTIFY_ARTIST_CONTINUATION_ENTRIES,
    MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES, MemStore, SpotifyArtistDiscoveryFence,
    SpotifyLibraryOwner, SqliteStore, Store,
};
use fsonos_types::Track;

const ACCOUNT: &str = "artist-continuation-owner";

fn sample(name: &str, weight: u32) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: format!("spotify:track:{name}"),
            title: name.into(),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            duration_secs: Some(180),
            uri: None,
        },
        origin: LibraryOrigin::Taste,
        candidate: Some(true),
        taste_weight: Some(weight),
        artist_id: Some("Artist".into()),
        album_uri: Some("spotify:album:Album".into()),
        added: 20,
        ..LibraryEntry::default()
    }
}

fn fence(expected: Option<&str>, progress: &str) -> SpotifyArtistDiscoveryFence {
    SpotifyArtistDiscoveryFence {
        expected: expected.map(str::to_owned),
        progress: progress.into(),
    }
}

fn publish(
    store: &mut dyn Store,
    owner: &SpotifyLibraryOwner,
    expected: Option<&str>,
    progress: &str,
    entries: &[LibraryEntry],
) -> bool {
    store
        .publish_spotify_artist_continuation(owner, &fence(expected, progress), entries)
        .unwrap()
}

fn revision(store: &dyn Store) -> u64 {
    store.library_catalog_revision().unwrap().unwrap()
}

fn additive_merge(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    let mut saved = sample("Saved", 1200);
    saved.origin = LibraryOrigin::Both;
    saved.track.title = "Saved metadata".into();
    saved.genres = vec!["saved genre".into()];
    saved.explicit = true;
    saved.added = 5;
    let mut taste = sample("ExistingTaste", 900);
    taste.track.title = "Current taste metadata".into();
    let mut retired = sample("Retired", 1300);
    retired.origin = LibraryOrigin::LikedTrack;
    retired.candidate = Some(false);
    let mut neutral = sample("LegacyNeutral", 1000);
    neutral.taste_weight = None;
    let untouched = sample("Untouched", 1500);
    store
        .upsert_library(&[
            saved.clone(),
            taste.clone(),
            retired,
            neutral.clone(),
            untouched.clone(),
        ])
        .unwrap();
    store
        .save_playlists(&[("spotify:playlist:Kept".into(), "Kept".into())])
        .unwrap();
    store
        .save_spotify_heard(&[("spotify:track:Heard".into(), 10)])
        .unwrap();
    let before = revision(store);
    let mut new = sample("New", 1100);
    new.track.uri = Some("x-sonos-spotify:account-specific-renderer".into());
    let rows = [
        sample("Saved", 1100),
        sample("ExistingTaste", 1100),
        sample("Retired", 1100),
        sample("LegacyNeutral", 1000),
        new,
    ];
    assert!(publish(store, &owner, None, "first batch", &rows));
    assert_eq!(revision(store), before + 1);
    let all = store.library().unwrap();
    let row = |name: &str| {
        all.iter()
            .find(|row| row.track.source_uri == format!("spotify:track:{name}"))
            .unwrap()
    };
    assert_eq!(row("Saved"), &saved);
    taste.taste_weight = Some(1100);
    assert_eq!(row("ExistingTaste"), &taste);
    assert_eq!(row("Retired"), &sample("Retired", 1100));
    assert_eq!(row("LegacyNeutral"), &neutral);
    assert_eq!(row("Untouched"), &untouched);
    assert_eq!(row("New"), &sample("New", 1100));
    assert_eq!(
        store.playlists().unwrap(),
        [("spotify:playlist:Kept".into(), "Kept".into())]
    );
    assert_eq!(
        store.spotify_heard().unwrap(),
        [("spotify:track:Heard".into(), 10)]
    );
    assert!(publish(
        store,
        &owner,
        Some("first batch"),
        "second batch",
        &rows
    ));
    assert_eq!(store.library().unwrap(), all);
    assert_eq!(
        revision(store),
        before + 1,
        "receipt-only progress is not a catalog change"
    );
    assert!(publish(store, &owner, Some("second batch"), "finished", &[]));
    assert_eq!(revision(store), before + 1);
}

#[test]
fn memory_merges_samples_without_replacing_saved_metadata_or_weaker_weights() {
    additive_merge(&mut MemStore::default());
}

#[test]
fn sqlite_merges_samples_without_replacing_saved_metadata_or_weaker_weights() {
    additive_merge(&mut SqliteStore::open_in_memory().unwrap());
}

fn competing_publications(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    assert!(
        store
            .save_spotify_library_progress(ACCOUNT, Some(&owner), None, "full read")
            .unwrap()
    );
    assert!(publish(
        store,
        &owner,
        None,
        "background batch",
        &[sample("Added", 1100)]
    ));
    let before = store.library().unwrap();
    let clock = revision(store);
    assert!(!publish(
        store,
        &owner,
        None,
        "stale",
        &[sample("Stale", 1100)]
    ));
    assert!(
        !store
            .publish_spotify_library_from_progress(
                &owner,
                "full read",
                &[sample("OldSnapshot", 1000)],
                None,
                &[],
                None,
                None,
                Some(&fence(None, "full publication")),
            )
            .unwrap()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(revision(store), clock);
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some("full read")
    );
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                "full read",
                &[sample("CurrentSnapshot", 1000)],
                None,
                &[],
                None,
                None,
                Some(&fence(Some("background batch"), "new cohort")),
            )
            .unwrap()
    );
    let before = store.library().unwrap();
    let clock = revision(store);
    assert!(!publish(
        store,
        &owner,
        Some("background batch"),
        "late sample",
        &[sample("Late", 1100)]
    ));
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(revision(store), clock);
    store.adopt_spotify_library_owner("other-owner").unwrap();
    let returned = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    assert_ne!(owner.generation, returned.generation);
    let before = store.library().unwrap();
    let clock = revision(store);
    assert!(!publish(
        store,
        &owner,
        None,
        "old adoption",
        &[sample("OldOwner", 1100)]
    ));
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(revision(store), clock);
    assert_eq!(store.spotify_artist_discovery(&returned).unwrap(), None);
    assert!(publish(
        store,
        &returned,
        None,
        "returned cohort",
        &[sample("Returned", 1100)]
    ));
}

#[test]
fn memory_fences_competing_full_reads_batches_and_returning_accounts() {
    competing_publications(&mut MemStore::default());
}

#[test]
fn sqlite_fences_competing_full_reads_batches_and_returning_accounts() {
    competing_publications(&mut SqliteStore::open_in_memory().unwrap());
}

fn reject_invalid(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    let large_cohort = "x".repeat(17 * 1024);
    let clock = revision(store);
    assert!(
        publish(store, &owner, None, &large_cohort, &[]),
        "cohorts can exceed the former cursor-only limit"
    );
    assert_eq!(revision(store), clock);
    for unchanged in ["", large_cohort.as_str()] {
        assert!(
            store
                .publish_spotify_artist_continuation(
                    &owner,
                    &fence(Some(&large_cohort), unchanged),
                    &[sample("Unfenced", 1100)],
                )
                .is_err()
        );
        assert!(store.library().unwrap().is_empty());
    }
    let mut bad = Vec::new();
    let mut saved = sample("Saved", 1100);
    saved.origin = LibraryOrigin::SavedAlbum;
    bad.push(vec![saved]);
    let mut retired = sample("Retired", 1100);
    retired.candidate = Some(false);
    bad.push(vec![retired]);
    let mut malformed = sample("Malformed", 1100);
    malformed.track.source_uri = "spotify:episode:WrongKind".into();
    bad.push(vec![malformed]);
    let mut oversized = sample("Oversized", 1100);
    oversized.track.title = "x".repeat(16 * 1024 + 1);
    bad.push(vec![oversized]);
    let mut genres = sample("Genres", 1100);
    genres.genres = vec!["".into(); 257];
    bad.push(vec![genres]);
    bad.push(vec![sample("Duplicate", 1100); 2]);
    bad.push(
        (0..=MAX_SPOTIFY_ARTIST_CONTINUATION_ENTRIES)
            .map(|n| sample(&format!("Batch{n}"), 1100))
            .collect(),
    );
    for entries in bad {
        assert!(
            store
                .publish_spotify_artist_continuation(
                    &owner,
                    &fence(Some(&large_cohort), "invalid"),
                    &entries,
                )
                .is_err()
        );
        assert!(store.library().unwrap().is_empty());
        assert_eq!(
            store.spotify_artist_discovery(&owner).unwrap().as_deref(),
            Some(large_cohort.as_str())
        );
        assert_eq!(revision(store), clock);
    }
    let too_large = "x".repeat(MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES + 1);
    assert!(
        store
            .publish_spotify_artist_continuation(
                &owner,
                &fence(Some(&large_cohort), &too_large),
                &[],
            )
            .is_err()
    );
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some(large_cohort.as_str())
    );
    let max_batch: Vec<_> = (0..MAX_SPOTIFY_ARTIST_CONTINUATION_ENTRIES)
        .map(|n| sample(&format!("Batch{n}"), 1100))
        .collect();
    assert!(publish(
        store,
        &owner,
        Some(&large_cohort),
        "valid maximum batch",
        &max_batch
    ));
    assert_eq!(
        store.library().unwrap().len(),
        MAX_SPOTIFY_ARTIST_CONTINUATION_ENTRIES
    );
    assert_eq!(revision(store), clock + 1);
}

#[test]
fn memory_bounds_the_cohort_and_candidate_batch_before_any_write() {
    reject_invalid(&mut MemStore::default());
}

#[test]
fn sqlite_bounds_the_cohort_and_candidate_batch_before_any_write() {
    reject_invalid(&mut SqliteStore::open_in_memory().unwrap());
}

#[test]
fn sqlite_reopen_and_a_second_writer_keep_exact_cohort_and_catalog_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("artist-batches.sqlite");
    let mut first = SqliteStore::open(&path).unwrap();
    let owner = first.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    assert!(publish(
        &mut first,
        &owner,
        None,
        "first",
        &[sample("First", 1100)]
    ));
    first.close().unwrap();
    let mut first = SqliteStore::open(&path).unwrap();
    let mut delayed = SqliteStore::open(&path).unwrap();
    let receipt = delayed.spotify_artist_discovery(&owner).unwrap().unwrap();
    let clock = revision(&delayed);
    assert!(publish(
        &mut first,
        &owner,
        Some(&receipt),
        "second",
        &[sample("Second", 1100)]
    ));
    assert_eq!(revision(&delayed), clock + 1);
    assert!(!publish(
        &mut delayed,
        &owner,
        Some(&receipt),
        "stale",
        &[sample("Stale", 1100)]
    ));
    assert_eq!(delayed.library().unwrap(), first.library().unwrap());
    assert_eq!(
        delayed.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("second")
    );
    assert_eq!(revision(&delayed), clock + 1);
    first.close().unwrap();
    delayed.close().unwrap();
}

#[test]
fn sqlite_failure_after_the_candidate_and_receipt_writes_rolls_both_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("artist-batch-rollback.sqlite");
    let mut store = SqliteStore::open(&path).unwrap();
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    assert!(publish(
        &mut store,
        &owner,
        None,
        "first",
        &[sample("First", 1100)]
    ));
    let before = store.library().unwrap();
    let raw = fsqlite::AsyncConnection::open_sync(path.to_string_lossy()).unwrap();
    // The revision write is last, after both candidates and the new receipt.
    raw.execute_sync("DROP TABLE library_catalog_clock").unwrap();
    assert!(
        store
            .publish_spotify_artist_continuation(
                &owner,
                &fence(Some("first"), "rejected"),
                &[sample("First", 1400), sample("Rejected", 1100)],
            )
            .is_err()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("first")
    );
}
