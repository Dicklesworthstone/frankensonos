//! Essential library checkpoints stay private, survive restarts, and cannot
//! adopt an account or publish prepared evidence after losing either fence.

use fsonos_core::store::{
    CachedPlaylist, LibraryEntry, MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES, MemStore, PlayRecord,
    SpotifyLibraryOwner, SqliteStore, Store,
};
use fsonos_types::Track;

const OWNER: &str = "verified-library-owner";
const OTHER: &str = "different-library-owner";
const PLAYLIST: &str = "spotify:playlist:LastGood";
const PREFIX: &str = r#"{"nonce":"first","page":1}"#;
const COMPLETE: &str = r#"{"nonce":"complete-α","page":2}"#;
const CLAIMED: &str = r#"{"nonce":"new-consumer","page":2}"#;

#[derive(Debug, PartialEq, Eq)]
struct PublicState {
    owner: Option<SpotifyLibraryOwner>,
    library: Vec<LibraryEntry>,
    playlists: Vec<(String, String)>,
    contents: Option<CachedPlaylist>,
    heard: Vec<(String, i64)>,
    history: Vec<PlayRecord>,
}

fn public(store: &dyn Store) -> PublicState {
    PublicState {
        owner: store.spotify_library_owner().unwrap(),
        library: store.library().unwrap(),
        playlists: store.playlists().unwrap(),
        contents: store.playlist_contents(PLAYLIST).unwrap(),
        heard: store.spotify_heard().unwrap(),
        history: store.recent_plays(None, 10).unwrap(),
    }
}

fn entry(name: &str) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: format!("spotify:track:{name}"),
            title: name.into(),
            artist: None,
            album: None,
            uri: None,
            duration_secs: Some(180),
        },
        artist_id: Some("CatalogArtist".into()),
        candidate: Some(true),
        taste_weight: Some(1200),
        ..LibraryEntry::default()
    }
}

fn observation() -> CachedPlaylist {
    CachedPlaylist::new(PLAYLIST.into(), 20, Some(0), 0, true, Vec::new())
}

fn seed(store: &mut dyn Store) {
    let old = entry("Old");
    store.upsert_library(std::slice::from_ref(&old)).unwrap();
    store
        .save_playlists(&[(PLAYLIST.into(), "Last good".into())])
        .unwrap();
    store.save_playlist_contents(&observation()).unwrap();
    store
        .save_spotify_heard(&[(old.track.source_uri.clone(), 10)])
        .unwrap();
    store.record_play("Kitchen", &old.track.source_uri, 10).unwrap();
}

fn publish(
    store: &mut dyn Store,
    owner: &SpotifyLibraryOwner,
    proof: &str,
    name: &str,
) -> bool {
    let song = entry(name);
    let heard = [(song.track.source_uri.clone(), 20)];
    store
        .publish_spotify_library_from_progress(
            owner,
            proof,
            &[song],
            Some(&[(PLAYLIST.into(), name.into())]),
            &[observation()],
            Some(&heard),
        )
        .unwrap()
}

