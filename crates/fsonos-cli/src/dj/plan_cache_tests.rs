//! Live plans, read-only rehearsals and feedback share current cache inputs.

use super::{PLAN_CHECK, SpotifyDj, State, sync};
use fsonos_api::dj::{DjEngine, DjSpeakers, DjSteer, SteerConstraints};
use fsonos_api::plan::DjAction;
use fsonos_api::surface::dj_feedback::DjFeedback;
use fsonos_api::surface::dj_rehearse::{DjRehearsal, RehearsalLength};
use fsonos_core::HouseholdState;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::{MemStore, Store};
use fsonos_proto::control;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::feedback::{FeedbackSignal, Signal};
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_spotify::steer::Moods;
use fsonos_types::{PlayerId, TransportState};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A song of `artist`'s, saved on its own album.
fn song(n: usize, artist: &str) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:{n:022}"),
        title: format!("Song {n}"),
        artists: vec![artist.into()],
        artist_id: None,
        album: Some(format!("Album {n}")),
        album_uri: Some(format!("spotify:album:{n:022}")),
        album_artists: vec![artist.into()],
        disc_number: Some(1),
        track_number: Some(1),
        added_at: None,
        genres: vec!["pop".into()],
        release_year: Some(2015),
        taste_pm: None,
        label: None,
        duration_secs: Some(200),
        explicit: false,
        origin: Origin::SavedAlbum,
    }
}

/// A fresh data directory's moods.toml path (the file itself absent).
fn data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-plan-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("moods.toml")
}

#[test]
fn the_pool_is_rebuilt_only_after_a_sync_or_an_edit_to_moods_or_preferences() {
    let moods_file = data_dir();
    let moods = Some(&moods_file);
    let mut store = MemStore::default();
    let mut items = vec![song(1, "Artist One"), song(2, "Artist Two")];
    apply_library_read(&mut store, &items).unwrap();
    let mut state = State {
        moods: Moods::builtin(),
        ..State::default()
    };
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 2, "the first refresh builds the pool");

    // The cache grows, but nothing says so: the next skip keeps the pool.
    items.push(song(3, "Artist Three"));
    apply_library_read(&mut store, &items).unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(
        state.pool.len(),
        2,
        "kept: nothing it is built from changed"
    );

    // A library sync (its freshness notice) rebuilds it.
    state.planned = false;
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 3, "rebuilt after a sync");

    // So does an edit to preferences.toml beside moods.toml...
    items.push(song(4, "Artist Four"));
    apply_library_read(&mut store, &items).unwrap();
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "explicit = true\n",
    )
    .unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 4, "rebuilt after a preferences edit");

    // ...and to moods.toml.
    items.push(song(5, "Artist Five"));
    apply_library_read(&mut store, &items).unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 4, "kept again");
    std::fs::write(&moods_file, "# the owner's moods\n").unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 5, "rebuilt after a moods edit");
}

#[test]
fn automatic_file_checks_are_shared_bounded_and_keep_failed_rebuilds_pending() {
    let moods_file = data_dir();
    let mut store = MemStore::default();
    apply_library_read(&mut store, &[song(1, "Artist One")]).unwrap();
    let mut state = State::default();
    state.reload(&store, Some(&moods_file), 0).unwrap();
    let due = state.plan_check_at.unwrap();
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "energy = 12\n",
    )
    .unwrap();
    for _ in 0..100 {
        state.check_plan_inputs(
            Some(&moods_file),
            due.checked_sub(Duration::from_secs(1)).unwrap(),
        );
        assert!(
            state.planned,
            "ordinary event churn does not re-read the files"
        );
        assert_eq!(state.plan_check_at, Some(due));
    }
    state.check_plan_inputs(Some(&moods_file), due);
    assert!(!state.planned);
    assert!(state.reload_due(due));
    state.reload(&store, Some(&moods_file), 0).unwrap();
    assert_eq!(state.pool.preferences().energy, Some(12));

    let old_moods = state.moods.get("focus").unwrap().clone();
    std::fs::write(&moods_file, "[moods.focus]\ninclude_keywords = ['jazz']\n").unwrap();
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "energy = 'broken'\n",
    )
    .unwrap();
    state.check_plan_inputs(Some(&moods_file), state.plan_check_at.unwrap());
    assert!(state.reload(&store, Some(&moods_file), 0).is_err());
    let retry_at = state.plan_retry_at.unwrap();
    for _ in 0..100 {
        state.check_plan_inputs(
            Some(&moods_file),
            retry_at.checked_sub(Duration::from_secs(1)).unwrap(),
        );
        assert!(!state.reload_due(retry_at.checked_sub(Duration::from_secs(1)).unwrap()));
        assert_eq!(state.plan_retry_at, Some(retry_at));
    }
    assert_eq!(state.pool.preferences().energy, Some(12));
    assert_eq!(state.moods.get("focus"), Some(&old_moods));

    // Read-only inspection must not publish half of a rejected pair.
    assert!(super::prefs::show(Some(&moods_file)).is_err());
    assert_eq!(state.moods.get("focus"), Some(&old_moods));
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "energy = 70\n",
    )
    .unwrap();
    assert!(state.reload_due(retry_at));
    state.reload(&store, Some(&moods_file), 0).unwrap();
    assert_eq!(state.pool.preferences().energy, Some(70));
    assert_eq!(state.moods.get("focus").unwrap().include_keywords, ["jazz"]);
}

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
}

