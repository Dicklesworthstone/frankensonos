//! Feedback follows an owned recording, not a mutable slot or a shared work key.

use super::super::attribution::movement as observed;
use super::*;
use crate::dj::SpotifyDj;
use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_core::HouseholdState;
use fsonos_core::clock::FakeClock;
use fsonos_core::playback::Playback;
use fsonos_core::store::MemStore;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::classical::CandidatePool;
use fsonos_spotify::dj::{DjConfig, PickReason};
use fsonos_spotify::feed::{FeedSnapshot, Movement as QueuedMovement, QueuedWork};
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_spotify::works::{Completeness, group_works};
use fsonos_types::PlayerId;
use std::time::Instant;

fn library_items(take: &str) -> Vec<LibraryItem> {
    ["I. Prelude", "II. Allemande", "III. Courante"]
        .iter()
        .enumerate()
        .map(|(index, title)| LibraryItem {
            source_uri: format!("spotify:track:{take}Track{index}"),
            title: format!("Cello Suite No. 1 in G Major, BWV 1007: {title}"),
            artists: vec![
                "Johann Sebastian Bach".into(),
                format!("Test Ensemble {take}"),
            ],
            album: Some(format!("Test Album {take}")),
            album_uri: Some(format!("spotify:album:{take}")),
            disc_number: Some(1),
            track_number: Some(u32::try_from(index + 1).unwrap()),
            duration_secs: Some(180),
            genres: vec!["baroque".into()],
            origin: Origin::SavedAlbum,
            ..LibraryItem::default()
        })
        .collect()
}

fn work(take: &str) -> Work {
    let works = group_works(CandidatePool::build(&library_items(take)).tracks());
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].completeness, Completeness::Complete);
    works[0].clone()
}

fn queued(work: &Work, start: u32) -> QueuedWork {
    QueuedWork {
        title: work.title.clone(),
        composer: work.composer.clone(),
        work_key: work.work_key.clone(),
        movements: work
            .movements
            .iter()
            .enumerate()
            .map(|(index, movement)| QueuedMovement {
                uri: movement.track.source_uri.clone(),
                title: movement.track.title.clone(),
                position: Some(start + u32::try_from(index).unwrap()),
                added: true,
            })
            .collect(),
        reason: PickReason {
            factors: Vec::new(),
            relaxed: Vec::new(),
            summary: "test".into(),
        },
    }
}

#[allow(clippy::field_reassign_with_default)] // The position clock is private to core.
fn playback(queued: &QueuedWork, index: usize) -> PlayerPlayback {
    let mut playback = PlayerPlayback::default();
    playback.transport = PLAYING;
    playback.media_uri = Some("x-rincon-queue:test-coordinator#0".into());
    playback.queue_position = queued.movements[index].position;
    playback.track_uri = Some(format!(
        "x-sonos-spotify:{}?sid=9",
        queued.movements[index].uri.replace(':', "%3a")
    ));
    playback
}

fn sample(work: &Work, index: usize) -> Movement {
    let queued = queued(work, 1);
    let state = playback(&queued, index);
    let pool = WorkPool::from_works(vec![work.clone()]);
    let mut movement = observed(&[queued], &pool, &state, None).unwrap();
    movement.started = true;
    movement
}

fn feed(queued: QueuedWork) -> QueueFeed {
    QueueFeed::restore(
        FeedSnapshot {
            suspended: false,
            zone: "test-coordinator".into(),
            earlier: Vec::new(),
            lookahead: 1,
            queued: vec![queued],
            playing: Some(1),
            recorded: None,
            played: Vec::new(),
            dry_end: None,
            recovery: Default::default(),
        },
        DjConfig::default(),
        1,
    )
}

/// Exercise the feed's observation checks and the daemon's taste learning
/// together, over real simulator sockets for both player generations.
struct LiveFeedback {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    id: PlayerId,
    dj: SpotifyDj,
    store: MemStore,
    clock: FakeClock,
    works: Vec<Work>,
}

