//! Real S1/S2 SOAP exchanges through the public feed event entry point.

use super::*;
use crate::classical::CandidatePool;
use crate::dj::{DjConfig, PickReason, WorkPool};
use crate::feed::{FeedSnapshot, Movement, Planning, QueuedWork};
use crate::library::LibraryItem;
use crate::works::group_works;
use fsonos_core::HouseholdState;
use fsonos_core::control;
use fsonos_core::store::MemStore;
use fsonos_proto::ProtoError;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::IpAddr;

const ROOM: &str = "Exhaustion Room";
const OTHER: &str = "Other Room";
const TRACKS: [&str; 4] = [
    "spotify:track:DryTestA",
    "spotify:track:DryTestB",
    "spotify:track:DryTestC",
    "spotify:track:DryTestD",
];

struct Rig {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 { SimModel::One } else { SimModel::Play5Gen1 };
        let players = [SimPlayerSpec::new(ROOM, model), SimPlayerSpec::new(OTHER, model)];
        let builder = SimHousehold::builder();
        let sim = if s2 { builder.s2(players) } else { builder.s1(players) }
            .spawn()
            .unwrap();
        let host = sim.player(ROOM).unwrap().ip;
        let id = PlayerId(sim.player(ROOM).unwrap().uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        let tracks: Vec<_> = TRACKS.iter().map(|uri| (*uri, "Test movement")).collect();
        control::queue_spotify_tracks(&lan, &houses, &id, &tracks).unwrap();
        // Reproduce the observable end-of-queue state without clock sleeps:
        // a stopped queue on its first track, after the feed observed its last.
        control::play_queue_from(&lan, &houses, &id, 1).unwrap();
        soap::stop(&lan, host).unwrap();
        Self { sim, lan, houses, id, host }
    }

    fn speakers<'a, T: Transport + ?Sized>(&'a self, transport: &'a T) -> Speakers<'a, T> {
        Speakers { transport, households: &self.houses, coordinator: &self.id }
    }

    fn feed(&self) -> QueueFeed {
        QueueFeed::restore(
            FeedSnapshot {
                zone: self.id.0.clone(),
                earlier: Vec::new(),
                lookahead: 1,
                queued: vec![QueuedWork {
                    title: "Finished work".into(),
                    composer: "Test artist".into(),
                    work_key: "finished-work".into(),
                    movements: TRACKS.iter().enumerate().map(|(index, uri)| Movement {
                        uri: (*uri).into(),
                        title: "Test movement".into(),
                        position: Some(u32::try_from(index + 1).unwrap()),
                        added: true,
                    }).collect(),
                    reason: PickReason { factors: Vec::new(), relaxed: Vec::new(), summary: "test".into() },
                }],
                played: Vec::new(),
                playing: Some(4),
                recorded: Some((4, TRACKS[3].into())),
            },
            DjConfig::default(),
            7,
        )
    }

    fn event(&self) -> PlayerPlayback {
        let position = soap::get_position_info(&self.lan, self.host).unwrap();
        PlayerPlayback {
            transport: Some(TransportState::Stopped),
            queue_position: Some(position.track),
            queue_length: Some(u32::try_from(browse_snapshot(&self.lan, self.host, QUEUE).unwrap().objects.len()).unwrap()),
            track_uri: Some(position.uri),
            play_mode: Some("NORMAL".into()),
            ..PlayerPlayback::default()
        }
    }

    fn tick<T: Transport + ?Sized>(
        &self,
        transport: &T,
        feed: &mut QueueFeed,
        event: &PlayerPlayback,
    ) -> Result<usize, FeedError> {
        let items: Vec<_> = (0..8).map(|n| LibraryItem {
            source_uri: format!("spotify:track:DryNext{n}"),
            title: format!("Next song {n}"),
            artists: vec![format!("Next artist {n}")],
            duration_secs: Some(180),
            ..LibraryItem::default()
        }).collect();
        let candidates = CandidatePool::build(&items);
        let pool = WorkPool::from_works(group_works(candidates.tracks()));
        feed.on_playback(
            &self.speakers(transport),
            Planning {
                pool: &pool,
                steer: None,
                feedback: None,
                now: 10_000,
                local_hour: Some(12),
                queued_elsewhere: &[],
            },
            &mut MemStore::default(),
            event,
        )
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log().into_iter().skip(start)
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action).collect()
    }
}

