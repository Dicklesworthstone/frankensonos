//! Recent listening must survive delayed readers and account changes atomically.

use fsonos_core::store::{MemStore, SpotifyLibraryOwner, SqliteStore, Store};

fn play(track: &str, at: i64) -> (String, i64) {
    (format!("spotify:track:{track}"), at)
}

fn exercise(store: &mut dyn Store) {
    let (owner, _) = store
        .adopt_spotify_library_owner("listening-test-owner")
        .unwrap();
    let revision = store.library_catalog_revision().unwrap();
    store
        .record_play("local-room", "spotify:track:Local", 50)
        .unwrap();
    assert!(
        store
            .merge_spotify_heard(&owner, &[play("A", 200), play("B", 100)])
            .unwrap()
    );
    assert!(
        store
            .merge_spotify_heard(&owner, &[play("A", 150), play("C", 300)])
            .unwrap()
    );
    // A full library read which began earlier must not replace fresher evidence.
    assert!(
        store
            .publish_spotify_library(&owner, &[], None, &[], Some(&[play("A", 120)]))
            .unwrap()
    );
    assert!(
        store
            .publish_spotify_library(&owner, &[], None, &[], Some(&[]))
            .unwrap()
    );
    assert_eq!(
        store.spotify_heard().unwrap(),
        [play("B", 100), play("A", 200), play("C", 300)]
    );
    assert_eq!(store.library_catalog_revision().unwrap(), revision);
    assert_eq!(store.recent_plays(Some("local-room"), 10).unwrap().len(), 1);

    let (other, _) = store
        .adopt_spotify_library_owner("other-listening-owner")
        .unwrap();
    assert!(store.spotify_heard().unwrap().is_empty());
    assert!(
        !store
            .merge_spotify_heard(&owner, &[play("Leaked", 500)])
            .unwrap()
    );
    assert!(
        store
            .merge_spotify_heard(&other, &[play("Other", 600)])
            .unwrap()
    );
    let (returning, _) = store.adopt_spotify_library_owner(&owner.owner_id).unwrap();
    assert_ne!(owner.generation, returning.generation);
    assert!(
        !store
            .merge_spotify_heard(&owner, &[play("Stale", 700)])
            .unwrap()
    );
    assert!(store.spotify_heard().unwrap().is_empty());
    assert!(
        store
            .merge_spotify_heard(&returning, &[play("Current", 800)])
            .unwrap()
    );
}

#[test]
fn memory_merges_monotonically_and_fences_account_adoption() {
    exercise(&mut MemStore::default());
}

#[test]
fn sqlite_merges_and_retains_listening_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recent-listening.db");
    let mut store = SqliteStore::open(&path).unwrap();
    exercise(&mut store);
    store.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(reopened.spotify_heard().unwrap(), [play("Current", 800)]);
    reopened.close().unwrap();
}

#[test]
fn concurrent_sqlite_readers_cannot_erase_newer_plays_or_publish_old_accounts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recent-listening-race.db");
    let mut first = SqliteStore::open(&path).unwrap();
    let mut second = SqliteStore::open(&path).unwrap();
    let (owner, _) = first.adopt_spotify_library_owner("first-listener").unwrap();
    assert!(
        first
            .merge_spotify_heard(&owner, &[play("A", 100)])
            .unwrap()
    );
    assert!(
        second
            .merge_spotify_heard(&owner, &[play("A", 300), play("B", 200)])
            .unwrap()
    );
    assert!(
        first
            .publish_spotify_library(&owner, &[], None, &[], Some(&[play("A", 150)]))
            .unwrap()
    );
    assert_eq!(
        first.spotify_heard().unwrap(),
        [play("B", 200), play("A", 300)]
    );
    second
        .adopt_spotify_library_owner("second-listener")
        .unwrap();
    let (returned, _) = second
        .adopt_spotify_library_owner("first-listener")
        .unwrap();
    assert!(
        !first
            .merge_spotify_heard(&owner, &[play("Stale", 999)])
            .unwrap()
    );
    assert!(
        first
            .merge_spotify_heard(&returned, &[play("Fresh", 1_000)])
            .unwrap()
    );
    assert_eq!(second.spotify_heard().unwrap(), [play("Fresh", 1_000)]);
    first.close().unwrap();
    second.close().unwrap();
}

#[test]
fn an_unadopted_store_cannot_accept_a_fabricated_owner() {
    let mut store = MemStore::default();
    let owner = SpotifyLibraryOwner {
        owner_id: "unknown-owner".into(),
        generation: 1,
    };
    assert!(
        !store
            .merge_spotify_heard(&owner, &[play("Unowned", 10)])
            .unwrap()
    );
    assert!(store.spotify_heard().unwrap().is_empty());
}