impl LiveFeedback {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Feedback Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Feedback Room", SimModel::One)])
        }
        .spawn()
        .unwrap();
        let player = sim.player("Feedback Room").unwrap();
        let id = PlayerId(player.uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, player.ip).unwrap());
        let households = vec![house];
        let works = vec![work("A"), work("B"), work("C")];
        let mut store = MemStore::default();
        let items: Vec<_> = ["A", "B", "C"]
            .into_iter()
            .flat_map(library_items)
            .collect();
        apply_library_read(&mut store, &items).unwrap();
        let tracks: Vec<_> = works
            .iter()
            .flat_map(|work| &work.movements)
            .map(|movement| {
                (
                    movement.track.source_uri.as_str(),
                    movement.track.title.as_str(),
                )
            })
            .collect();
        control::queue_spotify_tracks(&lan, &households, &id, &tracks).unwrap();
        let dj = SpotifyDj::new(None);
        {
            let mut state = dj.state();
            // Use the same accepted catalog receipt as a live plan, so a
            // refill reaches the injected speaker fault instead of failing
            // selection authorization before the request is sent.
            state.reload(&store, None, 0).unwrap();
            assert_eq!(state.pool.len(), works.len());
            state
                .starters
                .insert(id.clone(), vec![fsonos_core::policy::Client::Cli]);
            state.feeds.insert(
                id.clone(),
                QueueFeed::restore(
                    FeedSnapshot {
                        suspended: false,
                        zone: id.0.clone(),
                        earlier: Vec::new(),
                        lookahead: 1,
                        queued: works
                            .iter()
                            .enumerate()
                            .map(|(index, work)| {
                                queued(work, 1 + 3 * u32::try_from(index).unwrap())
                            })
                            .collect(),
                        playing: Some(1),
                        recorded: None,
                        played: Vec::new(),
                        dry_end: None,
                        recovery: Default::default(),
                    },
                    DjConfig::default(),
                    7,
                ),
            );
        }
        Self {
            sim,
            households,
            id,
            dj,
            store,
            clock: FakeClock::new(
                chrono::DateTime::parse_from_rfc3339("2026-10-07T12:00:00+00:00").unwrap(),
            ),
            works,
        }
    }

    fn read(&self) -> PlayerPlayback {
        let host = self.sim.player("Feedback Room").unwrap().ip;
        let info = soap::get_position_info(&self.sim.lan(), host).unwrap();
        let mut model = Playback::default();
        model.apply_position(&self.id, &info, Instant::now());
        let mut event = model.of(&self.id).unwrap().clone();
        let transport = soap::get_transport_info(&self.sim.lan(), host).unwrap();
        event.transport = Some(transport.state);
        event.transport_status = Some(transport.status);
        event.media_uri = Some(format!("x-rincon-queue:{}#0", self.id.0));
        event.queue_length = Some(9);
        event.play_mode = Some("NORMAL".into());
        // The synthetic recording's duration is authoritative for this test.
        event.duration_secs = Some(180);
        event
    }

    fn seek(&mut self, position: u32) -> PlayerPlayback {
        control::play_queue_from(&self.sim.lan(), &self.households, &self.id, position).unwrap();
        let event = self.read();
        self.observe(&event);
        event
    }

    fn observe(&mut self, event: &PlayerPlayback) {
        self.dj.on_playback(
            DjSpeakers {
                transport: &self.sim.lan(),
                households: &self.households,
                coordinator: &self.id,
            },
            &mut self.store,
            event,
            &self.clock,
        );
    }

    fn advance(&self, seconds: i64) {
        self.clock.advance(chrono::TimeDelta::seconds(seconds));
    }

    fn accepted(&self) -> bool {
        self.dj.state().feeds[&self.id].accepted_playback()
    }

    fn signals(&self) -> Vec<fsonos_core::store::Feedback> {
        self.store.feedback_between(0..i64::MAX).unwrap()
    }
}

#[test]
fn rejected_backward_events_do_not_teach_skips_or_fabricated_completions() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        let stale = rig.seek(1);
        assert!(rig.accepted());
        rig.advance(180);
        rig.seek(2);
        assert!(rig.accepted());
        rig.advance(10);
        rig.observe(&stale);
        assert!(!rig.accepted(), "the player still reports movement 2");
        assert!(
            rig.signals().is_empty(),
            "a buffered event is not an early skip"
        );
        assert_eq!(
            rig.dj.state().listening[&rig.id]
                .current
                .as_ref()
                .unwrap()
                .0
                .position,
            2,
            "the rejected observation never replaces the listened movement"
        );
        rig.advance(170);
        rig.seek(3);
        rig.advance(180);
        rig.seek(4);
        assert!(
            rig.signals().is_empty(),
            "unverified continuity cannot earn a later whole-work completion"
        );
        for position in [5, 6] {
            rig.advance(180);
            rig.seek(position);
        }
        rig.advance(180);
        let host = rig.sim.player("Feedback Room").unwrap().ip;
        soap::stop(&rig.sim.lan(), host).unwrap();
        rig.observe(&rig.read());
        let signals = rig.signals();
        assert_eq!(
            signals.len(),
            1,
            "fresh whole-work listening still teaches taste"
        );
        assert_eq!(signals[0].signal, Signal::FullListen.value());
        assert_eq!(
            signals[0].performer,
            FeedbackSignal::about(&rig.works[1], Signal::FullListen, 0).performer
        );
    }
}