#[test]
fn a_genuine_exhausted_queue_is_fed_and_starts_the_first_new_work() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let event = rig.event();
        assert!(feed.ran_dry(&event));
        assert!(rig.tick(&rig.lan, &mut feed, &event).unwrap() > 0);
        assert_eq!(soap::get_position_info(&rig.lan, rig.host).unwrap().track, 5);
        assert_eq!(soap::get_transport_info(&rig.lan, rig.host).unwrap().state, TransportState::Playing);
        assert!(feed.is_active());
    }
}

#[test]
fn an_owners_pause_or_resume_wins_over_a_buffered_exhaustion_event() {
    for s2 in [false, true] {
        for paused in [false, true] {
            let rig = Rig::new(s2);
            let mut feed = rig.feed();
            let event = rig.event();
            let before = feed.snapshot();
            soap::play(&rig.lan, rig.host).unwrap();
            if paused { soap::pause(&rig.lan, rig.host).unwrap(); }
            let start = rig.sim.soap_log().len();
            assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
            assert!(rig.writes_since(start).is_empty());
            assert_eq!(feed.snapshot(), before);
            assert_eq!(soap::get_position_info(&rig.lan, rig.host).unwrap().track, 1);
        }
    }
}

#[test]
fn a_new_stop_on_another_occurrence_does_not_restart_the_old_end() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let event = rig.event();
        control::play_queue_from(&rig.lan, &rig.houses, &rig.id, 2).unwrap();
        soap::stop(&rig.lan, rig.host).unwrap();
        let before = feed.snapshot();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
        assert!(rig.writes_since(start).is_empty());
        assert_eq!(feed.snapshot(), before);
    }
}

#[test]
fn a_new_source_or_coordinator_is_not_replaced_by_recovery() {
    for s2 in [false, true] {
        for regroup in [false, true] {
            let rig = Rig::new(s2);
            let mut feed = rig.feed();
            let event = rig.event();
            if regroup {
                let other = PlayerId(rig.sim.player(OTHER).unwrap().uuid.clone());
                soap::join_group(&rig.lan, rig.host, &other).unwrap();
            } else {
                soap::set_av_transport_uri(&rig.lan, rig.host, "http://example.invalid/owner.mp3", "").unwrap();
            }
            let before = feed.snapshot();
            let start = rig.sim.soap_log().len();
            assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
            assert!(rig.writes_since(start).is_empty());
            assert_eq!(feed.snapshot(), before);
        }
    }
}

#[test]
fn failed_observations_preserve_recovery_and_a_later_event_can_succeed() {
    for s2 in [false, true] {
        for action in ["GetMediaInfo", "GetTransportInfo", "GetPositionInfo", "GetZoneGroupState", "Browse"] {
            let rig = Rig::new(s2);
            let mut feed = rig.feed();
            let event = rig.event();
            let before = feed.snapshot();
            rig.sim.upnp_fault(ROOM, action, 701).unwrap();
            let start = rig.sim.soap_log().len();
            assert!(rig.tick(&rig.lan, &mut feed, &event).is_err(), "{action}");
            assert!(rig.writes_since(start).is_empty());
            assert_eq!(feed.snapshot(), before);
            assert!(feed.dry_end.is_none());
            rig.sim.clear_faults(ROOM).unwrap();
            assert!(rig.tick(&rig.lan, &mut feed, &event).unwrap() > 0);
        }
    }
}

struct PauseDuringRead<'a> {
    rig: &'a Rig,
    changed: Cell<bool>,
}

impl Transport for PauseDuringRead<'_> {
    fn soap_post(&self, host: IpAddr, path: &str, action: &str, body: &str) -> Result<String, ProtoError> {
        let response = self.rig.lan.soap_post(host, path, action, body)?;
        if action.contains("#GetPositionInfo") && !self.changed.replace(true) {
            // These are the owner's two writes, not recovery's writes.
            soap::play(&self.rig.lan, host)?;
            soap::pause(&self.rig.lan, host)?;
        }
        Ok(response)
    }
}

#[test]
fn a_pause_arriving_during_observation_invalidates_the_restart() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let event = rig.event();
        let observer = PauseDuringRead { rig: &rig, changed: Cell::new(false) };
        let before = feed.snapshot();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&observer, &mut feed, &event).unwrap(), 0);
        assert!(observer.changed.get());
        assert_eq!(rig.writes_since(start), ["Play", "Pause"]);
        assert_eq!(feed.snapshot(), before);
    }
}

