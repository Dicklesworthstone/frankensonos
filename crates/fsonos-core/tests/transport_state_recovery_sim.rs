//! A simulated player changes state, then the transport loses its reply.
//! The native controls must observe it, not repeat Play/Pause/Stop.

use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::cell::Cell;
use std::net::IpAddr;

struct LoseReply {
    lan: SimLan,
    action: &'static str,
    malformed: bool,
    writes: Cell<usize>,
}

impl Transport for LoseReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        if !action.ends_with(&format!("#{}\"", self.action)) {
            return Ok(reply);
        }
        self.writes.set(self.writes.get() + 1);
        fsonos_proto::soap::parse_response(&reply, self.action)?;
        if self.malformed {
            return Ok("lost acknowledgement".into());
        }
        Err(ProtoError::Network {
            target: "simulator".into(),
            detail: "accepted control lost its reply".into(),
        })
    }
}

#[test]
fn native_state_commands_recover_lost_and_malformed_replies_on_both_generations() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Test S1", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Test S2", SimModel::One)])
        .spawn()
        .unwrap();
    for room in ["Test S1", "Test S2"] {
        let host = sim.player(room).unwrap().ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        let houses = [house];
        let target = resolve_room(&houses, room).unwrap();
        let source = "x-rincon-mp3radio://example.invalid/synthetic-stream";
        for malformed in [false, true] {
            for (action, expected) in [
                ("Play", TransportState::Playing),
                ("Pause", TransportState::Paused),
                ("Stop", TransportState::Stopped),
            ] {
                control::play_uri(&sim.lan(), &houses, &target.coordinator.id, source, "")
                    .unwrap();
                if action == "Play" {
                    soap::pause(&sim.lan(), host).unwrap();
                }
                let t = LoseReply {
                    lan: sim.lan(),
                    action,
                    malformed,
                    writes: Cell::new(0),
                };
                let before = sim.soap_log().len();
                match action {
                    "Play" => control::resume(&t, &houses, &target.coordinator.id),
                    "Pause" => control::pause(&t, &houses, &target.coordinator.id),
                    _ => control::stop(&t, &houses, &target.coordinator.id),
                }
                .unwrap();
                assert_eq!(t.writes.get(), 1);
                assert_eq!(soap::get_transport_info(&sim.lan(), host).unwrap().state, expected);
                assert_eq!(soap::get_media_info(&sim.lan(), host).unwrap().uri, source);
                let writes: Vec<_> = sim.soap_log()[before..]
                    .iter()
                    .filter(|entry| !entry.action.starts_with("Get"))
                    .map(|entry| entry.action.clone())
                    .collect();
                assert_eq!(writes, [action], "no source reset, queue edit or second write");
            }
        }
    }
}