#[test]
fn failed_observation_checks_cannot_reuse_an_earlier_acceptance() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        let stale = rig.seek(1);
        rig.advance(180);
        rig.seek(2);
        assert!(rig.accepted());
        rig.sim
            .upnp_fault("Feedback Room", "GetMediaInfo", 701)
            .unwrap();
        rig.advance(10);
        rig.observe(&stale);
        assert!(!rig.accepted());
        assert!(rig.signals().is_empty());
        rig.sim.clear_faults("Feedback Room").unwrap();
        rig.advance(10);
        rig.seek(2);
        assert!(rig.accepted());
        rig.advance(10);
        rig.seek(3);
        assert!(
            rig.signals().is_empty(),
            "the gap and a later fresh event cannot manufacture an early skip"
        );
    }
}

#[test]
fn a_failed_top_up_preserves_valid_listening_evidence() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        rig.seek(1);
        rig.dj.state().feeds.get_mut(&rig.id).unwrap().lookahead = 3;
        rig.sim
            .upnp_fault("Feedback Room", "AddURIToQueue", 701)
            .unwrap();
        for position in [2, 3, 4] {
            rig.advance(180);
            rig.seek(position);
            assert!(
                rig.accepted(),
                "queue refill and playback evidence are separate"
            );
            assert!(rig.dj.state().problems.contains_key(&rig.id));
        }
        let signals = rig.signals();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].signal, Signal::FullListen.value());
        assert_eq!(
            signals[0].performer,
            FeedbackSignal::about(&rig.works[0], Signal::FullListen, 0).performer
        );
    }
}

#[test]
fn queue_notifications_keep_a_full_listen_when_the_occurrence_survives() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        let mut event = rig.seek(1);
        for position in [2, 3, 4] {
            // A queue event can be our own append or an edit elsewhere in
            // the queue. Only reconciliation can determine lost ownership.
            rig.advance(90);
            rig.dj.on_queue_changed(&rig.id);
            rig.observe(&event);
            assert!(rig.accepted());
            rig.advance(90);
            event = rig.seek(position);
        }
        let signals = rig.signals();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].signal, Signal::FullListen.value());
        assert_eq!(
            signals[0].performer,
            FeedbackSignal::about(&rig.works[0], Signal::FullListen, 0).performer
        );
    }
}

#[test]
fn observation_gaps_break_feedback_without_changing_the_owned_feed() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        rig.seek(1);
        rig.advance(180);
        rig.seek(2);
        let other = PlayerId("another-retained-feed".into());
        {
            let mut state = rig.dj.state();
            state.listening.entry(other.clone()).or_default().on_change(
                0,
                PLAYING,
                Some(sample(&rig.works[0], 0)),
            );
        }
        for (all, gap, next) in [(false, 3600, 3), (true, 10, 6)] {
            if all {
                rig.advance(180);
                rig.seek(5);
            }
            let before = rig.dj.state().feeds[&rig.id].snapshot();
            let calls = rig.sim.soap_log().len();
            let starter = rig.dj.starter(&rig.id);
            rig.dj.on_observation_gap((!all).then_some(&rig.id));
            assert_eq!(
                rig.sim.soap_log().len(),
                calls,
                "gaps perform no speaker I/O"
            );
            assert_eq!(rig.dj.state().feeds[&rig.id].snapshot(), before);
            assert_eq!(rig.dj.starter(&rig.id), starter);
            assert!(rig.dj.feeds(&rig.id));
            assert_eq!(rig.dj.state().listening[&other].advancing, !all);
            rig.advance(gap);
            rig.seek(next);
            rig.advance(180);
            rig.seek(next + 1);
            assert!(
                rig.signals().is_empty(),
                "a connectivity or stream gap cannot fabricate a completed work or early skip"
            );
        }
        for position in [8, 9] {
            rig.advance(180);
            rig.seek(position);
        }
        rig.advance(180);
        let host = rig.sim.player("Feedback Room").unwrap().ip;
        soap::stop(&rig.sim.lan(), host).unwrap();
        rig.observe(&rig.read());
        let signals = rig.signals();
        assert_eq!(signals.len(), 1, "fresh observed works still teach taste");
        assert_eq!(signals[0].signal, Signal::FullListen.value());
        assert_eq!(
            signals[0].performer,
            FeedbackSignal::about(&rig.works[2], Signal::FullListen, 0).performer
        );
    }
}

