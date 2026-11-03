//! Personal Spotify cache ownership is one durable transition, including failure.

use fsonos_core::store::{
    AlbumTrack, CachedPlaylist, LibraryEntry, MemStore, SpotifyLibraryOwner, SqliteStore, Store,
};
use fsonos_types::Track;

const FIRST: &str = "first-test-owner";
const SECOND: &str = "second-test-owner";
const PLAYLIST: &str = "spotify:playlist:OwnedList";
const TRACK: &str = "spotify:track:OwnedSong";
const ALBUM: &str = "spotify:album:PublicCatalog";

fn seed(store: &mut dyn Store) {
    store
        .upsert_library(&[LibraryEntry {
            track: Track {
                title: "Owner's song".into(),
                source_uri: TRACK.into(),
                artist: None,
                album: None,
                uri: None,
                duration_secs: None,
            },
            candidate: Some(true),
            taste_weight: Some(1136),
            ..LibraryEntry::default()
        }])
        .unwrap();
    store
        .save_playlists(&[(PLAYLIST.into(), "Owner's playlist".into())])
        .unwrap();
    store
        .save_playlist_contents(&CachedPlaylist::new(
            PLAYLIST.into(),
            10,
            Some(0),
            0,
            true,
            Vec::new(),
        ))
        .unwrap();
    store.save_spotify_heard(&[(TRACK.into(), 10)]).unwrap();
    store
        .save_album_tracks(
            ALBUM,
            &[AlbumTrack {
                disc_number: 1,
                track_number: 1,
                source_uri: TRACK.into(),
                title: "Catalog song".into(),
                duration_secs: Some(180),
            }],
            10,
        )
        .unwrap();
    store
        .save_artist_genres("CatalogArtist", &["jazz".into()], 10)
        .unwrap();
    store.record_play("test-room", TRACK, 10).unwrap();
    store
        .save_playlist_progress(FIRST, PLAYLIST, "first proof")
        .unwrap();
    store
        .save_playlist_progress(SECOND, PLAYLIST, "second proof")
        .unwrap();
}

fn switched(store: &dyn Store) {
    assert_eq!(
        store
            .spotify_library_owner()
            .unwrap()
            .map(|owner| owner.owner_id)
            .as_deref(),
        Some(SECOND)
    );
    let entries = store.library().unwrap();
    assert_eq!(entries.len(), 1, "retained metadata is not deleted");
    assert!(!entries[0].is_candidate());
    assert_eq!(entries[0].taste_weight, None);
    assert!(store.playlists().unwrap().is_empty());
    assert!(store.playlist_contents(PLAYLIST).unwrap().is_none());
    assert!(store.spotify_heard().unwrap().is_empty());
    assert_eq!(store.album_tracks(ALBUM).unwrap().unwrap().tracks.len(), 1);
    assert!(store.artist_genres("CatalogArtist").unwrap().is_some());
    assert_eq!(store.recent_plays(None, 10).unwrap().len(), 1);
    assert!(store.playlist_progress(FIRST, PLAYLIST).unwrap().is_some());
    assert!(store.playlist_progress(SECOND, PLAYLIST).unwrap().is_none());
}

fn exercise(store: &mut dyn Store) {
    assert_eq!(store.spotify_library_owner().unwrap(), None);
    let (first, changed) = store.adopt_spotify_library_owner(FIRST).unwrap();
    assert!(changed);
    assert_eq!(first.generation, 1);
    seed(store);
    let before = store.library().unwrap();
    assert_eq!(
        store.adopt_spotify_library_owner(FIRST).unwrap(),
        (first.clone(), false)
    );
    rejects_invalid_publication(store, &first);
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.playlists().unwrap().len(), 1);
    assert!(store.playlist_contents(PLAYLIST).unwrap().is_some());
    assert_eq!(store.spotify_heard().unwrap().len(), 1);
    for invalid in ["", " \t", "owner\n"] {
        assert!(store.adopt_spotify_library_owner(invalid).is_err());
        assert_eq!(
            store
                .spotify_library_owner()
                .unwrap()
                .map(|owner| owner.owner_id)
                .as_deref(),
            Some(FIRST)
        );
        assert_eq!(store.library().unwrap(), before);
    }
    let (second, changed) = store.adopt_spotify_library_owner(SECOND).unwrap();
    assert!(changed);
    assert_eq!(second.generation, first.generation + 1);
    switched(store);
}

