//! Unreadable feedback never becomes an empty feedback history.

use super::{ObservedStore, song};
use crate::dj::{PLAN_RETRY, SpotifyDj};
use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_api::plan::DjAction;
use fsonos_api::surface::dj_feedback::DjFeedback;
use fsonos_api::surface::dj_rehearse::{DjRehearsal, RehearsalLength};
use fsonos_core::HouseholdState;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::playback::{Playback, PlayerPlayback};
use fsonos_core::store::Store;
use fsonos_proto::control;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_spotify::cache::{apply_library_read, works_from_store};
use fsonos_spotify::feedback::{FeedbackSignal, Signal};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::time::Instant;

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
}

impl House {
    fn new(s1: bool) -> Self {
        let builder = SimHousehold::builder();
        let sim = if s1 {
            builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
        }
        .spawn()
        .unwrap();
        let player = sim.player("Den").unwrap();
        let coordinator = PlayerId(player.uuid.clone());
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        Self {
            sim,
            households: vec![household],
            coordinator,
        }
    }

    fn at<'a>(&'a self, lan: &'a SimLan) -> DjSpeakers<'a> {
        DjSpeakers {
            transport: lan,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }

    fn playback(&self) -> PlayerPlayback {
        let lan = self.sim.lan();
        let host = self.sim.player("Den").unwrap().ip;
        let position = control::get_position_info(&lan, host).unwrap();
        let media = control::get_media_info(&lan, host).unwrap();
        let transport = control::get_transport_info(&lan, host).unwrap();
        let mut model = Playback::default();
        model.apply_position(&self.coordinator, &position, Instant::now());
        let mut playback = model.of(&self.coordinator).unwrap().clone();
        playback.transport = Some(transport.state);
        playback.transport_status = Some(transport.status);
        playback.media_uri = Some(media.uri);
        playback.track_uri = Some(position.uri);
        playback.queue_position = Some(position.track);
        playback.queue_length = Some(media.tracks);
        playback
    }
}

fn clock() -> FakeClock {
    FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00Z").unwrap())
}

fn store(clock: &FakeClock) -> ObservedStore {
    let mut store = ObservedStore {
        revision_supported: Cell::new(true),
        ..ObservedStore::default()
    };
    apply_library_read(&mut store, &(1..=4).map(song).collect::<Vec<_>>()).unwrap();
    let pool = works_from_store(&store).unwrap();
    let disliked = pool.work_of(&song(1).source_uri).unwrap();
    for seconds_ago in [2, 1] {
        store
            .record_feedback(
                &FeedbackSignal::about(
                    disliked,
                    Signal::Dislike,
                    clock.now().timestamp() - seconds_ago,
                )
                .to_store(),
            )
            .unwrap();
    }
    store
}

fn rehearsal(clock: &FakeClock) -> DjRehearsal {
    DjRehearsal {
        start: clock.now(),
        length: RehearsalLength::Works(8),
        steer: None,
        seed: 1,
    }
}

#[test]
fn feedback_failure_defers_warm_and_cold_selection_and_recovers_without_losing_dislikes() {
    for s1 in [true, false] {
        let house = House::new(s1);
        let lan = house.sim.lan();
        let at = house.at(&lan);
        let clock = clock();
        let mut store = store(&clock);
        let dj = SpotifyDj::new(None);
        dj.act(at, &mut store, DjAction::Start, &clock).unwrap();
        let saved = dj.state().feeds[&house.coordinator].snapshot();
        let model = dj.state().feedback.clone();
        let marker = house.sim.soap_log().len();
        store.fail_feedback.set(true);

        for action in [DjAction::Start, DjAction::Skip] {
            let error = dj.act(at, &mut store, action, &clock).unwrap_err();
            assert!(error.detail.contains("feedback could not be read"));
        }
        assert!(dj.start_ready(at, &store, None, &clock).is_err());
        assert!(dj.rehearse(at, &store, &rehearsal(&clock), &clock).is_err());
        let cold = SpotifyDj::new(None);
        assert!(cold.start_ready(at, &store, None, &clock).is_err());
        assert!(cold.act(at, &mut store, DjAction::Start, &clock).is_err());
        assert!(cold.state().feeds.is_empty());
        assert_eq!(house.sim.soap_log().len(), marker);
        assert_eq!(dj.state().feeds[&house.coordinator].snapshot(), saved);
        assert_eq!(
            dj.state().feedback,
            model,
            "the accepted exclusions are retained"
        );

        // Already queued playback continues. Volume event churn must neither
        // bypass the feedback retry deadline nor add unfiltered music.
        control::next(&lan, house.sim.player("Den").unwrap().ip).unwrap();
        let mut playback = house.playback();
        let reads = store.feedback_reads.get();
        let marker = house.sim.soap_log().len();
        for volume in 0..10 {
            playback.volume = Some(volume);
            dj.on_playback(at, &mut store, &playback, &clock);
        }
        assert_eq!(store.feedback_reads.get(), reads);
        assert_eq!(house.sim.soap_log().len(), marker);
        assert_eq!(dj.state().feeds[&house.coordinator].snapshot(), saved);
        let status = dj
            .status(at, &store, playback.queue_position, &clock)
            .unwrap();
        assert!(status.running);
        assert!(
            status
                .problem
                .unwrap()
                .contains("feedback could not be read")
        );

        store.fail_feedback.set(false);
        assert!(dj.state().feedback_ready(
            &store,
            clock.now().timestamp(),
            Instant::now() + PLAN_RETRY,
        ));
        let current = playback.track_uri.clone();
        dj.on_playback(at, &mut store, &playback, &clock);
        let resumed = dj.state().feeds[&house.coordinator].snapshot().unwrap();
        assert_eq!(
            resumed.queued.len(),
            2,
            "the next permitted work was appended"
        );
        assert!(
            resumed
                .queued
                .iter()
                .flat_map(|work| &work.movements)
                .all(|movement| movement.uri != song(1).source_uri)
        );
        assert_eq!(
            house.playback().track_uri,
            current,
            "recovery never restarts current music"
        );
        let preview = dj.rehearse(at, &store, &rehearsal(&clock), &clock).unwrap();
        assert!(
            preview
                .works
                .iter()
                .all(|work| work.work.composer != "Artist 1")
        );

        store.fail_feedback.set(true);
        assert!(dj.act(at, &mut store, DjAction::Skip, &clock).is_err());
        dj.act(at, &mut store, DjAction::Stop, &clock).unwrap();
        assert!(
            !dj.feeds(&house.coordinator),
            "stopping does not need feedback reads"
        );
        assert!(
            dj.status(at, &store, None, &clock)
                .unwrap()
                .problem
                .is_none()
        );
    }
}

#[test]
fn a_saved_dislike_stays_successful_when_only_its_model_readback_fails() {
    let house = House::new(false);
    let lan = house.sim.lan();
    let at = house.at(&lan);
    let clock = clock();
    let mut store = store(&clock);
    let dj = SpotifyDj::new(None);
    dj.act(at, &mut store, DjAction::Start, &clock).unwrap();
    let rows = store
        .inner
        .borrow()
        .feedback_between(0..i64::MAX)
        .unwrap()
        .len();
    let saved = dj.state().feeds[&house.coordinator].snapshot();
    store.fail_feedback_after_record.set(true);
    let result = dj
        .feedback(at, &mut store, DjFeedback::Dislike, &clock)
        .unwrap();
    assert!(result.done.contains("your feedback was saved"));
    assert!(dj.state().feedback_failure.is_some());
    assert_eq!(
        store
            .inner
            .borrow()
            .feedback_between(0..i64::MAX)
            .unwrap()
            .len(),
        rows + 1
    );
    assert_eq!(dj.state().feeds[&house.coordinator].snapshot(), saved);
    let playback = house.playback();
    let marker = house.sim.soap_log().len();
    dj.on_playback(at, &mut store, &playback, &clock);
    assert_eq!(house.sim.soap_log().len(), marker);
    assert_eq!(
        store
            .inner
            .borrow()
            .feedback_between(0..i64::MAX)
            .unwrap()
            .len(),
        rows + 1
    );
}

#[test]
fn an_implicit_signal_readback_failure_waits_without_recording_it_twice() {
    let house = House::new(false);
    let lan = house.sim.lan();
    let at = house.at(&lan);
    let clock = clock();
    let mut store = store(&clock);
    let dj = SpotifyDj::new(None);
    dj.act(at, &mut store, DjAction::Start, &clock).unwrap();
    dj.on_playback(at, &mut store, &house.playback(), &clock);
    let rows = store
        .inner
        .borrow()
        .feedback_between(0..i64::MAX)
        .unwrap()
        .len();

    clock.advance(chrono::TimeDelta::seconds(10));
    control::next(&lan, house.sim.player("Den").unwrap().ip).unwrap();
    store.fail_feedback_after_record.set(true);
    dj.on_playback(at, &mut store, &house.playback(), &clock);
    assert!(dj.state().feedback_failure.is_some());
    let feedback = store.inner.borrow().feedback_between(0..i64::MAX).unwrap();
    assert_eq!(feedback.len(), rows + 1);
    assert_eq!(feedback.last().unwrap().signal, Signal::EarlySkip.value());

    let playback = house.playback();
    let marker = house.sim.soap_log().len();
    dj.on_playback(at, &mut store, &playback, &clock);
    assert_eq!(house.sim.soap_log().len(), marker);
    assert_eq!(
        store.inner.borrow().feedback_between(0..i64::MAX).unwrap(),
        feedback
    );
}
