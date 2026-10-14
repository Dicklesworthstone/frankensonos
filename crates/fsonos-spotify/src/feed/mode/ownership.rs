//! Mode recovery for an already-running, restored DJ queue over loopback.

use fsonos_core::HouseholdState;
use fsonos_proto::control as soap;
use fsonos_proto::soap::{AV_TRANSPORT, args_xml, call};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::time::Duration;

use super::*;
use crate::dj::{DjConfig, PickReason};
use crate::feed::{FeedSnapshot, Movement, QueuedWork};

const TRACKS: [&str; 2] = [
    "spotify:track:ModeOwnershipA",
    "spotify:track:ModeOwnershipB",
];
const RADIO: &str = "x-rincon-mp3radio://example.invalid/other.mp3";

struct RunningQueue {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
}

impl RunningQueue {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Mode Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Mode Room", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let player = sim.player("Mode Room").unwrap();
        let id = PlayerId(player.uuid.clone());
        let host = player.ip;
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        control::queue_spotify_tracks(
            &lan,
            &houses,
            &id,
            &[(TRACKS[0], "First movement"), (TRACKS[1], "Last movement")],
        )
        .unwrap();
        control::play_queue_from(&lan, &houses, &id, 1).unwrap();
        Self {
            sim,
            houses,
            id,
            host,
        }
    }

    fn restored(&self) -> QueueFeed {
        let movements = TRACKS
            .iter()
            .enumerate()
            .map(|(index, uri)| Movement {
                uri: (*uri).into(),
                title: format!("Movement {index}"),
                position: Some(u32::try_from(index + 1).unwrap()),
                added: true,
            })
            .collect();
        QueueFeed::restore(
            FeedSnapshot {
                zone: self.id.0.clone(),
                earlier: Vec::new(),
                lookahead: 1,
                queued: vec![QueuedWork {
                    title: "Mode test work".into(),
                    composer: "Test composer".into(),
                    work_key: "mode-test-work".into(),
                    movements,
                    reason: PickReason {
                        factors: Vec::new(),
                        relaxed: Vec::new(),
                        summary: "Test selection".into(),
                    },
                }],
                playing: Some(1),
                recorded: None,
                played: Vec::new(),
            },
            DjConfig::default(),
            1,
        )
    }

    fn mode(&self, mode: &str) {
        call(
            &self.sim.lan(),
            self.host,
            &AV_TRANSPORT,
            "SetPlayMode",
            &args_xml(&[("InstanceID", "0"), ("NewPlayMode", mode)]),
        )
        .unwrap();
    }

    fn observation(&self, mode: &str) -> PlayerPlayback {
        let position = soap::get_position_info(&self.sim.lan(), self.host).unwrap();
        let mut state = PlayerPlayback::default();
        state.transport = Some(
            soap::get_transport_info(&self.sim.lan(), self.host)
                .unwrap()
                .state,
        );
        state.track_uri = Some(position.uri);
        state.queue_position = Some(position.track);
        state.play_mode = Some(mode.into());
        state
    }

    fn repair(&self, feed: &mut QueueFeed, event: &PlayerPlayback) -> Result<bool, FeedError> {
        let lan = self.sim.lan();
        maintain(
            feed,
            &Speakers {
                transport: &lan,
                households: &self.houses,
                coordinator: &self.id,
            },
            event,
        )
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action)
            .collect()
    }
}

#[test]
fn restored_owned_queue_recovers_order_without_restarting_its_track() {
    for generation in [1, 2] {
        let rig = RunningQueue::new(generation);
        let mut feed = rig.restored();
        rig.sim.advance(Duration::from_secs(7));
        let lan = rig.sim.lan();
        let before = soap::get_position_info(&lan, rig.host).unwrap();
        let queue = browse_range_snapshot(&lan, rig.host, QUEUE, 0, 10).unwrap();
        for mode in ["SHUFFLE", "REPEAT_ONE", "REPEAT_ALL"] {
            rig.mode(mode);
            let event = rig.observation(mode);
            let marker = rig.sim.soap_log().len();
            assert!(rig.repair(&mut feed, &event).unwrap());
            assert_eq!(rig.writes_since(marker), ["SetPlayMode"]);
            let after = soap::get_position_info(&lan, rig.host).unwrap();
            assert_eq!(
                (after.track, after.uri, after.position_secs),
                (before.track, before.uri.clone(), before.position_secs)
            );
            assert_eq!(
                browse_range_snapshot(&lan, rig.host, QUEUE, 0, 10).unwrap(),
                queue
            );
            assert!(feed.is_active());
        }
    }
}

