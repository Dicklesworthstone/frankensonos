//! Room grouping against `fsonos-sim` over real loopback sockets: core's
//! `grouping` verbs send the SOAP, the simulator regroups, and a fresh
//! ZoneGroupTopology read shows the result. The households: S1 with a Den
//! stereo pair, a Kitchen and an Office; S2 with a Living Room.

use fsonos_core::grouping::{self, Skipped};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

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

/// Whether `room`'s player asked to hand its group on: how a coordinator
/// leaves, since its self-join changes nothing on real players.
fn delegated(sim: &SimHandle, room: &str) -> bool {
    let uuid = &sim.player(room).unwrap().uuid;
    sim.soap_log()
        .iter()
        .any(|e| e.action == "DelegateGroupCoordinationTo" && &e.player == uuid)
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
    assert_eq!(out.moved, names(&["Den", "Office"]));
    assert_eq!(
        out.skipped,
        [
            ("Living Room".into(), Skipped::OtherHousehold),
            ("Kitchen".into(), Skipped::Target),
            ("Den".into(), Skipped::Duplicate),
        ]
    );
    assert!(out.failed.is_empty(), "{:?}", out.failed);
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

    // Kitchen leaves the group it leads; the others keep a group of their
    // own. A coordinator's self-join changes nothing on real players, so it
    // hands the group on.
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let out = grouping::ungroup(&lan, &houses, &[kitchen]);
    assert_eq!(out.moved, names(&["Kitchen"]));
    assert!(delegated(&sim, "Kitchen"));
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

fn playing_group(generation: u8) -> (SimHandle, Vec<HouseholdState>) {
    let model = if generation == 1 {
        SimModel::Play5Gen1
    } else {
        SimModel::One
    };
    let players = [
        SimPlayerSpec::pair("Leader", model),
        SimPlayerSpec::new("First", model),
        SimPlayerSpec::new("Remaining", model),
    ];
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1(players)
    } else {
        builder.s2(players)
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let leader = sim.player("Leader").unwrap();
    let id = PlayerId(leader.uuid.clone());
    for room in ["First", "Remaining"] {
        soap::join_group(&lan, sim.player(room).unwrap().ip, &id).unwrap();
    }
    for n in 1..=3 {
        soap::add_uri_to_queue(
            &lan,
            leader.ip,
            &format!("x-file-cifs://example.invalid/track-{n}.flac"),
            "",
            false,
        )
        .unwrap();
    }
    soap::play_from_queue(&lan, leader.ip, &id).unwrap();
    soap::seek_track(&lan, leader.ip, 2).unwrap();
    soap::seek_position(&lan, leader.ip, 37).unwrap();
    soap::play(&lan, leader.ip).unwrap();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, leader.ip).unwrap());
    (sim, vec![house])
}

#[test]
fn ungrouping_a_subset_uses_current_heirs_and_keeps_the_remaining_music() {
    for generation in [1, 2] {
        // The surface plans members first; the core set verb also accepts
        // leader-first input. Both share one original planning snapshot.
        for order in [["First", "Leader"], ["Leader", "First"]] {
            let (sim, houses) = playing_group(generation);
            let lan = sim.lan();
            let rooms: Vec<_> = order
                .iter()
                .map(|name| resolve_room(&houses, name).unwrap())
                .collect();
            let since = sim.soap_log().len();
            let out = grouping::ungroup(&lan, &houses, &rooms);
            assert!(
                out.is_complete(),
                "S{generation} {order:?}: {:?}",
                out.failed
            );
            assert_eq!(out.moved, names(&order));

            let remaining = sim.player("Remaining").unwrap();
            let mut after = HouseholdState::default();
            after.apply_topology(&get_zone_group_state(&lan, remaining.ip).unwrap());
            assert_eq!(after.groups.len(), 3, "each requested room left");
            assert!(
                after
                    .rooms
                    .iter()
                    .all(|room| room.coordinator == room.primary)
            );
            assert_eq!(
                after
                    .rooms
                    .iter()
                    .find(|r| r.name == "Leader")
                    .unwrap()
                    .players
                    .len(),
                2
            );
            let position = soap::get_position_info(&lan, remaining.ip).unwrap();
            assert_eq!((position.track, position.position_secs), (2, Some(37)));
            assert_eq!(position.uri, "x-file-cifs://example.invalid/track-2.flac");
            assert_eq!(soap::get_media_info(&lan, remaining.ip).unwrap().tracks, 3);
            assert_eq!(
                soap::get_transport_info(&lan, remaining.ip).unwrap().state,
                TransportState::Playing,
            );
            let log = sim.soap_log();
            let writes: Vec<_> = log[since..]
                .iter()
                .filter(|entry| !entry.action.starts_with("Get"))
                .map(|entry| entry.action.as_str())
                .collect();
            assert_eq!(
                writes,
                if order[0] == "First" {
                    vec!["SetAVTransportURI", "DelegateGroupCoordinationTo"]
                } else {
                    vec!["DelegateGroupCoordinationTo", "DelegateGroupCoordinationTo"]
                },
                "no source restart, queue rewrite or refused delegation",
            );
        }
    }
}

struct UnreadableTopology(SimLan);

impl Transport for UnreadableTopology {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#GetZoneGroupState\"") {
            return Err(ProtoError::Network {
                target: "simulator".into(),
                detail: "topology unavailable".into(),
            });
        }
        self.0.soap_post(host, path, action, body)
    }
}

#[test]
fn an_unreadable_current_layout_never_uses_a_stale_heir() {
    let (sim, houses) = playing_group(2);
    let leader = resolve_room(&houses, "Leader").unwrap();
    let since = sim.soap_log().len();
    assert!(control::leave(&UnreadableTopology(sim.lan()), &houses, &leader.player.id).is_err(),);
    assert_eq!(sim.soap_log().len(), since, "no writes reached a player");
    assert_eq!(
        soap::get_transport_info(&sim.lan(), leader.player.ip)
            .unwrap()
            .state,
        TransportState::Playing,
    );
}
