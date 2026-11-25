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
        expected_revision: None,
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
            .publish_spotify_artist_continuation(&owner, &artist, &[entry()], &[])
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

fn artist_denials_publish_before_metadata(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    let mut current = entry();
    current.origin = LibraryOrigin::LikedTrack;
    current.track.title = "Current saved metadata".into();
    current.taste_weight = Some(1400);
    store.upsert_library(&[current.clone()]).unwrap();
    let mut other = entry();
    other.track.source_uri = "spotify:track:PlayableCompanion".into();
    let clock = store.library_catalog_revision().unwrap().unwrap();
    assert!(
        store
            .publish_spotify_artist_continuation(
                &owner,
                &SpotifyArtistDiscoveryFence {
                    expected: None,
                    progress: "new artist observation".into(),
                },
                &[entry(), other.clone()],
                &[URI.into(), "spotify:track:AbsentAlbumCompanion".into()],
            )
            .unwrap()
    );
    current.candidate = Some(false);
    let rows = store.library().unwrap();
    assert!(rows.contains(&current), "retain current saved metadata");
    assert!(rows.contains(&other), "publish unrelated fresh samples");
    assert_eq!(store.library_catalog_revision().unwrap(), Some(clock + 1));
    assert_eq!(
        store.spotify_unavailable_tracks().unwrap(),
        HashSet::from([URI.into(), "spotify:track:AbsentAlbumCompanion".into()])
    );

    // A missing movement can live only in an album expansion; invalidate
    // warmed pools even when no library row or metadata is changed.
    let clock = store.library_catalog_revision().unwrap().unwrap();
    for (expected, progress) in [
        ("new artist observation", "denial only"),
        ("denial only", "same denial"),
    ] {
        assert!(
            store
                .publish_spotify_artist_continuation(
                    &owner,
                    &SpotifyArtistDiscoveryFence {
                        expected: Some(expected.into()),
                        progress: progress.into(),
                    },
                    &[],
                    &["spotify:track:AnotherMissingMovement".into()],
                )
                .unwrap()
        );
        assert_eq!(store.library().unwrap(), rows);
        assert_eq!(store.library_catalog_revision().unwrap(), Some(clock + 1));
        assert_eq!(
            store.spotify_artist_discovery(&owner).unwrap().as_deref(),
            Some(progress)
        );
    }
}

#[test]
fn both_backends_accept_artist_denials_without_waiting_for_a_full_refresh() {
    artist_denials_publish_before_metadata(&mut MemStore::default());
    artist_denials_publish_before_metadata(&mut SqliteStore::open_in_memory().unwrap());
}

fn pending_playlist_denial(revision: Option<u64>) -> SpotifyPlaylistPublication {
    SpotifyPlaylistPublication {
        expected_directory: Some("directory before".into()),
        directory: Some("directory after".into()),
        unavailable: vec![URI.into()],
        unavailable_revision: revision,
        observations: vec![SpotifyPlaylistPublicationItem {
            contents: CachedPlaylist::new(PLAYLIST.into(), 20, Some(2), 1, false, Vec::new()),
            expected_progress: "unpublished snapshot".into(),
            applied_progress: "published snapshot".into(),
        }],
        ..SpotifyPlaylistPublication::default()
    }
}

fn stage_playlist(store: &mut dyn Store, owner: &SpotifyLibraryOwner) {
    assert!(
        store
            .save_spotify_playlist_directory(owner, None, "directory before", None)
            .unwrap()
    );
    assert!(
        store
            .save_spotify_playlist_progress(owner, PLAYLIST, None, "unpublished snapshot")
            .unwrap()
    );
}

