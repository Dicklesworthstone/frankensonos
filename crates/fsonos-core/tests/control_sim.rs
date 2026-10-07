//! Control orchestration end to end against `fsonos-sim` over real loopback
//! sockets: a room name resolves to its group's coordinator, `control`
//! sends the SOAP, and the simulated players' state (which enforces the
//! household's own Spotify render parameters) shows the effect.

use fsonos_core::{CoreError, HouseholdState, control, grouping, resolve_room};
use fsonos_proto::ProtoError;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
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

fn snapshot(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

fn fault(r: Result<impl std::fmt::Debug, CoreError>) -> u16 {
    match r {
        Err(CoreError::Proto(ProtoError::SoapFault { code, .. })) => code,
        other => panic!("expected a UPnP fault, got {other:?}"),
    }
}

#[test]
fn a_room_in_a_group_plays_through_its_coordinator() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot(&sim, &lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    assert!(grouping::group(&lan, &houses, &kitchen, &[office]).is_complete());
    let houses = snapshot(&sim, &lan);

    // "Play this on the Office": the Office plays in Kitchen's group.
    let target = resolve_room(&houses, "Office").unwrap();
    assert_eq!(target.coordinator.room_name, "Kitchen");
    let coordinator = &target.coordinator.id;

    // Render parameters come from this household's own favorites, and the
    // simulator accepts the result only if they match its linked account.
    let learned = control::spotify_params(&lan, &houses, coordinator)
        .unwrap()
        .unwrap();
    assert_eq!(learned, sim.render_params(1).unwrap());
    let (uri, didl) = control::spotify_track_source(
        &lan,
        &houses,
        coordinator,
        "spotify:track:0SimNewTrack0000000001",
        "Gymnopédie No. 1",
    )
    .unwrap()
    .unwrap();
    control::play_uri(&lan, &houses, coordinator, &uri, &didl).unwrap();
    let now = control::playback(&lan, &houses, coordinator).unwrap();
    assert_eq!(now.transport.state, TransportState::Playing);
    assert_eq!(now.position.uri, uri);

    control::pause(&lan, &houses, coordinator).unwrap();
    assert_eq!(
        control::playback(&lan, &houses, coordinator)
            .unwrap()
            .transport
            .state,
        TransportState::Paused
    );
    control::resume(&lan, &houses, coordinator).unwrap();
    control::stop(&lan, &houses, coordinator).unwrap();
    assert_eq!(
        control::playback(&lan, &houses, coordinator)
            .unwrap()
            .transport
            .state,
        TransportState::Stopped
    );

    // Sending the transport verb to the member instead is refused, which is
    // why commands are coordinator-addressed.
    assert_eq!(
        fault(control::resume(&lan, &houses, &target.player.id)),
        800
    );
}

#[test]
fn queued_tracks_play_continuously_in_each_household() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot(&sim, &lan);
    for (room, sw_gen) in [("Kitchen", 1), ("Living Room", 2)] {
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        assert_eq!(
            control::spotify_params(&lan, &houses, coordinator).unwrap(),
            sim.render_params(sw_gen),
            "{room} learns its own household's parameters"
        );
        let first = control::queue_spotify_tracks(
            &lan,
            &houses,
            coordinator,
            &[
                ("spotify:track:0SimQueueA000000000001", "First"),
                ("spotify:track:0SimQueueB000000000002", "Second"),
            ],
        )
        .unwrap();
        assert_eq!(first, Some(1));
        control::play_queue_from(&lan, &houses, coordinator, 1).unwrap();
        sim.clock().advance(Duration::from_secs(42));
        let now = control::playback(&lan, &houses, coordinator).unwrap();
        assert_eq!(
            (now.transport.state, now.position.track),
            (TransportState::Playing, 1)
        );
        assert_eq!(now.position.position_secs, Some(42));
        assert_eq!(now.position.metadata.unwrap().title, "First");

        control::next(&lan, &houses, coordinator).unwrap();
        assert_eq!(
            control::playback(&lan, &houses, coordinator)
                .unwrap()
                .position
                .track,
            2
        );
        // The end of the queue is the end: the player refuses to go further.
        assert_eq!(fault(control::next(&lan, &houses, coordinator)), 711);
        control::previous(&lan, &houses, coordinator).unwrap();
        assert_eq!(
            control::playback(&lan, &houses, coordinator)
                .unwrap()
                .position
                .track,
            1
        );
    }
}

#[test]
fn room_and_group_volume_reach_the_right_players() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot(&sim, &lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    grouping::group(&lan, &houses, &kitchen, &[office]);
    let houses = snapshot(&sim, &lan);
    let office = resolve_room(&houses, "Office").unwrap();
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();

    // A room's volume is its own player's.
    assert_eq!(
        control::set_volume(&lan, &houses, &office.player.id, 40).unwrap(),
        40
    );
    assert_eq!(
        control::volume(&lan, &houses, &office.player.id).unwrap(),
        40
    );
    assert_eq!(
        control::volume(&lan, &houses, &kitchen.player.id).unwrap(),
        20
    );
    assert_eq!(
        control::adjust_volume(&lan, &houses, &office.player.id, -5).unwrap(),
        35
    );

    // Group volume goes to the coordinator and scales every member.
    control::set_group_volume(&lan, &houses, &kitchen.coordinator.id, 55).unwrap();
    let levels = (
        control::volume(&lan, &houses, &kitchen.player.id).unwrap(),
        control::volume(&lan, &houses, &office.player.id).unwrap(),
    );
    // Kitchen 20 and Office 35 average 28; scaled to 55: 1114/28 and 1939/28.
    assert_eq!(levels, (39, 69));
    control::set_mute(&lan, &houses, &office.player.id, true).unwrap();
    assert!(
        sim.soap_log()
            .iter()
            .any(|e| e.action == "SetMute" && e.room == "Office")
    );
}
