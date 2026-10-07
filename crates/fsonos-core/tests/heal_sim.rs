//! Self-healing against `fsonos-sim` faults over real sockets: a player that
//! moved is re-resolved and the call retried; an offline player fails
//! cleanly; a coordinator change is followed; and a UPnP 800 that is not a
//! coordinator change is not retried.

use fsonos_core::heal::{self, Recovery};
use fsonos_core::{CoreError, HouseholdState, control, grouping, resolve_room};
use fsonos_proto::ProtoError;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

fn model(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let mut st = HouseholdState::default();
    st.apply_topology(&get_zone_group_state(lan, sim.player("Kitchen").unwrap().ip).unwrap());
    vec![st]
}

#[test]
fn a_player_that_moved_is_found_again_and_the_call_retried() {
    let mut sim = sim();
    let lan = sim.lan();
    let mut houses = model(&sim, &lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let old = sim.player("Kitchen").unwrap().ip;

    let new = sim.change_address("Kitchen").unwrap();
    assert_ne!(old, new);
    let (level, recovery) = heal::readdressing(&lan, &[], &mut houses, &kitchen, |hs| {
        control::set_volume(&lan, hs, &kitchen, 21)
    })
    .unwrap();
    assert_eq!(level, 21);
    assert_eq!(
        recovery,
        Recovery::Readdressed {
            player: kitchen.clone(),
            from: old,
            to: new
        }
    );
    assert_eq!(
        control::volume(&lan, &houses, &kitchen).unwrap(),
        21,
        "the model now has the new address"
    );
}

#[test]
fn an_offline_player_fails_cleanly() {
    let sim = sim();
    let lan = sim.lan();
    let mut houses = model(&sim, &lan);
    let office = resolve_room(&houses, "Office").unwrap().player.id.clone();
    sim.set_offline("Office", true).unwrap();
    let err = heal::readdressing(&lan, &[], &mut houses, &office, |hs| {
        control::set_volume(&lan, hs, &office, 10)
    })
    .unwrap_err();
    assert!(heal::is_unreachable(&err), "{err:?}");
}

#[test]
fn a_coordinator_change_is_followed_once() {
    let sim = sim();
    let lan = sim.lan();
    let houses = model(&sim, &lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    assert!(grouping::group(&lan, &houses, &kitchen, &[office]).is_complete());
    let mut houses = model(&sim, &lan);
    let member = resolve_room(&houses, "Office").unwrap().player.id.clone();
    let before = houses[0].coordinator_of(&member).unwrap().clone();

    // The group re-elects behind the model's back; the stale coordinator
    // now answers group commands with 800. A group-volume change follows the
    // new coordinator.
    sim.reelect_coordinator("Kitchen").unwrap();
    let (level, recovery) = heal::on_coordinator(&lan, &mut houses, &member, |hs, coordinator| {
        control::set_group_volume(&lan, hs, coordinator, 20)
    })
    .unwrap();
    assert_eq!(level, 20);
    let Recovery::CoordinatorMoved { from, to } = recovery else {
        panic!("expected a coordinator change, got {recovery:?}");
    };
    assert_eq!(from, before);
    assert_ne!(to, before);
    assert_eq!(
        houses[0].coordinator_of(&member),
        Some(&to),
        "the model caught up"
    );
    let host = control::locate(&houses, &to).unwrap().ip;
    assert_eq!(
        fsonos_proto::control::get_group_volume(&lan, host).unwrap(),
        20
    );
}

#[test]
fn an_800_that_is_not_a_coordinator_change_is_not_retried() {
    let sim = sim();
    let lan = sim.lan();
    let mut houses = model(&sim, &lan);
    let member = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    // A Spotify URI with a wrong account descriptor: the player refuses it
    // with 800, and the coordinator has not moved.
    let bad_didl = "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
                    xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
                    xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
                    <item id=\"10032020spotify%3atrack%3ax\" parentID=\"-1\" restricted=\"true\">\
                    <dc:title>x</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>\
                    <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">\
                    SA_RINCON1_X_#Svc1-0-Token</desc></item></DIDL-Lite>";
    let calls = std::cell::Cell::new(0);
    let err = heal::on_coordinator(&lan, &mut houses, &member, |hs, coordinator| {
        calls.set(calls.get() + 1);
        control::play_uri(
            &lan,
            hs,
            coordinator,
            "x-sonos-spotify:spotify%3atrack%3ax?sid=1&flags=8224&sn=1",
            bad_didl,
        )
    })
    .unwrap_err();
    assert!(
        matches!(
            err,
            CoreError::Proto(ProtoError::SoapFault { code: 800, .. })
        ),
        "{err:?}"
    );
    assert_eq!(calls.get(), 1, "the same coordinator is not asked twice");
}
