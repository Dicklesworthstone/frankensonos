//! Native saved playlists through real S1/S2 loopback SOAP. No Spotify grant
//! or favorite is needed to discover and play the speaker's saved containers.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::{CoreError, HouseholdState, control, favorite_queue, playlists, resolve_room};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{DidlKind, parse_didl};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::cell::Cell;
use std::net::IpAddr;

fn fixture() -> (SimHandle, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let houses = ["Kitchen", "Den"]
        .iter()
        .map(|room| {
            let mut state = HouseholdState::default();
            state.apply_topology(
                &get_zone_group_state(&sim.lan(), sim.player(room).unwrap().ip).unwrap(),
            );
            state
        })
        .collect();
    (sim, houses)
}

fn queue(lan: &impl Transport, host: IpAddr) -> Vec<String> {
    content::browse_all(lan, host, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn seed(sim: &SimHandle, houses: &[HouseholdState], room: &str) -> Vec<String> {
    let target = resolve_room(houses, room).unwrap();
    for name in ["first", "second"] {
        soap::add_uri_to_queue(
            &sim.lan(),
            target.coordinator.ip,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            false,
        )
        .unwrap();
    }
    control::play_queue_from(&sim.lan(), houses, &target.coordinator.id, 1).unwrap();
    queue(&sim.lan(), target.coordinator.ip)
}

#[test]
fn saved_playlists_browse_and_replace_the_queue_without_any_favorites() {
    let (sim, houses) = fixture();
    for (room, generation) in [("Kitchen", 1), ("Den", 2)] {
        let original = seed(&sim, &houses, room);
        sim.retain_favorites(generation, |_| false).unwrap();
        let lan = sim.lan();
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        assert!(
            favorites::list(&lan, &houses, coordinator)
                .unwrap()
                .is_empty()
        );
        let saved = playlists::list(&lan, &houses, coordinator).unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].title, format!("Saved Mix S{generation}"));
        assert_eq!(saved[0].id, "SQ:1");
        assert_eq!(saved[1].id, "SQ:7", "catalog ids are not positions");
        assert_eq!(
            parse_didl(&saved[0].metadata).unwrap()[0].kind,
            DidlKind::Container
        );
        let chosen = favorites::find(&saved, "SQ:7").unwrap();
        assert_eq!(chosen.kind, FavoriteKind::Container);
        assert_eq!(
            parse_didl(&chosen.metadata).unwrap()[0].kind,
            DidlKind::Item
        );
        favorites::play(&lan, &houses, coordinator, chosen).unwrap();
        let expanded = queue(&lan, target.coordinator.ip);
        assert_eq!(expanded.len(), 3);
        assert_eq!(expanded[0], expanded[2], "repeated occurrences survive");
        assert_ne!(expanded[0], expanded[1]);
        assert!(
            expanded
                .iter()
                .all(|uri| uri.contains(&format!("0SimS{generation}")))
        );
        assert!(original.iter().all(|uri| !expanded.contains(uri)));
        let now = control::playback(&lan, &houses, coordinator).unwrap();
        assert_eq!(now.transport.state, TransportState::Playing);
        assert_eq!(now.position.track, 1);
        assert_eq!(now.position.uri, expanded[0]);
        control::next(&lan, &houses, coordinator).unwrap();
        assert_eq!(
            control::playback(&lan, &houses, coordinator)
                .unwrap()
                .position
                .uri,
            expanded[1]
        );
    }
}

#[test]
fn saved_playlist_append_and_next_keep_the_current_music_and_can_be_unqueued() {
    let (sim, houses) = fixture();
    for room in ["Kitchen", "Den"] {
        let original = seed(&sim, &houses, room);
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        let lan = sim.lan();
        let saved = playlists::list(&lan, &houses, coordinator).unwrap();
        for next in [false, true] {
            let added =
                favorite_queue::enqueue(&lan, &houses, coordinator, &saved[0], next).unwrap();
            assert_eq!(added.first, if next { 2 } else { 3 });
            assert_eq!(added.uris.len(), 2);
            assert!(added.playing_queue);
            let mut expected = original.clone();
            let at = usize::try_from(added.first - 1).unwrap();
            expected.splice(at..at, added.uris.clone());
            assert_eq!(queue(&lan, target.coordinator.ip), expected);
            let now = control::playback(&lan, &houses, coordinator).unwrap();
            assert_eq!(now.transport.state, TransportState::Playing);
            assert_eq!(now.position.uri, original[0]);
            assert!(
                control::unqueue(&lan, &houses, coordinator, added.first, &added.uris).unwrap()
            );
            assert_eq!(queue(&lan, target.coordinator.ip), original);
        }
    }
}

struct LostReply {
    lan: SimLan,
    adds: Cell<usize>,
}

impl Transport for LostReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#AddURIToQueue\"") {
            self.adds.set(self.adds.get() + 1);
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "reply lost after enqueue".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn a_lost_saved_playlist_reply_keeps_the_original_queue_and_does_not_add_twice() {
    let (sim, houses) = fixture();
    let original = seed(&sim, &houses, "Kitchen");
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let saved = playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap();
    let transport = LostReply {
        lan: sim.lan(),
        adds: Cell::new(0),
    };
    let result = favorite_queue::enqueue(
        &transport,
        &houses,
        &target.coordinator.id,
        &saved[0],
        false,
    );
    assert!(matches!(result, Err(CoreError::Uncertain(_))));
    assert_eq!(transport.adds.get(), 1);
    let after = queue(&sim.lan(), target.coordinator.ip);
    assert_eq!(&after[..original.len()], &original);
    assert_eq!(after.len(), original.len() + 2);
}