fn stage_before_adoption(store: &mut dyn Store) -> SpotifyLibraryOwner {
    seed(store);
    let before = public(store);
    assert_eq!(before.owner, None);
    assert_eq!(store.spotify_library_progress(OWNER, None).unwrap(), None);
    assert!(
        store
            .save_spotify_library_progress(OWNER, None, None, PREFIX)
            .unwrap()
    );
    assert!(
        store
            .save_spotify_library_progress(OTHER, None, None, "other prefix")
            .unwrap()
    );
    assert!(
        !store
            .save_spotify_library_progress(OWNER, None, None, "late writer")
            .unwrap()
    );
    assert!(
        !store
            .save_spotify_library_progress(OWNER, None, Some("wrong"), COMPLETE)
            .unwrap()
    );
    assert_eq!(public(store), before, "unfinished reads must stay private");
    assert_eq!(
        store.spotify_library_progress(OTHER, None).unwrap().as_deref(),
        Some("other prefix")
    );
    assert!(
        store
            .save_spotify_library_progress(OWNER, None, Some(PREFIX), COMPLETE)
            .unwrap()
    );
    assert_eq!(
        store
            .adopt_spotify_library_from_progress(OWNER, None, r#"{"page":2,"nonce":"complete-α"}"#)
            .unwrap(),
        None,
        "CAS compares exact bytes, not equivalent JSON"
    );
    assert_eq!(public(store), before);
    let (owner, changed) = store
        .adopt_spotify_library_from_progress(OWNER, None, COMPLETE)
        .unwrap()
        .unwrap();
    assert!(changed);
    assert_eq!(owner.owner_id, OWNER);
    assert_eq!(store.spotify_library_owner().unwrap(), Some(owner.clone()));
    assert_eq!(store.spotify_library_progress(OWNER, None).unwrap(), None);
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(&owner)).unwrap().as_deref(),
        Some(COMPLETE),
        "adoption must retain and rebind the completed proof"
    );
    assert_eq!(store.spotify_library_progress(OTHER, Some(&owner)).unwrap(), None);
    assert!(
        !store
            .save_spotify_library_progress(OWNER, None, None, "stale unknown owner")
            .unwrap()
    );
    assert_eq!(
        store.adopt_spotify_library_from_progress(OWNER, None, COMPLETE).unwrap(),
        None
    );
    owner
}

fn publish_after_claim(store: &mut dyn Store, owner: &SpotifyLibraryOwner) {
    let before = public(store);
    assert!(!publish(store, owner, "wrong", "Leaked"));
    assert_eq!(public(store), before);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(owner), Some(COMPLETE), CLAIMED)
            .unwrap()
    );
    assert_eq!(
        store
            .adopt_spotify_library_from_progress(OWNER, Some(owner), CLAIMED)
            .unwrap(),
        Some((owner.clone(), false)),
        "same-owner adoption must preserve its receipt"
    );
    assert_eq!(
        store.adopt_spotify_library_owner(OWNER).unwrap(),
        (owner.clone(), false),
        "compatible ordinary same-owner adoption must preserve progress"
    );
    assert!(!publish(store, owner, COMPLETE, "OldPreparedResult"));
    assert_eq!(public(store), before);
    let invalid = CachedPlaylist::new("not-a-playlist".into(), 20, Some(0), 0, true, Vec::new());
    assert!(
        store
            .publish_spotify_library_from_progress(
                owner,
                CLAIMED,
                &[entry("InvalidPublication")],
                Some(&[]),
                &[invalid],
                Some(&[]),
            )
            .is_err()
    );
    assert_eq!(public(store), before);
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(owner)).unwrap().as_deref(),
        Some(CLAIMED),
        "failed publication must preserve resumable essential work"
    );
    assert!(publish(store, owner, CLAIMED, "Published"));
    assert_eq!(store.spotify_library_progress(OWNER, Some(owner)).unwrap(), None);
    let after = public(store);
    assert_eq!(after.owner, Some(owner.clone()));
    assert_eq!(after.playlists, [(PLAYLIST.into(), "Published".into())]);
    assert_eq!(after.heard, [("spotify:track:Published".into(), 20)]);
    assert_eq!(after.history, before.history);
    assert!(after.library.iter().any(|song| {
        song.track.source_uri == "spotify:track:Published" && song.candidate == Some(true)
    }));
    assert!(!publish(store, owner, CLAIMED, "Replay"));
    assert_eq!(public(store), after);
}

