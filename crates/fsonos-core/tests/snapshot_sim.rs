//! Zone snapshot round trips against `fsonos-sim` over real loopback sockets,
//! in both generations: capture, change everything, restore, compare.

use fsonos_core::snapshot::{self, Aspect, SnapshotSource};
use fsonos_core::{HouseholdState, control, grouping, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Living Room", SimModel::One),
            SimPlayerSpec::new("Bedroom", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn snapshot_of(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

/// A two-room group led by `lead` playing its queue at track 2, 40 s in,
/// with distinct member levels; returns the coordinator.
fn set_scene(sim: &SimHandle, lan: &SimLan, lead: &str, member: &str) -> PlayerId {
    let houses = snapshot_of(sim, lan);
    let target = resolve_room(&houses, lead).unwrap();
    let other = resolve_room(&houses, member).unwrap();
    assert!(grouping::group(lan, &houses, &target, &[other]).is_complete());
    let houses = snapshot_of(sim, lan);
    let coord = id(&houses, lead);
    control::queue_spotify_tracks(
        lan,
        &houses,
        &coord,
        &[
            ("spotify:track:0SimSnapA0000000000001", "First"),
            ("spotify:track:0SimSnapB0000000000002", "Second"),
        ],
    )
    .unwrap()
    .unwrap();
    control::play_queue_from(lan, &houses, &coord, 2).unwrap();
    sim.clock().advance(Duration::from_secs(40));
    control::set_volume(lan, &houses, &coord, 25).unwrap();
    control::set_volume(lan, &houses, &id(&houses, member), 35).unwrap();
    control::set_mute(lan, &houses, &id(&houses, member), true).unwrap();
    coord
}

/// Change everything a snapshot covers.
fn disturb(sim: &SimHandle, lan: &SimLan, lead: &str, member: &str) {
    let houses = snapshot_of(sim, lan);
    let m = resolve_room(&houses, member).unwrap();
    assert!(grouping::ungroup(lan, &houses, &[m]).is_complete());
    let houses = snapshot_of(sim, lan);
    let coord = id(&houses, lead);
    control::play_uri(
        lan,
        &houses,
        &coord,
        "x-rincon-mp3radio://stream.example.invalid/other.mp3",
        "",
    )
    .unwrap();
    control::set_volume(lan, &houses, &coord, 5).unwrap();
    control::set_volume(lan, &houses, &id(&houses, member), 90).unwrap();
    control::set_mute(lan, &houses, &id(&houses, member), false).unwrap();
}

#[test]
fn round_trip_in_both_generations() {
    let sim = sim();
    let lan = sim.lan();
    for (lead, member) in [("Kitchen", "Office"), ("Living Room", "Bedroom")] {
        let coord = set_scene(&sim, &lan, lead, member);
        let houses = snapshot_of(&sim, &lan);
        let snap = snapshot::capture(&lan, &houses, &coord, 1_000).unwrap();
        assert_eq!(snap.members.len(), 2);
        assert_eq!(snap.transport_state, TransportState::Playing);
        assert!(
            matches!(
                snap.source,
                SnapshotSource::Queue {
                    track: 2,
                    position_secs: 40,
                    ..
                }
            ),
            "{:?}",
            snap.source
        );
        assert_eq!(
            snap.levels
                .iter()
                .map(|l| (l.volume, l.mute))
                .collect::<Vec<_>>(),
            [(25, false), (35, true)]
        );

        disturb(&sim, &lan, lead, member);
        let houses = snapshot_of(&sim, &lan);
        assert_ne!(
            resolve_room(&houses, member).unwrap().coordinator.id,
            coord,
            "ungrouped"
        );

        let report = snapshot::restore(&lan, &houses, &snap).unwrap();
        assert_eq!(report.skipped.len(), 0, "{report:?}");
        assert_eq!(
            report.restored,
            [
                Aspect::Group,
                Aspect::Source,
                Aspect::Position,
                Aspect::Volume,
                Aspect::Mute,
                Aspect::Transport
            ]
        );

        // Compare what the players now report with the snapshot.
        let houses = snapshot_of(&sim, &lan);
        let lead_room = resolve_room(&houses, lead).unwrap();
        let member_room = resolve_room(&houses, member).unwrap();
        assert_eq!(member_room.coordinator.id, coord, "regrouped");
        let now = control::playback(&lan, &houses, &coord).unwrap();
        assert_eq!(now.transport.state, TransportState::Playing);
        assert_eq!(now.position.track, 2);
        assert!(now.position.position_secs.unwrap().abs_diff(40) <= 2);
        assert_eq!(
            control::volume(&lan, &houses, &lead_room.player.id).unwrap(),
            25
        );
        assert_eq!(
            control::volume(&lan, &houses, &member_room.player.id).unwrap(),
            35
        );
        // A second capture equals the first in everything but the time.
        let again = snapshot::capture(&lan, &houses, &coord, 2_000).unwrap();
        assert_eq!(again.members, snap.members);
        assert_eq!(again.levels, snap.levels);
        assert_eq!(again.transport_state, snap.transport_state);
        assert!(matches!(
            again.source,
            SnapshotSource::Queue {
                track: 2,
                position_secs: 40,
                ..
            }
        ));
    }
}

#[test]
fn a_paused_stream_comes_back_paused_in_place() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot_of(&sim, &lan);
    let kitchen = id(&houses, "Kitchen");
    let radio = "x-rincon-mp3radio://stream.example.invalid/sim.mp3";
    control::play_uri(&lan, &houses, &kitchen, radio, "").unwrap();
    control::pause(&lan, &houses, &kitchen).unwrap();
    let snap = snapshot::capture(&lan, &houses, &kitchen, 1).unwrap();
    assert!(
        matches!(&snap.source, SnapshotSource::Uri { uri, position_secs: None, .. } if uri == radio)
    );

    control::play_uri(
        &lan,
        &houses,
        &kitchen,
        "x-rincon-mp3radio://stream.example.invalid/other.mp3",
        "",
    )
    .unwrap();
    let report = snapshot::restore(&lan, &houses, &snap).unwrap();
    assert_eq!(report.skipped.len(), 0, "{report:?}");
    let now = control::playback(&lan, &houses, &kitchen).unwrap();
    assert_eq!(now.position.uri, radio);
    assert_ne!(
        now.transport.state,
        TransportState::Playing,
        "not resumed: it was paused"
    );
}

#[test]
fn a_replaced_queue_is_reported_not_resumed() {
    let sim = sim();
    let lan = sim.lan();
    let coord = set_scene(&sim, &lan, "Kitchen", "Office");
    let houses = snapshot_of(&sim, &lan);
    let snap = snapshot::capture(&lan, &houses, &coord, 1).unwrap();

    // Someone replaces the queue in the meantime.
    let t = sim.transport("Kitchen").unwrap();
    fsonos_proto::control::remove_all_tracks_from_queue(&t, t.ip()).unwrap();
    control::queue_spotify_tracks(
        &lan,
        &houses,
        &coord,
        &[("spotify:track:0SimOtherQ0000000000001", "Other")],
    )
    .unwrap();

    let report = snapshot::restore(&lan, &houses, &snap).unwrap();
    let skipped: Vec<Aspect> = report.skipped.iter().map(|(a, _)| *a).collect();
    assert_eq!(
        skipped,
        [Aspect::Source, Aspect::Position, Aspect::Transport]
    );
    assert!(report.skipped[0].1.contains("queue changed"));
    assert_eq!(
        report.restored,
        [Aspect::Volume, Aspect::Mute],
        "{report:?}"
    );
    let now = control::playback(&lan, &houses, &coord).unwrap();
    assert_ne!(
        now.transport.state,
        TransportState::Playing,
        "the new queue is not resumed"
    );
}
