//! Public catalog revisions permit reuse across browse pages without hiding a
//! committed write. Private sync progress and failed publication never invalidate.

use fsonos_core::store::{
    AlbumTrack, CachedPlaylist, LibraryEntry, LibraryOrigin, MemStore, PlaylistTrack,
    SpotifyArtistDiscoveryFence, SpotifyPlaylistPublication, SpotifyPlaylistPublicationItem,
    SqliteStore, Store,
};
use fsonos_types::Track;

const OWNER: &str = "catalog-owner";
const PLAYLIST: &str = "spotify:playlist:Catalog";
const ALBUM: &str = "spotify:album:Catalog";

fn entry(name: &str) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: format!("spotify:track:{name}"),
            title: name.into(),
            artist: None,
            album: None,
            uri: None,
            duration_secs: None,
        },
        origin: LibraryOrigin::Taste,
        candidate: Some(true),
        taste_weight: Some(1080),
        ..LibraryEntry::default()
    }
}

fn observation(entries: &[LibraryEntry], complete: bool) -> CachedPlaylist {
    let examined = u32::try_from(entries.len()).unwrap();
    CachedPlaylist::new(
        PLAYLIST.into(),
        20,
        Some(examined + u32::from(!complete)),
        examined,
        complete,
        (1_u32..)
            .zip(entries)
            .map(|(position, row)| PlaylistTrack {
                position,
                source_uri: row.track.source_uri.clone(),
                title: row.track.title.clone(),
                artist: row.track.artist.clone(),
                album: row.track.album.clone(),
                album_uri: row.album_uri.clone(),
                duration_secs: row.track.duration_secs,
                disc_number: row.disc_number,
                track_number: row.track_number,
                explicit: row.explicit,
            })
            .collect(),
    )
}

fn revision(store: &dyn Store) -> u64 {
    store.library_catalog_revision().unwrap().unwrap()
}

fn advanced(store: &dyn Store, previous: u64) -> u64 {
    let current = revision(store);
    assert_eq!(current, previous + 1, "one public operation advances once");
    current
}

fn both(check: fn(&mut dyn Store)) {
    check(&mut MemStore::default());
    check(&mut SqliteStore::open_in_memory().unwrap());
}

fn ordinary_writes(store: &mut dyn Store) {
    let mut before = revision(store);
    assert_eq!(before, 0);
    store.upsert_library(&[]).unwrap();
    store.save_playlists(&[]).unwrap();
    store.save_album_tracks(ALBUM, &[], 0).unwrap();
    assert_eq!(revision(store), before, "empty writes remain empty");

    let mut row = entry("Original");
    store.upsert_library(std::slice::from_ref(&row)).unwrap();
    before = advanced(store, before);
    row.track.title = "A changed title".into();
    store.upsert_library(std::slice::from_ref(&row)).unwrap();
    before = advanced(store, before);
    assert_eq!(store.library().unwrap(), [row.clone()]);

    let track = AlbumTrack {
        disc_number: 1,
        track_number: 1,
        source_uri: row.track.source_uri.clone(),
        title: row.track.title.clone(),
        duration_secs: Some(120),
        explicit: Some(false),
    };
    store
        .save_album_tracks(ALBUM, std::slice::from_ref(&track), 10)
        .unwrap();
    before = advanced(store, before);
    store.save_album_tracks(ALBUM, &[track], 20).unwrap();
    before = advanced(store, before);
    assert_eq!(store.album_tracks(ALBUM).unwrap().unwrap().fetched_at, 20);
    store.save_album_tracks(ALBUM, &[], 30).unwrap();
    before = advanced(store, before);
    store.save_album_tracks(ALBUM, &[], 40).unwrap();
    assert_eq!(revision(store), before, "absent album deletion is a no-op");

    store
        .save_playlists(&[(PLAYLIST.into(), "First".into())])
        .unwrap();
    before = advanced(store, before);
    store
        .save_playlists(&[(PLAYLIST.into(), "Renamed".into())])
        .unwrap();
    before = advanced(store, before);
    store.save_playlists(&[]).unwrap();
    before = advanced(store, before);
    store.save_playlists(&[]).unwrap();
    assert_eq!(revision(store), before);
    store
        .save_playlist_contents(&observation(&[row], true))
        .unwrap();
    advanced(store, before);
}

#[test]
fn ordinary_catalog_writes_and_deletions_invalidate_both_stores() {
    both(ordinary_writes);
}