fn playlist_denials_publish_without_replacing_complete_contents(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(store, &owner);
    let complete = CachedPlaylist::new(PLAYLIST.into(), 10, Some(0), 0, true, Vec::new());
    store.save_playlist_contents(&complete).unwrap();
    let mut publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    let clock = store.library_catalog_revision().unwrap().unwrap();
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert!(!store.library().unwrap()[0].is_candidate());
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(complete));
    assert_eq!(store.library_catalog_revision().unwrap(), Some(clock + 1));
    assert_eq!(
        store
            .playlist_progress(ACCOUNT, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some("published snapshot")
    );
    assert!(
        store
            .save_spotify_playlist_progress(
                &owner,
                PLAYLIST,
                Some("published snapshot"),
                "repeated fresh observation",
            )
            .unwrap()
    );
    publication.unavailable_revision = store.library_catalog_revision().unwrap();
    publication.expected_directory = publication.directory.clone();
    publication.directory = Some("repeated denial directory".into());
    publication.observations[0].expected_progress = "repeated fresh observation".into();
    publication.observations[0].applied_progress = "repeated denial snapshot".into();
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert_eq!(store.library_catalog_revision().unwrap(), Some(clock + 1));

    // A cached replay cannot restore a denial cleared by a fresh full read
    // after this playlist's observation has already been consumed.
    stage(store, &owner);
    assert!(publish(
        store,
        &owner,
        &[entry()],
        Some(&delta(&[URI], &[]))
    ));
    let clock = store.library_catalog_revision().unwrap();
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
    assert!(store.library().unwrap()[0].is_candidate());
    assert_eq!(store.library_catalog_revision().unwrap(), clock);
}

#[test]
fn both_backends_accept_playlist_denials_with_their_unpublished_receipt() {
    playlist_denials_publish_without_replacing_complete_contents(&mut MemStore::default());
    playlist_denials_publish_without_replacing_complete_contents(
        &mut SqliteStore::open_in_memory().unwrap(),
    );
}

#[derive(Debug, PartialEq, Eq)]
struct BackgroundState {
    library: Vec<LibraryEntry>,
    unavailable: HashSet<String>,
    revision: Option<u64>,
    artist: Option<String>,
    playlist: Option<String>,
    directory: Option<String>,
}

fn background_state(store: &dyn Store, owner: &SpotifyLibraryOwner) -> BackgroundState {
    BackgroundState {
        library: store.library().unwrap(),
        unavailable: store.spotify_unavailable_tracks().unwrap(),
        revision: store.library_catalog_revision().unwrap(),
        artist: store.spotify_artist_discovery(owner).unwrap(),
        playlist: store.playlist_progress(ACCOUNT, PLAYLIST).unwrap(),
        directory: store.spotify_playlist_directory(owner).unwrap(),
    }
}

fn background_denials_require_current_receipts(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(store, &owner);
    let publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    let fence = SpotifyArtistDiscoveryFence {
        expected: None,
        progress: "new artist receipt".into(),
    };
    let before = background_state(store, &owner);
    let mut stale_artist = fence.clone();
    stale_artist.expected = Some("stale cohort".into());
    assert!(
        !store
            .publish_spotify_artist_continuation(&owner, &stale_artist, &[], &[URI.into()])
            .unwrap()
    );
    for directory in [false, true] {
        let mut stale_playlist = publication.clone();
        if directory {
            stale_playlist.expected_directory = Some("stale directory".into());
        } else {
            stale_playlist.observations[0].expected_progress = "stale snapshot".into();
        }
        assert!(
            !store
                .publish_spotify_playlist_continuation(&owner, &stale_playlist)
                .unwrap()
        );
    }
    assert_eq!(background_state(store, &owner), before);
    store
        .adopt_spotify_library_owner("different-account")
        .unwrap();
    let returned = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    stage_playlist(store, &returned);
    store.upsert_library(&[entry()]).unwrap();
    let before = background_state(store, &returned);
    assert!(
        !store
            .publish_spotify_artist_continuation(&owner, &fence, &[], &[URI.into()])
            .unwrap()
    );
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert_eq!(background_state(store, &returned), before);
}

#[test]
fn both_backends_reject_stale_background_denials_atomically() {
    background_denials_require_current_receipts(&mut MemStore::default());
    background_denials_require_current_receipts(&mut SqliteStore::open_in_memory().unwrap());
}