#[test]
fn cached_playback_after_a_gap_waits_for_actual_renderer_recovery() {
    for generation in [1, 2] {
        for all in [false, true] {
            let mut rig = LiveFeedback::new(generation);
            rig.seek(1);
            let host = rig.sim.player("Feedback Room").unwrap().ip;
            // Prepare a plausible later snapshot without giving it to the DJ.
            // A volume/Healthy event or Resync can carry this retained state
            // even though the renderer has been stopped throughout the gap.
            control::play_queue_from(&rig.sim.lan(), &rig.households, &rig.id, 4).unwrap();
            let mut stale = rig.read();
            soap::stop(&rig.sim.lan(), host).unwrap();
            rig.dj.on_observation_gap((!all).then_some(&rig.id));
            let before = rig.dj.state().feeds[&rig.id].snapshot();
            let calls = rig.sim.soap_log().len();
            rig.observe(&stale);
            assert!(!rig.dj.state().listening[&rig.id].advancing);
            assert!(rig.dj.state().listening[&rig.id].awaiting_observation);
            assert_eq!(rig.dj.state().feeds[&rig.id].snapshot(), before);
            let read_back = rig.sim.soap_log();
            assert!(
                read_back.len() > calls,
                "recovery checks the actual renderer"
            );
            assert!(
                read_back[calls..]
                    .iter()
                    .all(|call| call.action.starts_with("Get"))
            );

            rig.advance(180);
            stale.volume = Some(47);
            rig.observe(&stale);
            assert_eq!(
                rig.sim.soap_log().len(),
                read_back.len(),
                "volume-only Healthy/maintenance observations cannot bypass the bounded retry"
            );
            rig.dj.on_observation_gap(None);
            rig.dj.on_queue_changed(&rig.id);
            rig.observe(&stale);
            assert_eq!(rig.sim.soap_log().len(), read_back.len());
            assert!(!rig.dj.state().listening[&rig.id].advancing);
            assert!(rig.signals().is_empty());

            // A fresh changed AVTransport snapshot agrees with a complete
            // renderer read and bypasses the old snapshot's retry deadline.
            rig.seek(5);
            assert!(!rig.dj.state().listening[&rig.id].awaiting_observation);
            assert!(rig.dj.state().listening[&rig.id].advancing);
            for position in [6, 7] {
                rig.advance(180);
                rig.seek(position);
            }
            assert!(
                rig.signals().is_empty(),
                "the unheard opening movement cannot manufacture a full listen"
            );
            for position in [8, 9] {
                rig.advance(180);
                rig.seek(position);
            }
            rig.advance(180);
            soap::stop(&rig.sim.lan(), host).unwrap();
            rig.observe(&rig.read());
            let signals = rig.signals();
            assert_eq!(signals.len(), 1);
            assert_eq!(signals[0].signal, Signal::FullListen.value());
            assert_eq!(
                signals[0].performer,
                FeedbackSignal::about(&rig.works[2], Signal::FullListen, 0).performer
            );
        }
    }
}

#[test]
fn an_unobserved_owned_feed_also_requires_post_gap_playback_proof() {
    let mut rig = LiveFeedback::new(2);
    assert!(rig.dj.state().listening.is_empty());
    rig.dj.on_observation_gap(Some(&rig.id));
    assert!(rig.dj.state().listening[&rig.id].awaiting_observation);
    rig.seek(1);
    assert!(!rig.dj.state().listening[&rig.id].awaiting_observation);
    assert!(rig.dj.state().listening[&rig.id].advancing);
}

#[test]
fn a_failed_post_gap_read_keeps_the_feed_waiting_until_fresh_playback() {
    let mut rig = LiveFeedback::new(2);
    let cached = rig.seek(1);
    rig.dj.on_observation_gap(None);
    rig.sim
        .upnp_fault("Feedback Room", "GetTransportInfo", 701)
        .unwrap();
    let before = rig.dj.state().feeds[&rig.id].snapshot();
    rig.observe(&cached);
    assert!(rig.dj.state().listening[&rig.id].awaiting_observation);
    assert!(!rig.dj.state().listening[&rig.id].advancing);
    assert_eq!(rig.dj.state().feeds[&rig.id].snapshot(), before);
    let calls = rig.sim.soap_log().len();
    rig.observe(&cached);
    assert_eq!(rig.sim.soap_log().len(), calls);
    rig.sim.clear_faults("Feedback Room").unwrap();
    rig.seek(2);
    assert!(!rig.dj.state().listening[&rig.id].awaiting_observation);
    assert!(rig.dj.state().listening[&rig.id].advancing);
    assert!(rig.signals().is_empty());
}