fn private_writes(store: &mut dyn Store) {
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    let before = revision(store);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "ready")
            .unwrap()
    );
    assert!(
        store
            .save_spotify_playlist_directory(&owner, None, "directory", None)
            .unwrap()
    );
    assert!(
        store
            .save_spotify_followed_directory(&owner, None, "followed")
            .unwrap()
    );
    assert!(
        store
            .save_spotify_playlist_progress(&owner, PLAYLIST, None, "proof")
            .unwrap()
    );
    assert!(
        store
            .acknowledge_spotify_playlist(&owner, PLAYLIST, "proof", "applied")
            .unwrap()
    );
    store.save_playlist_scan_cursor(OWNER, "Catalog").unwrap();
    store
        .save_spotify_heard(&[("spotify:track:Heard".into(), 10)])
        .unwrap();
    store.record_play("Den", "spotify:track:Heard", 20).unwrap();
    store
        .save_artist_genres("Artist", &["jazz".into()], 20)
        .unwrap();
    store.save_album_misses(&[(ALBUM.into(), 100)]).unwrap();
    assert_eq!(revision(store), before);

    assert!(
        store
            .publish_spotify_library(&owner, &[], None, &[], Some(&[]))
            .unwrap()
    );
    let artist = SpotifyArtistDiscoveryFence {
        expected: None,
        progress: "next private artist".into(),
    };
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                "ready",
                &[],
                None,
                &[],
                None,
                None,
                Some(&artist),
            )
            .unwrap()
    );
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("next private artist")
    );
    assert!(
        store
            .publish_spotify_playlist_continuation(
                &owner,
                &SpotifyPlaylistPublication {
                    entries: Vec::new(),
                    playlists: None,
                    observations: Vec::new(),
                    expected_directory: Some("directory".into()),
                    directory: Some("private continuation".into()),
                },
            )
            .unwrap()
    );
    assert_eq!(
        revision(store),
        before,
        "private-only publication leaves the catalog usable"
    );
}

#[test]
fn checkpoints_listening_and_unrelated_caches_do_not_invalidate() {
    both(private_writes);
}

fn ownership(store: &mut dyn Store) {
    let mut before = revision(store);
    let (owner, changed) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert!(changed);
    before = advanced(store, before);
    assert_eq!(
        store.adopt_spotify_library_owner(OWNER).unwrap(),
        (owner.clone(), false)
    );
    assert!(store.adopt_spotify_library_owner("").is_err());
    assert_eq!(revision(store), before);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "same")
            .unwrap()
    );
    assert_eq!(
        store
            .adopt_spotify_library_from_progress(OWNER, Some(&owner), "same")
            .unwrap(),
        Some((owner.clone(), false))
    );
    assert_eq!(
        revision(store),
        before,
        "same-owner proof rebinding is private"
    );

    assert!(
        store
            .save_spotify_library_progress("incoming", Some(&owner), None, "complete")
            .unwrap()
    );
    assert!(
        store
            .adopt_spotify_library_from_progress("incoming", Some(&owner), "stale")
            .unwrap()
            .is_none()
    );
    assert_eq!(revision(store), before);
    let (incoming, changed) = store
        .adopt_spotify_library_from_progress("incoming", Some(&owner), "complete")
        .unwrap()
        .unwrap();
    assert!(changed);
    advanced(store, before);
    assert_eq!(store.spotify_library_owner().unwrap(), Some(incoming));
}

#[test]
fn only_a_changed_verified_owner_invalidates() {
    both(ownership);
}

