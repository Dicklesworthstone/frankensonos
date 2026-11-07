//! EQ changes complete after a lost acknowledgement only when the affected
//! field can be read back. Every speaker exchange uses the socket simulator.

use fsonos_core::control::{self, Tone, ToneChange};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::IpAddr;

const ACTIONS: [&str; 3] = ["SetBass", "SetTreble", "SetLoudness"];

struct LoseReply {
    lan: SimLan,
    action: &'static str,
    malformed: bool,
    apply: bool,
    attempts: Cell<usize>,
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "simulator".into(),
        detail: "tone write acknowledgement lost".into(),
    }
}

impl Transport for LoseReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let affected = action.ends_with(&format!("#{}\"", self.action));
        if affected {
            self.attempts.set(self.attempts.get() + 1);
            if !self.apply {
                return Err(lost());
            }
        }
        let reply = self.lan.soap_post(host, path, action, body)?;
        if !affected {
            return Ok(reply);
        }
        fsonos_proto::soap::parse_response(&reply, self.action)?;
        if self.malformed {
            Ok("lost acknowledgement".into())
        } else {
            Err(lost())
        }
    }
}

fn house(sim: &SimHandle, room: &str) -> ([HouseholdState; 1], PlayerId) {
    let player = sim.player(room).unwrap();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
    ([house], PlayerId(player.uuid.clone()))
}

fn flat() -> Tone {
    Tone {
        bass: 0,
        treble: 0,
        loudness: true,
    }
}

#[test]
fn each_eq_stage_recovers_lost_and_malformed_replies_on_both_generations() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let wanted = Tone {
        bass: -4,
        treble: 6,
        loudness: false,
    };
    for room in ["Kitchen", "Living Room"] {
        let (houses, player) = house(&sim, room);
        for malformed in [false, true] {
            for action in ACTIONS {
                control::set_tone(&sim.lan(), &houses, &player, ToneChange::all(flat())).unwrap();
                let transport = LoseReply {
                    lan: sim.lan(),
                    action,
                    malformed,
                    apply: true,
                    attempts: Cell::new(0),
                };
                let before = sim.soap_log().len();
                control::set_tone(&transport, &houses, &player, ToneChange::all(wanted)).unwrap();
                let exchanges = sim.soap_log();
                let writes: Vec<_> = exchanges[before..]
                    .iter()
                    .filter(|entry| entry.action.starts_with("Set"))
                    .map(|entry| entry.action.as_str())
                    .collect();
                assert_eq!(writes, ACTIONS, "each requested field is written once");
                let reads: Vec<_> = exchanges[before..]
                    .iter()
                    .filter(|entry| entry.action.starts_with("Get"))
                    .map(|entry| entry.action.as_str())
                    .collect();
                assert_eq!(reads, [action.replacen("Set", "Get", 1)]);
                assert_eq!(transport.attempts.get(), 1, "no replay after a lost reply");
                assert_eq!(control::tone(&sim.lan(), &houses, &player).unwrap(), wanted);
            }
        }
    }
}

#[test]
fn a_definite_refusal_stops_even_when_the_requested_tone_already_matches() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let (houses, player) = house(&sim, "Kitchen");
    for (index, action) in ACTIONS.iter().enumerate() {
        sim.upnp_fault("Kitchen", action, 501).unwrap();
        let before = sim.soap_log().len();
        let error = control::set_tone(&sim.lan(), &houses, &player, ToneChange::all(flat()))
            .unwrap_err();
        assert!(matches!(
            error,
            CoreError::Proto(ProtoError::SoapFault { code: 501, .. })
        ));
        let exchanges = sim.soap_log();
        let actions: Vec<_> = exchanges[before..]
            .iter()
            .map(|entry| entry.action.as_str())
            .collect();
        assert_eq!(actions, ACTIONS[..=index], "no read-back or later writes");
        sim.clear_faults("Kitchen").unwrap();
    }
}

#[test]
fn an_unconfirmed_field_preserves_the_write_error_and_stops_later_changes() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let (houses, player) = house(&sim, "Kitchen");
    for apply in [false, true] {
        control::set_tone(&sim.lan(), &houses, &player, ToneChange::all(flat())).unwrap();
        if apply {
            sim.upnp_fault("Kitchen", "GetBass", 501).unwrap();
        }
        let transport = LoseReply {
            lan: sim.lan(),
            action: "SetBass",
            malformed: false,
            apply,
            attempts: Cell::new(0),
        };
        let before = sim.soap_log().len();
        let error = control::set_tone(
            &transport,
            &houses,
            &player,
            ToneChange::all(Tone {
                bass: 4,
                treble: -3,
                loudness: false,
            }),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CoreError::Proto(ProtoError::Network { detail, .. })
                if detail == "tone write acknowledgement lost"
        ));
        assert_eq!(transport.attempts.get(), 1);
        assert!(sim.soap_log()[before..].iter().all(|entry| {
            entry.action != "SetTreble" && entry.action != "SetLoudness"
        }));
        sim.clear_faults("Kitchen").unwrap();
        assert_eq!(
            control::tone(&sim.lan(), &houses, &player).unwrap(),
            Tone {
                bass: if apply { 4 } else { 0 },
                ..flat()
            }
        );
    }
}

#[test]
fn readback_compares_against_the_clamped_level_sent_to_the_speaker() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let (houses, player) = house(&sim, "Kitchen");
    for action in ["SetBass", "SetTreble"] {
        let transport = LoseReply {
            lan: sim.lan(),
            action,
            malformed: false,
            apply: true,
            attempts: Cell::new(0),
        };
        control::set_tone(
            &transport,
            &houses,
            &player,
            ToneChange {
                bass: Some(i8::MAX),
                treble: Some(i8::MIN),
                loudness: None,
            },
        )
        .unwrap();
        let tone = control::tone(&sim.lan(), &houses, &player).unwrap();
        assert_eq!((tone.bass, tone.treble), (10, -10));
        assert_eq!(transport.attempts.get(), 1);
    }
}
