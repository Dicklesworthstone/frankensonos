//! Native seeks against real S1/S2 simulator sockets. A wrapper
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
    SourceChanged,
    QueueChanged,
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
        if matches!(self.mode, LostReply::SourceChanged) {
            // Retain the selected occurrence and clock, but change the
            // source metadata so only the retained destination detects it.
            let source = soap::get_media_info(&self.lan, host)?;
            soap::set_av_transport_uri(&self.lan, host, &source.uri, "<DIDL-Lite/>")?;
            soap::seek_position(&self.lan, host, 100)?;
        }
        if matches!(self.mode, LostReply::QueueChanged) {
            // The selected occurrence is unchanged, but its queue proof is not.
            let (current, metadata) = soap::get_position_info_with_metadata(&self.lan, host)?;
            soap::enqueue_uri(&self.lan, host, &current.uri, &metadata, 0, false)?;
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
    control::queue_spotify_tracks(&lan, houses, &target.coordinator.id, &[track, track]).unwrap();
    control::play_queue_from(&lan, houses, &target.coordinator.id, 1).unwrap();
    soap::seek_position(&lan, target.coordinator.ip, 70).unwrap();
    if paused {
        control::pause(&lan, houses, &target.coordinator.id).unwrap();
    }
}

#[test]
fn accepted_seek_with_a_lost_reply_completes_on_s1_and_s2_without_another_write() {
    for (mode, seek) in [
        (LostReply::Network, control::Seek::By(30)),
        (LostReply::Malformed, control::Seek::By(30)),
        (LostReply::Network, control::Seek::At(100)),
        (LostReply::Malformed, control::Seek::At(100)),
    ] {
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
                    control::seek(&t, &houses, &target.coordinator.id, seek).unwrap(),
                    100
                );
                assert_eq!(t.writes.get(), 1);
                let after = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
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
                    .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
                    .map(|entry| entry.action.clone())
                    .collect();
                assert_eq!(
                    writes,
                    ["Seek"],
                    "no Play, source, volume or queue mutation"
                );
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
    let error =
        control::seek(&t, &houses, &target.coordinator.id, control::Seek::By(30)).unwrap_err();
    assert!(matches!(
        error,
        fsonos_core::CoreError::Proto(ProtoError::Network { .. })
    ));
    assert_eq!(t.writes.get(), 1);
    let now = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
    assert_eq!(now.position.track, 2);
    assert_eq!(now.position.position_secs, Some(100));
}

#[test]
fn absolute_seek_cannot_be_confirmed_by_a_different_occurrence_source_or_queue() {
    for mode in [
        LostReply::DuplicateTrack,
        LostReply::SourceChanged,
        LostReply::QueueChanged,
    ] {
        let sim = sim();
        let houses = snapshot(&sim);
        for room in ["Test S1", "Test S2"] {
            prepare(&sim, &houses, room, true);
            let target = resolve_room(&houses, room).unwrap();
            let t = LoseSeek {
                lan: sim.lan(),
                mode,
                writes: Cell::new(0),
            };
            let error = control::seek(
                &t,
                &houses,
                &target.coordinator.id,
                control::Seek::At(100),
            )
            .unwrap_err();
            assert!(matches!(
                error,
                fsonos_core::CoreError::Proto(ProtoError::Network { .. })
            ));
            assert_eq!(t.writes.get(), 1, "an uncertain seek is never repeated");
            if matches!(mode, LostReply::SourceChanged | LostReply::QueueChanged) {
                let current = soap::get_position_info(&sim.lan(), target.coordinator.ip).unwrap();
                assert_eq!(current.track, 1);
                assert_eq!(current.position_secs, Some(100));
                assert_eq!(current.duration_secs, Some(180));
            }
        }
    }
}

struct ObserveSeek {
    lan: SimLan,
    unreadable_position: bool,
    refused: bool,
    writes: Cell<usize>,
    reads_after_write: Cell<usize>,
}

impl Transport for ObserveSeek {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#Seek\"") {
            self.writes.set(self.writes.get() + 1);
            if self.refused {
                return Err(ProtoError::SoapFault {
                    code: 711,
                    reason: "synthetic illegal seek target".into(),
                });
            }
        } else if self.writes.get() > 0 {
            self.reads_after_write.set(self.reads_after_write.get() + 1);
        }
        if self.unreadable_position && action.ends_with("#GetPositionInfo\"") {
            return Err(ProtoError::Malformed("synthetic unavailable clock".into()));
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn absolute_seek_remains_available_without_optional_observation_and_preserves_refusals() {
    for (unreadable_position, refused) in [(true, false), (false, true), (true, true)] {
        let sim = sim();
        let houses = snapshot(&sim);
        for room in ["Test S1", "Test S2"] {
            prepare(&sim, &houses, room, true);
            let target = resolve_room(&houses, room).unwrap();
            let t = ObserveSeek {
                lan: sim.lan(),
                unreadable_position,
                refused,
                writes: Cell::new(0),
                reads_after_write: Cell::new(0),
            };
            let result = control::seek(
                &t,
                &houses,
                &target.coordinator.id,
                control::Seek::At(100),
            );
            if refused {
                assert!(matches!(
                    result,
                    Err(fsonos_core::CoreError::Proto(ProtoError::SoapFault { code: 711, .. }))
                ));
            } else {
                assert_eq!(result.unwrap(), 100);
            }
            assert_eq!(t.writes.get(), 1);
            assert_eq!(
                t.reads_after_write.get(),
                0,
                "definite results need no recovery"
            );
            let position = soap::get_position_info(&sim.lan(), target.coordinator.ip).unwrap();
            assert_eq!(position.position_secs, Some(if refused { 70 } else { 100 }));
        }
    }
}
