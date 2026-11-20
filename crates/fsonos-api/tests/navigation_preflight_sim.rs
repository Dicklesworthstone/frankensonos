//! A read-only preflight may recover an unsent navigation request at the
//! player's new address. A reused address must leave its new owner unchanged.

use fsonos_api::heal::execute_healing;
use fsonos_api::plan::TransportAction;
use fsonos_api::{Command, Guard, NoteCode};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

const TRACKS: [&str; 4] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/last.flac",
];

fn house(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = ["Target", "Peer", "Other"].map(|room| SimPlayerSpec::new(room, model));
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
    house
        .apply_topology(&get_zone_group_state(&sim.lan(), sim.player("Peer").unwrap().ip).unwrap());
    vec![house]
}

fn prepare(sim: &SimHandle, houses: &[HouseholdState], room: &str, track: u32, seconds: u32) {
    let player = sim.player(room).unwrap();
    let lan = sim.lan();
    for uri in TRACKS {
        soap::add_uri_to_queue(&lan, player.ip, uri, "", false).unwrap();
    }
    control::play_queue_from(&lan, houses, &PlayerId(player.uuid.clone()), track).unwrap();
    soap::seek_position(&lan, player.ip, seconds).unwrap();
}

#[test]
fn unsent_navigation_follows_the_original_player_on_s1_and_s2() {
    let cases = [
        (TransportAction::Next, "Next", 2, 3, 0),
        (TransportAction::Previous, "Previous", 3, 2, 0),
        (
            TransportAction::Seek(control::Seek::By(30)),
            "Seek",
            2,
            2,
            100,
        ),
        (
            TransportAction::Seek(control::Seek::At(100)),
            "Seek",
            2,
            2,
            100,
        ),
    ];
    for s2 in [false, true] {
        for reused in [false, true] {
            for (action, expected_write, before, after, seconds) in cases {
                let mut sim = house(s2);
                let houses = snapshot(&sim);
                prepare(&sim, &houses, "Target", before, 70);
                prepare(&sim, &houses, "Other", 4, 45);
                let player = PlayerId(sim.player("Target").unwrap().uuid.clone());
                let old = sim.player("Target").unwrap().ip;
                let new = sim.change_address("Target").unwrap();
                if reused {
                    let IpAddr::V4(address) = old else {
                        panic!("the simulator uses IPv4");
                    };
                    sim.move_to("Other", address, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                        .unwrap();
                }
                let command = Command::Transport {
                    member: player.clone(),
                    coordinator: player,
                    action,
                };
                let guard = Guard {
                    policy: Arc::new(Policy::default()),
                    client: &Client::Cli,
                    clock: &SystemClock,
                };
                let surveys = Cell::new(0);
                let mark = sim.soap_log().len();
                let outcome = execute_healing(&sim.lan(), &houses, &guard, &command, || {
                    surveys.set(surveys.get() + 1);
                    Some(snapshot(&sim))
                })
                .unwrap();
                assert_eq!(surveys.get(), 1, "{action:?}, reused={reused}");
                assert!(
                    outcome
                        .notes
                        .iter()
                        .any(|note| note.code == NoteCode::Healed)
                );
                let position = soap::get_position_info(&sim.lan(), new).unwrap();
                assert_eq!(position.track, after, "{action:?}, reused={reused}");
                assert_eq!(position.position_secs, Some(seconds));
                let other = sim.player("Other").unwrap().ip;
                let unchanged = soap::get_position_info(&sim.lan(), other).unwrap();
                assert_eq!(
                    unchanged.track, 4,
                    "the unrelated player's queue is untouched"
                );
                assert_eq!(unchanged.position_secs, Some(45));
                assert_eq!(
                    soap::get_transport_info(&sim.lan(), other).unwrap().state,
                    TransportState::Playing
                );
                let log = sim.soap_log();
                let writes: Vec<_> = log[mark..]
                    .iter()
                    .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
                    .map(|entry| (entry.room.as_str(), entry.action.as_str()))
                    .collect();
                assert_eq!(
                    writes,
                    [("Target", expected_write)],
                    "{action:?}, reused={reused}: {log:?}"
                );
            }
        }
    }
}