impl House {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s2([SimPlayerSpec::new("Den", SimModel::One)])
            .spawn()
            .unwrap();
        let player = sim.player("Den").unwrap();
        let coordinator = PlayerId(player.uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        Self {
            sim,
            households: vec![house],
            coordinator,
        }
    }

    fn speakers<'a>(&'a self, lan: &'a SimLan) -> DjSpeakers<'a> {
        DjSpeakers {
            transport: lan,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }
}

fn clock() -> FakeClock {
    FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00+00:00").unwrap())
}

fn rehearsal(clock: &dyn Clock) -> DjRehearsal {
    DjRehearsal {
        start: clock.now(),
        length: RehearsalLength::Works(12),
        steer: None,
        seed: 1,
    }
}

fn library(moods_file: &std::path::Path) -> Arc<sync::LibrarySync> {
    Arc::new(
        sync::LibrarySync::new(
            SpotifyConfig {
                client_id: "0123456789abcdef0123456789abcdef".into(),
                redirect_uri: "http://127.0.0.1:8888/callback".into(),
            },
            moods_file.parent().unwrap().to_owned(),
        )
        .unwrap(),
    )
}

#[test]
#[allow(clippy::field_reassign_with_default)] // The interpolation clock is private to core.
fn automatic_top_up_uses_external_preferences_without_replacing_queued_music() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.speakers(&lan);
    let moods_file = data_dir();
    let dj = SpotifyDj::new(Some(moods_file.clone()));
    let mut store = MemStore::default();
    apply_library_read(
        &mut store,
        &[
            song(1, "Artist One"),
            song(2, "Artist Two"),
            song(3, "Artist Three"),
        ],
    )
    .unwrap();
    let clock = clock();
    dj.act(at, &mut store, DjAction::Start, &clock).unwrap();
    let before = dj
        .state()
        .feeds
        .get(&house.coordinator)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(before.queued.len(), 2);
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "[ban]\nartists = ['Artist One', 'Artist Two']\n",
    )
    .unwrap();
    dj.state().plan_check_at = Some(Instant::now().checked_sub(PLAN_CHECK).unwrap());
    let host = house.sim.player("Den").unwrap().ip;
    control::next(&lan, host).unwrap();
    let position = control::get_position_info(&lan, host).unwrap();
    let mut playback = PlayerPlayback::default();
    playback.transport = Some(TransportState::Playing);
    playback.media_uri = Some(control::get_media_info(&lan, host).unwrap().uri);
    playback.queue_position = Some(position.track);
    playback.track_uri = Some(position.uri);
    playback.queue_length = Some(2);
    dj.on_playback(at, &mut store, &playback, &clock);
    let after = dj
        .state()
        .feeds
        .get(&house.coordinator)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(after.played, before.queued[..1]);
    assert_eq!(after.queued[0], before.queued[1]);
    assert_eq!(after.queued[1].composer, "Artist Three");
    assert_eq!(after.queued[1].movements[0].position, Some(3));
    assert_eq!(
        control::get_position_info(&lan, host).unwrap().track,
        2,
        "a refreshed plan must not restart the current track"
    );
}

