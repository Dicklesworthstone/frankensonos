//! Room grouping against `fsonos-sim` over real loopback sockets: core's
//! `grouping` verbs send the SOAP, the simulator regroups, and a fresh
//! ZoneGroupTopology read shows the result. The households: S1 with a Den
//! stereo pair, a Kitchen and an Office; S2 with a Living Room.

use fsonos_core::grouping::{self, GroupingOutcome, Skipped};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::pair("Den", SimModel::Play5Gen1),
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

/// A fresh topology snapshot of both households.
fn snapshot(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Den", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

/// Each group as its room names, coordinator's room first.
fn groups(houses: &[HouseholdState]) -> Vec<Vec<String>> {
    houses
        .iter()
        .flat_map(|h| {
            h.groups.iter().map(move |g| {
                let mut rooms: Vec<String> = h
                    .rooms
                    .iter()
                    .filter(|r| r.coordinator == g.coordinator)
                    .map(|r| r.name.clone())
                    .collect();
                rooms.sort_by_key(|name| {
                    *name
                        != h.rooms
                            .iter()
                            .find(|r| r.players.contains(&g.coordinator))
                            .unwrap()
                            .name
                });
                rooms
            })
        })
        .collect()
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn group_and_ungroup_rooms_through_the_simulator() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot(&sim, &lan);
    assert_eq!(groups(&houses).len(), 4, "every room starts alone");

    // Group Den (a stereo pair), Office, and the other household's Living
    // Room under Kitchen; Kitchen itself is listed too.
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let rooms: Vec<_> = ["Den", "Office", "Living Room", "Kitchen", "den"]
        .iter()
        .map(|q| resolve_room(&houses, q).unwrap())
        .collect();
    let out = grouping::group(&lan, &houses, &target, &rooms);
    assert_eq!(
        out,
        GroupingOutcome {
            moved: names(&["Den", "Office"]),
            skipped: vec![
                ("Living Room".into(), Skipped::OtherHousehold),
                ("Kitchen".into(), Skipped::Target),
                ("Den".into(), Skipped::Duplicate),
            ],
            failed: Vec::new(),
        }
    );
    assert!(out.is_complete());

    // The join went to the pair's visible primary only, and nothing reached
    // the other household.
    let den = sim.player("Den").unwrap();
    let hidden = sim.players().iter().find(|p| p.hidden).unwrap();
    let log = sim.soap_log();
    let joins: Vec<&str> = log
        .iter()
        .filter(|e| e.action == "SetAVTransportURI")
        .map(|e| e.player.as_str())
        .collect();
    assert_eq!(
        joins,
        [
            den.uuid.as_str(),
            sim.player("Office").unwrap().uuid.as_str()
        ]
    );
    let mutating = log.iter().filter(|e| e.action != "GetZoneGroupState");
    assert!(mutating.clone().all(|e| e.player != hidden.uuid));
    assert!(mutating.clone().all(|e| e.room != "Living Room"));

    let houses = snapshot(&sim, &lan);
    assert_eq!(
        groups(&houses),
        [
            names(&["Kitchen", "Den", "Office"]),
            names(&["Living Room"])
        ]
    );
    let s1 = &houses[0];
    assert_eq!(
        s1.groups[0].members.len(),
        4,
        "the pair's hidden half followed"
    );

    // Grouping again changes nothing and sends nothing.
    let sent = sim.soap_log().len();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let rooms: Vec<_> = ["Den", "Office"]
        .iter()
        .map(|q| resolve_room(&houses, q).unwrap())
        .collect();
    let again = grouping::group(&lan, &houses, &target, &rooms);
    assert_eq!(again.moved.len(), 0);
    assert!(
        again
            .skipped
            .iter()
            .all(|(_, why)| *why == Skipped::AlreadyGrouped)
    );
    assert_eq!(sim.soap_log().len(), sent);

    // Kitchen leaves the group it leads; the others keep a group of their own.
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let out = grouping::ungroup(&lan, &houses, &[kitchen]);
    assert_eq!(out.moved, names(&["Kitchen"]));
    let houses = snapshot(&sim, &lan);
    assert_eq!(
        groups(&houses),
        [
            names(&["Den", "Office"]),
            names(&["Kitchen"]),
            names(&["Living Room"])
        ]
    );

    // Ungroup the rest; a room already alone is left alone.
    let rooms: Vec<_> = ["Den", "Office", "Kitchen"]
        .iter()
        .map(|q| resolve_room(&houses, q).unwrap())
        .collect();
    let out = grouping::ungroup(&lan, &houses, &rooms);
    assert_eq!(out.moved, names(&["Den", "Office"]));
    assert_eq!(
        out.skipped,
        [("Kitchen".to_string(), Skipped::AlreadyAlone)]
    );
    let houses = snapshot(&sim, &lan);
    assert_eq!(groups(&houses).len(), 4);
    let den = resolve_room(&houses, "Den").unwrap();
    assert_eq!(
        den.room.players.len(),
        2,
        "the pair stayed one room throughout"
    );
}

#[test]
fn failures_are_reported_per_room() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot(&sim, &lan);
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    let den = resolve_room(&houses, "Den").unwrap();
    // Stop the simulator: every command now fails at the network.
    drop(sim);
    let out = grouping::group(&lan, &houses, &target, &[office, den]);
    assert_eq!(out.moved.len(), 0);
    assert_eq!(out.failed.len(), 2);
    assert!(!out.is_complete());
    assert_eq!(out.failed[0].0, "Office");
}
