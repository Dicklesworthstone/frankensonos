//! Golden tests: real (scrubbed) ZoneGroupTopology and device-description
//! bodies from an S1 and an S2 household, folded into `HouseholdState` and
//! resolved the way the CLI, HTTP API and MCP tools address rooms.

use fsonos_core::{CoreError, HouseholdState, resolve_room};
use fsonos_proto::description::parse_device_description;
use fsonos_proto::{soap, topology};
use fsonos_types::{Generation, HouseholdId, PlayerId};

const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s2.xml");
const DESC_S2_PLAY1: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s2_play1.xml");
const DESC_S1_PLAY5: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s1_play5.xml");
const DESC_S1_BRIDGE: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s1_bridge.xml");

fn pid(n: u8) -> PlayerId {
    PlayerId(format!("RINCON_000E58A000{n:02X}01400"))
}

fn household(body: &str) -> HouseholdState {
    let response = soap::parse_response(body, "GetZoneGroupState").unwrap();
    let zgs =
        topology::parse_zone_group_state(response.require("ZoneGroupState").unwrap()).unwrap();
    let mut st = HouseholdState::default();
    st.apply_topology(&zgs);
    st
}

fn names(st: &HouseholdState) -> Vec<&str> {
    st.rooms.iter().map(|r| r.name.as_str()).collect()
}

#[test]
fn s1_household_rooms_pairs_and_bridges() {
    let st = household(ZGS_S1);
    // Bridges are dropped; two groups remain.
    assert_eq!(st.groups.len(), 2);
    assert_eq!(
        names(&st),
        ["Owner\u{2019}s Study", "Den", "Bedroom", "Bedroom 2"]
    );
    assert_eq!(st.players.len(), 7);
    assert!(st.players.iter().all(|p| p.generation == Generation::S1));
    assert_eq!(st.generation(), Some(Generation::S1));

    // A stereo pair is one room addressed through its visible member, playing
    // in a group another room coordinates.
    let study = &st.rooms[0];
    assert_eq!(study.primary, pid(5));
    assert_eq!(study.players, [pid(5), pid(4)]);
    assert!(!study.is_degraded());
    assert_eq!(study.coordinator, pid(2));
    assert!(!study.is_group_coordinator());
    assert!(st.rooms[1].is_group_coordinator());

    // Group-wide commands for the invisible half of the pair reach the
    // group's coordinator.
    assert_eq!(st.coordinator_of(&pid(4)), Some(&pid(2)));
    assert_eq!(st.groups[0].members.len(), 4);

    assert_eq!(st.rooms[3].players, [pid(8)]);
    assert_eq!(st.rooms[3].coordinator, pid(3));
}

#[test]
fn s2_household_degraded_pair_stays_addressable() {
    let st = household(ZGS_S2);
    assert_eq!(
        names(&st),
        ["Lounge", "Guest\u{2019}s Room", "Parlor", "Kitchen"]
    );
    assert_eq!(st.players.len(), 5);
    assert_eq!(st.generation(), Some(Generation::S2));

    // The Lounge pair's visible LF and its sub are offline. The room survives,
    // addressed through the invisible RF that now coordinates it.
    let lounge = &st.rooms[0];
    assert_eq!(lounge.primary, pid(0x0A));
    assert_eq!(lounge.players, [pid(0x0A)]);
    assert_eq!(lounge.missing, [pid(0x0B), pid(0x0C)]);
    assert!(lounge.is_degraded() && lounge.is_group_coordinator());

    let kitchen = &st.rooms[3];
    assert_eq!(kitchen.primary, pid(0x0F));
    assert_eq!(kitchen.players, [pid(0x0F), pid(0x10)]);
    assert!(!kitchen.is_degraded());
}

