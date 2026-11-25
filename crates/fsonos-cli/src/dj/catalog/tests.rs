//! A failed publication or read must never revive retired selection music.

use super::super::State;
use fsonos_core::store::{
    Action, ActionFilter, AlbumTrack, ArtistGenresBatch, AuthEntry, CachedAlbum, CachedPlayer,
    CachedPlaylist, DjSession, Feedback, FeedbackKey, LibraryEntry, LoggedAction, MemStore,
    PlayName, PlayRecord, SpotifyArtistDiscoveryFence, SpotifyAvailability, SpotifyLibraryOwner,
    SpotifyPlaylistPublication, SpotifyPlaylistPublicationFence, Store, StoreError, StoredScene,
    StoredSchedule,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_types::{Player, ZoneGroup};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ops::Range;

#[derive(Default)]
struct ObservedStore {
    inner: RefCell<MemStore>,
    library_reads: Cell<usize>,
    feedback_reads: Cell<usize>,
    fail_library: Cell<bool>,
    fail_feedback: Cell<bool>,
    fail_feedback_after_record: Cell<bool>,
    fail_receipt: Cell<bool>,
    revision_supported: Cell<bool>,
    publish_while_reading: Cell<bool>,
    publish_while_reading_feedback: Cell<bool>,
    genres_while_reading: RefCell<Option<(String, Vec<String>)>>,
    configuration_edit: RefCell<Option<(std::path::PathBuf, String)>>,
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
    fn artist_genres_many(&self, artist_ids: &[&str]) -> ArtistGenresBatch {
        let genres = self.inner.borrow().artist_genres_many(artist_ids);
        if let Some((artist, tags)) = self.genres_while_reading.borrow_mut().take() {
            self.inner
                .borrow_mut()
                .save_artist_genres(&artist, &tags, 1)
                .unwrap();
        }
        genres
    }

    fn feedback_between(&self, window: Range<i64>) -> Result<Vec<Feedback>, StoreError> {
        self.feedback_reads.set(self.feedback_reads.get() + 1);
        if self.fail_feedback.get() {
            return Err(StoreError::Backend("injected feedback read failure".into()));
        }
        let feedback = self.inner.borrow().feedback_between(window)?;
        if self.publish_while_reading_feedback.replace(false) {
            let mut inner = self.inner.borrow_mut();
            inner.adopt_spotify_library_owner("replacement-owner")?;
            apply_library_read(&mut *inner, &[song(2)])
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(feedback)
    }

    fn record_feedback(&mut self, feedback: &Feedback) -> Result<(), StoreError> {
        self.inner.borrow_mut().record_feedback(feedback)?;
        if self.fail_feedback_after_record.replace(false) {
            self.fail_feedback.set(true);
        }
        Ok(())
    }

    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        self.library_reads.set(self.library_reads.get() + 1);
        if self.fail_library.get() {
            return Err(StoreError::Backend("injected library read failure".into()));
        }
        let entries = self.inner.borrow().library()?;
        if let Some((path, text)) = self.configuration_edit.borrow_mut().take() {
            std::fs::write(path, text).map_err(|error| StoreError::Backend(error.to_string()))?;
        }
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
        fn spotify_unavailable_tracks(&self) -> Result<HashSet<String>, StoreError>;
        fn spotify_artist_discovery(&self, owner: &SpotifyLibraryOwner) -> Result<Option<String>, StoreError>;
        fn publish_spotify_artist_continuation(&mut self, owner: &SpotifyLibraryOwner, fence: &SpotifyArtistDiscoveryFence, entries: &[LibraryEntry], unavailable: &[String]) -> Result<bool, StoreError>;
        fn spotify_library_progress(&self, incoming_id: &str, base: Option<&SpotifyLibraryOwner>) -> Result<Option<String>, StoreError>;
        fn save_spotify_library_progress(&mut self, incoming_id: &str, base: Option<&SpotifyLibraryOwner>, expected: Option<&str>, progress: &str) -> Result<bool, StoreError>;
        fn adopt_spotify_library_from_progress(&mut self, incoming_id: &str, base: Option<&SpotifyLibraryOwner>, expected: &str) -> Result<Option<(SpotifyLibraryOwner, bool)>, StoreError>;
        fn publish_spotify_library_from_progress(&mut self, owner: &SpotifyLibraryOwner, expected: &str, entries: &[LibraryEntry], playlists: Option<&[(String, String)]>, contents: &[CachedPlaylist], heard: Option<&[(String, i64)]>, playlist_fence: Option<&SpotifyPlaylistPublicationFence>, artist_fence: Option<&SpotifyArtistDiscoveryFence>, availability: Option<&SpotifyAvailability>) -> Result<bool, StoreError>;
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
        fn feedback( &self, key: FeedbackKey<'_>, window: Range<i64>, ) -> Result<Vec<Feedback>, StoreError>;
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

mod feedback_failures;
mod genres;

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
fn accepted_availability_changes_retire_cached_selection_and_allow_fresh_recovery() {
    let (mut store, mut state) = fixture();
    let owner = store.spotify_library_owner().unwrap().unwrap();
    let entries = store.library().unwrap();
    let uri = song(1).source_uri;
    for (proof, availability, expected_pool_size) in [
        (
            "unavailable",
            SpotifyAvailability {
                unavailable: vec![uri.clone()],
                ..SpotifyAvailability::default()
            },
            0,
        ),
        (
            "available-again",
            SpotifyAvailability {
                available: vec![uri.clone()],
                ..SpotifyAvailability::default()
            },
            1,
        ),
    ] {
        assert!(state.plan_is_current(&store, None).unwrap());
        assert!(
            store
                .save_spotify_library_progress(&owner.owner_id, Some(&owner), None, proof)
                .unwrap()
        );
        assert!(
            store
                .publish_spotify_library_from_progress(
                    &owner,
                    proof,
                    &entries,
                    None,
                    &[],
                    None,
                    None,
                    None,
                    Some(&availability),
                )
                .unwrap()
        );
        assert!(!state.plan_is_current(&store, None).unwrap());
        state.refresh(&store, None, 1).unwrap();
        assert_eq!(state.pool.len(), expected_pool_size);
        assert_eq!(
            state.pool.work_of(&uri).is_some(),
            expected_pool_size == 1,
            "a retained plan must obey the newly published availability"
        );
    }
}

#[test]
fn background_artist_denial_retires_warm_and_cold_selection_without_new_tracks() {
    let (mut store, mut state) = fixture();
    let owner = store.spotify_library_owner().unwrap().unwrap();
    let uri = song(1).source_uri;
    let fence = SpotifyArtistDiscoveryFence {
        expected: None,
        progress: "completed background batch".into(),
    };
    assert!(state.plan_is_current(&store, None).unwrap());
    assert!(
        store
            .publish_spotify_artist_continuation(&owner, &fence, &[], std::slice::from_ref(&uri),)
            .unwrap()
    );
    assert!(
        !state.plan_is_current(&store, None).unwrap(),
        "negative-only publication must retire the warmed selection"
    );
    state.refresh(&store, None, 1).unwrap();
    assert!(state.pool.is_empty());
    assert!(state.pool.work_of(&uri).is_none());

    let mut restarted = State::default();
    restarted.reload(&store, None, 2).unwrap();
    assert!(restarted.pool.is_empty());
    assert!(
        !store
            .publish_spotify_artist_continuation(&owner, &fence, &[], std::slice::from_ref(&uri),)
            .unwrap(),
        "an already consumed cohort cannot publish again"
    );
    assert!(state.plan_is_current(&store, None).unwrap());
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
fn a_publication_during_feedback_refresh_cannot_reuse_the_old_account() {
    let (store, mut state) = fixture();
    let reads = store.library_reads.get();
    store.publish_while_reading_feedback.set(true);
    state.refresh(&store, None, 1).unwrap();
    assert!(store.library_reads.get() > reads);
    assert_eq!(state.pool.works()[0].composer, "Artist 2");
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

#[test]
fn starting_and_checking_another_room_reuses_music_but_rereads_feedback() {
    use super::super::SpotifyDj;
    use fsonos_api::dj::{DjEngine, DjSpeakers};
    use fsonos_api::plan::DjAction;
    use fsonos_core::HouseholdState;
    use fsonos_core::clock::{Clock, FakeClock};
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_spotify::feedback::{FeedbackModel, StoreFeedback};
    use fsonos_types::PlayerId;

    for s1 in [true, false] {
        let builder = SimHousehold::builder();
        let sim = if s1 {
            builder.s1([
                SimPlayerSpec::new("Den", SimModel::Play5Gen1),
                SimPlayerSpec::new("Study", SimModel::Play5Gen1),
            ])
        } else {
            builder.s2([
                SimPlayerSpec::new("Den", SimModel::One),
                SimPlayerSpec::new("Study", SimModel::One),
            ])
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let mut household = HouseholdState::default();
        household
            .apply_topology(&get_zone_group_state(&lan, sim.player("Den").unwrap().ip).unwrap());
        let households = vec![household];
        let first = PlayerId(sim.player("Den").unwrap().uuid.clone());
        let second = PlayerId(sim.player("Study").unwrap().uuid.clone());
        let first_at = DjSpeakers {
            transport: &lan,
            households: &households,
            coordinator: &first,
        };
        let second_at = DjSpeakers {
            coordinator: &second,
            ..first_at
        };
        let clock =
            FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00Z").unwrap());
        let dj = SpotifyDj::new(None);
        let (mut store, _) = fixture();
        apply_library_read(&mut store, &(1..=12).map(song).collect::<Vec<_>>()).unwrap();
        dj.act(first_at, &mut store, DjAction::Start, &clock)
            .unwrap();
        let first_feed = dj.state().feeds[&first].snapshot();
        let reads = store.library_reads.get();
        let feedback_reads = store.feedback_reads.get();
        store
            .record_feedback(&Feedback {
                at: clock.now().timestamp(),
                artist_key: Some("artist 1".into()),
                signal: 1,
                ..Feedback::default()
            })
            .unwrap();

        dj.start_ready(second_at, &store, None, &clock).unwrap();
        assert_eq!(store.library_reads.get(), reads);
        assert!(store.feedback_reads.get() > feedback_reads);
        assert_eq!(dj.state().feeds[&first].snapshot(), first_feed);
        assert!(!dj.feeds(&second), "a readiness read cannot start a feed");

        let feedback_reads = store.feedback_reads.get();
        dj.act(second_at, &mut store, DjAction::Start, &clock)
            .unwrap();
        assert_eq!(
            store.library_reads.get(),
            reads,
            "another room must not scan and regroup the entire library"
        );
        assert!(store.feedback_reads.get() > feedback_reads);
        assert_eq!(
            dj.state().feedback,
            FeedbackModel::load(&StoreFeedback(&store), clock.now().timestamp()).ok()
        );
        assert_eq!(dj.state().feeds[&first].snapshot(), first_feed);
        assert!(dj.feeds(&first) && dj.feeds(&second));

        // Receipt changes from an independent importer defeat both reuse paths.
        apply_library_read(&mut store, &[song(20), song(21)]).unwrap();
        dj.start_ready(second_at, &store, None, &clock).unwrap();
        assert!(store.library_reads.get() > reads);
        assert_eq!(dj.state().pool.len(), 12, "readiness cannot publish a plan");
        dj.act(second_at, &mut store, DjAction::Start, &clock)
            .unwrap();
        assert_eq!(dj.state().pool.len(), 2);
        assert!(dj.state().pool.work_of(&song(20).source_uri).is_some());
        assert_eq!(dj.state().feeds[&first].snapshot(), first_feed);
    }
}

#[test]
fn readiness_reuse_requires_readable_current_catalog_and_configuration() {
    let (store, state) = fixture();
    assert!(state.plan_is_current(&store, None).unwrap());
    store.fail_receipt.set(true);
    assert!(state.plan_is_current(&store, None).is_err());
    store.fail_receipt.set(false);
    store.revision_supported.set(false);
    assert!(!state.plan_is_current(&store, None).unwrap());
    store.revision_supported.set(true);

    let dir = tempfile::tempdir().unwrap();
    let moods = dir.path().join("moods.toml");
    let mut state = State::default();
    state.reload(&store, Some(&moods), 0).unwrap();
    assert!(state.plan_is_current(&store, Some(&moods)).unwrap());
    std::fs::write(moods.with_file_name("preferences.toml"), "energy = 20\n").unwrap();
    assert!(!state.plan_is_current(&store, Some(&moods)).unwrap());
    state.refresh(&store, Some(&moods), 1).unwrap();
    assert_eq!(state.pool.preferences().energy, Some(20));
    assert!(state.plan_is_current(&store, Some(&moods)).unwrap());
    std::fs::write(&moods, "[moods.fixture]\ninclude_genres = ['jazz']\n").unwrap();
    assert!(!state.plan_is_current(&store, Some(&moods)).unwrap());
}

#[test]
fn unreadable_configuration_cannot_reuse_a_plan_built_from_absent_files() {
    let (store, _) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let moods = dir.path().join("moods.toml");
    let mut state = State::default();
    state.reload(&store, Some(&moods), 0).unwrap();
    // A directory produces a deterministic read error, including when tests
    // run as root and chmod would not make a file unreadable.
    std::fs::create_dir(moods.with_file_name("preferences.toml")).unwrap();
    assert!(!state.plan_is_current(&store, Some(&moods)).unwrap());
    assert!(state.refresh(&store, Some(&moods), 1).is_err());
    assert!(!state.planned);
}

#[test]
fn configuration_edits_during_pool_construction_cannot_publish_a_mismatched_fingerprint() {
    let (store, _) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let moods = dir.path().join("moods.toml");
    let preferences = moods.with_file_name("preferences.toml");
    let mut state = State::default();
    *store.configuration_edit.borrow_mut() = Some((preferences, "energy = 20\n".into()));
    let failure = state.reload(&store, Some(&moods), 0).unwrap_err();
    assert_eq!(failure.code, fsonos_api::ErrorCode::NotReady);
    assert!(!state.planned);
    assert!(state.pool.is_empty());
    assert!(state.plan_inputs.is_none());

    state.refresh(&store, Some(&moods), 1).unwrap();
    assert_eq!(state.pool.preferences().energy, Some(20));
    assert!(state.plan_is_current(&store, Some(&moods)).unwrap());
}

#[test]
fn direction_readiness_needs_configuration_but_never_scans_the_library() {
    use super::super::SpotifyDj;
    use fsonos_api::ErrorCode;
    use fsonos_api::dj::{DjEngine, DjSpeakers, DjSteer, SteerConstraints};
    use fsonos_core::clock::FakeClock;
    use fsonos_proto::{ProtoError, Transport};
    use fsonos_types::PlayerId;

    struct NoSpeakerIo;
    impl Transport for NoSpeakerIo {
        fn soap_post(
            &self,
            _host: std::net::IpAddr,
            _path: &str,
            _action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            panic!("direction validation must not contact speakers")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let moods = dir.path().join("moods.toml");
    let dj = SpotifyDj::new(Some(moods.clone()));
    let (store, _) = fixture();
    store.fail_library.set(true);
    let reads = store.library_reads.get();
    let transport = NoSpeakerIo;
    let coordinator = PlayerId("synthetic-direction-room".into());
    let at = DjSpeakers {
        transport: &transport,
        households: &[],
        coordinator: &coordinator,
    };
    let clock =
        FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00Z").unwrap());
    let direction = DjSteer::Set {
        mood: Some("focus".into()),
        constraints: SteerConstraints::default(),
        for_secs: Some(600),
    };
    dj.steer_ready(at, &store, &direction, &clock).unwrap();
    assert_eq!(store.library_reads.get(), reads);
    assert!(store.dj_sessions().unwrap().is_empty());
    assert!(!dj.state().planned);

    std::fs::write(
        moods.with_file_name("preferences.toml"),
        "energy = 'invalid'\n",
    )
    .unwrap();
    assert_eq!(
        dj.steer_ready(at, &store, &direction, &clock)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(store.library_reads.get(), reads);
}

fn authorize_catalog(dir: &std::path::Path) -> String {
    let cache = fsonos_spotify::client::TokenCache::in_data_dir(dir);
    cache
        .store(&fsonos_spotify::client::CachedToken {
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            expires_at: i64::MAX,
            scope: fsonos_spotify::client::SCOPE.into(),
        })
        .unwrap();
    cache.authorization_id().unwrap().unwrap()
}

fn acknowledge_catalog(dir: &std::path::Path, authorization: &str) {
    let stamp = serde_json::json!({
        "playlist_continuation": {
            "authorization_observed": true,
            "authorization_id": authorization,
            "accepted_authorization_id": authorization,
            "owner": "original-owner",
            "pending": 0,
            "limited": 0,
            "stalled": 0,
            "retry_at": 0,
            "not_before": 0,
            "requires_refresh": false
        }
    });
    std::fs::write(
        dir.join("library-sync.json"),
        serde_json::to_vec(&stamp).unwrap(),
    )
    .unwrap();
}

#[test]
fn reauthorization_defers_warm_and_cold_selection_until_a_full_refresh_is_accepted() {
    let (mut store, mut state) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let moods = dir.path().join("moods.toml");
    let receipt = authorize_catalog(dir.path());
    acknowledge_catalog(dir.path(), &receipt);
    state.reload(&store, Some(&moods), 0).unwrap();
    assert!(state.plan_is_current(&store, Some(&moods)).unwrap());
    let reads = store.library_reads.get();

    let replacement = authorize_catalog(dir.path());
    assert!(state.plan_is_current(&store, Some(&moods)).is_err());
    assert!(State::read_plan(&store, Some(&moods), 1).is_err());
    let failure = state.refresh(&store, Some(&moods), 1).unwrap_err();
    assert_eq!(failure.code, fsonos_api::ErrorCode::NotReady);
    assert!(failure.detail.contains("sign-in changed"));
    assert!(state.pool.is_empty() && !state.planned && state.catalog.is_none());
    assert_eq!(
        store.library_reads.get(),
        reads,
        "pending consent cannot read old candidate music"
    );

    apply_library_read(&mut store, &[song(2)]).unwrap();
    acknowledge_catalog(dir.path(), &replacement);
    state.refresh(&store, Some(&moods), 2).unwrap();
    assert_eq!(state.pool.len(), 1);
    assert_eq!(state.pool.works()[0].composer, "Artist 2");
    assert!(state.plan_is_current(&store, Some(&moods)).unwrap());
}

#[test]
fn sign_in_during_pool_construction_cannot_install_the_retired_selection_proof() {
    let (store, mut state) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let moods = dir.path().join("moods.toml");
    let receipt = authorize_catalog(dir.path());
    acknowledge_catalog(dir.path(), &receipt);
    state.reload(&store, Some(&moods), 0).unwrap();
    let cache = fsonos_spotify::client::TokenCache::in_data_dir(dir.path());
    let previous = std::fs::read(cache.path()).unwrap();
    authorize_catalog(dir.path());
    let replacement = std::fs::read_to_string(cache.path()).unwrap();
    std::fs::write(cache.path(), previous).unwrap();
    *store.configuration_edit.borrow_mut() = Some((cache.path().to_owned(), replacement));

    let failure = state.reload(&store, Some(&moods), 0).unwrap_err();
    assert_eq!(failure.code, fsonos_api::ErrorCode::NotReady);
    assert!(state.pool.is_empty());
    assert!(state.catalog.is_none());
}

#[test]
fn sign_in_during_speaker_preflight_cannot_append_old_picks_but_stop_still_works() {
    use fsonos_proto::{ProtoError, Transport};

    struct ChangingSpeaker {
        dir: std::path::PathBuf,
        sent: RefCell<Vec<String>>,
    }
    impl Transport for ChangingSpeaker {
        fn soap_post(
            &self,
            _host: std::net::IpAddr,
            _path: &str,
            action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            self.sent.borrow_mut().push(action.to_owned());
            if action.ends_with("#GetPositionInfo") {
                authorize_catalog(&self.dir);
            }
            Ok(String::new())
        }
    }
    let (store, _) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let receipt = authorize_catalog(dir.path());
    acknowledge_catalog(dir.path(), &receipt);
    let evidence = super::Receipt::read(&store, Some(dir.path())).unwrap();
    let raw = ChangingSpeaker {
        dir: dir.path().to_owned(),
        sent: RefCell::default(),
    };
    let guarded = super::SelectionTransport::new(&raw, Some(&evidence));
    let host = "127.0.0.1".parse().unwrap();
    let service = "urn:schemas-upnp-org:service:AVTransport:1";
    guarded
        .soap_post(host, "/Control", &format!("{service}#GetPositionInfo"), "")
        .unwrap();
    for action in ["AddURIToQueue", "AddMultipleURIsToQueue"] {
        let error = guarded
            .soap_post(host, "/Control", &format!("{service}#{action}"), "")
            .unwrap_err();
        assert!(error.to_string().contains("sign-in changed"));
    }
    guarded
        .soap_post(host, "/Control", &format!("{service}#Stop"), "")
        .unwrap();
    assert_eq!(raw.sent.borrow().len(), 2, "no retired selection was sent");
}
