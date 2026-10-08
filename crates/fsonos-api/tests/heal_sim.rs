//! A command whose speakers changed under it heals once, through
//! `Surface::control`, against `fsonos-sim` players over real sockets:
//!
//! * a group command sent to a coordinator that just handed its group over
//!   (UPnP 800) is retried on the new coordinator;
//! * a command that is safe to repeat, sent to a player that moved to a new
//!   address (a DHCP lease change), is retried there;
//! * a command that is not safe to repeat (`next`) is not retried.
//!
//! The surface's cached survey is what makes each change "under" the
//! command: it plans against the speakers as they were.

use fsonos_api::plan::{plan_mute, plan_transport, plan_volume};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, MuteRequest, NoteCode, VolumeRequest, ZoneRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control::{get_group_volume, get_mute};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::time::{Duration, Instant};

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

/// A surface that surveys the sim over SSDP (its unicast responder).
fn surface(sim: &SimHandle) -> Surface {
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

fn healed(notes: &[fsonos_api::Note]) -> Option<&str> {
    notes
        .iter()
        .find(|n| n.code == NoteCode::Healed)
        .map(|n| n.detail.as_str())
}

#[test]
fn a_group_command_follows_a_new_coordinator() {
    let sim = sim();
    let lan = sim.lan();
    let houses = fsonos_core::inventory::survey(&lan, &[], Duration::from_millis(500))
        .unwrap()
        .households;
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    control::join(&lan, &houses, &office, &kitchen).unwrap();
    let kitchen_ip = sim.player("Kitchen").unwrap().ip;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !get_zone_group_state(&lan, kitchen_ip).is_ok_and(|z| {
        z.groups
            .iter()
            .any(|g| g.coordinator == kitchen && g.members.len() == 2)
    }) {
        assert!(Instant::now() < deadline, "Office never joined Kitchen");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A surface that caches the grouped house (Kitchen leads); then the
    // group is handed over before the command goes out.
    let surface = surface(&sim);
    let grouped = surface.households().unwrap();
    assert_eq!(
        resolve_room(&grouped, "Office").unwrap().coordinator.id,
        kitchen
    );
    sim.reelect_coordinator("Kitchen").unwrap();

    let req = VolumeRequest {
        zone: "Office".into(),
        volume: Some(31),
        delta: None,
        group: true,
    };
    let out = surface
        .control(&Client::Cli, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
    let note = healed(&out.notes).expect("a HEALED note");
    assert!(note.contains("coordinator"), "{note}");
    let office_ip = sim.player("Office").unwrap().ip;
    assert_eq!(get_group_volume(&lan, office_ip).unwrap(), 31);
}

#[test]
fn a_repeat_safe_command_follows_a_moved_player() {
    let mut sim = sim();
    let surface = surface(&sim);
    let _ = surface.households().unwrap();
    let to = sim.change_address("Office").unwrap();

    let req = MuteRequest {
        zone: "Office".into(),
        mute: true,
    };
    let out = surface
        .control(&Client::Cli, "mute", |h| plan_mute(h, &req))
        .unwrap();
    let note = healed(&out.notes).expect("a HEALED note");
    assert!(note.contains(&to.to_string()), "{note}");
    assert!(get_mute(&sim.lan(), to).unwrap());
}

#[test]
fn a_step_is_never_repeated_at_a_new_address() {
    let mut sim = sim();
    let surface = surface(&sim);
    let _ = surface.households().unwrap();
    let to = sim.change_address("Office").unwrap();

    let req = ZoneRequest {
        zone: "Office".into(),
    };
    let err = surface
        .control(&Client::Cli, "next", |h| {
            plan_transport(h, &req, fsonos_api::plan::TransportAction::Next)
        })
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PlayerUnreachable, "{err:?}");
    let sent_next = sim.soap_log().iter().any(|e| e.action == "Next");
    assert!(!sent_next, "Next must not be resent to {to}");
    // The failure made the surface look again: a retry by the caller works.
    let out = surface
        .control(&Client::Cli, "next", |h| {
            plan_transport(h, &req, fsonos_api::plan::TransportAction::Next)
        })
        .map(|o| o.changed);
    assert!(
        !matches!(&out, Err(e) if e.code == ErrorCode::PlayerUnreachable),
        "{out:?}"
    );
}