#[test]
fn rooms_resolve_across_both_households() {
    let houses = [household(ZGS_S1), household(ZGS_S2)];

    // Straight apostrophe and any case match the curly-apostrophe room name.
    let t = resolve_room(&houses, "owner's study").unwrap();
    assert_eq!(t.room.name, "Owner\u{2019}s Study");
    assert_eq!(t.player.id, pid(5));
    assert_eq!(
        t.player.ip,
        "192.0.2.12".parse::<std::net::IpAddr>().unwrap()
    );
    assert_eq!(t.coordinator.id, pid(2));
    assert_eq!(t.household.generation(), Some(Generation::S1));

    let t = resolve_room(&houses, "  LOUNGE ").unwrap();
    assert!(t.room.is_degraded());
    assert_eq!(
        (t.player.id.clone(), t.coordinator.id.clone()),
        (pid(0x0A), pid(0x0A))
    );

    // A player id (any case) resolves to its room, even for a hidden player.
    let t = resolve_room(&houses, "rincon_000e58a0000401400").unwrap();
    assert_eq!(t.room.name, "Owner\u{2019}s Study");
    assert_eq!(t.player.id, pid(5));

    // Household-qualified names work even when unambiguous.
    assert_eq!(
        resolve_room(&houses, "Kitchen@s2").unwrap().player.id,
        pid(0x0F)
    );
    assert!(matches!(
        resolve_room(&houses, "Kitchen@S1"),
        Err(CoreError::UnknownRoom { .. })
    ));

    // Unknown names list every room, qualified, so an agent can retry.
    let err = resolve_room(&houses, "Garage").unwrap_err();
    let CoreError::UnknownRoom { name, known } = &err else {
        panic!("expected UnknownRoom, got {err:?}");
    };
    assert_eq!(name, "Garage");
    assert_eq!(known.len(), 8);
    assert!(known.contains(&"Den@S1".to_string()));
    assert!(known.contains(&"Guest\u{2019}s Room@S2".to_string()));
    assert!(
        err.to_string()
            .starts_with("unknown room \"Garage\"; known rooms: Owner")
    );
}

#[test]
fn ambiguous_names_offer_qualified_candidates() {
    // Two households of the same generation: labels fall back to the ids.
    let mut a = household(ZGS_S1);
    a.id = Some(HouseholdId("HH_UPSTAIRS".into()));
    let mut b = household(ZGS_S1);
    b.id = Some(HouseholdId("HH_DOWNSTAIRS".into()));
    let houses = [a, b];
    let err = resolve_room(&houses, "den").unwrap_err();
    let CoreError::AmbiguousRoom { candidates, .. } = &err else {
        panic!("expected AmbiguousRoom, got {err:?}");
    };
    assert_eq!(candidates, &["Den@HH_UPSTAIRS", "Den@HH_DOWNSTAIRS"]);
    assert_eq!(
        err.to_string(),
        "room \"den\" is ambiguous; use one of: Den@HH_UPSTAIRS, Den@HH_DOWNSTAIRS"
    );
    // Each candidate resolves.
    for c in candidates {
        resolve_room(&houses, c).unwrap();
    }
    let t = resolve_room(&houses, "Den@hh_downstairs").unwrap();
    assert_eq!(t.household.id, Some(HouseholdId("HH_DOWNSTAIRS".into())));

    // No ids and the same generation: households fall back to index labels,
    // which still make every candidate retryable.
    let houses = [household(ZGS_S1), household(ZGS_S1)];
    let err = resolve_room(&houses, "Bedroom 2").unwrap_err();
    let CoreError::AmbiguousRoom { candidates, .. } = err else {
        panic!("expected AmbiguousRoom");
    };
    assert_eq!(candidates, ["Bedroom 2@#0", "Bedroom 2@#1"]);
    let t = resolve_room(&houses, "Bedroom 2@#1").unwrap();
    assert!(std::ptr::eq(t.household, &raw const houses[1]));
}

#[test]
fn descriptions_fill_models_and_survive_topology_updates() {
    let mut s2 = household(ZGS_S2);
    let lounge_ip = s2.player(&pid(0x0A)).unwrap().ip;
    assert_eq!(s2.player(&pid(0x0A)).unwrap().model, "");

    let play1 = parse_device_description(DESC_S2_PLAY1).unwrap();
    assert!(s2.apply_description(&play1, lounge_ip));
    assert!(!s2.apply_description(&play1, lounge_ip), "idempotent");
    let p = s2.player(&pid(0x0A)).unwrap();
    assert_eq!(
        (p.model.as_str(), p.generation),
        ("Sonos Play:1", Generation::S2)
    );

    // A later topology event keeps what the description taught us.
    let response = soap::parse_response(ZGS_S2, "GetZoneGroupState").unwrap();
    let zgs =
        topology::parse_zone_group_state(response.require("ZoneGroupState").unwrap()).unwrap();
    s2.apply_topology(&zgs);
    assert_eq!(s2.player(&pid(0x0A)).unwrap().model, "Sonos Play:1");

    // S1: a Play:5 Gen 1 is S1 by swGen; the Bridge is not a renderer.
    let mut s1 = household(ZGS_S1);
    let play5 = parse_device_description(DESC_S1_PLAY5).unwrap();
    assert!(s1.apply_description(&play5, "192.0.2.11".parse().unwrap()));
    assert_eq!(s1.player(&pid(4)).unwrap().model, "Sonos Play:5");
    let bridge = parse_device_description(DESC_S1_BRIDGE).unwrap();
    assert!(!s1.apply_description(&bridge, "192.0.2.10".parse().unwrap()));
    assert!(s1.player(&pid(1)).is_none());
}
