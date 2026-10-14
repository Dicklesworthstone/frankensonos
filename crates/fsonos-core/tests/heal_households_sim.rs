//! A command recovery must not turn a partial LAN survey into loss of the
//! other household. All player calls use the simulator's real sockets.

use fsonos_core::heal::{self, Recovery};
use fsonos_core::inventory::survey;
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::ProtoError;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::cell::Cell;
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

fn model(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let seeds: Vec<_> = sim.players().iter().map(|p| p.ip).collect();
    survey(lan, &seeds, Duration::ZERO).unwrap().households
}

#[test]
fn readdressing_keeps_a_household_that_did_not_answer_the_survey() {
    let mut sim = sim();
    let lan = sim.lan();
    let mut houses = model(&sim, &lan);
    assert_eq!(houses.len(), 2);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let living = resolve_room(&houses, "Living Room")
        .unwrap()
        .player
        .id
        .clone();
    let untouched = houses
        .iter()
        .find(|h| h.player(&living).is_some())
        .unwrap()
        .clone();
    let old = control::locate(&houses, &kitchen).unwrap().ip;
    sim.set_offline("Living Room", true).unwrap();
    let new = sim.change_address("Kitchen").unwrap();

    let attempts = Cell::new(0);
    let (level, recovery) = heal::readdressing(&lan, &[], &mut houses, &kitchen, |hs| {
        attempts.set(attempts.get() + 1);
        control::set_volume(&lan, hs, &kitchen, 21)
    })
    .unwrap();

    assert_eq!(attempts.get(), 2);
    assert_eq!(level, 21);
    assert_eq!(
        recovery,
        Recovery::Readdressed {
            player: kitchen.clone(),
            from: old,
            to: new,
        }
    );
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 21);
    assert_eq!(houses.len(), 2);
    assert!(
        houses.contains(&untouched),
        "unrelated state must be untouched"
    );
    assert!(resolve_room(&houses, "Living Room").is_ok());
}

#[test]
fn a_failed_retry_keeps_the_new_address_without_erasing_other_households() {
    let mut sim = sim();
    let lan = sim.lan();
    let mut houses = model(&sim, &lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let living = resolve_room(&houses, "Living Room")
        .unwrap()
        .player
        .id
        .clone();
    let untouched = houses
        .iter()
        .find(|h| h.player(&living).is_some())
        .unwrap()
        .clone();
    sim.set_offline("Living Room", true).unwrap();
    let new = sim.change_address("Kitchen").unwrap();
    let attempts = Cell::new(0);

    let error = heal::readdressing(&lan, &[], &mut houses, &kitchen, |hs| {
        attempts.set(attempts.get() + 1);
        if attempts.get() == 2 {
            // A second failure is reported, never followed by a third write.
            return Err(CoreError::Proto(ProtoError::SoapFault {
                code: 701,
                reason: "retry refused".into(),
            }));
        }
        control::set_volume(&lan, hs, &kitchen, 21)
    })
    .unwrap_err();

    assert!(matches!(
        error,
        CoreError::Proto(ProtoError::SoapFault { code: 701, .. })
    ));
    assert_eq!(attempts.get(), 2);
    assert_eq!(control::locate(&houses, &kitchen).unwrap().ip, new);
    assert!(houses.contains(&untouched));
}

#[test]
fn an_unresolved_player_leaves_every_household_unchanged() {
    let sim = sim();
    let lan = sim.lan();
    let mut houses = model(&sim, &lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let before = houses.clone();
    sim.set_offline("Kitchen", true).unwrap();
    let attempts = Cell::new(0);

    let error = heal::readdressing(&lan, &[], &mut houses, &kitchen, |hs| {
        attempts.set(attempts.get() + 1);
        control::set_volume(&lan, hs, &kitchen, 21)
    })
    .unwrap_err();

    assert!(heal::is_unreachable(&error));
    assert_eq!(attempts.get(), 1);
    assert_eq!(houses, before);
}