#[test]
fn rehearsal_consumes_a_completed_sync_without_touching_speakers_feeds_or_history() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.speakers(&lan);
    let moods_file = data_dir();
    let library = library(&moods_file);
    let dj = SpotifyDj::new(Some(moods_file)).with_library(Some(Arc::clone(&library)));
    let mut store = MemStore::default();
    apply_library_read(&mut store, &[song(1, "Artist One"), song(2, "Artist Two")]).unwrap();
    let clock = clock();
    dj.act(at, &mut store, DjAction::Start, &clock).unwrap();
    let before = dj.state().feeds.get(&house.coordinator).unwrap().snapshot();
    let history = store.recent_plays(None, 100).unwrap();
    let rows = store.feedback_between(0..i64::MAX).unwrap();
    let calls = house.sim.soap_log().len();
    apply_library_read(&mut store, &[song(3, "Fresh Artist")]).unwrap();
    library.mark_fresh_for_test();
    let rehearsed = dj.rehearse(at, &store, &rehearsal(&clock), &clock).unwrap();
    assert_eq!(rehearsed.works.len(), 12);
    assert!(
        rehearsed
            .works
            .iter()
            .all(|w| w.work.composer == "Fresh Artist")
    );
    assert!(
        !library.take_fresh(),
        "rehearsal consumed the sync notification"
    );
    assert_eq!(
        house.sim.soap_log().len(),
        calls,
        "no speaker reads or writes"
    );
    assert_eq!(
        dj.state().feeds.get(&house.coordinator).unwrap().snapshot(),
        before
    );
    assert_eq!(store.recent_plays(None, 100).unwrap(), history);
    assert_eq!(store.feedback_between(0..i64::MAX).unwrap(), rows);
}

#[test]
fn rehearsal_refreshes_external_preferences_and_new_feedback_without_a_start() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.speakers(&lan);
    let moods_file = data_dir();
    let dj = SpotifyDj::new(Some(moods_file.clone()));
    let mut store = MemStore::default();
    apply_library_read(
        &mut store,
        &[
            song(1, "Artist One"),
            song(2, "Artist Two"),
            song(3, "Artist Three"),
        ],
    )
    .unwrap();
    let clock = clock();
    let preview = DjRehearsal {
        length: RehearsalLength::Works(1),
        ..rehearsal(&clock)
    };
    dj.rehearse(at, &store, &preview, &clock).unwrap();
    let work = dj
        .state()
        .pool
        .work_of(&song(1, "Artist One").source_uri)
        .unwrap()
        .clone();
    for seconds_ago in [1, 2] {
        store
            .record_feedback(
                &FeedbackSignal::about(
                    &work,
                    Signal::Dislike,
                    clock.now().timestamp() - seconds_ago,
                )
                .to_store(),
            )
            .unwrap();
    }
    let rehearsed = dj.rehearse(at, &store, &preview, &clock).unwrap();
    assert!(dj.state().feedback.as_ref().unwrap().excludes(&work));
    assert!(
        rehearsed
            .works
            .iter()
            .all(|w| w.work.composer != "Artist One")
    );
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "[ban]\nartists = ['Artist Two']\n",
    )
    .unwrap();
    let rehearsed = dj.rehearse(at, &store, &preview, &clock).unwrap();
    assert!(
        rehearsed
            .works
            .iter()
            .all(|w| w.work.composer == "Artist Three")
    );
    assert!(dj.state().feeds.is_empty());
    assert!(store.recent_plays(None, 100).unwrap().is_empty());
}

#[test]
fn feedback_uses_newly_synced_music_and_rejects_invalid_edits_before_any_io() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.speakers(&lan);
    let moods_file = data_dir();
    let library = library(&moods_file);
    let dj = SpotifyDj::new(Some(moods_file.clone())).with_library(Some(Arc::clone(&library)));
    let mut store = MemStore::default();
    apply_library_read(&mut store, &[song(1, "Artist One")]).unwrap();
    let clock = clock();
    dj.rehearse(at, &store, &rehearsal(&clock), &clock).unwrap();
    let fresh = song(2, "Fresh Artist");
    apply_library_read(&mut store, std::slice::from_ref(&fresh)).unwrap();
    library.mark_fresh_for_test();
    fsonos_core::control::play_spotify_track(
        &lan,
        &house.households,
        &house.coordinator,
        &fresh.source_uri,
        &fresh.title,
    )
    .unwrap();
    let liked = dj
        .feedback(at, &mut store, DjFeedback::Like, &clock)
        .unwrap();
    assert_eq!(liked.composer, "Fresh Artist");
    assert!(!library.take_fresh());
    let rows = store.feedback_between(0..i64::MAX).unwrap();
    assert_eq!(rows.len(), 1);

    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "explicit = 'bad'\n",
    )
    .unwrap();
    let calls = house.sim.soap_log().len();
    assert!(
        dj.feedback(at, &mut store, DjFeedback::Dislike, &clock)
            .is_err()
    );
    assert!(dj.rehearse(at, &store, &rehearsal(&clock), &clock).is_err());
    assert_eq!(house.sim.soap_log().len(), calls);
    assert_eq!(store.feedback_between(0..i64::MAX).unwrap(), rows);
    assert_eq!(dj.state().pool.len(), 1);
    assert_eq!(dj.state().pool.works()[0].composer, "Fresh Artist");
}

