//! Followed-artist cursors stay private, durable and fenced to their reader.

use fsonos_core::store::{
    LibraryEntry, MAX_FOLLOWED_ARTIST_BYTES, MemStore, SpotifyLibraryOwner, SqliteStore, Store,
};
use fsonos_types::Track;

const OWNER: &str = "followed-directory-owner";
const OTHER: &str = "other-directory-owner";
const TRACK: &str = "spotify:track:DirectorySong";
const PLAYLIST: &str = "spotify:playlist:DirectoryList";
const PREFIX: &str = "accepted followed-artist prefix";
const COMPLETE: &str = "complete followed-artist directory";

fn exercise(store: &mut dyn Store) {
    let unadopted = SpotifyLibraryOwner {
        owner_id: OWNER.into(),
        generation: 1,
    };
    assert_eq!(store.spotify_followed_directory(&unadopted).unwrap(), None);
    assert!(
        !store
            .save_spotify_followed_directory(&unadopted, None, PREFIX)
            .unwrap(),
        "an unproved account cannot checkpoint personal evidence"
    );
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    store
        .upsert_library(&[LibraryEntry {
            track: Track {
                source_uri: TRACK.into(),
                title: "Published song".into(),
                artist: None,
                album: None,
                uri: None,
                duration_secs: None,
            },
            candidate: Some(true),
            taste_weight: Some(1080),
            ..LibraryEntry::default()
        }])
        .unwrap();
    store
        .save_playlists(&[(PLAYLIST.into(), "Published list".into())])
        .unwrap();
    store.save_spotify_heard(&[(TRACK.into(), 123)]).unwrap();
    let published = store.library().unwrap();
    assert!(
        store
            .save_spotify_playlist_directory(&owner, None, "playlist prefix", None)
            .unwrap()
    );
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "essential prefix")
            .unwrap()
    );

    assert!(
        store
            .save_spotify_followed_directory(&owner, None, PREFIX)
            .unwrap()
    );
    for stale in [None, Some("different prefix")] {
        assert!(
            !store
                .save_spotify_followed_directory(&owner, stale, "stale response")
                .unwrap()
        );
    }
    assert_eq!(
        store.spotify_followed_directory(&owner).unwrap().as_deref(),
        Some(PREFIX)
    );
    assert!(
        store
            .save_spotify_followed_directory(&owner, Some(PREFIX), COMPLETE)
            .unwrap()
    );
    assert_eq!(
        store.adopt_spotify_library_owner(OWNER).unwrap(),
        (owner.clone(), false)
    );
    assert_eq!(
        store.spotify_followed_directory(&owner).unwrap().as_deref(),
        Some(COMPLETE),
        "the same adoption retains progress"
    );
    assert_eq!(store.library().unwrap(), published);
    assert_eq!(
        store.playlists().unwrap(),
        [(PLAYLIST.into(), "Published list".into())]
    );
    assert_eq!(store.spotify_heard().unwrap(), [(TRACK.into(), 123)]);
    assert_eq!(
        store.spotify_playlist_directory(&owner).unwrap().as_deref(),
        Some("playlist prefix")
    );
    assert_eq!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .as_deref(),
        Some("essential prefix")
    );

    let (other, _) = store.adopt_spotify_library_owner(OTHER).unwrap();
    assert_eq!(store.spotify_followed_directory(&owner).unwrap(), None);
    assert_eq!(store.spotify_followed_directory(&other).unwrap(), None);
    assert!(
        !store
            .save_spotify_followed_directory(&owner, Some(COMPLETE), "late response")
            .unwrap()
    );
    let (returning, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert_eq!(returning.generation, owner.generation + 2);
    assert_eq!(store.spotify_followed_directory(&returning).unwrap(), None);
    assert!(
        store
            .save_spotify_followed_directory(&returning, None, "new adoption")
            .unwrap()
    );
    let wrong_id = SpotifyLibraryOwner {
        owner_id: OTHER.into(),
        generation: returning.generation,
    };
    for stale in [&owner, &other, &wrong_id] {
        assert_eq!(store.spotify_followed_directory(stale).unwrap(), None);
        assert!(
            !store
                .save_spotify_followed_directory(stale, None, "late response")
                .unwrap()
        );
        assert!(
            !store
                .save_spotify_followed_directory(stale, Some("new adoption"), "late response")
                .unwrap()
        );
    }
    assert_eq!(
        store
            .spotify_followed_directory(&returning)
            .unwrap()
            .as_deref(),
        Some("new adoption")
    );
}

