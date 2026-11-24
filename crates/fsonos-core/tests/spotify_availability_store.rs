//! Accepted denials belong to one Spotify account and cannot be erased by
//! cached metadata, stale publications, failed transactions or daemon restarts.

use std::collections::HashSet;

use fsonos_core::store::{
    CachedPlaylist, LibraryEntry, LibraryOrigin, MAX_SPOTIFY_AVAILABILITY_ENTRIES, MemStore,
    PlaylistTrack, SpotifyArtistDiscoveryFence, SpotifyAvailability, SpotifyLibraryOwner,
    SpotifyPlaylistPublication, SpotifyPlaylistPublicationFence, SpotifyPlaylistPublicationItem,
    SqliteStore, Store,
};
use fsonos_types::Track;

const ACCOUNT: &str = "availability-owner";
const URI: &str = "spotify:track:UnavailableMovement";
const PLAYLIST: &str = "spotify:playlist:CachedTaste";
const PROOF: &str = "verified saved-library read";

fn entry() -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: URI.into(),
            title: "Concerto: II. Andante".into(),
            artist: Some("Fixture Composer".into()),
            album: Some("Fixture Concerto".into()),
            duration_secs: Some(180),
            uri: None,
        },
        candidate: Some(true),
        origin: LibraryOrigin::Taste,
        ..LibraryEntry::default()
    }
}

fn delta(available: &[&str], unavailable: &[&str]) -> SpotifyAvailability {
    SpotifyAvailability {
        available: available.iter().map(|uri| (*uri).into()).collect(),
        unavailable: unavailable.iter().map(|uri| (*uri).into()).collect(),
    }
}

fn stage(store: &mut dyn Store, owner: &SpotifyLibraryOwner) {
    assert!(
        store
            .save_spotify_library_progress(ACCOUNT, Some(owner), None, PROOF)
            .unwrap()
    );
}

fn publish(
    store: &mut dyn Store,
    owner: &SpotifyLibraryOwner,
    entries: &[LibraryEntry],
    availability: Option<&SpotifyAvailability>,
) -> bool {
    store
        .publish_spotify_library_from_progress(
            owner,
            PROOF,
            entries,
            None,
            &[],
            None,
            None,
            None,
            availability,
        )
        .unwrap()
}

fn seed_denial(store: &mut dyn Store) -> SpotifyLibraryOwner {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    let clock = store.library_catalog_revision().unwrap().unwrap();
    stage(store, &owner);
    assert!(publish(store, &owner, &[], Some(&delta(&[], &[URI]))));
    assert_eq!(
        store.spotify_unavailable_tracks().unwrap(),
        HashSet::from([URI.into()])
    );
    assert!(!store.library().unwrap()[0].is_candidate());
    assert_eq!(store.library_catalog_revision().unwrap(), Some(clock + 1));
    owner
}

fn retention_and_recovery(store: &mut dyn Store) {
    let owner = seed_denial(store);
    // An ordinary cache upsert and legacy/full publication are not proof that
    // a previously unavailable movement is currently playable.
    store.upsert_library(&[entry()]).unwrap();
    assert!(
        store
            .publish_spotify_library(&owner, &[entry()], None, &[], None)
            .unwrap()
    );
    stage(store, &owner);
    assert!(publish(store, &owner, &[entry()], None));
    assert!(!store.library().unwrap()[0].is_candidate());
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));

    // A repeated denial or absence changes no public catalog evidence.
    let clock = store.library_catalog_revision().unwrap();
    stage(store, &owner);
    assert!(publish(store, &owner, &[], Some(&delta(&[URI], &[URI]))));
    assert_eq!(store.library_catalog_revision().unwrap(), clock);
    stage(store, &owner);
    assert!(publish(
        store,
        &owner,
        &[],
        Some(&SpotifyAvailability::default())
    ));
    assert_eq!(store.library_catalog_revision().unwrap(), clock);

    // Only a fresh positive clears the denial. It does not invent a candidate
    // without fresh metadata; a later explicit row can now be admitted.
    stage(store, &owner);
    assert!(publish(store, &owner, &[], Some(&delta(&[URI], &[]))));
    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
    assert!(!store.library().unwrap()[0].is_candidate());
    assert_eq!(
        store.library_catalog_revision().unwrap(),
        clock.map(|value| value + 1)
    );
    store.upsert_library(&[entry()]).unwrap();
    assert!(store.library().unwrap()[0].is_candidate());

    // Album completion can supply a movement absent from the library itself.
    // Its new denial must still invalidate a warm candidate pool.
    let before = store.library().unwrap();
    let clock = store.library_catalog_revision().unwrap().unwrap();
    stage(store, &owner);
    assert!(publish(
        store,
        &owner,
        &[],
        Some(&delta(&[], &["spotify:track:CachedAlbumCompanion"]))
    ));
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.library_catalog_revision().unwrap(), Some(clock + 1));
}