#[test]
fn incomplete_track_identity_never_authorizes_automatic_play() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let mut event = rig.event();
        feed.dry_end = Some(4); // an earlier retry, without enough current identity
        event.track_uri = None;
        let before = feed.snapshot();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
        assert!(rig.writes_since(start).is_empty());
        assert_eq!(feed.snapshot(), before);
        assert_eq!(feed.dry_end, Some(4));
    }
}

#[test]
fn unrelated_playback_events_do_not_add_polling() {
    struct NoIo;
    impl Transport for NoIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("ordinary playback must stay push-driven")
        }
    }
    let id = PlayerId("TestCoordinator".into());
    let at = Speakers { transport: &NoIo, households: &[], coordinator: &id };
    let feed = QueueFeed::new(&id, DjConfig::default(), 7);
    for state in [None, Some(TransportState::Playing), Some(TransportState::Paused)] {
        let event = PlayerPlayback { transport: state, ..PlayerPlayback::default() };
        assert!(permits(&feed, &at, &event).unwrap());
    }
}

#[test]
fn a_same_length_owner_edit_cannot_be_reinterpreted_as_the_old_exhausted_queue() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let event = rig.event();
        soap::remove_track_range_from_queue(&rig.lan, rig.host, 0, 4, 1).unwrap();
        soap::add_uri_to_queue(&rig.lan, rig.host, "http://example.invalid/owner.mp3", "", false).unwrap();
        let before = feed.snapshot();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
        assert!(rig.writes_since(start).is_empty());
        assert_eq!(feed.snapshot(), before);
    }
}

struct EditDuringRead<'a> {
    rig: &'a Rig,
    changed: Cell<bool>,
}

impl Transport for EditDuringRead<'_> {
    fn soap_post(&self, host: IpAddr, path: &str, action: &str, body: &str) -> Result<String, ProtoError> {
        let response = self.rig.lan.soap_post(host, path, action, body)?;
        if action.contains("#GetPositionInfo") && !self.changed.replace(true) {
            soap::remove_track_range_from_queue(&self.rig.lan, host, 0, 4, 1)?;
            soap::add_uri_to_queue(&self.rig.lan, host, "http://example.invalid/owner.mp3", "", false)?;
        }
        Ok(response)
    }
}

#[test]
fn a_queue_edit_between_the_snapshot_and_final_observation_cancels_recovery() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let event = rig.event();
        let observer = EditDuringRead { rig: &rig, changed: Cell::new(false) };
        let before = feed.snapshot();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&observer, &mut feed, &event).unwrap(), 0);
        assert!(observer.changed.get());
        assert_eq!(rig.writes_since(start), ["RemoveTrackRangeFromQueue", "AddURIToQueue"]);
        assert_eq!(feed.snapshot(), before);
    }
}

#[test]
fn a_pending_retry_can_play_its_already_queued_successor() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        control::queue_spotify_tracks(&rig.lan, &rig.houses, &rig.id, &[("spotify:track:DryNextA", "Successor")]).unwrap();
        let mut successor = feed.queued[0].clone();
        successor.work_key = "successor-work".into();
        successor.movements = vec![Movement {
            uri: "spotify:track:DryNextA".into(),
            title: "Successor".into(),
            position: Some(5),
            added: true,
        }];
        feed.queued.push(successor);
        feed.dry_end = Some(4);
        feed.queue_len = Some(5);
        let event = rig.event();
        assert!(rig.tick(&rig.lan, &mut feed, &event).unwrap() > 0);
        assert_eq!(soap::get_position_info(&rig.lan, rig.host).unwrap().track, 5);
        assert_eq!(soap::get_transport_info(&rig.lan, rig.host).unwrap().state, TransportState::Playing);
        assert!(feed.dry_end.is_none());
    }
}

#[test]
fn an_unclaimed_suffix_does_not_authorize_a_pending_restart() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        soap::add_uri_to_queue(&rig.lan, rig.host, "http://example.invalid/owner.mp3", "", false).unwrap();
        feed.dry_end = Some(4);
        feed.queue_len = Some(5);
        let event = rig.event();
        let before = feed.snapshot();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
        assert!(rig.writes_since(start).is_empty());
        assert_eq!(feed.snapshot(), before);
    }
}