fn positive_only_refresh_supersedes_pending_playlist_denials(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(store, &owner);
    let mut publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    let rows = store.library().unwrap();
    let revision = store.library_catalog_revision().unwrap().unwrap();
    stage(store, &owner);
    assert!(publish(store, &owner, &[], Some(&delta(&[URI], &[]))));
    assert_eq!(store.library().unwrap(), rows);
    assert_eq!(
        store.library_catalog_revision().unwrap(),
        Some(revision + 1),
        "accepted positive observations fence old negatives without changing rows"
    );
    let before = background_state(store, &owner);
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert_eq!(background_state(store, &owner), before);

    // A genuinely new observation can still remove eligibility after a later
    // provider restriction. Its private proof and original revision are new.
    assert!(
        store
            .save_spotify_playlist_progress(
                &owner,
                PLAYLIST,
                Some("unpublished snapshot"),
                "fresh negative observation",
            )
            .unwrap()
    );
    publication.observations[0].expected_progress = "fresh negative observation".into();
    publication.observations[0].applied_progress = "fresh negative accepted".into();
    publication.unavailable_revision = store.library_catalog_revision().unwrap();
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert!(store.spotify_unavailable_tracks().unwrap().contains(URI));
    assert!(!store.library().unwrap()[0].is_candidate());
}

#[test]
fn both_backends_fence_pending_denials_against_positive_only_refreshes() {
    positive_only_refresh_supersedes_pending_playlist_denials(&mut MemStore::default());
    positive_only_refresh_supersedes_pending_playlist_denials(
        &mut SqliteStore::open_in_memory().unwrap(),
    );
}

fn full_refresh_fences_retained_negative_observations(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    let retained = SpotifyAvailability {
        unavailable: vec![URI.into()],
        expected_revision: store.library_catalog_revision().unwrap(),
        ..SpotifyAvailability::default()
    };
    stage(store, &owner);
    assert!(publish(store, &owner, &[], Some(&delta(&[URI], &[]))));
    // The essential read is current, but the optional playlist observation was
    // retained before the newer full read accepted a fresh playable response.
    stage(store, &owner);
    let before = background_state(store, &owner);
    assert!(!publish(store, &owner, &[entry()], Some(&retained)));
    assert_eq!(background_state(store, &owner), before);
    assert_eq!(
        store
            .spotify_library_progress(ACCOUNT, Some(&owner))
            .unwrap()
            .as_deref(),
        Some(PROOF)
    );
    assert!(publish(store, &owner, &[entry()], None));
}

#[test]
fn both_backends_reject_stale_retained_denials_in_full_publications() {
    full_refresh_fences_retained_negative_observations(&mut MemStore::default());
    full_refresh_fences_retained_negative_observations(&mut SqliteStore::open_in_memory().unwrap());
}

#[test]
fn sqlite_checks_negative_observation_revision_across_connections() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("background-availability-race.db");
    let mut delayed = SqliteStore::open(&file).unwrap();
    let owner = delayed.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    delayed.upsert_library(&[entry()]).unwrap();
    stage_playlist(&mut delayed, &owner);
    let publication = pending_playlist_denial(delayed.library_catalog_revision().unwrap());
    let mut current = SqliteStore::open(&file).unwrap();
    stage(&mut current, &owner);
    assert!(publish(
        &mut current,
        &owner,
        &[],
        Some(&delta(&[URI], &[]))
    ));
    let before = background_state(&delayed, &owner);
    assert!(
        !delayed
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert_eq!(background_state(&delayed, &owner), before);
    current.close().unwrap();
    delayed.close().unwrap();
}

