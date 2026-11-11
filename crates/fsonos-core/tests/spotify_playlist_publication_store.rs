//! Playlist continuation publishes current evidence without replaying an older
//! library snapshot. Both stores must honor the same atomic receipts and merge.

use fsonos_core::store::{
    CachedPlaylist, LibraryEntry, LibraryOrigin, MemStore, PlaylistTrack, SpotifyLibraryOwner,
    SpotifyPlaylistPublication, SpotifyPlaylistPublicationFence, SpotifyPlaylistPublicationItem,
    SqliteStore, Store,
};
use fsonos_types::Track;

const OWNER: &str = "playlist-publication-owner";
const PLAYLIST: &str = "spotify:playlist:PrivateList";
const DIRECTORY: &str = "directory before publication";
const PUBLISHED: &str = "directory after publication";
const PROOF: &str = "validated playlist snapshot";
const APPLIED: &str = "applied playlist snapshot";

fn entry(name: &str) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            source_uri: format!("spotify:track:{name}"),
            title: name.into(),
            artist: Some("Catalog Artist".into()),
            album: Some("Catalog Album".into()),
            duration_secs: Some(180),
            uri: None,
        },
        origin: LibraryOrigin::Taste,
        artist_id: Some("CatalogArtist".into()),
        album_uri: Some("spotify:album:CatalogAlbum".into()),
        added: 20,
        candidate: Some(true),
        taste_weight: Some(1080),
        ..LibraryEntry::default()
    }
}

fn contents(entries: &[LibraryEntry], complete: bool) -> CachedPlaylist {
    let examined = u32::try_from(entries.len()).unwrap();
    CachedPlaylist::new(
        PLAYLIST.into(),
        20,
        Some(examined + u32::from(!complete)),
        examined,
        complete,
        (1_u32..)
            .zip(entries)
            .map(|(position, entry)| PlaylistTrack {
                position,
                source_uri: entry.track.source_uri.clone(),
                title: entry.track.title.clone(),
                artist: entry.track.artist.clone(),
                album: entry.track.album.clone(),
                album_uri: entry.album_uri.clone(),
                duration_secs: entry.track.duration_secs,
                disc_number: entry.disc_number,
                track_number: entry.track_number,
                explicit: entry.explicit,
            })
            .collect(),
    )
}

fn publication(entries: Vec<LibraryEntry>) -> SpotifyPlaylistPublication {
    SpotifyPlaylistPublication {
        expected_directory: Some(DIRECTORY.into()),
        directory: Some(PUBLISHED.into()),
        playlists: Some(vec![(PLAYLIST.into(), "New name".into())]),
        observations: vec![SpotifyPlaylistPublicationItem {
            contents: contents(&entries, true),
            expected_progress: PROOF.into(),
            applied_progress: APPLIED.into(),
        }],
        entries,
    }
}

fn seed(store: &mut dyn Store) -> SpotifyLibraryOwner {
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert!(
        store
            .save_spotify_playlist_directory(&owner, None, DIRECTORY, None)
            .unwrap()
    );
    assert!(
        store
            .save_spotify_playlist_progress(&owner, PLAYLIST, None, PROOF)
            .unwrap()
    );
    store
        .save_playlists(&[(PLAYLIST.into(), "Old name".into())])
        .unwrap();
    store
        .save_spotify_heard(&[("spotify:track:Heard".into(), 10)])
        .unwrap();
    owner
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    library: Vec<LibraryEntry>,
    names: Vec<(String, String)>,
    contents: Option<CachedPlaylist>,
    progress: Option<String>,
    directory: Option<String>,
    heard: Vec<(String, i64)>,
}

fn state(store: &dyn Store, owner: &SpotifyLibraryOwner) -> State {
    State {
        library: store.library().unwrap(),
        names: store.playlists().unwrap(),
        contents: store.playlist_contents(PLAYLIST).unwrap(),
        progress: store.playlist_progress(OWNER, PLAYLIST).unwrap(),
        directory: store.spotify_playlist_directory(owner).unwrap(),
        heard: store.spotify_heard().unwrap(),
    }
}