fn reject_invalid(store: &mut dyn Store) {
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert!(
        store
            .save_spotify_followed_directory(&owner, None, PREFIX)
            .unwrap()
    );
    for owner_id in ["".into(), "owner\n".into(), "x".repeat(1_025)] {
        let invalid = SpotifyLibraryOwner {
            owner_id,
            generation: owner.generation,
        };
        assert!(store.spotify_followed_directory(&invalid).is_err());
        assert!(
            store
                .save_spotify_followed_directory(&invalid, None, "invalid")
                .is_err()
        );
    }
    for generation in [0, u64::MAX] {
        let invalid = SpotifyLibraryOwner {
            owner_id: OWNER.into(),
            generation,
        };
        assert!(store.spotify_followed_directory(&invalid).is_err());
        assert!(
            store
                .save_spotify_followed_directory(&invalid, None, "invalid")
                .is_err()
        );
    }
    let oversized = "x".repeat(MAX_FOLLOWED_ARTIST_BYTES + 1);
    assert!(
        store
            .save_spotify_followed_directory(&owner, Some(PREFIX), &oversized)
            .is_err()
    );
    assert!(
        store
            .save_spotify_followed_directory(&owner, Some(&oversized), COMPLETE)
            .is_err()
    );
    assert_eq!(
        store.spotify_followed_directory(&owner).unwrap().as_deref(),
        Some(PREFIX)
    );
}

#[test]
fn memory_followed_directory_contract() {
    exercise(&mut MemStore::default());
    reject_invalid(&mut MemStore::default());
}

#[test]
fn sqlite_followed_directory_contract_survives_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("followed-directory.db");
    let mut store = SqliteStore::open(&file).unwrap();
    exercise(&mut store);
    let owner = store.spotify_library_owner().unwrap().unwrap();
    store.close().unwrap();
    let reopened = SqliteStore::open(&file).unwrap();
    assert_eq!(
        reopened
            .spotify_followed_directory(&owner)
            .unwrap()
            .as_deref(),
        Some("new adoption")
    );
    reopened.close().unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    reject_invalid(&mut store);
    store.close().unwrap();
}

#[test]
fn sqlite_connections_cannot_replace_a_competing_followed_scan() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("followed-directory-race.db");
    let mut first = SqliteStore::open(&file).unwrap();
    let owner = first.adopt_spotify_library_owner(OWNER).unwrap().0;
    let mut second = SqliteStore::open(&file).unwrap();
    assert_eq!(first.spotify_followed_directory(&owner).unwrap(), None);
    assert_eq!(second.spotify_followed_directory(&owner).unwrap(), None);
    assert!(
        first
            .save_spotify_followed_directory(&owner, None, PREFIX)
            .unwrap()
    );
    assert!(
        !second
            .save_spotify_followed_directory(&owner, None, "losing first page")
            .unwrap()
    );
    assert!(
        second
            .save_spotify_followed_directory(&owner, Some(PREFIX), COMPLETE)
            .unwrap()
    );
    assert!(
        !first
            .save_spotify_followed_directory(&owner, Some(PREFIX), "losing next page")
            .unwrap()
    );
    assert_eq!(
        first.spotify_followed_directory(&owner).unwrap().as_deref(),
        Some(COMPLETE)
    );
    second.adopt_spotify_library_owner(OTHER).unwrap();
    let returning = second.adopt_spotify_library_owner(OWNER).unwrap().0;
    assert!(
        !first
            .save_spotify_followed_directory(&owner, Some(COMPLETE), "late old adoption")
            .unwrap()
    );
    assert_eq!(first.spotify_followed_directory(&returning).unwrap(), None);
    first.close().unwrap();
    second.close().unwrap();
}