#[test]
fn steering_accepts_new_moods_only_with_their_validated_preferences_and_library() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.speakers(&lan);
    let moods_file = data_dir();
    let library = library(&moods_file);
    let dj = SpotifyDj::new(Some(moods_file.clone())).with_library(Some(Arc::clone(&library)));
    let mut store = MemStore::default();
    apply_library_read(&mut store, &[song(1, "Artist One")]).unwrap();
    let clock = clock();
    dj.steer(
        at,
        &mut store,
        &DjSteer::Set {
            mood: Some("focus".into()),
            constraints: SteerConstraints::default(),
            for_secs: None,
        },
        &clock,
    )
    .unwrap();
    let previous = store.dj_session(&house.coordinator.0).unwrap();
    let calls = house.sim.soap_log().len();
    std::fs::write(
        &moods_file,
        "[moods.afterhours]\ninclude_keywords = ['pop']\n",
    )
    .unwrap();
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "energy = 'broken'\n",
    )
    .unwrap();
    apply_library_read(&mut store, &[song(2, "Fresh Artist")]).unwrap();
    library.mark_fresh_for_test();
    let steering = DjSteer::Set {
        mood: Some("afterhours".into()),
        constraints: SteerConstraints::default(),
        for_secs: None,
    };
    assert!(dj.steer(at, &mut store, &steering, &clock).is_err());
    assert_eq!(store.dj_session(&house.coordinator.0).unwrap(), previous);
    assert!(dj.state().moods.get("afterhours").is_none());
    assert_eq!(dj.state().pool.works()[0].composer, "Artist One");
    assert!(!dj.state().planned);
    assert!(!library.take_fresh(), "the failed rebuild remains pending");

    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "energy = 12\n",
    )
    .unwrap();
    dj.steer(at, &mut store, &steering, &clock).unwrap();
    let session = store.dj_session(&house.coordinator.0).unwrap().unwrap();
    assert_eq!(session.mood.as_deref(), Some("afterhours"));
    let state = dj.state();
    assert!(state.planned);
    assert_eq!(state.pool.works()[0].composer, "Fresh Artist");
    assert_eq!(state.pool.preferences().energy, Some(12));
    assert!(state.context(&store, &house.coordinator, &clock).is_ok());
    assert_eq!(house.sim.soap_log().len(), calls);
    assert!(state.feeds.is_empty());
    assert!(store.recent_plays(None, 100).unwrap().is_empty());
}

#[test]
fn clearing_steering_works_without_a_library_or_valid_files_and_keeps_sync_pending() {
    let house = House::new();
    let lan = house.sim.lan();
    let at = house.speakers(&lan);
    let moods_file = data_dir();
    let library = library(&moods_file);
    let dj = SpotifyDj::new(Some(moods_file.clone())).with_library(Some(Arc::clone(&library)));
    let mut store = MemStore::default();
    let clock = clock();
    dj.steer(
        at,
        &mut store,
        &DjSteer::Set {
            mood: Some("focus".into()),
            constraints: SteerConstraints::default(),
            for_secs: None,
        },
        &clock,
    )
    .unwrap();
    assert!(dj.state().pool.is_empty(), "steering needs no synced music");
    let inputs = dj.state().plan_inputs;
    let focus = dj.state().moods.get("focus").unwrap().clone();
    std::fs::write(&moods_file, "not valid moods = [\n").unwrap();
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "energy = 'broken'\n",
    )
    .unwrap();
    library.mark_fresh_for_test();
    let calls = house.sim.soap_log().len();
    let cleared = dj.steer(at, &mut store, &DjSteer::Clear, &clock).unwrap();
    assert!(cleared.changed);
    assert!(store.dj_session(&house.coordinator.0).unwrap().is_none());
    assert_eq!(dj.state().plan_inputs, inputs);
    assert_eq!(dj.state().moods.get("focus"), Some(&focus));
    assert!(dj.state().planned, "the valid plan was not disturbed");
    assert!(
        library.take_fresh(),
        "automatic processing still owns this notice"
    );
    assert!(
        !dj.steer(at, &mut store, &DjSteer::Clear, &clock)
            .unwrap()
            .changed,
        "clearing again is still a no-op while files are invalid"
    );
    assert_eq!(house.sim.soap_log().len(), calls);
    assert!(store.recent_plays(None, 100).unwrap().is_empty());
}