#[test]
fn unknown_cached_status_accepts_fresh_ok_but_never_hides_a_renderer_error() {
    let mut rig = LiveFeedback::new(2);
    let mut cached = rig.seek(1);
    cached.transport_status = None;
    rig.dj.on_observation_gap(Some(&rig.id));
    rig.observe(&cached);
    assert!(!rig.dj.state().listening[&rig.id].awaiting_observation);
    assert!(rig.dj.state().listening[&rig.id].advancing);

    rig.sim.playback_error("Feedback Room").unwrap();
    let mut error = rig.read();
    assert_eq!(error.transport_status.as_deref(), Some("ERROR_OCCURRED"));
    error.transport_status = None;
    rig.dj.on_observation_gap(Some(&rig.id));
    let before = rig.dj.state().feeds[&rig.id].snapshot();
    let calls = rig.sim.soap_log().len();
    rig.observe(&error);
    assert!(rig.dj.state().listening[&rig.id].awaiting_observation);
    assert!(!rig.dj.state().listening[&rig.id].advancing);
    assert_eq!(rig.dj.state().feeds[&rig.id].snapshot(), before);
    assert!(
        rig.sim.soap_log()[calls..]
            .iter()
            .all(|call| call.action.starts_with("Get"))
    );
    assert!(rig.signals().is_empty());
}

#[test]
fn a_same_slot_seek_during_the_gap_cannot_manufacture_an_early_skip() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        let host = rig.sim.player("Feedback Room").unwrap().ip;
        control::play_queue_from(&rig.sim.lan(), &rig.households, &rig.id, 1).unwrap();
        let cached = rig.read();
        soap::seek_position(&rig.sim.lan(), host, 170).unwrap();
        // No previous Listening movement can suppress this near-start claim.
        // The fresh position, not the retained clock, must establish it.
        rig.dj.on_observation_gap(None);
        rig.observe(&cached);
        let state = rig.dj.state();
        let listening = &state.listening[&rig.id];
        assert!(!listening.awaiting_observation);
        assert!(!listening.current.as_ref().unwrap().0.started);
        drop(state);
        rig.advance(10);
        rig.seek(2);
        assert!(
            rig.signals().is_empty(),
            "a late arrival is not an early skip"
        );
    }
}

#[test]
fn a_paused_recovery_cannot_authorize_a_later_stale_near_start_claim() {
    let mut rig = LiveFeedback::new(2);
    let host = rig.sim.player("Feedback Room").unwrap().ip;
    control::play_queue_from(&rig.sim.lan(), &rig.households, &rig.id, 1).unwrap();
    soap::pause(&rig.sim.lan(), host).unwrap();
    let mut cached = rig.read();
    rig.dj.on_observation_gap(None);
    rig.observe(&cached);
    assert!(!rig.dj.state().listening[&rig.id].awaiting_observation);

    // A transport-only resume can retain the old paused-at-zero clock after
    // an unobserved seek. Paused proof was not evidence of hearing the start.
    soap::seek_position(&rig.sim.lan(), host, 170).unwrap();
    soap::play(&rig.sim.lan(), host).unwrap();
    cached.transport = Some(TransportState::Playing);
    rig.observe(&cached);
    assert!(
        !rig.dj.state().listening[&rig.id]
            .current
            .as_ref()
            .unwrap()
            .0
            .started
    );
    rig.advance(10);
    rig.seek(2);
    assert!(rig.signals().is_empty());
}

#[test]
fn a_same_length_current_replacement_breaks_listening_after_reconciliation() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        let stale = rig.seek(1);
        rig.advance(10);
        let host = rig.sim.player("Feedback Room").unwrap().ip;
        soap::remove_track_range_from_queue(&rig.sim.lan(), host, 0, 1, 1).unwrap();
        soap::enqueue_uri(
            &rig.sim.lan(),
            host,
            "http://example.invalid/owner.mp3",
            "",
            1,
            false,
        )
        .unwrap();
        control::play_queue_from(&rig.sim.lan(), &rig.households, &rig.id, 1).unwrap();
        rig.dj.on_queue_changed(&rig.id);
        // Length, slot and cached track are all unchanged. The CD event must
        // make the old track lose its claim before it can teach an early skip.
        rig.observe(&stale);
        assert!(rig.signals().is_empty());
        assert!(!rig.dj.state().listening[&rig.id].advancing);
        assert!(!rig.dj.state().listening[&rig.id].prefix_complete);
        for position in [2, 3, 4] {
            rig.advance(180);
            rig.seek(position);
        }
        assert!(
            rig.signals().is_empty(),
            "a replaced opening movement cannot earn a whole-work completion"
        );
    }
}