fn full_publications(store: &mut dyn Store) {
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    let mut before = revision(store);
    let row = entry("Published");
    let names = [(PLAYLIST.into(), "Published list".into())];
    let contents = observation(std::slice::from_ref(&row), true);
    assert!(
        store
            .publish_spotify_library(
                &owner,
                std::slice::from_ref(&row),
                Some(&names),
                std::slice::from_ref(&contents),
                None,
            )
            .unwrap()
    );
    before = advanced(store, before);
    let mut stale = owner.clone();
    stale.generation += 1;
    assert!(
        !store
            .publish_spotify_library(&stale, &[entry("Wrong")], Some(&[]), &[], None)
            .unwrap()
    );
    let mut malformed = contents.clone();
    malformed.revision.push_str("invalid");
    assert!(
        store
            .publish_spotify_library(&owner, &[entry("Wrong")], Some(&[]), &[malformed], None)
            .is_err()
    );
    assert_eq!(revision(store), before);
    assert_eq!(store.library().unwrap(), [row]);
    assert_eq!(store.playlists().unwrap(), names);
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(contents));

    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "ready")
            .unwrap()
    );
    assert!(
        !store
            .publish_spotify_library_from_progress(
                &owner,
                "superseded",
                &[entry("Wrong")],
                None,
                &[],
                None,
                None,
                None,
            )
            .unwrap()
    );
    assert_eq!(revision(store), before);
    let mut artist = SpotifyArtistDiscoveryFence {
        expected: Some("superseded artist cursor".into()),
        progress: "next artist cursor".into(),
    };
    assert!(
        !store
            .publish_spotify_library_from_progress(
                &owner,
                "ready",
                &[entry("Wrong")],
                None,
                &[],
                None,
                None,
                Some(&artist),
            )
            .unwrap()
    );
    assert_eq!(revision(store), before);
    assert_eq!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .as_deref(),
        Some("ready")
    );
    artist.expected = None;
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                "ready",
                &[entry("Fresh")],
                None,
                &[],
                None,
                None,
                Some(&artist),
            )
            .unwrap()
    );
    advanced(store, before);
    assert_eq!(
        store.spotify_artist_discovery(&owner).unwrap().as_deref(),
        Some("next artist cursor")
    );
    assert_eq!(
        store.spotify_library_progress(OWNER, Some(&owner)).unwrap(),
        None
    );
}

#[test]
fn full_publication_and_proof_consumption_share_the_revision_fence() {
    both(full_publications);
}

fn playlist_publications(store: &mut dyn Store) {
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    let row = entry("Playlist");
    assert!(
        store
            .save_spotify_playlist_directory(&owner, None, "directory", None)
            .unwrap()
    );
    assert!(
        store
            .save_spotify_playlist_progress(&owner, PLAYLIST, None, "proof")
            .unwrap()
    );
    let publication = SpotifyPlaylistPublication {
        entries: vec![row.clone()],
        playlists: Some(vec![(PLAYLIST.into(), "Name".into())]),
        observations: vec![SpotifyPlaylistPublicationItem {
            contents: observation(std::slice::from_ref(&row), true),
            expected_progress: "proof".into(),
            applied_progress: "applied".into(),
        }],
        expected_directory: Some("directory".into()),
        directory: Some("published".into()),
    };
    let before = revision(store);
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    let before = advanced(store, before);
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &publication)
            .unwrap()
    );
    assert_eq!(
        revision(store),
        before,
        "replayed proof does not invalidate"
    );

    assert!(
        store
            .save_spotify_playlist_progress(&owner, PLAYLIST, Some("applied"), "partial")
            .unwrap()
    );
    let partial = SpotifyPlaylistPublication {
        entries: vec![row.clone()],
        playlists: None,
        observations: vec![SpotifyPlaylistPublicationItem {
            contents: observation(std::slice::from_ref(&row), false),
            expected_progress: "partial".into(),
            applied_progress: "partial applied".into(),
        }],
        expected_directory: Some("published".into()),
        directory: None,
    };
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &partial)
            .unwrap()
    );
    assert_eq!(
        revision(store),
        before,
        "retaining complete contents and equal weight changes only proof"
    );
    assert!(store.playlist_contents(PLAYLIST).unwrap().unwrap().complete);

    let mut stronger = partial;
    stronger.entries[0].taste_weight = Some(1200);
    stronger.observations[0].expected_progress = "partial applied".into();
    stronger.observations[0].applied_progress = "stronger applied".into();
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &stronger)
            .unwrap()
    );
    advanced(store, before);
    assert_eq!(store.library().unwrap()[0].taste_weight, Some(1200));
}

#[test]
fn playlist_merge_invalidates_only_when_public_rows_are_written() {
    both(playlist_publications);
}

#[test]
fn another_sqlite_connection_observes_the_revision_and_reopen_preserves_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-revision.db");
    let mut writer = SqliteStore::open(&path).unwrap();
    let reader = SqliteStore::open(&path).unwrap();
    let before = revision(&reader);
    writer
        .upsert_library(&[entry("AcrossConnections")])
        .unwrap();
    let after = advanced(&reader, before);
    assert_eq!(
        reader.library().unwrap()[0].track.title,
        "AcrossConnections"
    );
    reader.close().unwrap();
    writer.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(revision(&reopened), after);
    reopened.close().unwrap();
}
