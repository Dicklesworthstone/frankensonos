//! Native relative seeks against real S1/S2 simulator sockets. A wrapper
//! discards an acknowledgement only after the simulator accepted the write.

use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::cell::Cell;
use std::net::IpAddr;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Test S1", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Test S2", SimModel::One)])
        .spawn()
        .unwrap()
}

fn snapshot(sim: &SimHandle) -> Vec<HouseholdState> {
    ["Test S1", "Test S2"]
        .iter()
        .map(|room| {
            let mut house = HouseholdState::default();
            let host = sim.player(room).unwrap().ip;
            house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
            house
        })
        .collect()
}

#[derive(Clone, Copy)]
enum LostReply {
    Network,
    Malformed,
    DuplicateTrack,
}

struct LoseSeek {
    lan: SimLan,
    mode: LostReply,
    writes: Cell<usize>,
}

impl Transport for LoseSeek {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body)?;
        if !action.ends_with("#Seek\"") {
            return Ok(answer);
        }
        self.writes.set(self.writes.get() + 1);
        // The real SOAP decoder establishes that this was an accepted seek.
        fsonos_proto::soap::parse_response(&answer, "Seek")?;
        if matches!(self.mode, LostReply::DuplicateTrack) {
            soap::next(&self.lan, host)?;
            soap::seek_position(&self.lan, host, 100)?;
        }
        if matches!(self.mode, LostReply::Malformed) {
            return Ok("a lost seek acknowledgement".into());
        }
        Err(ProtoError::Network {
            target: "simulator".into(),
            detail: "accepted seek lost its reply".into(),
        })
    }
}

fn prepare(sim: &SimHandle, houses: &[HouseholdState], room: &str, paused: bool) {
    let lan = sim.lan();
    let target = resolve_room(houses, room).unwrap();
    // Equal URIs deliberately exercise queue-slot identity, not just URI equality.
    let track = ("spotify:track:0SimSeek00000000000001", "Synthetic track");
    control::queue_spotify_tracks(&lan, houses, &target.coordinator.id, &[track, track])
        .unwrap();
    control::play_queue_from(&lan, houses, &target.coordinator.id, 1).unwrap();
    soap::seek_position(&lan, target.coordinator.ip, 70).unwrap();
    if paused {
        control::pause(&lan, houses, &target.coordinator.id).unwrap();
    }
}

#[test]
fn accepted_seek_with_a_lost_reply_completes_on_s1_and_s2_without_another_write() {
    for mode in [LostReply::Network, LostReply::Malformed] {
        for paused in [false, true] {
            let sim = sim();
            let houses = snapshot(&sim);
            for room in ["Test S1", "Test S2"] {
                prepare(&sim, &houses, room, paused);
                let target = resolve_room(&houses, room).unwrap();
                let t = LoseSeek {
                    lan: sim.lan(),
                    mode,
                    writes: Cell::new(0),
                };
                let before = sim.soap_log().len();
                assert_eq!(
                    control::seek(&t, &houses, &target.coordinator.id, control::Seek::By(30))
                        .unwrap(),
                    100
                );
                assert_eq!(t.writes.get(), 1);
                let after = control::playback(&sim.lan(), &houses, &target.coordinator.id)
                    .unwrap();
                assert_eq!(after.position.position_secs, Some(100));
                assert_eq!(after.position.track, 1);
                assert_eq!(
                    after.transport.state,
                    if paused {
                        TransportState::Paused
                    } else {
                        TransportState::Playing
                    }
                );
                let writes: Vec<_> = sim.soap_log()[before..]
                    .iter()
                    .filter(|entry| !entry.action.starts_with("Get"))
                    .map(|entry| entry.action.clone())
                    .collect();
                assert_eq!(writes, ["Seek"], "no Play, source, volume or queue mutation");
            }
        }
    }
}

#[test]
fn a_duplicate_track_in_a_different_slot_is_not_the_acknowledged_seek() {
    let sim = sim();
    let houses = snapshot(&sim);
    prepare(&sim, &houses, "Test S1", false);
    let target = resolve_room(&houses, "Test S1").unwrap();
    let t = LoseSeek {
        lan: sim.lan(),
        mode: LostReply::DuplicateTrack,
        writes: Cell::new(0),
    };
    let error = control::seek(&t, &houses, &target.coordinator.id, control::Seek::By(30))
        .unwrap_err();
    assert!(matches!(error, fsonos_core::CoreError::Proto(ProtoError::Network { .. })));
    assert_eq!(t.writes.get(), 1);
    let now = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
    assert_eq!(now.position.track, 2);
    assert_eq!(now.position.position_secs, Some(100));
}