#[test]
fn queue_notifications_do_not_bypass_a_failed_top_up_backoff() {
    for generation in [1, 2] {
        let mut rig = LiveFeedback::new(generation);
        let event = rig.seek(1);
        rig.dj.state().feeds.get_mut(&rig.id).unwrap().lookahead = 3;
        rig.sim
            .upnp_fault("Feedback Room", "AddURIToQueue", 701)
            .unwrap();
        rig.observe(&event);
        assert!(rig.dj.state().refill_retries.contains_key(&rig.id));
        let before = rig.sim.soap_log().len();
        rig.dj.on_queue_changed(&rig.id);
        rig.observe(&event);
        assert!(rig.dj.state().refill_retries.contains_key(&rig.id));
        assert_eq!(
            rig.sim.soap_log().len(),
            before,
            "a self notification cannot turn a failed top-up into a retry loop"
        );
        assert!(rig.signals().is_empty());
    }
}

#[test]
fn the_exact_recording_supplies_performer_and_duration_not_the_first_work_key_match() {
    let a = work("A");
    let b = work("B");
    assert_eq!(a.work_key, b.work_key);
    let queued = queued(&b, 10);
    let state = playback(&queued, 1);
    let pool = WorkPool::from_works(vec![a, b.clone()]);
    let seen = observed(&[queued], &pool, &state, None).unwrap();
    assert_eq!(
        seen.duration_secs,
        Some(180),
        "library duration fills missing live metadata"
    );
    assert_eq!(seen.index, 1);
    assert!(
        !seen.started,
        "an unknown initial position is not an observed track start"
    );
    assert_eq!(
        seen.evidence,
        FeedbackSignal::about(&b, Signal::FullListen, 0)
    );
}

#[test]
fn ownership_needs_both_the_uri_and_position_and_a_unique_recording() {
    let work = work("A");
    let queue = queued(&work, 1);
    let pool = WorkPool::from_works(vec![work.clone()]);
    let mut state = playback(&queue, 0);
    state.track_uri = Some("spotify:track:Unrelated".into());
    assert!(observed(std::slice::from_ref(&queue), &pool, &state, None).is_none());
    state = playback(&queue, 0);
    state.queue_position = Some(99);
    assert!(observed(std::slice::from_ref(&queue), &pool, &state, None).is_none());
    state = playback(&queue, 0);
    assert!(observed(&[queue.clone(), queue.clone()], &pool, &state, None).is_none());
    let duplicate = WorkPool::from_works(vec![work.clone(), work]);
    assert!(observed(std::slice::from_ref(&queue), &duplicate, &state, None).is_none());
    let mut reversed = queue.clone();
    reversed.movements.reverse();
    assert!(observed(&[reversed], &pool, &state, None).is_none());
    let mut duplicate_slot = queue.clone();
    duplicate_slot.movements.push(queue.movements[0].clone());
    assert!(observed(&[duplicate_slot], &pool, &state, None).is_none());
}

#[test]
fn all_movements_must_be_heard_in_order_before_one_completion_signal() {
    let work = work("A");
    let mut listening = Listening::default();
    for index in 0..3 {
        assert_eq!(
            listening.on_change(
                i64::try_from(index).unwrap() * 180,
                PLAYING,
                Some(sample(&work, index)),
            ),
            None
        );
    }
    assert_eq!(
        listening.on_change(540, Some(TransportState::Stopped), None),
        Some((work.work_key.clone(), Signal::FullListen))
    );
    assert_eq!(
        listening.on_change(541, Some(TransportState::Stopped), None),
        None
    );
}