fn additive_merge(store: &mut dyn Store) {
    let owner = seed(store);
    let mut saved = entry("Saved");
    saved.origin = LibraryOrigin::Both;
    saved.added = 41;
    saved.track.title = "Current full-sync title".into();
    saved.genres = vec!["current catalog genre".into()];
    saved.explicit = true;
    saved.taste_weight = Some(1136);
    let mut current_taste = entry("Taste");
    current_taste.track.title = "Current taste metadata".into();
    current_taste.artist_id = Some("CurrentArtist".into());
    current_taste.taste_weight = Some(1000);
    let mut retired = entry("Retired");
    retired.origin = LibraryOrigin::LikedTrack;
    retired.candidate = Some(false);
    retired.taste_weight = Some(1200);
    let fresh = entry("AddedByNewFullSync");
    store
        .upsert_library(&[saved.clone(), current_taste.clone(), retired, fresh.clone()])
        .unwrap();

    let mut new = entry("New");
    new.track.uri = Some("x-sonos-spotify:spotify%3atrack%3aNew".into());
    let incoming = publication(vec![entry("Saved"), entry("Taste"), entry("Retired"), new]);
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &incoming)
            .unwrap()
    );
    let after = state(store, &owner);
    assert_eq!(
        after
            .library
            .iter()
            .find(|row| row.track.source_uri == saved.track.source_uri),
        Some(&saved)
    );
    current_taste.taste_weight = Some(1080);
    assert_eq!(
        after
            .library
            .iter()
            .find(|row| row.track.source_uri == current_taste.track.source_uri),
        Some(&current_taste)
    );
    assert_eq!(
        after
            .library
            .iter()
            .find(|row| row.track.source_uri == fresh.track.source_uri),
        Some(&fresh),
        "unmentioned new candidates must survive"
    );
    assert_eq!(
        after
            .library
            .iter()
            .find(|row| row.track.source_uri == "spotify:track:Retired"),
        Some(&entry("Retired")),
        "a retired like becomes current taste without restoring the like"
    );
    assert_eq!(
        after
            .library
            .iter()
            .find(|row| row.track.source_uri == "spotify:track:New"),
        Some(&entry("New")),
        "household-specific renderer URIs never enter the shared library cache"
    );
    assert_eq!(after.library.len(), 5);
    assert_eq!(after.names, incoming.playlists.clone().unwrap());
    assert_eq!(
        after.contents,
        Some(incoming.observations[0].contents.clone())
    );
    assert_eq!(after.progress.as_deref(), Some(APPLIED));
    assert_eq!(after.directory.as_deref(), Some(PUBLISHED));
    assert_eq!(after.heard, [("spotify:track:Heard".into(), 10)]);
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &incoming)
            .unwrap()
    );
    assert_eq!(
        state(store, &owner),
        after,
        "replaying a consumed proof writes nothing"
    );
}

#[test]
fn memory_preserves_newer_rows_and_merges_only_current_playlist_evidence() {
    additive_merge(&mut MemStore::default());
}

#[test]
fn sqlite_preserves_newer_rows_and_commits_projections_with_their_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("playlist-publication.db");
    let mut store = SqliteStore::open(&path).unwrap();
    additive_merge(&mut store);
    let owner = store.spotify_library_owner().unwrap().unwrap();
    let before = state(&store, &owner);
    store.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(state(&reopened, &owner), before);
    reopened.close().unwrap();
}

