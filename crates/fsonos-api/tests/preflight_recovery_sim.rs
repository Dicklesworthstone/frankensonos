//! Recovery may move an unsent relative command, but never repeat a write
//! whose reply was lost. Both cases use real S1/S2 simulator sockets.

use fsonos_api::heal::execute_healing;
use fsonos_api::plan::VolumeScope;
use fsonos_api::{Command, ErrorCode, Guard, NoteCode, VolumeChange};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::IpAddr;
use std::sync::Arc;

fn house(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Target", model),
        SimPlayerSpec::new("Peer", model),
    ];
    let builder = SimHousehold::builder();
    if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap()
}

fn snapshot(sim: &SimHandle) -> Vec<HouseholdState> {
    let mut house = HouseholdState::default();
    house.apply_topology(
        &get_zone_group_state(&sim.lan(), sim.player("Peer").unwrap().ip).unwrap(),
    );
    vec![house]
}

fn guard(client: &Client) -> Guard<'_> {
    Guard {
        policy: Arc::new(Policy::default()),
        client,
        clock: &SystemClock,
    }
}

fn relative_volume(player: &PlayerId) -> Command {
    Command::Volume {
        member: player.clone(),
        target: player.clone(),
        scope: VolumeScope::Room,
        change: VolumeChange::Adjust(5),
    }
}

#[test]
fn a_relative_volume_recovers_when_only_its_policy_read_reached_the_old_address() {
    for s2 in [false, true] {
        let mut sim = house(s2);
        let houses = snapshot(&sim);
        let player = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let old = sim.player("Target").unwrap().ip;
        soap::set_volume(&sim.lan(), old, 10).unwrap();
        let new = sim.change_address("Target").unwrap();
        let surveys = Cell::new(0);
        let mark = sim.soap_log().len();
        let outcome = execute_healing(
            &sim.lan(),
            &houses,
            &guard(&Client::McpStdio),
            &relative_volume(&player),
            || {
                surveys.set(surveys.get() + 1);
                Some(snapshot(&sim))
            },
        )
        .unwrap();
        assert_eq!(surveys.get(), 1);
        assert_eq!(outcome.volume, Some(15));
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.code == NoteCode::Healed)
        );
        assert_eq!(soap::get_volume(&sim.lan(), new).unwrap(), 15);
        let log = sim.soap_log();
        let writes: Vec<_> = log[mark..]
            .iter()
            .filter(|entry| entry.action == "SetRelativeVolume")
            .map(|entry| entry.room.as_str())
            .collect();
        assert_eq!(writes, ["Target"], "{log:?}");
    }
}

/// Accept the relative volume on the real speaker, then discard its reply.
struct LostVolume {
    lan: SimLan,
    writes: Cell<usize>,
}

impl Transport for LostVolume {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, header, body)?;
        if !header.ends_with("#SetRelativeVolume\"") {
            return Ok(reply);
        }
        fsonos_proto::soap::parse_response(&reply, "SetRelativeVolume")?;
        self.writes.set(self.writes.get() + 1);
        Err(ProtoError::Network {
            target: "simulator".into(),
            detail: "accepted relative volume lost its reply".into(),
        })
    }
}

#[test]
fn an_accepted_relative_volume_with_a_lost_reply_never_starts_address_recovery() {
    for s2 in [false, true] {
        let sim = house(s2);
        let houses = snapshot(&sim);
        let player = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let host = sim.player("Target").unwrap().ip;
        soap::set_volume(&sim.lan(), host, 10).unwrap();
        let transport = LostVolume {
            lan: sim.lan(),
            writes: Cell::new(0),
        };
        let failure = execute_healing(
            &transport,
            &houses,
            &guard(&Client::Cli),
            &relative_volume(&player),
            || panic!("a relative write may have taken effect; do not survey or repeat it"),
        )
        .unwrap_err();
        assert_eq!(failure.code, ErrorCode::PlayerUnreachable);
        assert!(failure.hint.contains("not repeated"), "{failure:?}");
        assert_eq!(transport.writes.get(), 1);
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 15);
    }
}