#[test]
fn memory_preserves_denials_until_explicit_fresh_availability() {
    retention_and_recovery(&mut MemStore::default());
}

#[test]
fn sqlite_preserves_denials_until_explicit_fresh_availability() {
    retention_and_recovery(&mut SqliteStore::open_in_memory().unwrap());
}

fn continuations_cannot_restore_denied_candidates(store: &mut dyn Store) {
    let owner = seed_denial(store);
    let clock = store.library_catalog_revision().unwrap();
    let artist = SpotifyArtistDiscoveryFence {
        expected: None,
        progress: "accepted artist batch".into(),
    };
    assert!(
        store
            .publish_spotify_artist_continuation(&owner, &artist, &[entry()])
            .unwrap()
    );
    assert!(!store.library().unwrap()[0].is_candidate());
    assert_eq!(store.library_catalog_revision().unwrap(), clock);

    assert!(
        store
            .save_spotify_playlist_progress(&owner, PLAYLIST, None, "playlist read")
            .unwrap()
    );
    let observation = CachedPlaylist::new(
        PLAYLIST.into(),
        20,
        Some(1),
        1,
        true,
        vec![PlaylistTrack {
            position: 1,
            source_uri: URI.into(),
            title: entry().track.title,
            artist: None,
            album: None,
            album_uri: None,
            duration_secs: Some(180),
            disc_number: Some(1),
            track_number: Some(2),
            explicit: false,
        }],
    );
    let publication = SpotifyPlaylistPublication {
        entries: vec![entry()],
        observations: vec![SpotifyPlaylistPublicationItem {
            contents: observation,
            expected_progress: "playlist read".into(),
            applied_progress: "playlist accepted".into(),
        }],
        ..SpotifyPlaylistPublication::default()
    };
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert!(!store.library().unwrap()[0].is_candidate());
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert_eq!(
        store
            .playlist_progress(ACCOUNT, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some("playlist accepted")
    );
}

#[test]
fn both_backends_keep_artist_and_playlist_continuations_behind_the_denial() {
    continuations_cannot_restore_denied_candidates(&mut MemStore::default());
    continuations_cannot_restore_denied_candidates(&mut SqliteStore::open_in_memory().unwrap());
}

fn stale_publications_and_invalid_evidence_are_atomic(store: &mut dyn Store) {
    let owner = seed_denial(store);
    stage(store, &owner);
    let before = (
        store.library().unwrap(),
        store.library_catalog_revision().unwrap(),
    );
    let clearing = delta(&[URI], &[]);
    let artist = SpotifyArtistDiscoveryFence {
        expected: Some("stale artist receipt".into()),
        progress: "new artist receipt".into(),
    };
    let playlists = SpotifyPlaylistPublicationFence {
        expected_directory: Some("stale directory receipt".into()),
        ..SpotifyPlaylistPublicationFence::default()
    };
    for (proof, playlist_fence, artist_fence) in [
        ("stale saved read", None, None),
        (PROOF, Some(&playlists), None),
        (PROOF, None, Some(&artist)),
    ] {
        assert!(
            !store
                .publish_spotify_library_from_progress(
                    &owner,
                    proof,
                    &[entry()],
                    None,
                    &[],
                    None,
                    playlist_fence,
                    artist_fence,
                    Some(&clearing),
                )
                .unwrap()
        );
    }
    for uri in [
        "",
        "spotify:album:WrongKind",
        "spotify:track:",
        "spotify:track:bad/id",
        "spotify:track:é",
    ] {
        assert!(
            store
                .publish_spotify_library_from_progress(
                    &owner,
                    PROOF,
                    &[entry()],
                    None,
                    &[],
                    None,
                    None,
                    None,
                    Some(&delta(&[URI], &[uri])),
                )
                .is_err()
        );
    }
    let mut invalid = CachedPlaylist::new(PLAYLIST.into(), 20, Some(0), 0, true, Vec::new());
    invalid.revision = "wrong revision".into();
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                PROOF,
                &[entry()],
                None,
                &[invalid],
                None,
                None,
                None,
                Some(&clearing),
            )
            .is_err()
    );
    assert_eq!(
        (
            store.library().unwrap(),
            store.library_catalog_revision().unwrap()
        ),
        before
    );
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF)
    );
    assert!(publish(store, &owner, &[entry()], Some(&clearing)));
    assert!(store.library().unwrap()[0].is_candidate());
}