fn rejected_evidence(store: &mut dyn Store) {
    let owner = seed(store);
    store.upsert_library(&[entry("Existing")]).unwrap();
    let before = state(store, &owner);
    let good = publication(vec![entry("New")]);

    let mut stale = good.clone();
    stale.expected_directory = Some("older directory".into());
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &stale)
            .unwrap()
    );
    stale = good.clone();
    stale.observations[0].expected_progress = "older snapshot".into();
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &stale)
            .unwrap()
    );
    stale = good.clone();
    let mut second = stale.observations[0].clone();
    second.contents = CachedPlaylist::new(
        "spotify:playlist:OtherList".into(),
        20,
        Some(0),
        0,
        true,
        Vec::new(),
    );
    stale.observations.push(second);
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &stale)
            .unwrap()
    );
    assert_eq!(
        state(store, &owner),
        before,
        "a late proof mismatch must prevent every write"
    );

    for invalid in 0..6 {
        let mut bad = good.clone();
        match invalid {
            0 => bad.entries[0].candidate = Some(false),
            1 => bad.entries[0].origin = LibraryOrigin::LikedTrack,
            2 => bad.entries[0].track.source_uri = "spotify:track:Unproved".into(),
            3 => bad.entries.push(bad.entries[0].clone()),
            4 => bad.observations[0].contents.revision = "invalid revision".into(),
            _ => bad.expected_directory = None,
        }
        assert!(
            store
                .publish_spotify_playlist_continuation(&owner, &bad)
                .is_err()
        );
        assert_eq!(state(store, &owner), before);
    }

    assert!(
        !store
            .save_spotify_playlist_progress(&owner, PLAYLIST, None, "stale writer")
            .unwrap()
    );
    assert!(
        !store
            .save_spotify_playlist_progress(
                &owner,
                PLAYLIST,
                Some("older snapshot"),
                "stale writer"
            )
            .unwrap()
    );
    assert_eq!(state(store, &owner), before);
    assert!(
        store
            .save_spotify_playlist_progress(&owner, PLAYLIST, Some(PROOF), "new reader")
            .unwrap()
    );
    let newer = state(store, &owner);
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &good)
            .unwrap()
    );
    assert_eq!(state(store, &owner), newer);

    store.adopt_spotify_library_owner("other-owner").unwrap();
    let (returning, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    assert!(returning.generation > owner.generation);
    assert!(
        !store
            .save_spotify_playlist_progress(&owner, PLAYLIST, None, "stale adoption")
            .unwrap()
    );
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &good)
            .unwrap()
    );
    assert!(store.playlist_progress(OWNER, PLAYLIST).unwrap().is_none());
    assert!(
        store
            .save_spotify_playlist_progress(&returning, PLAYLIST, None, "returning owner")
            .unwrap()
    );
}

#[test]
fn memory_rejects_stale_or_invalid_publication_without_partial_changes() {
    rejected_evidence(&mut MemStore::default());
}

#[test]
fn sqlite_rejects_stale_or_invalid_publication_without_partial_changes() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    rejected_evidence(&mut store);
}

fn retains_complete_contents(store: &mut dyn Store) {
    let owner = seed(store);
    let old = contents(&[entry("PreviouslyComplete")], true);
    store.save_playlist_contents(&old).unwrap();
    let mut incoming = publication(vec![entry("AcceptedPartial")]);
    incoming.observations[0].contents = contents(&incoming.entries, false);
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &incoming)
            .unwrap()
    );
    assert_eq!(store.playlist_contents(PLAYLIST).unwrap(), Some(old));
    assert_eq!(store.library().unwrap(), incoming.entries);
    assert_eq!(
        store.playlist_progress(OWNER, PLAYLIST).unwrap().as_deref(),
        Some(APPLIED)
    );
}

#[test]
fn memory_retains_complete_contents_while_adding_a_proved_partial_window() {
    retains_complete_contents(&mut MemStore::default());
}

#[test]
fn sqlite_retains_complete_contents_while_adding_a_proved_partial_window() {
    retains_complete_contents(&mut SqliteStore::open_in_memory().unwrap());
}

