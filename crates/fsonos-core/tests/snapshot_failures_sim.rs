//! Snapshot restoration must not seek or start a source whose prerequisite
//! failed. Every accepted request still crosses real S1/S2 simulator sockets.

use fsonos_core::snapshot::{self, Aspect, RestoreOp, RestoreReport, ZoneSnapshot};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::RefCell;
use std::net::IpAddr;
use std::time::Duration;

const OTHER: &str = "x-rincon-mp3radio://example.invalid/other.mp3";

struct Rig {
    sim: SimHandle,
    lan: SimLan,
    host: IpAddr,
    peer: IpAddr,
    snapshot: ZoneSnapshot,
    houses: Vec<HouseholdState>,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let rooms = [
            SimPlayerSpec::new("Room", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let builder = if s2 {
            builder.s2(rooms)
        } else {
            builder.s1(rooms)
        };
        let sim = builder.spawn().unwrap();
        let lan = sim.lan();
        let host = sim.player("Room").unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let id = PlayerId(sim.player("Room").unwrap().uuid.clone());
        soap::join_group(&lan, peer, &id).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        control::queue_spotify_tracks(
            &lan,
            &houses,
            &id,
            &[
                ("spotify:track:0SimSnapA0000000000001", "First"),
                ("spotify:track:0SimSnapB0000000000002", "Second"),
            ],
        )
        .unwrap()
        .unwrap();
        control::play_queue_from(&lan, &houses, &id, 2).unwrap();
        sim.clock().advance(Duration::from_secs(40));
        soap::set_volume(&lan, host, 25).unwrap();
        soap::set_volume(&lan, peer, 35).unwrap();
        soap::set_mute(&lan, peer, true).unwrap();
        let snapshot = snapshot::capture(&lan, &houses, &id, 1).unwrap();
        soap::set_av_transport_uri(&lan, host, OTHER, "").unwrap();
        soap::play(&lan, host).unwrap();
        soap::set_volume(&lan, host, 5).unwrap();
        soap::set_volume(&lan, peer, 70).unwrap();
        soap::set_mute(&lan, peer, false).unwrap();
        Self {
            sim,
            lan,
            host,
            peer,
            snapshot,
            houses,
        }
    }

    fn levels_restored(&self) {
        assert_eq!(soap::get_volume(&self.lan, self.host).unwrap(), 25);
        assert_eq!(soap::get_volume(&self.lan, self.peer).unwrap(), 35);
        assert!(soap::get_mute(&self.lan, self.peer).unwrap());
    }
}

struct Refuse<'a> {
    inner: &'a SimLan,
    action: &'static str,
    calls: RefCell<Vec<String>>,
}

impl Transport for Refuse<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if name == self.action {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "injected refusal".into(),
            });
        }
        self.inner.soap_post(host, path, action, body)
    }
}

fn skipped(report: &RestoreReport, aspect: Aspect) -> bool {
    !report.restored.contains(&aspect) && report.skipped.iter().any(|(a, _)| *a == aspect)
}

#[test]
fn refused_sources_do_not_seek_or_replay_the_interruption() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Refuse {
            inner: &rig.lan,
            action: "SetAVTransportURI",
            calls: RefCell::default(),
        };
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        for aspect in [Aspect::Source, Aspect::Position, Aspect::Transport] {
            assert!(skipped(&report, aspect), "{report:?}");
        }
        assert!(!t.calls.borrow().iter().any(|a| a == "Play" || a == "Seek"));
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OTHER);
        rig.levels_restored();
    }
}

#[test]
fn a_refused_track_seek_blocks_relative_seek_and_play() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Refuse {
            inner: &rig.lan,
            action: "Seek",
            calls: RefCell::default(),
        };
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        assert!(report.restored.contains(&Aspect::Source));
        assert!(skipped(&report, Aspect::Position));
        assert!(skipped(&report, Aspect::Transport));
        assert_eq!(t.calls.borrow().iter().filter(|a| *a == "Seek").count(), 1);
        assert!(!t.calls.borrow().iter().any(|a| a == "Play"));
        let state = soap::get_transport_info(&rig.lan, rig.host).unwrap().state;
        assert_eq!(state, TransportState::Stopped);
        rig.levels_restored();
    }
}

#[test]
fn policy_rejection_has_the_same_dependency_semantics_as_a_device_failure() {
    for reject_position in [false, true] {
        let rig = Rig::new(true);
        let from = rig.sim.soap_log().len();
        let report = snapshot::restore_vetted(
            &rig.lan,
            &rig.houses,
            &rig.snapshot,
            &Aspect::ALL,
            &|op| {
                if matches!(op, RestoreOp::SeekTrack(_)) && reject_position
                    || matches!(op, RestoreOp::PlayQueue) && !reject_position
                {
                    Err("policy refused prerequisite".into())
                } else {
                    Ok(op.clone())
                }
            },
        )
        .unwrap();
        assert!(skipped(&report, Aspect::Transport));
        assert!(
            rig.sim.soap_log()[from..]
                .iter()
                .all(|e| e.action != "Seek" && e.action != "Play")
        );
        rig.levels_restored();
    }
}

#[test]
fn failed_regrouping_cannot_install_or_start_the_saved_source() {
    for s2 in [false, true] {
        let mut rig = Rig::new(s2);
        let peer = PlayerId(rig.sim.player("Peer").unwrap().uuid.clone());
        control::leave(&rig.lan, &rig.houses, &peer).unwrap();
        rig.houses[0].apply_topology(&get_zone_group_state(&rig.lan, rig.host).unwrap());
        let t = Refuse {
            inner: &rig.lan,
            action: "SetAVTransportURI",
            calls: RefCell::default(),
        };
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        assert!(skipped(&report, Aspect::Group));
        assert!(skipped(&report, Aspect::Source));
        let installs = t
            .calls
            .borrow()
            .iter()
            .filter(|a| *a == "SetAVTransportURI")
            .count();
        assert_eq!(installs, 1);
        assert!(!t.calls.borrow().iter().any(|a| a == "Seek" || a == "Play"));
        rig.levels_restored();
    }
}

#[test]
fn a_room_level_failure_does_not_block_other_levels_or_valid_playback() {
    let rig = Rig::new(true);
    let report = snapshot::restore_vetted(
        &rig.lan,
        &rig.houses,
        &rig.snapshot,
        &Aspect::ALL,
        &|op| match op {
            RestoreOp::SetVolume { player, .. } if *player == rig.snapshot.coordinator => {
                Err("this room's volume is unavailable".into())
            }
            _ => Ok(op.clone()),
        },
    )
    .unwrap();
    assert!(skipped(&report, Aspect::Volume));
    assert!(report.restored.contains(&Aspect::Transport));
    assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
    assert_eq!(soap::get_volume(&rig.lan, rig.peer).unwrap(), 35);
    assert!(soap::get_mute(&rig.lan, rig.peer).unwrap());
    assert_eq!(soap::get_position_info(&rig.lan, rig.host).unwrap().track, 2);
}

#[test]
fn a_normal_restore_still_resumes_the_saved_track_and_position() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let report = snapshot::restore(&rig.lan, &rig.houses, &rig.snapshot).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        let at = soap::get_position_info(&rig.lan, rig.host).unwrap();
        assert_eq!((at.track, at.position_secs), (2, Some(40)));
        let state = soap::get_transport_info(&rig.lan, rig.host).unwrap().state;
        assert_eq!(state, TransportState::Playing);
        rig.levels_restored();
    }
}
