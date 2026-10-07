//! Favorites end to end against `fsonos-sim` over real loopback sockets, on
//! both an S1 and an S2 virtual household: list `FV:2`, play a track, a
//! stream, and an album, and read the players' state back.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
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

#[test]
fn favorites_play_on_both_generations() {
    let sim = sim();
    let lan = sim.lan();
    let houses = snapshot(&sim, &lan);
    for room in ["Kitchen", "Living Room"] {
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = target.coordinator.id.clone();
        let ip = target.coordinator.ip;
        let listed = favorites::list(&lan, &houses, &coordinator).unwrap();
        assert!(listed.len() >= 5, "{room}: {listed:?}");

        // A track renders directly and plays.
        let track = listed
            .iter()
            .find(|f| f.kind == FavoriteKind::Track)
            .unwrap();
        favorites::play(&lan, &houses, &coordinator, track).unwrap();
        let p = control::playback(&lan, &houses, &coordinator).unwrap();
        assert_eq!(p.transport.state, TransportState::Playing, "{room}: track");
        assert_eq!(
            Some(p.position.uri.as_str()),
            track.uri.as_deref(),
            "{room}"
        );

        // A stream renders directly too.
        let stream = listed
            .iter()
            .find(|f| f.kind == FavoriteKind::Stream)
            .unwrap();
        favorites::play(&lan, &houses, &coordinator, stream).unwrap();
        let p = control::playback(&lan, &houses, &coordinator).unwrap();
        assert_eq!(p.transport.state, TransportState::Playing, "{room}: stream");

        // An album replaces the queue (the sim expands it to three tracks)
        // and plays it from the top.
        let album = favorites::find(&listed, "sim symphonies").unwrap();
        assert_eq!(album.kind, FavoriteKind::Container);
        favorites::play(&lan, &houses, &coordinator, album).unwrap();
        let media = soap::get_media_info(&lan, ip).unwrap();
        assert_eq!(
            media.tracks, 3,
            "{room}: the album's tracks are the whole queue"
        );
        assert!(
            media.uri.starts_with("x-rincon-queue:"),
            "{room}: {}",
            media.uri
        );
        let p = control::playback(&lan, &houses, &coordinator).unwrap();
        assert_eq!(p.transport.state, TransportState::Playing, "{room}: album");
        assert_eq!(p.position.track, 1, "{room}: from the top");
    }
}