#[test]
fn memory_store_adopts_only_a_proved_owner_and_preserves_catalog_data() {
    exercise(&mut MemStore::default());
}

#[test]
fn sqlite_owner_and_retirement_survive_reopening_together() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("spotify-owner.db");
    let mut store = SqliteStore::open(&file).unwrap();
    exercise(&mut store);
    store.close().unwrap();
    let mut reopened = SqliteStore::open(&file).unwrap();
    switched(&reopened);
    assert!(!reopened.adopt_spotify_library_owner(SECOND).unwrap().1);
    switched(&reopened);
    reopened.close().unwrap();
}

#[test]
fn adopting_an_unowned_legacy_cache_does_not_claim_its_taste() {
    let mut store = MemStore::default();
    seed(&mut store);
    assert!(store.adopt_spotify_library_owner(SECOND).unwrap().1);
    switched(&store);
}

#[test]
fn a_failed_sqlite_adoption_rolls_back_retirement_and_owner_publication() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("spotify-owner-rollback.db");
    let mut store = SqliteStore::open(&file).unwrap();
    store.adopt_spotify_library_owner(FIRST).unwrap();
    seed(&mut store);
    let before = store.library().unwrap();
    // Break a later statement in this test database's transition, after its
    // candidate retirement and playlist clearing would otherwise have run.
    let raw = fsqlite::AsyncConnection::open_sync(file.to_string_lossy()).unwrap();
    raw.execute_sync("DROP TABLE spotify_heard").unwrap();
    assert!(store.adopt_spotify_library_owner(SECOND).is_err());
    assert_eq!(
        store
            .spotify_library_owner()
            .unwrap()
            .map(|owner| owner.owner_id)
            .as_deref(),
        Some(FIRST)
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.playlists().unwrap().len(), 1);
    assert!(store.playlist_contents(PLAYLIST).unwrap().is_some());
    assert!(store.playlist_progress(SECOND, PLAYLIST).unwrap().is_some());
}

fn candidate(song: &str) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: format!("spotify:track:{song}"),
            title: song.into(),
            artist: None,
            album: None,
            uri: None,
            duration_secs: Some(180),
        },
        candidate: Some(true),
        taste_weight: Some(1030),
        ..LibraryEntry::default()
    }
}

fn publish(store: &mut dyn Store, owner: &SpotifyLibraryOwner, song: &str) -> bool {
    let entry = candidate(song);
    let heard = [(entry.track.source_uri.clone(), 20)];
    store
        .publish_spotify_library(
            owner,
            &[entry],
            Some(&[(PLAYLIST.into(), song.into())]),
            &[CachedPlaylist::new(
                PLAYLIST.into(),
                20,
                Some(0),
                0,
                true,
                Vec::new(),
            )],
            Some(&heard),
        )
        .unwrap()
}

fn rejects_invalid_publication(store: &mut dyn Store, owner: &SpotifyLibraryOwner) {
    let before = store.library().unwrap();
    let names = store.playlists().unwrap();
    let contents = store.playlist_contents(PLAYLIST).unwrap();
    let heard = store.spotify_heard().unwrap();
    let invalid = CachedPlaylist::new("not-a-playlist".into(), 20, Some(0), 0, true, Vec::new());
    assert!(
        store
            .publish_spotify_library(
                owner,
                &[candidate("Rejected")],
                Some(&[]),
                &[invalid],
                Some(&[]),
            )
            .is_err()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(store.playlists().unwrap(), names);
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), contents);
    assert_eq!(store.spotify_heard().unwrap(), heard);
}