#[test]
fn starting_at_the_finale_skipping_a_movement_or_changing_recordings_is_not_a_full_listen() {
    let a = work("A");
    let b = work("B");
    for route in [
        vec![(&a, 2)],
        vec![(&a, 0), (&a, 2)],
        vec![(&a, 0), (&b, 1), (&b, 2)],
    ] {
        let mut listening = Listening::default();
        let mut now = 0;
        for (work, index) in route {
            assert_eq!(
                listening.on_change(now, PLAYING, Some(sample(work, index))),
                None
            );
            now += 180;
        }
        assert_eq!(
            listening.on_change(now, Some(TransportState::Stopped), None),
            None
        );
    }
    let mut listening = Listening::default();
    listening.on_change(0, PLAYING, Some(sample(&a, 0)));
    // An early skipped first movement emits a negative signal, not eventual completion.
    assert_eq!(
        listening.on_change(10, PLAYING, Some(sample(&a, 1))),
        Some((a.work_key.clone(), Signal::EarlySkip))
    );
    listening.on_change(190, PLAYING, Some(sample(&a, 2)));
    assert_eq!(
        listening.on_change(370, Some(TransportState::Stopped), None),
        None
    );
}

#[test]
fn unproven_durations_and_partial_works_do_not_earn_whole_work_credit() {
    let mut work = work("A");
    for partial in [false, true] {
        let mut listening = Listening::default();
        if partial {
            work.completeness = Completeness::Partial;
        }
        for index in 0..3 {
            let mut movement = sample(&work, index);
            if !partial && index == 0 {
                movement.duration_secs = None;
            }
            assert_eq!(
                listening.on_change(i64::try_from(index).unwrap() * 180, PLAYING, Some(movement),),
                None
            );
        }
        assert_eq!(
            listening.on_change(540, Some(TransportState::Stopped), None),
            None
        );
    }
}

#[test]
fn queue_position_changes_and_mid_track_observations_cannot_invent_early_skips() {
    let work = work("A");
    let mut listening = Listening::default();
    let mut current = sample(&work, 0);
    current.started = false;
    listening.on_change(0, PLAYING, Some(current));
    assert_eq!(
        listening.on_change(10, Some(TransportState::Stopped), None),
        None
    );
    listening.on_change(20, PLAYING, Some(sample(&work, 0)));
    let mut shifted = sample(&work, 0);
    shifted.position = 50;
    assert_eq!(listening.on_change(30, PLAYING, Some(shifted)), None);
    assert_eq!(
        listening.on_change(40, Some(TransportState::Stopped), None),
        None
    );
}

#[test]
fn same_track_observations_retain_attribution_when_the_pool_is_reloaded() {
    let work = work("A");
    let previous = sample(&work, 0);
    let queue = queued(&work, 1);
    let state = playback(&queue, 0);
    let seen = observed(&[queue], &WorkPool::default(), &state, Some(&previous)).unwrap();
    assert_eq!(seen.evidence, previous.evidence);
    assert_eq!(seen.recording, previous.recording);
}

#[test]
fn completed_feedback_keeps_its_recording_metadata_through_reload_and_is_written_once() {
    for classical in [true, false] {
        let a = work("A");
        let mut b = work("B");
        for movement in &mut b.movements {
            movement.classical = classical;
        }
        let queue = queued(&b, 1);
        let mut state = playback(&queue, 2);
        state.transport = Some(TransportState::Stopped);
        let feed = feed(queue);
        let mut listening = Listening {
            current: Some((sample(&b, 2), 180)),
            prefix_complete: true,
            ..Listening::default()
        };
        // The recording that played was removed from the pool by a library refresh.
        let pool = WorkPool::from_works(vec![a]);
        let mut store = MemStore::default();
        assert!(implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            0,
            1000
        ));
        let rows = store.feedback_between(0..2000).unwrap();
        assert_eq!(
            rows,
            [FeedbackSignal::about(&b, Signal::FullListen, 1000).to_store()]
        );
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            0,
            1001
        ));
        assert_eq!(store.feedback_between(0..2000).unwrap(), rows);
    }
}

#[test]
fn loss_of_ownership_and_non_dj_playback_are_not_dislikes() {
    let work = work("A");
    for active in [true, false] {
        let queue = queued(&work, 1);
        let mut state = playback(&queue, 0);
        state.track_uri = Some("https://stream.example.invalid/music".into());
        let feed = if active {
            feed(queue)
        } else {
            QueueFeed::new(
                &fsonos_types::PlayerId("test-coordinator".into()),
                DjConfig::default(),
                1,
            )
        };
        let mut listening = Listening::default();
        listening.on_change(0, PLAYING, Some(sample(&work, 0)));
        let mut store = MemStore::default();
        let pool = WorkPool::from_works(vec![work.clone()]);
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            0,
            10
        ));
        assert!(store.feedback_between(0..100).unwrap().is_empty());
        assert_eq!(
            listening.on_change(20, Some(TransportState::Stopped), None),
            None
        );
    }
}

