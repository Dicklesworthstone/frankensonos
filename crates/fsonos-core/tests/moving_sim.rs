//! Moving playback between rooms and whole-house mode, against `fsonos-sim`
//! over real loopback sockets, in both generations.

use fsonos_core::moving::{self, MoveError, MoveMethod};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
            SimPlayerSpec::new("Den", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Living Room", SimModel::One),
            SimPlayerSpec::new("Bedroom", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

fn id(h: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(h, room).unwrap().player.id.clone()
}

/// `room` plays a two-track Spotify queue, at track 2, 40 s in.
fn play_queue(sim: &SimHandle, lan: &SimLan, room: &str) {
    let h = houses(sim, lan);
    let coord = resolve_room(&h, room).unwrap().coordinator.id.clone();
    control::queue_spotify_tracks(
        lan,
        &h,
        &coord,
        &[
            ("spotify:track:0SimMoveA0000000000001", "First"),
            ("spotify:track:0SimMoveB0000000000002", "Second"),
        ],
    )
    .unwrap()
    .unwrap();
    control::play_queue_from(lan, &h, &coord, 2).unwrap();
    sim.clock().advance(Duration::from_secs(40));
}

/// What `room`'s group is playing: (state, track, position, track URI).
fn playing(lan: &SimLan, h: &[HouseholdState], room: &str) -> (TransportState, u32, u32, String) {
    let coord = resolve_room(h, room).unwrap().coordinator.id.clone();
    let now = control::playback(lan, h, &coord).unwrap();
    (
        now.transport.state,
        now.position.track,
        now.position.position_secs.unwrap_or(0),
        now.position.uri,
    )
}

#[test]
fn moving_a_coordinator_delegates_and_playback_follows() {
    let sim = sim();
    let lan = sim.lan();
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    let (_, _, _, uri) = playing(&lan, &h, "Kitchen");

    let report = moving::move_playback(
        &lan,
        &h,
        &resolve_room(&h, "Kitchen").unwrap(),
        &resolve_room(&h, "Office").unwrap(),
    )
    .unwrap();
    assert_eq!(report.method, MoveMethod::Delegated);
    assert_eq!(
        (report.old_coordinator, report.new_coordinator.clone()),
        (id(&h, "Kitchen"), id(&h, "Office"))
    );

    let h = houses(&sim, &lan);
    let (state, track, position, now_uri) = playing(&lan, &h, "Office");
    assert_eq!((state, track, now_uri), (TransportState::Playing, 2, uri));
    assert!(position.abs_diff(40) <= 2);
    assert_eq!(
        resolve_room(&h, "Office").unwrap().coordinator.id,
        id(&h, "Office")
    );
    assert_eq!(
        resolve_room(&h, "Kitchen").unwrap().coordinator.id,
        id(&h, "Kitchen"),
        "the source left"
    );
    assert_ne!(playing(&lan, &h, "Kitchen").0, TransportState::Playing);
}

#[test]
fn without_delegation_the_music_is_replayed_on_the_target() {
    let sim = sim();
    let lan = sim.lan();
    sim.upnp_fault("Kitchen", "DelegateGroupCoordinationTo", 401)
        .unwrap();
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    let (_, _, _, uri) = playing(&lan, &h, "Kitchen");

    let report = moving::move_playback(
        &lan,
        &h,
        &resolve_room(&h, "Kitchen").unwrap(),
        &resolve_room(&h, "Office").unwrap(),
    )
    .unwrap();
    assert_eq!(report.method, MoveMethod::Copied);
    let h = houses(&sim, &lan);
    let (state, track, position, now_uri) = playing(&lan, &h, "Office");
    assert_eq!((state, track), (TransportState::Playing, 2));
    assert!(position.abs_diff(40) <= 2);
    assert_eq!(
        now_uri, uri,
        "the same track, re-rendered for this household"
    );
    assert_ne!(
        playing(&lan, &h, "Kitchen").0,
        TransportState::Playing,
        "the source stopped"
    );
    assert_ne!(
        resolve_room(&h, "Kitchen").unwrap().coordinator.id,
        id(&h, "Office")
    );
}

#[test]
fn s2_moves_a_single_track_and_a_member_can_move_out() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let living = id(&h, "Living Room");
    let (uri, didl) = control::spotify_track_source(
        &lan,
        &h,
        &living,
        "spotify:track:0SimMoveS2000000000001",
        "Aria",
    )
    .unwrap()
    .unwrap();
    control::play_uri(&lan, &h, &living, &uri, &didl).unwrap();
    let report = moving::move_playback(
        &lan,
        &h,
        &resolve_room(&h, "Living Room").unwrap(),
        &resolve_room(&h, "Bedroom").unwrap(),
    )
    .unwrap();
    assert_eq!(report.method, MoveMethod::Delegated);
    let h = houses(&sim, &lan);
    let (state, _, _, now_uri) = playing(&lan, &h, "Bedroom");
    assert_eq!((state, now_uri), (TransportState::Playing, uri));

    // S1: Office plays as a member of Kitchen's group; moving it to the Den
    // brings the Den in and takes the Office out, and Kitchen keeps leading.
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    control::join(&lan, &h, &id(&h, "Office"), &id(&h, "Kitchen")).unwrap();
    let h = houses(&sim, &lan);
    let report = moving::move_playback(
        &lan,
        &h,
        &resolve_room(&h, "Office").unwrap(),
        &resolve_room(&h, "Den").unwrap(),
    )
    .unwrap();
    assert_eq!(report.method, MoveMethod::Regrouped);
    assert_eq!(report.new_coordinator, id(&h, "Kitchen"));
    let h = houses(&sim, &lan);
    assert_eq!(
        resolve_room(&h, "Den").unwrap().coordinator.id,
        id(&h, "Kitchen")
    );
    assert_eq!(
        resolve_room(&h, "Office").unwrap().coordinator.id,
        id(&h, "Office")
    );
}

#[test]
fn across_households_moving_is_refused_and_copying_works() {
    let sim = sim();
    let lan = sim.lan();
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    let kitchen = resolve_room(&h, "Kitchen").unwrap();
    let living = resolve_room(&h, "Living Room").unwrap();
    let err = moving::move_playback(&lan, &h, &kitchen, &living).unwrap_err();
    assert!(matches!(err, MoveError::CrossHousehold { .. }));
    assert!(err.to_string().contains("never be grouped"));

    // Copy: the S2 household renders the same tracks with its own
    // parameters (the simulator refuses S1's), at the same place.
    let report = moving::copy_playback(&lan, &h, &kitchen, &living).unwrap();
    assert_eq!(report.method, MoveMethod::Copied);
    let (state, track, position, uri) = playing(&lan, &h, "Living Room");
    assert_eq!((state, track), (TransportState::Playing, 2));
    assert!(position.abs_diff(40) <= 2);
    let s2 = sim.render_params(2).unwrap();
    assert!(uri.ends_with(&format!("sn={}", s2.sn)), "{uri}");
    assert_ne!(playing(&lan, &h, "Kitchen").0, TransportState::Playing);
}

#[test]
fn party_groups_the_household_under_the_playing_zone() {
    let sim = sim();
    let lan = sim.lan();
    play_queue(&sim, &lan, "Office");
    let h = houses(&sim, &lan);
    let outcome = moving::party(&lan, &h, &h[0], None).unwrap();
    assert!(outcome.is_complete());
    assert_eq!(outcome.moved.len(), 2, "{outcome:?}");
    let h = houses(&sim, &lan);
    let office = id(&h, "Office");
    for room in ["Kitchen", "Office", "Den"] {
        assert_eq!(
            resolve_room(&h, room).unwrap().coordinator.id,
            office,
            "{room}"
        );
    }
    assert_eq!(h[1].groups.len(), 2, "the other household is untouched");
}