fn full_publication_fences(store: &mut dyn Store) {
    let owner = seed(store);
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "full read")
            .unwrap()
    );
    let stale = SpotifyPlaylistPublicationFence {
        expected_directory: Some(DIRECTORY.into()),
        directory: Some("stale full publication".into()),
        observations: Vec::new(),
    };
    let incoming = publication(vec![entry("NewBackgroundCandidate")]);
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &incoming)
            .unwrap()
    );
    let before = state(store, &owner);
    let mut retire = entry("NewBackgroundCandidate");
    retire.candidate = Some(false);
    assert!(
        !store
            .publish_spotify_library_from_progress(
                &owner,
                "full read",
                &[retire],
                Some(&[]),
                &[],
                Some(&[]),
                Some(&stale),
                None
            )
            .unwrap()
    );
    assert_eq!(
        state(store, &owner),
        before,
        "a full read cannot erase a newer disjoint playlist publication"
    );
    assert_eq!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .as_deref(),
        Some("full read")
    );

    let mut acknowledged = incoming.observations[0].clone();
    acknowledged.expected_progress = APPLIED.into();
    acknowledged.applied_progress = "full publication acknowledged".into();
    let fresh = SpotifyPlaylistPublicationFence {
        expected_directory: Some(PUBLISHED.into()),
        directory: Some("fresh full publication".into()),
        observations: vec![acknowledged],
    };
    assert!(
        store
            .publish_spotify_library_from_progress(
                &owner,
                "full read",
                &[entry("FullCandidate")],
                None,
                &[],
                None,
                Some(&fresh),
                None
            )
            .unwrap()
    );
    assert!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.playlist_progress(OWNER, PLAYLIST).unwrap().as_deref(),
        Some("full publication acknowledged")
    );
    assert_eq!(
        store.spotify_playlist_directory(&owner).unwrap().as_deref(),
        Some("fresh full publication")
    );
    let after = state(store, &owner);
    let mut late = incoming;
    late.expected_directory = Some(PUBLISHED.into());
    late.observations[0].expected_progress = APPLIED.into();
    assert!(
        !store
            .publish_spotify_playlist_continuation(&owner, &late)
            .unwrap()
    );
    assert_eq!(
        state(store, &owner),
        after,
        "the accepted full publication also fences an older continuation"
    );
}

#[test]
fn memory_serializes_full_and_background_publications_in_both_orders() {
    full_publication_fences(&mut MemStore::default());
}

#[test]
fn sqlite_serializes_full_and_background_publications_in_both_orders() {
    full_publication_fences(&mut SqliteStore::open_in_memory().unwrap());
}

#[test]
fn another_sqlite_connection_can_supersede_a_pending_playlist_writer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("playlist-competing-read.db");
    let mut first = SqliteStore::open(&path).unwrap();
    let owner = seed(&mut first);
    let mut second = SqliteStore::open(&path).unwrap();
    assert!(
        second
            .save_spotify_playlist_progress(&owner, PLAYLIST, Some(PROOF), "second reader")
            .unwrap()
    );
    assert!(
        !first
            .save_spotify_playlist_progress(&owner, PLAYLIST, Some(PROOF), "old reader")
            .unwrap()
    );
    assert!(
        !first
            .publish_spotify_playlist_continuation(&owner, &publication(vec![entry("Stale")]))
            .unwrap()
    );
    assert!(second.library().unwrap().is_empty());
    assert_eq!(
        second
            .playlist_progress(OWNER, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some("second reader")
    );
}

#[test]
fn sqlite_failure_after_library_writes_keeps_the_entire_publication_pending() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("playlist-rollback.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let owner = seed(&mut store);
    store.upsert_library(&[entry("Existing")]).unwrap();
    let before = state(&store, &owner);
    let incoming = publication(vec![entry("MustRollBack")]);
    let raw = fsqlite::AsyncConnection::open_sync(path.to_string_lossy()).unwrap();
    // Contents are written after candidate and public directory updates. The
    // exact private fences remain readable, so this exercises a late failure.
    raw.execute_sync("DROP TABLE spotify_playlist_contents")
        .unwrap();
    assert!(
        store
            .publish_spotify_playlist_continuation(&owner, &incoming)
            .is_err()
    );
    assert_eq!(store.library().unwrap(), before.library);
    assert_eq!(store.playlists().unwrap(), before.names);
    assert_eq!(
        store.playlist_progress(OWNER, PLAYLIST).unwrap(),
        before.progress
    );
    assert_eq!(
        store.spotify_playlist_directory(&owner).unwrap(),
        before.directory
    );
    raw.execute_sync("CREATE TABLE spotify_playlist_contents (playlist_uri TEXT PRIMARY KEY, observation TEXT NOT NULL)").unwrap();
    store.close().unwrap();
    let mut reopened = SqliteStore::open(&path).unwrap();
    assert!(
        reopened
            .publish_spotify_playlist_continuation(&owner, &incoming)
            .unwrap()
    );
    assert!(
        reopened
            .library()
            .unwrap()
            .iter()
            .any(|entry| entry.track.source_uri == "spotify:track:MustRollBack")
    );
    assert_eq!(
        reopened
            .playlist_progress(OWNER, PLAYLIST)
            .unwrap()
            .as_deref(),
        Some(APPLIED)
    );
    reopened.close().unwrap();
}