#[test]
fn spotify_identity_is_a_whole_track_uri_not_a_substring_in_another_source() {
    for uri in [
        "https://example.invalid/spotify:track:AbC123",
        "spotify:track:AbC123/other",
        "spotify:track:AbC123#fragment",
        "spotify:track:",
        "spotify:album:AbC123",
    ] {
        assert_eq!(source_of(uri), None, "{uri}");
    }
    for uri in [
        "spotify:track:AbC123",
        "spotify%3atrack%3aAbC123",
        "x-sonos-spotify:spotify%3Atrack%3AAbC123?sid=9",
        "x-sonos-spotify:spotify:track:AbC123?sid=9",
    ] {
        assert_eq!(source_of(uri).as_deref(), Some("spotify:track:AbC123"));
    }
}

#[test]
fn repeated_track_uris_in_one_recording_still_have_distinct_movement_positions() {
    let mut work = work("A");
    work.movements[1].track.source_uri = work.movements[0].track.source_uri.clone();
    let mut listening = Listening::default();
    for index in 0..3 {
        let movement = sample(&work, index);
        assert_eq!(movement.index, index);
        assert_eq!(
            listening.on_change(i64::try_from(index).unwrap() * 180, PLAYING, Some(movement)),
            None
        );
    }
    assert_eq!(
        listening.on_change(540, Some(TransportState::Stopped), None),
        Some((work.work_key, Signal::FullListen))
    );
}

#[test]
fn temporary_sources_do_not_count_as_listens_even_when_the_track_identity_matches() {
    let work = work("A");
    let queue = queued(&work, 1);
    let feed = feed(queue.clone());
    let pool = WorkPool::from_works(vec![work.clone()]);
    for source in [
        None,
        Some("http://example.invalid/announcement.wav"),
        Some("x-sonos-vli:test-coordinator:2,spotify:test-session"),
        Some("x-rincon-queue:another-coordinator#0"),
    ] {
        let mut listening = Listening {
            current: Some((sample(&work, 2), 0)),
            prefix_complete: true,
            observed_at: Some(0),
            advancing: true,
            ..Listening::default()
        };
        let mut state = playback(&queue, 2);
        state.media_uri = source.map(str::to_owned);
        let mut store = MemStore::default();
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            20,
            1000
        ));
        assert!(!listening.advancing);
        assert!(!listening.prefix_complete);
        assert_eq!(listening.current.as_ref().unwrap().1, 0);

        // Return to the same movement after a long clip. The gap cannot
        // manufacture a complete work or an early-skip dislike.
        state = playback(&queue, 2);
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            300,
            1300
        ));
        assert_eq!(listening.current.as_ref().unwrap().1, 0);
        state.transport = Some(TransportState::Stopped);
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            310,
            1310
        ));
        assert!(store.feedback_between(0..2000).unwrap().is_empty());
    }
}

#[test]
fn renderer_errors_are_neither_skips_nor_completed_listens() {
    let work = work("A");
    let queue = queued(&work, 1);
    let feed = feed(queue.clone());
    let pool = WorkPool::from_works(vec![work.clone()]);
    for transport in [
        None,
        Some(TransportState::Playing),
        Some(TransportState::Paused),
        Some(TransportState::Stopped),
        Some(TransportState::Transitioning),
    ] {
        for heard in [0, 180] {
            let mut listening = Listening {
                current: Some((sample(&work, 2), heard)),
                prefix_complete: true,
                observed_at: Some(0),
                advancing: true,
                ..Listening::default()
            };
            let mut state = playback(&queue, 2);
            state.transport = transport;
            state.transport_status = Some("ERROR_OCCURRED".into());
            let mut store = MemStore::default();
            assert!(!implicit(
                &mut listening,
                &feed,
                &pool,
                &mut store,
                &state,
                20,
                1000
            ));
            assert!(!listening.advancing);
            assert!(!listening.prefix_complete);
            assert_eq!(
                listening.on_change(21, Some(TransportState::Stopped), None),
                None
            );
            assert!(store.feedback_between(0..2000).unwrap().is_empty());
        }
    }
}

#[test]
fn a_retired_suffix_cannot_turn_its_confirmed_prefix_into_a_full_listen() {
    let work = work("Prefix");
    let pool = WorkPool::from_works(vec![work.clone()]);
    let previous = sample(&work, 0);
    let mut retained = queued(&work, 1);
    retained.movements.truncate(1);
    let state = playback(&retained, 0);
    assert!(
        observed(&[retained], &pool, &state, Some(&previous)).is_none(),
        "the shortened owned recording cannot inherit full-work attribution"
    );
}
