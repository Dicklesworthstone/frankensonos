//! A failed publication or read must never revive retired selection music.

use super::super::State;
use fsonos_core::store::{
    Action, ActionFilter, AlbumTrack, ArtistGenresBatch, AuthEntry, CachedAlbum, CachedPlayer,
    CachedPlaylist, DjSession, Feedback, FeedbackKey, LibraryEntry, LoggedAction, MemStore,
    PlayName, PlayRecord, SpotifyArtistDiscoveryFence, SpotifyLibraryOwner,
    SpotifyPlaylistPublication, SpotifyPlaylistPublicationFence, Store, StoreError, StoredScene,
    StoredSchedule,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_types::{Player, ZoneGroup};
use std::cell::{Cell, RefCell};
use std::ops::Range;

#[derive(Default)]
struct ObservedStore {
    inner: RefCell<MemStore>,
    library_reads: Cell<usize>,
    fail_library: Cell<bool>,
    fail_receipt: Cell<bool>,
    revision_supported: Cell<bool>,
    publish_while_reading: Cell<bool>,
}

// Inject failures and a publication between snapshot reads using the real
// in-memory store contract for everything else.
macro_rules! forward {
    () => {};
    (fn $name:ident(&self $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty; $($rest:tt)*) => {
        fn $name(&self, $($arg: $ty),*) -> $result {
            self.inner.borrow().$name($($arg),*)
        }
        forward!($($rest)*);
    };
    (fn $name:ident(&mut self $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty; $($rest:tt)*) => {
        #[allow(clippy::too_many_arguments, reason = "forward the Store publication contract")]
        fn $name(&mut self, $($arg: $ty),*) -> $result {
            self.inner.borrow_mut().$name($($arg),*)
        }
        forward!($($rest)*);
    };
}

impl Store for ObservedStore {
    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        self.library_reads.set(self.library_reads.get() + 1);
        if self.fail_library.get() {
            return Err(StoreError::Backend("injected library read failure".into()));
        }
        let entries = self.inner.borrow().library()?;
        if self.publish_while_reading.replace(false) {
            let mut inner = self.inner.borrow_mut();
            inner.adopt_spotify_library_owner("replacement-owner")?;
            apply_library_read(&mut *inner, &[song(2)])
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(entries)
    }

    fn spotify_library_owner(&self) -> Result<Option<SpotifyLibraryOwner>, StoreError> {
        if self.fail_receipt.get() {
            return Err(StoreError::Backend("injected account read failure".into()));
        }
        self.inner.borrow().spotify_library_owner()
    }

    fn library_catalog_revision(&self) -> Result<Option<u64>, StoreError> {
        if self.revision_supported.get() {
            self.inner.borrow().library_catalog_revision()
        } else {
            Ok(None)
        }
    }

    forward! {
        fn artist_genres(&self, artist_id: &str) -> Result<Option<(Vec<String>, i64)>, StoreError>;
        fn artist_genres_many(&self, artist_ids: &[&str]) -> ArtistGenresBatch;
        fn save_artist_genres(&mut self, artist_id: &str, genres: &[String], fetched_at: i64) -> Result<(), StoreError>;
        fn record_play( &mut self, zone: &str, source_uri: &str, played_at: i64, ) -> Result<(), StoreError>;
        fn recent_plays(&self, zone: Option<&str>, limit: usize) -> Result<Vec<PlayRecord>, StoreError>;
        fn name_play(&mut self, name: &PlayName, at: i64) -> Result<(), StoreError>;
        fn play_names(&self, uris: &[&str]) -> Result<Vec<PlayName>, StoreError>;
        fn save_players( &mut self, household: &str, players: &[Player], seen_at: i64, ) -> Result<(), StoreError>;
        fn cached_players(&self) -> Result<Vec<CachedPlayer>, StoreError>;
        fn save_groups( &mut self, household: &str, groups: &[ZoneGroup], updated: i64, ) -> Result<(), StoreError>;
        fn cached_groups(&self, household: &str) -> Result<Vec<ZoneGroup>, StoreError>;
        fn upsert_library(&mut self, entries: &[LibraryEntry]) -> Result<(), StoreError>;
        fn spotify_artist_discovery(&self, owner: &SpotifyLibraryOwner) -> Result<Option<String>, StoreError>;
        fn publish_spotify_artist_continuation(&mut self, owner: &SpotifyLibraryOwner, fence: &SpotifyArtistDiscoveryFence, entries: &[LibraryEntry]) -> Result<bool, StoreError>;
        fn spotify_library_progress(&self, incoming_id: &str, base: Option<&SpotifyLibraryOwner>) -> Result<Option<String>, StoreError>;
        fn save_spotify_library_progress(&mut self, incoming_id: &str, base: Option<&SpotifyLibraryOwner>, expected: Option<&str>, progress: &str) -> Result<bool, StoreError>;
        fn adopt_spotify_library_from_progress(&mut self, incoming_id: &str, base: Option<&SpotifyLibraryOwner>, expected: &str) -> Result<Option<(SpotifyLibraryOwner, bool)>, StoreError>;
        fn publish_spotify_library_from_progress(&mut self, owner: &SpotifyLibraryOwner, expected: &str, entries: &[LibraryEntry], playlists: Option<&[(String, String)]>, contents: &[CachedPlaylist], heard: Option<&[(String, i64)]>, playlist_fence: Option<&SpotifyPlaylistPublicationFence>, artist_fence: Option<&SpotifyArtistDiscoveryFence>) -> Result<bool, StoreError>;
        fn adopt_spotify_library_owner( &mut self, owner_id: &str, ) -> Result<(SpotifyLibraryOwner, bool), StoreError>;
        fn publish_spotify_library( &mut self, owner: &SpotifyLibraryOwner, entries: &[LibraryEntry], playlists: Option<&[(String, String)]>, contents: &[CachedPlaylist], heard: Option<&[(String, i64)]>, ) -> Result<bool, StoreError>;
        fn publish_spotify_playlist_continuation(&mut self, owner: &SpotifyLibraryOwner, publication: &SpotifyPlaylistPublication) -> Result<bool, StoreError>;
        fn save_spotify_playlist_progress(&mut self, owner: &SpotifyLibraryOwner, playlist_uri: &str, expected: Option<&str>, progress: &str) -> Result<bool, StoreError>;
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
        fn save_album_misses(&mut self, misses: &[(String, i64)]) -> Result<(), StoreError>;
        fn album_misses(&self) -> Result<Vec<(String, i64)>, StoreError>;
        fn save_playlists(&mut self, playlists: &[(String, String)]) -> Result<(), StoreError>;
        fn playlists(&self) -> Result<Vec<(String, String)>, StoreError>;
        fn spotify_followed_directory(&self, owner: &SpotifyLibraryOwner) -> Result<Option<String>, StoreError>;
        fn save_spotify_followed_directory(&mut self, owner: &SpotifyLibraryOwner, expected: Option<&str>, progress: &str) -> Result<bool, StoreError>;
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

fn song(n: usize) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:{n:022}"),
        title: format!("Song {n}"),
        artists: vec![format!("Artist {n}")],
        genres: vec!["pop".into()],
        duration_secs: Some(180),
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

fn fixture() -> (ObservedStore, State) {
    let mut store = ObservedStore {
        revision_supported: Cell::new(true),
        ..ObservedStore::default()
    };
    store.adopt_spotify_library_owner("original-owner").unwrap();
    apply_library_read(&mut store, &[song(1)]).unwrap();
    let mut state = State::default();
    state.reload(&store, None, 0).unwrap();
    assert_eq!(state.pool.len(), 1);
    (store, state)
}

#[test]
fn unchanged_catalog_and_listening_writes_reuse_the_pool() {
    let (mut store, mut state) = fixture();
    let reads = store.library_reads.get();
    store.record_play("Den", &song(1).source_uri, 1).unwrap();
    store
        .save_spotify_heard(&[(song(1).source_uri, 2)])
        .unwrap();
    store
        .record_feedback(&Feedback {
            at: 3,
            artist_key: Some("artist 1".into()),
            signal: 1,
            ..Feedback::default()
        })
        .unwrap();
    for now in 4..14 {
        state.refresh(&store, None, now).unwrap();
    }
    assert_eq!(
        store.library_reads.get(),
        reads,
        "listening and feedback must not cause a full catalog rebuild"
    );
    assert_eq!(state.pool.len(), 1);
}

#[test]
fn adoption_without_publication_retires_the_old_pool_even_when_rebuilding_fails() {
    let (mut store, mut state) = fixture();
    store
        .adopt_spotify_library_owner("replacement-owner")
        .unwrap();
    store.fail_library.set(true);
    assert!(state.refresh(&store, None, 1).is_err());
    assert!(state.pool.is_empty());
    assert!(state.catalog.is_none());
    assert!(!state.planned);
    assert!(state.plan_retry_at.is_some());

    store.fail_library.set(false);
    apply_library_read(&mut store, &[song(2)]).unwrap();
    state.refresh(&store, None, 2).unwrap();
    assert_eq!(state.pool.works()[0].composer, "Artist 2");
}

#[test]
fn a_same_account_publication_cannot_reuse_retired_music_after_a_read_failure() {
    let (mut store, mut state) = fixture();
    apply_library_read(&mut store, &[song(2)]).unwrap();
    store.fail_library.set(true);
    assert!(state.refresh(&store, None, 1).is_err());
    assert!(state.pool.is_empty());
    assert!(state.catalog.is_none());

    store.fail_library.set(false);
    state.refresh(&store, None, 2).unwrap();
    assert_eq!(state.pool.works()[0].composer, "Artist 2");
}

#[test]
fn a_publication_during_pool_construction_rejects_the_mixed_observation() {
    let (store, mut state) = fixture();
    store.publish_while_reading.set(true);
    let error = state.reload(&store, None, 1).unwrap_err();
    assert_eq!(error.code, fsonos_api::ErrorCode::NotReady);
    assert!(state.pool.is_empty());
    assert!(state.catalog.is_none());

    state.refresh(&store, None, 2).unwrap();
    assert_eq!(state.pool.works()[0].composer, "Artist 2");
}

#[test]
fn an_unreadable_account_receipt_defers_selection_until_it_can_be_verified() {
    let (store, mut state) = fixture();
    store.fail_receipt.set(true);
    assert!(state.refresh(&store, None, 1).is_err());
    assert!(state.pool.is_empty());
    assert!(!state.planned);
    assert!(state.plan_retry_at.is_some());

    store.fail_receipt.set(false);
    state.refresh(&store, None, 2).unwrap();
    assert_eq!(state.pool.works()[0].composer, "Artist 1");
}

#[test]
fn stores_without_revision_support_rebuild_instead_of_reusing_unproved_music() {
    let (mut store, mut state) = fixture();
    store.revision_supported.set(false);
    state.refresh(&store, None, 1).unwrap();
    let reads = store.library_reads.get();
    state.refresh(&store, None, 2).unwrap();
    assert!(store.library_reads.get() > reads);

    apply_library_read(&mut store, &[song(2)]).unwrap();
    state.refresh(&store, None, 3).unwrap();
    assert_eq!(state.pool.works()[0].composer, "Artist 2");
}