fn reject_changed_receipts(store: &mut dyn Store) {
    let first = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    seed(store);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&first), None, COMPLETE)
            .unwrap()
    );
    store.adopt_spotify_library_owner(OTHER).unwrap();
    let returning = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    assert_eq!(returning.generation, first.generation + 2);
    assert_eq!(store.spotify_library_progress(OWNER, Some(&first)).unwrap(), None);
    assert_eq!(store.spotify_library_progress(OWNER, Some(&returning)).unwrap(), None);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&returning), None, COMPLETE)
            .unwrap()
    );
    let wrong_identity = SpotifyLibraryOwner {
        owner_id: OTHER.into(),
        generation: returning.generation,
    };
    let before = public(store);
    for stale in [&first, &wrong_identity] {
        assert_eq!(store.spotify_library_progress(OWNER, Some(stale)).unwrap(), None);
        assert!(
            !store
                .save_spotify_library_progress(OWNER, Some(stale), None, "late prefix")
                .unwrap()
        );
        assert_eq!(
            store
                .adopt_spotify_library_from_progress(OWNER, Some(stale), COMPLETE)
                .unwrap(),
            None
        );
        assert!(!publish(store, stale, COMPLETE, "Leaked"));
    }
    assert_eq!(public(store), before);
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(&returning)).unwrap().as_deref(),
        Some(COMPLETE)
    );
}

fn reject_invalid_progress(store: &mut dyn Store) {
    let owner = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    seed(store);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, COMPLETE)
            .unwrap()
    );
    let before = public(store);
    for invalid in [String::new(), "  ".into(), "bad\nowner".into(), "x".repeat(1025)] {
        assert!(store.spotify_library_progress(&invalid, Some(&owner)).is_err());
        assert!(
            store
                .save_spotify_library_progress(&invalid, Some(&owner), None, PREFIX)
                .is_err()
        );
        assert!(
            store
                .adopt_spotify_library_from_progress(&invalid, Some(&owner), COMPLETE)
                .is_err()
        );
    }
    let oversized = "x".repeat(MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES + 1);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), Some(COMPLETE), &oversized)
            .is_err()
    );
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), Some(&oversized), PREFIX)
            .is_err()
    );
    assert!(
        store
            .adopt_spotify_library_from_progress(OWNER, Some(&owner), &oversized)
            .is_err()
    );
    assert!(
        store
            .publish_spotify_library_from_progress(&owner, &oversized, &[], Some(&[]), &[], Some(&[]))
            .is_err()
    );
    assert_eq!(public(store), before);
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(&owner)).unwrap().as_deref(),
        Some(COMPLETE)
    );
}

#[test]
fn memory_progress_is_private_and_publication_consumes_only_its_exact_proof() {
    let mut store = MemStore::default();
    let owner = stage_before_adoption(&mut store);
    publish_after_claim(&mut store, &owner);
}

#[test]
fn sqlite_progress_is_private_and_publication_consumes_only_its_exact_proof() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let owner = stage_before_adoption(&mut store);
    publish_after_claim(&mut store, &owner);
    store.close().unwrap();
}

#[test]
fn both_backends_reject_prior_adoptions_and_invalid_progress() {
    reject_changed_receipts(&mut MemStore::default());
    reject_invalid_progress(&mut MemStore::default());
    let mut store = SqliteStore::open_in_memory().unwrap();
    reject_changed_receipts(&mut store);
    store.close().unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    reject_invalid_progress(&mut store);
    store.close().unwrap();
}

#[test]
fn sqlite_progress_survives_restarts_before_and_after_adoption() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("library-progress.db");
    let mut store = SqliteStore::open(&file).unwrap();
    seed(&mut store);
    let original = public(&store);
    assert!(store.save_spotify_library_progress(OWNER, None, None, PREFIX).unwrap());
    store.close().unwrap();

    let mut store = SqliteStore::open(&file).unwrap();
    assert_eq!(public(&store), original);
    assert_eq!(store.spotify_library_progress(OWNER, None).unwrap().as_deref(), Some(PREFIX));
    assert!(store.save_spotify_library_progress(OWNER, None, Some(PREFIX), COMPLETE).unwrap());
    let owner = store
        .adopt_spotify_library_from_progress(OWNER, None, COMPLETE)
        .unwrap()
        .unwrap()
        .0;
    store.close().unwrap();

    let mut store = SqliteStore::open(&file).unwrap();
    assert_eq!(store.spotify_library_owner().unwrap(), Some(owner.clone()));
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(&owner)).unwrap().as_deref(),
        Some(COMPLETE)
    );
    assert!(publish(&mut store, &owner, COMPLETE, "AfterRestart"));
    let published = public(&store);
    store.close().unwrap();

    let store = SqliteStore::open(&file).unwrap();
    assert_eq!(public(&store), published);
    assert_eq!(store.spotify_library_progress(OWNER, Some(&owner)).unwrap(), None);
    store.close().unwrap();
}