#[test]
fn both_backends_fence_denials_with_saved_playlist_and_artist_evidence() {
    stale_publications_and_invalid_evidence_are_atomic(&mut MemStore::default());
    stale_publications_and_invalid_evidence_are_atomic(&mut SqliteStore::open_in_memory().unwrap());
}

#[test]
fn oversized_availability_does_not_consume_a_completed_proof() {
    let mut store = MemStore::default();
    let owner = seed_denial(&mut store);
    stage(&mut store, &owner);
    let oversized = SpotifyAvailability {
        available: vec![String::new(); MAX_SPOTIFY_AVAILABILITY_ENTRIES + 1],
        unavailable: Vec::new(),
    };
    let failure = store
        .publish_spotify_library_from_progress(
            &owner,
            PROOF,
            &[],
            None,
            &[],
            None,
            None,
            None,
            Some(&oversized),
        )
        .unwrap_err();
    assert!(failure.to_string().contains("too large"));
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF)
    );
}

#[test]
fn sqlite_denials_survive_restart_and_clear_only_with_a_new_adoption() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("availability.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let previous = seed_denial(&mut store);
    store.close().unwrap();
    let mut store = SqliteStore::open(&file).unwrap();
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert_eq!(
        store.adopt_spotify_library_owner(ACCOUNT).unwrap(),
        (previous.clone(), false)
    );
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    store
        .adopt_spotify_library_owner("different-owner")
        .unwrap();
    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
    let current = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    stage(&mut store, &current);
    assert!(!publish(
        &mut store,
        &previous,
        &[],
        Some(&delta(&[], &[URI]))
    ));
    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
    assert!(publish(
        &mut store,
        &current,
        &[entry()],
        Some(&delta(&[URI], &[]))
    ));
    assert!(store.library().unwrap()[0].is_candidate());
    store.close().unwrap();
}

#[test]
fn sqlite_failure_rolls_back_denials_candidates_clock_and_consumption() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("availability-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let owner = seed_denial(&mut store);
    stage(&mut store, &owner);
    let before = (
        store.library().unwrap(),
        store.library_catalog_revision().unwrap(),
    );
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    raw.execute_sync("DROP TABLE spotify_heard").unwrap();
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                PROOF,
                &[entry()],
                None,
                &[],
                Some(&[]),
                None,
                None,
                Some(&delta(&[URI], &[])),
            )
            .is_err()
    );
    assert_eq!(
        (
            store.library().unwrap(),
            store.library_catalog_revision().unwrap()
        ),
        before
    );
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF)
    );
}
