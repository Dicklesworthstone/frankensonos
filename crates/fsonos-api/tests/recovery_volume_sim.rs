//! Real simulator writes lose one acknowledgement while their address changes.
//! Recovery must preserve the first policy allowance, not grant a second step.

use fsonos_api::heal::execute_healing;
use fsonos_api::plan::VolumeScope;
use fsonos_api::{Command, Guard, NoteCode, VolumeChange};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control::{get_group_volume, get_volume, set_volume};
use fsonos_proto::soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

fn survey(lan: &SimLan) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(lan, &[], Duration::from_millis(500))
        .unwrap()
        .households
}

struct Relocating<'a> {
    sim: RefCell<&'a mut SimHandle>,
    lan: SimLan,
    action: &'static str,
    lost: Cell<bool>,
    lower_after_move: bool,
    bodies: RefCell<Vec<String>>,
}

impl Transport for Relocating<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        if action == self.action {
            self.bodies.borrow_mut().push(body.into());
        }
        let reply = self.lan.soap_post(host, path, header, body)?;
        if action == self.action
            && soap::parse_response(&reply, action).is_ok()
            && !self.lost.replace(true)
        {
            let new = self.sim.borrow_mut().change_address("Room").unwrap();
            if self.lower_after_move {
                // Another controller lowers the room between attempts. The
                // retry must enforce max_step against that new lower level.
                set_volume(&self.lan, new, 0).unwrap();
            }
            return Err(ProtoError::Network {
                target: "synthetic-player".into(),
                detail: "accepted volume, then lost acknowledgement during relocation".into(),
            });
        }
        Ok(reply)
    }
}

fn scenario(s2: bool, group: bool, client: &Client, lower_after_move: bool) {
    let builder = SimHousehold::builder();
    let mut sim = if s2 {
        builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
    } else {
        builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let houses = survey(&lan);
    let target = resolve_room(&houses, "Room").unwrap().player.id.clone();
    set_volume(&lan, sim.player("Room").unwrap().ip, 20).unwrap();
    let t = Relocating {
        sim: RefCell::new(&mut sim),
        lan,
        action: if group { "SetGroupVolume" } else { "SetVolume" },
        lost: Cell::new(false),
        lower_after_move,
        bodies: RefCell::new(Vec::new()),
    };
    let mut policy = Policy::default();
    policy.defaults.max_step = 5;
    let guard = Guard {
        policy: Arc::new(policy),
        client,
        clock: &SystemClock,
    };
    let command = Command::Volume {
        target,
        scope: if group {
            VolumeScope::Group
        } else {
            VolumeScope::Room
        },
        change: VolumeChange::Set(80),
    };
    let surveys = Cell::new(0);
    let outcome = execute_healing(&t, &houses, &guard, &command, || {
        surveys.set(surveys.get() + 1);
        Some(survey(&t.lan))
    })
    .unwrap();
    let capped = !matches!(client, Client::Cli);
    let first = if capped { 25 } else { 80 };
    let final_level = if capped && lower_after_move {
        5
    } else {
        first
    };
    assert_eq!(surveys.get(), 1);
    assert!(t.lost.get());
    assert_eq!(outcome.volume, Some(final_level));
    assert!(outcome.notes.iter().any(|note| note.code == NoteCode::Healed));
    assert_eq!(
        outcome
            .notes
            .iter()
            .any(|note| note.code == NoteCode::VolumeClamped),
        capped,
        "the first clamp remains visible after recovery: {outcome:?}"
    );
    let bodies = t.bodies.borrow();
    assert_eq!(bodies.len(), 2, "one retry only");
    assert!(bodies[0].contains(&format!("<DesiredVolume>{first}</DesiredVolume>")));
    assert!(bodies[1].contains(&format!("<DesiredVolume>{final_level}</DesiredVolume>")));
    let new = t.sim.borrow().player("Room").unwrap().ip;
    let read = if group {
        get_group_volume(&t.lan, new)
    } else {
        get_volume(&t.lan, new)
    };
    assert_eq!(read.unwrap(), final_level);
}

#[test]
fn room_relocation_does_not_spend_a_second_volume_step_on_s1_or_s2() {
    for s2 in [false, true] {
        scenario(s2, false, &Client::McpStdio, false);
    }
}

#[test]
fn group_relocation_preserves_the_first_allowance_and_clamp_note() {
    for s2 in [false, true] {
        scenario(s2, true, &Client::LoopbackHttp, false);
    }
}

#[test]
fn a_lower_level_between_attempts_is_still_bounded_by_the_live_policy() {
    for group in [false, true] {
        scenario(true, group, &Client::McpStdio, true);
    }
}

#[test]
fn uncapped_cli_recovery_remains_uncapped() {
    scenario(true, false, &Client::Cli, false);
}