#[test]
fn a_temporary_non_queue_source_suspends_mode_recovery_until_the_queue_returns() {
    for generation in [1, 2] {
        let rig = RunningQueue::new(generation);
        let mut feed = rig.restored();
        rig.mode("SHUFFLE");
        let buffered = rig.observation("SHUFFLE");
        let saved = feed.snapshot();
        let lan = rig.sim.lan();
        soap::set_av_transport_uri(&lan, rig.host, RADIO, "").unwrap();
        soap::play(&lan, rig.host).unwrap();
        let marker = rig.sim.soap_log().len();
        assert!(!rig.repair(&mut feed, &buffered).unwrap());
        assert!(rig.writes_since(marker).is_empty());
        assert_eq!(feed.snapshot(), saved);
        assert_eq!(soap::get_media_info(&lan, rig.host).unwrap().uri, RADIO);
        control::play_queue_from(&lan, &rig.houses, &rig.id, 1).unwrap();
        let event = rig.observation("SHUFFLE");
        let marker = rig.sim.soap_log().len();
        assert!(rig.repair(&mut feed, &event).unwrap());
        assert_eq!(rig.writes_since(marker), ["SetPlayMode"]);
    }
}

#[test]
fn a_replaced_queue_releases_the_feed_without_overriding_the_owners_mode() {
    for generation in [1, 2] {
        let rig = RunningQueue::new(generation);
        let mut feed = rig.restored();
        let lan = rig.sim.lan();
        soap::remove_all_tracks_from_queue(&lan, rig.host).unwrap();
        soap::add_uri_to_queue(&lan, rig.host, RADIO, "", false).unwrap();
        control::play_queue_from(&lan, &rig.houses, &rig.id, 1).unwrap();
        rig.mode("SHUFFLE");
        let event = rig.observation("SHUFFLE");
        let marker = rig.sim.soap_log().len();
        assert!(!rig.repair(&mut feed, &event).unwrap());
        assert!(rig.writes_since(marker).is_empty());
        assert!(!feed.is_active());
        assert!(feed.snapshot().is_none());
        assert_eq!(soap::get_position_info(&lan, rig.host).unwrap().uri, RADIO);
    }
}

#[test]
fn failed_observations_keep_the_original_feed_and_allow_a_later_repair() {
    for generation in [1, 2] {
        let rig = RunningQueue::new(generation);
        let mut feed = rig.restored();
        rig.mode("SHUFFLE");
        let event = rig.observation("SHUFFLE");
        let saved = feed.snapshot();
        for action in [
            "GetMediaInfo",
            "Browse",
            "GetPositionInfo",
            "GetZoneGroupState",
        ] {
            rig.sim.upnp_fault("Mode Room", action, 501).unwrap();
            let marker = rig.sim.soap_log().len();
            assert!(rig.repair(&mut feed, &event).is_err());
            assert!(rig.writes_since(marker).is_empty());
            assert_eq!(feed.snapshot(), saved);
            rig.sim.clear_faults("Mode Room").unwrap();
        }
        let marker = rig.sim.soap_log().len();
        assert!(rig.repair(&mut feed, &event).unwrap());
        assert_eq!(rig.writes_since(marker), ["SetPlayMode"]);
    }
}

#[test]
fn an_old_track_event_waits_for_current_identity_instead_of_changing_mode() {
    for generation in [1, 2] {
        let rig = RunningQueue::new(generation);
        let mut feed = rig.restored();
        rig.mode("SHUFFLE");
        let previous = rig.observation("SHUFFLE");
        soap::seek_track(&rig.sim.lan(), rig.host, 2).unwrap();
        let marker = rig.sim.soap_log().len();
        assert!(!rig.repair(&mut feed, &previous).unwrap());
        assert!(rig.writes_since(marker).is_empty());
        assert!(feed.is_active());
        let current = rig.observation("SHUFFLE");
        let marker = rig.sim.soap_log().len();
        assert!(rig.repair(&mut feed, &current).unwrap());
        assert_eq!(rig.writes_since(marker), ["SetPlayMode"]);
        assert_eq!(
            soap::get_position_info(&rig.sim.lan(), rig.host)
                .unwrap()
                .track,
            2
        );
    }
}
