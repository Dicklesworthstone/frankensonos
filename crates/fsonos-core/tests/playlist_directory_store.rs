//! Directory checkpoints are bounded, adoption-fenced, atomic and durable.

use fsonos_core::store::{MAX_PLAYLIST_BYTES, MemStore, SqliteStore, Store};

const OWNER: &str = "directory-owner";
const KEEP: &str = "spotify:playlist:StillFollowed";
const GONE: &str = "spotify:playlist:Unfollowed";

fn exercise(store: &mut dyn Store) {
    let (first, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert_eq!(store.spotify_playlist_directory(&first).unwrap(), None);
    store.save_playlist_progress(OWNER, KEEP, "keep").unwrap();
    store.save_playlist_progress(OWNER, GONE, "old").unwrap();
    store
        .save_playlist_progress("other", GONE, "other")
        .unwrap();
    store
        .save_playlists(&[(GONE.into(), "Last published".into())])
        .unwrap();

    assert!(
        store
            .save_spotify_playlist_directory(&first, None, "page one", None)
            .unwrap()
    );
    assert!(
        !store
            .save_spotify_playlist_directory(&first, None, "stale writer", Some(&[]))
            .unwrap()
    );
    assert!(
        !store
            .save_spotify_playlist_directory(&first, Some("wrong"), "stale writer", Some(&[]))
            .unwrap()
    );
    assert_eq!(
        store.spotify_playlist_directory(&first).unwrap().as_deref(),
        Some("page one")
    );
    assert_eq!(
        store.playlist_progress(OWNER, GONE).unwrap().as_deref(),
        Some("old")
    );

    assert!(
        store
            .save_spotify_playlist_directory(
                &first,
                Some("page one"),
                "complete directory",
                Some(&[KEEP.into()]),
            )
            .unwrap()
    );
    assert_eq!(store.playlist_progress(OWNER, GONE).unwrap(), None);
    assert_eq!(
        store.playlist_progress(OWNER, KEEP).unwrap().as_deref(),
        Some("keep")
    );
    assert_eq!(
        store.playlist_progress("other", GONE).unwrap().as_deref(),
        Some("other")
    );
    assert_eq!(
        store.playlists().unwrap(),
        [(GONE.into(), "Last published".into())],
        "private progress does not publish names"
    );

    let oversized = "x".repeat(MAX_PLAYLIST_BYTES + 1);
    assert!(
        store
            .save_spotify_playlist_directory(
                &first,
                Some("complete directory"),
                &oversized,
                Some(&[]),
            )
            .is_err()
    );
    assert_eq!(
        store.spotify_playlist_directory(&first).unwrap().as_deref(),
        Some("complete directory")
    );
    assert!(store.playlist_progress(OWNER, KEEP).unwrap().is_some());

    store
        .adopt_spotify_library_owner("different-owner")
        .unwrap();
    assert_eq!(store.spotify_playlist_directory(&first).unwrap(), None);
    let (returning, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert!(returning.generation > first.generation);
    assert_eq!(store.spotify_playlist_directory(&returning).unwrap(), None);
    assert!(
        store
            .save_spotify_playlist_directory(&returning, None, "new adoption", None)
            .unwrap()
    );
    store
        .save_playlist_progress(OWNER, KEEP, "new proof")
        .unwrap();
    assert!(
        !store
            .save_spotify_playlist_directory(
                &first,
                Some("complete directory"),
                "late old response",
                Some(&[])
            )
            .unwrap()
    );
    assert_eq!(
        store
            .spotify_playlist_directory(&returning)
            .unwrap()
            .as_deref(),
        Some("new adoption")
    );
    assert_eq!(
        store.playlist_progress(OWNER, KEEP).unwrap().as_deref(),
        Some("new proof")
    );
}

#[test]
fn memory_directory_checkpoint_contract() {
    exercise(&mut MemStore::default());
}

#[test]
fn sqlite_directory_checkpoint_contract_survives_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("playlist-directory.db");
    let mut store = SqliteStore::open(&file).unwrap();
    exercise(&mut store);
    let owner = store.spotify_library_owner().unwrap().unwrap();
    store.close().unwrap();
    let reopened = SqliteStore::open(&file).unwrap();
    assert_eq!(
        reopened
            .spotify_playlist_directory(&owner)
            .unwrap()
            .as_deref(),
        Some("new adoption")
    );
    assert_eq!(
        reopened.playlist_progress(OWNER, KEEP).unwrap().as_deref(),
        Some("new proof")
    );
    reopened.close().unwrap();
}

#[test]
fn failed_sqlite_pruning_rolls_back_the_directory_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("playlist-directory-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert!(
        store
            .save_spotify_playlist_directory(&owner, None, "accepted prefix", None)
            .unwrap()
    );
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    raw.execute_sync("DROP TABLE spotify_playlist_progress")
        .unwrap();
    assert!(
        store
            .save_spotify_playlist_directory(
                &owner,
                Some("accepted prefix"),
                "replacement",
                Some(&[]),
            )
            .is_err()
    );
    assert_eq!(
        store.spotify_playlist_directory(&owner).unwrap().as_deref(),
        Some("accepted prefix")
    );
}