#[test]
fn two_sqlite_connections_reject_old_publication_and_acknowledgment_after_returning_to_an_owner() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("spotify-owner-race.db");
    let mut first = SqliteStore::open(&file).unwrap();
    let mut second = SqliteStore::open(&file).unwrap();
    let (old_first, _) = first.adopt_spotify_library_owner(FIRST).unwrap();
    assert!(publish(&mut first, &old_first, "FirstSong"));
    let (other, _) = second.adopt_spotify_library_owner(SECOND).unwrap();
    assert!(publish(&mut second, &other, "SecondSong"));

    // An old connection cannot publish its account over the newly adopted one.
    let before = second.library().unwrap();
    assert!(!publish(&mut first, &old_first, "LeakedFirstSong"));
    assert_eq!(second.library().unwrap(), before);
    assert_eq!(
        second.playlists().unwrap(),
        [(PLAYLIST.into(), "SecondSong".into())]
    );

    let (returning, changed) = second.adopt_spotify_library_owner(FIRST).unwrap();
    assert!(changed);
    assert_eq!(returning.owner_id, old_first.owner_id);
    assert_eq!(returning.generation, old_first.generation + 2);
    assert!(publish(&mut second, &returning, "ReturningSong"));
    let before = second.library().unwrap();
    let names = second.playlists().unwrap();
    let contents = second.playlist_contents(PLAYLIST).unwrap();
    let heard = second.spotify_heard().unwrap();
    for stale in [&old_first, &other] {
        assert!(!publish(&mut first, stale, "LeakedSong"));
        assert_eq!(second.library().unwrap(), before);
        assert_eq!(second.playlists().unwrap(), names);
        assert_eq!(second.playlist_contents(PLAYLIST).unwrap(), contents);
        assert_eq!(second.spotify_heard().unwrap(), heard);
    }

    // Even identical snapshot text belongs to this new adoption. The old
    // account receipt must not acknowledge its not-yet-published checkpoint.
    second
        .save_playlist_progress(FIRST, PLAYLIST, "pending snapshot")
        .unwrap();
    assert!(
        !first
            .acknowledge_spotify_playlist(
                &old_first,
                PLAYLIST,
                "pending snapshot",
                "applied snapshot",
            )
            .unwrap()
    );
    assert!(
        !first
            .acknowledge_spotify_playlist(
                &returning,
                PLAYLIST,
                "older snapshot",
                "applied snapshot",
            )
            .unwrap()
    );
    assert_eq!(
        second
            .playlist_progress(FIRST, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some("pending snapshot")
    );
    assert!(
        first
            .acknowledge_spotify_playlist(
                &returning,
                PLAYLIST,
                "pending snapshot",
                "applied snapshot",
            )
            .unwrap()
    );
    first.close().unwrap();
    second.close().unwrap();
    let reopened = SqliteStore::open(&file).unwrap();
    assert_eq!(reopened.spotify_library_owner().unwrap(), Some(returning));
    assert_eq!(reopened.library().unwrap(), before);
    assert_eq!(
        reopened
            .playlist_progress(FIRST, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some("applied snapshot")
    );
    reopened.close().unwrap();
}

#[test]
fn memory_store_rejects_returning_owner_stale_publications_and_acknowledgments() {
    let mut store = MemStore::default();
    let (first, _) = store.adopt_spotify_library_owner(FIRST).unwrap();
    store.adopt_spotify_library_owner(SECOND).unwrap();
    let (returning, _) = store.adopt_spotify_library_owner(FIRST).unwrap();
    assert!(publish(&mut store, &returning, "ReturningSong"));
    let before = store.library().unwrap();
    assert!(!publish(&mut store, &first, "StaleSong"));
    assert_eq!(store.library().unwrap(), before);
    store
        .save_playlist_progress(FIRST, PLAYLIST, "pending")
        .unwrap();
    assert!(
        !store
            .acknowledge_spotify_playlist(&first, PLAYLIST, "pending", "applied",)
            .unwrap()
    );
    assert!(
        store
            .acknowledge_spotify_playlist(&returning, PLAYLIST, "pending", "applied",)
            .unwrap()
    );
}