#[test]
fn competing_sqlite_handles_cannot_adopt_or_publish_a_superseded_claim() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("library-progress-race.db");
    let mut first = SqliteStore::open(&file).unwrap();
    let mut second = SqliteStore::open(&file).unwrap();
    seed(&mut first);
    let original = public(&first);
    assert!(first.save_spotify_library_progress(OWNER, None, None, PREFIX).unwrap());
    assert!(!second.save_spotify_library_progress(OWNER, None, None, "loser").unwrap());
    assert!(second.save_spotify_library_progress(OWNER, None, Some(PREFIX), COMPLETE).unwrap());
    assert_eq!(
        first.adopt_spotify_library_from_progress(OWNER, None, PREFIX).unwrap(),
        None
    );
    assert_eq!(public(&first), original);
    let owner = second
        .adopt_spotify_library_from_progress(OWNER, None, COMPLETE)
        .unwrap()
        .unwrap()
        .0;
    let adopted = public(&second);
    assert!(first.save_spotify_library_progress(OWNER, Some(&owner), Some(COMPLETE), CLAIMED).unwrap());
    assert!(!second.save_spotify_library_progress(OWNER, Some(&owner), Some(COMPLETE), "loser").unwrap());
    assert!(!publish(&mut second, &owner, COMPLETE, "StalePrepared"));
    assert_eq!(public(&second), adopted);
    assert_eq!(
        second.spotify_library_progress(OWNER, Some(&owner)).unwrap().as_deref(),
        Some(CLAIMED)
    );
    assert!(publish(&mut first, &owner, CLAIMED, "Winner"));
    assert_eq!(second.spotify_library_progress(OWNER, Some(&owner)).unwrap(), None);
    first.close().unwrap();
    second.close().unwrap();
}

#[test]
fn failed_sqlite_adoption_restores_the_previous_cache_and_private_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("library-adoption-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let owner = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    seed(&mut store);
    assert!(
        store
            .save_spotify_library_progress(OTHER, Some(&owner), None, COMPLETE)
            .unwrap()
    );
    let before = public(&store);
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    raw.execute_sync("DROP TABLE spotify_heard").unwrap();
    assert!(
        store
            .adopt_spotify_library_from_progress(OTHER, Some(&owner), COMPLETE)
            .is_err()
    );
    assert_eq!(store.spotify_library_owner().unwrap(), before.owner);
    assert_eq!(store.library().unwrap(), before.library);
    assert_eq!(store.playlists().unwrap(), before.playlists);
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), before.contents);
    assert_eq!(
        store.spotify_library_progress(OTHER, Some(&owner)).unwrap().as_deref(),
        Some(COMPLETE)
    );
}

#[test]
fn failed_sqlite_publication_retains_completed_progress_and_rolls_back_all_earlier_writes() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("library-publication-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let owner = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    seed(&mut store);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, COMPLETE)
            .unwrap()
    );
    let before = public(&store);
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    raw.execute_sync("DROP TABLE spotify_heard").unwrap();
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                COMPLETE,
                &[entry("MustRollBack")],
                Some(&[(PLAYLIST.into(), "Must roll back".into())]),
                &[CachedPlaylist::new(PLAYLIST.into(), 30, Some(0), 0, true, Vec::new())],
                Some(&[]),
            )
            .is_err()
    );
    assert_eq!(store.spotify_library_owner().unwrap(), before.owner);
    assert_eq!(store.library().unwrap(), before.library);
    assert_eq!(store.playlists().unwrap(), before.playlists);
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), before.contents);
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(&owner)).unwrap().as_deref(),
        Some(COMPLETE)
    );
}
