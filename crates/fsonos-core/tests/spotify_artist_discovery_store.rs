//! Artist-discovery rotation commits with the library it sampled, and never
//! consumes another refresh's progress or an account's earlier adoption.

use fsonos_core::store::{
    LibraryEntry, MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES, MemStore, SpotifyArtistDiscoveryFence,
    SpotifyLibraryOwner, SpotifyPlaylistPublicationFence, SqliteStore, Store,
};
use fsonos_types::Track;

const ACCOUNT: &str = "artist-discovery-owner";
const PROOF: &str = "complete-saved-library";

fn song(title: &str) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: format!("spotify:track:{title}"),
            title: title.into(),
            artist: None,
            album: None,
            uri: None,
            duration_secs: Some(180),
        },
        candidate: Some(true),
        ..LibraryEntry::default()
    }
}

fn stage(store: &mut dyn Store, owner: &SpotifyLibraryOwner) {
    assert!(
        store
            .save_spotify_library_progress(&owner.owner_id, Some(owner), None, PROOF,)
            .unwrap()
    );
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
    cursor: &SpotifyArtistDiscoveryFence,
    title: &str,
) -> bool {
    store
        .publish_spotify_library_from_progress(
            owner,
            PROOF,
            &[song(title)],
            None,
            &[],
            None,
            None,
            Some(cursor),
        )
        .unwrap()
}

fn publication_is_atomic(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    stage(store, &owner);
    assert_eq!(store.spotify_artist_discovery(&owner).unwrap(), None);
    assert!(publish(store, &owner, &fence(None, "first"), "First"));
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("first"),
    );
    assert!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .is_none()
    );
    let accepted = store.library().unwrap();

    stage(store, &owner);
    assert!(!publish(store, &owner, &fence(None, "stale"), "Stale"));
    assert_eq!(store.library().unwrap(), accepted);
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF),
    );
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("first"),
    );
    assert!(publish(
        store,
        &owner,
        &fence(Some("first"), "second"),
        "Second"
    ));
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("second"),
    );
    assert!(
        store
            .library()
            .unwrap()
            .iter()
            .any(|entry| entry.track.title == "Second")
    );
}

#[test]
fn memory_publishes_music_and_rotation_together() {
    publication_is_atomic(&mut MemStore::default());
}

#[test]
fn sqlite_publishes_music_and_rotation_together() {
    publication_is_atomic(&mut SqliteStore::open_in_memory().unwrap());
}

fn rejects_other_receipts(store: &mut dyn Store) {
    let first = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    stage(store, &first);
    assert!(publish(store, &first, &fence(None, "first"), "First"));
    assert_eq!(
        store.adopt_spotify_library_owner(ACCOUNT).unwrap(),
        (first.clone(), false)
    );
    assert_eq!(
        store.spotify_artist_discovery(&first).unwrap().as_deref(),
        Some("first")
    );

    store
        .adopt_spotify_library_owner("different-artist-owner")
        .unwrap();
    assert_eq!(store.spotify_artist_discovery(&first).unwrap(), None);
    let returned = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    assert_ne!(first, returned);
    assert_eq!(store.spotify_artist_discovery(&returned).unwrap(), None);
    stage(store, &returned);
    let before = store.library().unwrap();
    assert!(!publish(
        store,
        &first,
        &fence(None, "obsolete"),
        "Obsolete"
    ));
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.spotify_artist_discovery(&returned).unwrap(), None);
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&returned))
            .unwrap()
            .as_deref(),
        Some(PROOF),
    );

    let bad = fence(None, &"x".repeat(MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES + 1));
    assert!(
        store
            .publish_spotify_library_from_progress(
                &returned,
                PROOF,
                &[song("TooLarge")],
                None,
                &[],
                None,
                None,
                Some(&bad),
            )
            .is_err()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.spotify_artist_discovery(&returned).unwrap(), None);

    let valid = fence(None, "valid");
    let wrong_playlist = SpotifyPlaylistPublicationFence {
        expected_directory: Some("no-such-observation".into()),
        ..SpotifyPlaylistPublicationFence::default()
    };
    assert!(
        !store
            .publish_spotify_library_from_progress(
                &returned,
                PROOF,
                &[song("WrongPlaylist")],
                None,
                &[],
                None,
                Some(&wrong_playlist),
                Some(&valid),
            )
            .unwrap()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.spotify_artist_discovery(&returned).unwrap(), None);
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&returned))
            .unwrap()
            .as_deref(),
        Some(PROOF),
    );
    assert!(publish(store, &returned, &valid, "Valid"));
}

#[test]
fn memory_checks_account_bounds_and_other_publication_fences() {
    rejects_other_receipts(&mut MemStore::default());
}

#[test]
fn sqlite_checks_account_bounds_and_other_publication_fences() {
    rejects_other_receipts(&mut SqliteStore::open_in_memory().unwrap());
}

#[test]
fn sqlite_rotation_resumes_after_restart_and_rejects_a_delayed_writer() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("artist-rotation.db");
    let mut first = SqliteStore::open(&file).unwrap();
    let owner = first.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    stage(&mut first, &owner);
    assert!(publish(
        &mut first,
        &owner,
        &fence(None, "artist050"),
        "First"
    ));
    first.close().unwrap();

    let mut first = SqliteStore::open(&file).unwrap();
    let mut delayed = SqliteStore::open(&file).unwrap();
    let old = delayed.spotify_artist_discovery(&owner).unwrap();
    assert_eq!(old.as_deref(), Some("artist050"));
    stage(&mut first, &owner);
    assert!(publish(
        &mut first,
        &owner,
        &fence(old.as_deref(), "artist100"),
        "Second",
    ));
    // A fresh essential read does not make the delayed artist observation current.
    stage(&mut first, &owner);
    let before = first.library().unwrap();
    assert!(!publish(
        &mut delayed,
        &owner,
        &fence(old.as_deref(), "artist075"),
        "Delayed",
    ));
    assert_eq!(delayed.library().unwrap(), before);
    assert_eq!(
        delayed.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("artist100"),
    );
    assert_eq!(
        delayed
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF),
    );
}

#[test]
fn sqlite_failed_projection_keeps_rotation_and_library_progress() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("artist-publication-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    stage(&mut store, &owner);
    assert!(publish(&mut store, &owner, &fence(None, "first"), "First"));
    stage(&mut store, &owner);
    let before = store.library().unwrap();
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    raw.execute_sync("DROP TABLE spotify_heard").unwrap();

    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                PROOF,
                &[song("Rejected")],
                None,
                &[],
                Some(&[]),
                None,
                Some(&fence(Some("first"), "rejected")),
            )
            .is_err()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("first")
    );
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF),
    );
}