fn background_denials_validate_before_consuming_receipts(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(store, &owner);
    let publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    let fence = SpotifyArtistDiscoveryFence {
        expected: None,
        progress: "new artist receipt".into(),
    };
    let before = background_state(store, &owner);
    for uri in [
        "",
        "spotify:album:WrongKind",
        "spotify:track:bad/id",
        "spotify:track:é",
    ] {
        assert!(
            store
                .publish_spotify_artist_continuation(&owner, &fence, &[entry()], &[uri.into()])
                .is_err()
        );
        let mut invalid = publication.clone();
        invalid.unavailable = vec![uri.into()];
        assert!(
            store
                .publish_spotify_playlist_continuation(&owner, &invalid)
                .is_err()
        );
    }
    for revision in [None, Some(u64::try_from(i64::MAX).unwrap() + 1)] {
        let mut invalid = publication.clone();
        invalid.unavailable_revision = revision;
        assert!(
            store
                .publish_spotify_playlist_continuation(&owner, &invalid)
                .is_err()
        );
    }
    for no_observation in [false, true] {
        let mut invalid = publication.clone();
        if no_observation {
            invalid.observations.clear();
        } else {
            invalid.observations[0].applied_progress = "unpublished snapshot".into();
        }
        assert!(
            store
                .publish_spotify_playlist_continuation(&owner, &invalid)
                .is_err()
        );
    }
    assert_eq!(background_state(store, &owner), before);
}

#[test]
fn both_backends_reject_invalid_background_denials_before_any_write() {
    background_denials_validate_before_consuming_receipts(&mut MemStore::default());
    background_denials_validate_before_consuming_receipts(
        &mut SqliteStore::open_in_memory().unwrap(),
    );
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
        expected_revision: None,
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
fn oversized_background_denials_leave_both_readers_unacknowledged() {
    let mut store = MemStore::default();
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(&mut store, &owner);
    let mut publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    publication.unavailable = vec![String::new(); MAX_SPOTIFY_AVAILABILITY_ENTRIES + 1];
    let before = background_state(&store, &owner);
    assert!(
        store
            .publish_spotify_artist_continuation(
                &owner,
                &SpotifyArtistDiscoveryFence {
                    expected: None,
                    progress: "oversized observations".into(),
                },
                &[],
                &publication.unavailable,
            )
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
    assert_eq!(background_state(&store, &owner), before);
}

#[test]
fn sqlite_background_denials_and_consumed_receipts_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("background-availability.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(&mut store, &owner);
    let publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert!(
        store
            .publish_spotify_artist_continuation(
                &owner,
                &SpotifyArtistDiscoveryFence {
                    expected: None,
                    progress: "accepted missing artist track".into(),
                },
                &[],
                &["spotify:track:ArtistAlbumCompanion".into()],
            )
            .unwrap()
    );
    let before = background_state(&store, &owner);
    store.close().unwrap();
    let store = SqliteStore::open(&file).unwrap();
    assert_eq!(background_state(&store, &owner), before);
    assert_eq!(
        store.spotify_unavailable_tracks().unwrap(),
        HashSet::from([URI.into(), "spotify:track:ArtistAlbumCompanion".into()])
    );
    assert!(!store.library().unwrap()[0].is_candidate());
    store.close().unwrap();
}

#[test]
fn sqlite_background_failure_rolls_back_denials_candidates_and_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("background-availability-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let owner = store.adopt_spotify_library_owner(ACCOUNT).unwrap().0;
    store.upsert_library(&[entry()]).unwrap();
    stage_playlist(&mut store, &owner);
    let before = store.library().unwrap();
    let publication = pending_playlist_denial(store.library_catalog_revision().unwrap());
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    // Force failure after the denial, candidate retirement and receipt writes.
    raw.execute_sync("DROP TABLE library_catalog_clock")
        .unwrap();
    assert!(
        store
            .publish_spotify_artist_continuation(
                &owner,
                &SpotifyArtistDiscoveryFence {
                    expected: None,
                    progress: "uncommitted artist receipt".into(),
                },
                &[],
                &[URI.into()],
            )
            .is_err()
    );
    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.spotify_artist_discovery(&owner).unwrap(), None);
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .is_err()
    );
    assert!(store.spotify_unavailable_tracks().unwrap().is_empty());
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), None);
    assert_eq!(
        store
            .playlist_progress(ACCOUNT, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some("unpublished snapshot")
    );
    assert_eq!(
        store.spotify_playlist_directory(&owner).unwrap().as_deref(),
        Some("directory before")
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
