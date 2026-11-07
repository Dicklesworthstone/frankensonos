//! Optional artist-cache failures must not silence an otherwise playable DJ.

use super::{apply_library_read, genre_completion, pool_from_store, works_from_store};
use crate::SpotifyError;
use crate::library::{LibraryItem, Origin};
use fsonos_core::store::{
    Action, ActionFilter, AlbumTrack, AuthEntry, CachedAlbum, CachedPlayer, CachedPlaylist,
    DjSession, Feedback, FeedbackKey, LibraryEntry, LoggedAction, MemStore, PlayName, PlayRecord,
    SpotifyLibraryOwner, Store, StoreError, StoredScene, StoredSchedule,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Player, ZoneGroup};
use std::cell::RefCell;
use std::ops::Range;

#[derive(Default)]
struct FailingStore {
    inner: MemStore,
    fail_library: bool,
    genre_reads: RefCell<Vec<String>>,
}

// Delegate unrelated store operations so this fault tests the production
// pool-building entry points, without adding faults to the production Store.
macro_rules! forward {
    () => {};
    (fn $name:ident(&self $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty; $($rest:tt)*) => {
        fn $name(&self, $($arg: $ty),*) -> $result {
            self.inner.$name($($arg),*)
        }
        forward!($($rest)*);
    };
    (fn $name:ident(&mut self $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty; $($rest:tt)*) => {
        fn $name(&mut self, $($arg: $ty),*) -> $result {
            self.inner.$name($($arg),*)
        }
        forward!($($rest)*);
    };
}

impl Store for FailingStore {
    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        if self.fail_library {
            Err(StoreError::Backend("library unavailable".into()))
        } else {
            self.inner.library()
        }
    }

    fn artist_genres(&self, artist_id: &str) -> Result<Option<(Vec<String>, i64)>, StoreError> {
        self.genre_reads.borrow_mut().push(artist_id.to_owned());
        if artist_id == "broken-artist" {
            Err(StoreError::Backend("artist cache unavailable".into()))
        } else {
            self.inner.artist_genres(artist_id)
        }
    }

    forward! {
        fn record_play( &mut self, zone: &str, source_uri: &str, played_at: i64, ) -> Result<(), StoreError>;
        fn recent_plays(&self, zone: Option<&str>, limit: usize) -> Result<Vec<PlayRecord>, StoreError>;
        fn name_play(&mut self, name: &PlayName, at: i64) -> Result<(), StoreError>;
        fn play_names(&self, uris: &[&str]) -> Result<Vec<PlayName>, StoreError>;
        fn save_players( &mut self, household: &str, players: &[Player], seen_at: i64, ) -> Result<(), StoreError>;
        fn cached_players(&self) -> Result<Vec<CachedPlayer>, StoreError>;
        fn save_groups( &mut self, household: &str, groups: &[ZoneGroup], updated: i64, ) -> Result<(), StoreError>;
        fn cached_groups(&self, household: &str) -> Result<Vec<ZoneGroup>, StoreError>;
        fn upsert_library(&mut self, entries: &[LibraryEntry]) -> Result<(), StoreError>;
        fn spotify_library_owner(&self) -> Result<Option<SpotifyLibraryOwner>, StoreError>;
        fn adopt_spotify_library_owner( &mut self, owner_id: &str, ) -> Result<(SpotifyLibraryOwner, bool), StoreError>;
        fn publish_spotify_library( &mut self, owner: &SpotifyLibraryOwner, entries: &[LibraryEntry], playlists: Option<&[(String, String)]>, contents: &[CachedPlaylist], heard: Option<&[(String, i64)]>, ) -> Result<bool, StoreError>;
        fn acknowledge_spotify_playlist( &mut self, owner: &SpotifyLibraryOwner, playlist_uri: &str, expected: &str, applied: &str, ) -> Result<bool, StoreError>;
        fn save_render_params( &mut self, household: &str, params: &SpotifyRenderParams, learned_at: i64, ) -> Result<(), StoreError>;
        fn render_params( &self, household: &str, ) -> Result<Option<(SpotifyRenderParams, i64)>, StoreError>;
        fn save_auth( &mut self, service: &str, refresh_token: &str, expires: i64, ) -> Result<(), StoreError>;
        fn auth(&self, service: &str) -> Result<Option<AuthEntry>, StoreError>;
        fn save_dj_session(&mut self, session: &DjSession) -> Result<(), StoreError>;
        fn dj_session(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError>;
        fn dj_sessions(&self) -> Result<Vec<DjSession>, StoreError>;
        fn delete_dj_session(&mut self, coordinator: &str) -> Result<(), StoreError>;
        fn record_feedback(&mut self, feedback: &Feedback) -> Result<(), StoreError>;
        fn feedback( &self, key: FeedbackKey<'_>, window: Range<i64>, ) -> Result<Vec<Feedback>, StoreError>;
        fn feedback_between(&self, window: Range<i64>) -> Result<Vec<Feedback>, StoreError>;
        fn save_album_tracks( &mut self, album_uri: &str, tracks: &[AlbumTrack], fetched_at: i64, ) -> Result<(), StoreError>;
        fn album_tracks(&self, album_uri: &str) -> Result<Option<CachedAlbum>, StoreError>;
        fn save_artist_genres( &mut self, artist_id: &str, genres: &[String], fetched_at: i64, ) -> Result<(), StoreError>;
        fn save_album_misses(&mut self, misses: &[(String, i64)]) -> Result<(), StoreError>;
        fn album_misses(&self) -> Result<Vec<(String, i64)>, StoreError>;
        fn save_playlists(&mut self, playlists: &[(String, String)]) -> Result<(), StoreError>;
        fn playlists(&self) -> Result<Vec<(String, String)>, StoreError>;
        fn spotify_playlist_directory( &self, owner: &SpotifyLibraryOwner, ) -> Result<Option<String>, StoreError>;
        fn save_spotify_playlist_directory( &mut self, owner: &SpotifyLibraryOwner, expected: Option<&str>, progress: &str, active: Option<&[String]>, ) -> Result<bool, StoreError>;
        fn save_playlist_contents(&mut self, contents: &CachedPlaylist) -> Result<(), StoreError>;
        fn playlist_contents(&self, playlist_uri: &str) -> Result<Option<CachedPlaylist>, StoreError>;
        fn playlist_scan_cursor(&self, owner_id: &str) -> Result<Option<String>, StoreError>;
        fn save_playlist_scan_cursor( &mut self, owner_id: &str, playlist_id: &str, ) -> Result<(), StoreError>;
        fn playlist_progress( &self, owner_id: &str, playlist_uri: &str, ) -> Result<Option<String>, StoreError>;
        fn save_playlist_progress( &mut self, owner_id: &str, playlist_uri: &str, progress: &str, ) -> Result<(), StoreError>;
        fn retain_playlist_progress( &mut self, owner_id: &str, playlist_uris: &[String], ) -> Result<(), StoreError>;
        fn save_spotify_heard(&mut self, plays: &[(String, i64)]) -> Result<(), StoreError>;
        fn spotify_heard(&self) -> Result<Vec<(String, i64)>, StoreError>;
        fn record_action(&mut self, action: &Action) -> Result<i64, StoreError>;
        fn recent_actions(&self, filter: &ActionFilter) -> Result<Vec<LoggedAction>, StoreError>;
        fn last_undoable_action( &self, client: Option<&str>, ) -> Result<Option<LoggedAction>, StoreError>;
        fn prune_actions(&mut self, keep: usize, older_than: i64) -> Result<usize, StoreError>;
        fn save_scene(&mut self, scene: &StoredScene) -> Result<(), StoreError>;
        fn scene(&self, name: &str) -> Result<Option<StoredScene>, StoreError>;
        fn scenes(&self) -> Result<Vec<StoredScene>, StoreError>;
        fn delete_scene(&mut self, name: &str) -> Result<bool, StoreError>;
        fn add_schedule(&mut self, schedule: &StoredSchedule) -> Result<i64, StoreError>;
        fn put_schedule(&mut self, schedule: &StoredSchedule) -> Result<bool, StoreError>;
        fn schedules(&self) -> Result<Vec<StoredSchedule>, StoreError>;
        fn set_schedule_enabled(&mut self, id: i64, enabled: bool) -> Result<bool, StoreError>;
        fn mark_schedule_fired(&mut self, id: i64, at: i64) -> Result<bool, StoreError>;
        fn delete_schedule(&mut self, id: i64) -> Result<bool, StoreError>;
    }
}

fn store() -> (FailingStore, SpotifyLibraryOwner) {
    let mut store = FailingStore::default();
    let (owner, _) = store.adopt_spotify_library_owner("test-owner").unwrap();
    let broken = LibraryItem {
        source_uri: "spotify:track:BrokenArtistTrack".into(),
        title: "Test song".into(),
        artists: vec!["Test performer".into()],
        artist_id: Some("broken-artist".into()),
        genres: vec!["folk".into()],
        duration_secs: Some(180),
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    };
    let mut another = broken.clone();
    another.source_uri = "spotify:track:AnotherBrokenArtistTrack".into();
    let mut good = broken.clone();
    good.source_uri = "spotify:track:GoodArtistTrack".into();
    good.artist_id = Some("good-artist".into());
    apply_library_read(&mut store, &[broken, another, good]).unwrap();
    store
        .save_artist_genres("good-artist", &["jazz".into()], 1_000)
        .unwrap();
    (store, owner)
}

#[test]
fn optional_artist_cache_failures_preserve_music_and_allow_other_artists_to_enrich() {
    let (store, owner) = store();
    let accepted = store.library().unwrap();
    let pool = pool_from_store(&store).unwrap();
    assert_eq!(pool.tracks().len(), 3);
    assert_eq!(
        pool.get("spotify:track:BrokenArtistTrack").unwrap().genres,
        ["folk"]
    );
    assert_eq!(
        pool.get("spotify:track:AnotherBrokenArtistTrack")
            .unwrap()
            .genres,
        ["folk"]
    );
    assert_eq!(
        pool.get("spotify:track:GoodArtistTrack").unwrap().genres,
        ["folk", "jazz"]
    );
    assert_eq!(
        store
            .genre_reads
            .borrow()
            .iter()
            .filter(|id| *id == "broken-artist")
            .count(),
        1
    );
    assert_eq!(
        store
            .genre_reads
            .borrow()
            .iter()
            .filter(|id| *id == "good-artist")
            .count(),
        1
    );
    let works = works_from_store(&store).unwrap();
    assert_eq!(
        works
            .work_of("spotify:track:BrokenArtistTrack")
            .unwrap()
            .genres(),
        ["folk"]
    );
    assert_eq!(
        works
            .work_of("spotify:track:GoodArtistTrack")
            .unwrap()
            .genres(),
        ["folk", "jazz"]
    );
    let pending = genre_completion::status(&store, &owner, 1_000).unwrap();
    assert_eq!(
        pending.pending, 1,
        "an unreadable cache remains unfinished metadata"
    );
    assert!(!pending.requires_refresh);
    assert_eq!(store.library().unwrap(), accepted);
}

#[test]
fn required_library_failures_still_reject_pool_builds_and_progress_reports() {
    let (mut store, owner) = store();
    store.fail_library = true;
    assert!(matches!(
        pool_from_store(&store),
        Err(SpotifyError::Store(_))
    ));
    assert!(matches!(
        works_from_store(&store),
        Err(SpotifyError::Store(_))
    ));
    assert!(matches!(
        genre_completion::status(&store, &owner, 1_000),
        Err(SpotifyError::Store(_))
    ));
    assert!(store.genre_reads.borrow().is_empty());
}
