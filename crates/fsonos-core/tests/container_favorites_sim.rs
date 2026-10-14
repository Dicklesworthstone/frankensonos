//! Container-only Spotify setup exercised through real loopback SOAP. The
//! simulator rejects mismatched household credentials and expands containers
//! into actual queue tracks, rather than merely accepting a rendered string.

use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::didl::{
    SpotifyContainer, SpotifyRenderParams, spotify_container_didl, spotify_container_uri, xml_escape,
};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::cell::Cell;
use std::net::IpAddr;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

fn households(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut state = HouseholdState::default();
            state.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            state
        })
        .collect()
}

fn queue(sim: &SimHandle, lan: &SimLan, room: &str) -> Vec<String> {
    content::browse_all(lan, sim.player(room).unwrap().ip, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn seed(sim: &SimHandle, lan: &SimLan, houses: &[HouseholdState], room: &str) -> Vec<String> {
    let target = resolve_room(houses, room).unwrap();
    let coordinator = &target.coordinator.id;
    assert_eq!(
        control::queue_spotify_tracks(
            lan,
            houses,
            coordinator,
            &[
                ("spotify:track:0SimQueueA000000000001", "Original first"),
                ("spotify:track:0SimQueueB000000000002", "Original second"),
            ],
        )
        .unwrap(),
        Some(1)
    );
    control::play_queue_from(lan, houses, coordinator, 1).unwrap();
    queue(sim, lan, room)
}

#[test]
fn album_and_playlist_favorites_bootstrap_playback_in_each_household() {
    for (kind, name) in [
        (SpotifyContainer::Album, "album"),
        (SpotifyContainer::Playlist, "playlist"),
    ] {
        let sim = sim();
        let lan = sim.lan();
        let houses = households(&sim, &lan);
        for (room, generation) in [("Kitchen", 1), ("Living Room", 2)] {
            let original = seed(&sim, &lan, &houses, room);
            let marker = format!("spotify%3a{name}%3a");
            sim.retain_favorites(generation, |uri| uri.contains(&marker))
                .unwrap();
            let target = resolve_room(&houses, room).unwrap();
            let coordinator = &target.coordinator.id;
            assert!(
                control::spotify_params(&lan, &houses, coordinator)
                    .unwrap()
                    .is_none()
            );
            control::play_spotify_container(
                &lan,
                &houses,
                coordinator,
                kind,
                &format!("spotify:{name}:0SimNewContainer000001"),
                "New container",
            )
            .unwrap();
            let now = control::playback(&lan, &houses, coordinator).unwrap();
            assert_eq!(now.transport.state, TransportState::Playing);
            assert_eq!(now.position.track, 1);
            let expanded = queue(&sim, &lan, room);
            assert!(expanded.len() > 1, "the speaker must expand the container");
            assert!(original.iter().all(|uri| !expanded.contains(uri)));
            assert_eq!(now.position.uri, expanded[0]);
            control::next(&lan, &houses, coordinator).unwrap();
            let next = control::playback(&lan, &houses, coordinator).unwrap();
            assert_eq!(next.position.track, 2);
            assert_eq!(next.position.uri, expanded[1]);
        }
    }
}

#[test]
fn container_next_and_undo_preserve_the_existing_music_without_a_track_favorite() {
    let sim = sim();
    let lan = sim.lan();
    let houses = households(&sim, &lan);
    for (room, generation) in [("Kitchen", 1), ("Living Room", 2)] {
        let original = seed(&sim, &lan, &houses, room);
        sim.retain_favorites(generation, |uri| uri.contains("spotify%3aplaylist%3a"))
            .unwrap();
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        let added = control::enqueue_spotify(
            &lan,
            &houses,
            coordinator,
            "spotify:playlist:0SimNextContainer00001",
            "Next playlist",
            true,
        )
        .unwrap();
        assert_eq!(added.first, 2);
        assert!(added.playing_queue);
        assert!(added.uris.len() > 1);
        let mut expected = vec![original[0].clone()];
        expected.extend(added.uris.clone());
        expected.push(original[1].clone());
        assert_eq!(queue(&sim, &lan, room), expected);
        let now = control::playback(&lan, &houses, coordinator).unwrap();
        assert_eq!(now.transport.state, TransportState::Playing);
        assert_eq!(now.position.uri, original[0]);
        assert!(control::unqueue(&lan, &houses, coordinator, added.first, &added.uris).unwrap());
        assert_eq!(queue(&sim, &lan, room), original);
        assert_eq!(
            control::playback(&lan, &houses, coordinator)
                .unwrap()
                .position
                .uri,
            original[0]
        );
    }
}

#[test]
fn a_missing_template_does_not_clear_or_interrupt_the_existing_queue() {
    let sim = sim();
    let lan = sim.lan();
    let houses = households(&sim, &lan);
    for (room, generation) in [("Kitchen", 1), ("Living Room", 2)] {
        let original = seed(&sim, &lan, &houses, room);
        sim.retain_favorites(generation, |_| false).unwrap();
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        let result = control::play_spotify_container(
            &lan,
            &houses,
            coordinator,
            SpotifyContainer::Album,
            "spotify:album:0SimNewContainer000001",
            "Unavailable album",
        );
        assert!(matches!(result, Err(CoreError::NoSpotifyFavorite)));
        assert_eq!(queue(&sim, &lan, room), original);
        let now = control::playback(&lan, &houses, coordinator).unwrap();
        assert_eq!(now.transport.state, TransportState::Playing);
        assert_eq!(now.position.uri, original[0]);
    }
}

#[test]
fn track_only_favorites_still_bootstrap_both_container_kinds() {
    let sim = sim();
    let lan = sim.lan();
    let houses = households(&sim, &lan);
    for (room, generation) in [("Kitchen", 1), ("Living Room", 2)] {
        sim.retain_favorites(generation, |uri| uri.starts_with("x-sonos-spotify:"))
            .unwrap();
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        for (kind, source) in [
            (
                SpotifyContainer::Album,
                "spotify:album:0SimFallbackAlbum00001",
            ),
            (
                SpotifyContainer::Playlist,
                "spotify:playlist:0SimFallbackList000001",
            ),
        ] {
            control::play_spotify_container(&lan, &houses, coordinator, kind, source, "Fallback")
                .unwrap();
            assert!(queue(&sim, &lan, room).len() > 1);
            assert_eq!(
                control::playback(&lan, &houses, coordinator)
                    .unwrap()
                    .transport
                    .state,
                TransportState::Playing
            );
        }
    }
}

// Only the favorites reply is replaced, to model stale saved favorites after
// relinking Spotify. Every queue mutation and playback/read-back still travels
// over the real simulator socket and is checked against its linked account.
struct FavoritesReply<'a> {
    lan: &'a SimLan,
    host: IpAddr,
    reply: String,
    lose_add_reply: Cell<bool>,
}

impl Transport for FavoritesReply<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if host == self.host
            && action.contains("#Browse")
            && body.contains("<ObjectID>FV:2</ObjectID>")
        {
            return Ok(self.reply.clone());
        }
        let reply = self.lan.soap_post(host, path, action, body)?;
        if host == self.host
            && action.contains("#AddURIToQueue")
            && self.lose_add_reply.replace(false)
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "simulated lost reply after applying the enqueue".into(),
            });
        }
        Ok(reply)
    }
}

fn favorites_reply(
    params: &SpotifyRenderParams,
    kind: SpotifyContainer,
    stale_copies: usize,
    include_current: bool,
) -> String {
    let name = match kind {
        SpotifyContainer::Album => "album",
        SpotifyContainer::Playlist => "playlist",
    };
    let prefix = kind.prefix_in(&[]);
    let favorite = |p: &SpotifyRenderParams, index: usize| {
        let source = format!("spotify:{name}:0SimTemplate{index:06}");
        let uri = spotify_container_uri(&source, &prefix, p);
        let metadata = spotify_container_didl(&source, "Template", kind, &prefix, p);
        format!(
            "<item id=\"FV:{index}\"><res>{}</res><r:resMD>{}</r:resMD></item>",
            xml_escape(&uri),
            xml_escape(&metadata)
        )
    };
    let mut stale = params.clone();
    stale.sn = stale.sn.saturating_add(100);
    stale.cdudn.push_str("-stale");
    let mut items: Vec<_> = (0..stale_copies)
        .map(|index| favorite(&stale, index))
        .collect();
    if include_current {
        items.push(favorite(params, stale_copies));
    }
    let didl = format!(
        "<DIDL-Lite xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</DIDL-Lite>",
        items.join("")
    );
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body><u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
         <Result>{}</Result><NumberReturned>{}</NumberReturned>\
         <TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID>\
         </u:BrowseResponse></s:Body></s:Envelope>",
        xml_escape(&didl),
        items.len(),
        items.len()
    )
}

#[test]
fn rejected_stale_majorities_fail_over_for_play_and_enqueue_in_both_households() {
    for (kind, name) in [
        (SpotifyContainer::Album, "album"),
        (SpotifyContainer::Playlist, "playlist"),
    ] {
        for replace in [false, true] {
            let sim = sim();
            let lan = sim.lan();
            let houses = households(&sim, &lan);
            for (room, generation) in [("Kitchen", 1), ("Living Room", 2)] {
                let original = seed(&sim, &lan, &houses, room);
                let target = resolve_room(&houses, room).unwrap();
                let coordinator = &target.coordinator.id;
                let transport = FavoritesReply {
                    lan: &lan,
                    host: sim.player(room).unwrap().ip,
                    reply: favorites_reply(
                        &sim.render_params(generation).unwrap(),
                        kind,
                        2,
                        true,
                    ),
                    lose_add_reply: Cell::new(false),
                };
                let source = format!("spotify:{name}:0SimRecoveredContainer");
                let before = sim.soap_log().len();
                if replace {
                    control::play_spotify_container(
                        &transport, &houses, coordinator, kind, &source, "Recovered",
                    )
                    .unwrap();
                    let after = queue(&sim, &lan, room);
                    assert!(after.len() > 1);
                    assert!(original.iter().all(|uri| !after.contains(uri)));
                } else {
                    let added = control::enqueue_spotify(
                        &transport, &houses, coordinator, &source, "Recovered", false,
                    )
                    .unwrap();
                    let after = queue(&sim, &lan, room);
                    assert_eq!(&after[..original.len()], original.as_slice());
                    assert_eq!(added.uris.as_slice(), &after[original.len()..]);
                }
                let log = sim.soap_log();
                let adds: Vec<_> = log[before..]
                    .iter()
                    .filter(|entry| entry.action == "AddURIToQueue")
                    .collect();
                assert_eq!(adds.len(), 2, "exactly one bounded retry");
                assert_eq!(adds[0].result.as_ref().unwrap_err(), &800);
                assert!(adds[1].result.is_ok(), "the alternative really rendered");
                assert_eq!(
                    control::playback(&lan, &houses, coordinator)
                        .unwrap()
                        .transport
                        .state,
                    TransportState::Playing
                );
            }
        }
    }
}

#[test]
fn repeated_rejection_is_bounded_and_does_not_duplicate_the_queue() {
    let sim = sim();
    let lan = sim.lan();
    let houses = households(&sim, &lan);
    let original = seed(&sim, &lan, &houses, "Kitchen");
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let transport = FavoritesReply {
        lan: &lan,
        host: sim.player("Kitchen").unwrap().ip,
        reply: favorites_reply(
            &sim.render_params(1).unwrap(),
            SpotifyContainer::Album,
            2,
            false,
        ),
        lose_add_reply: Cell::new(false),
    };
    let before = sim.soap_log().len();
    let result = control::enqueue_spotify(
        &transport,
        &houses,
        &target.coordinator.id,
        "spotify:album:0SimUnrenderableAlbum",
        "Unavailable",
        false,
    );
    assert!(matches!(result, Err(CoreError::RenderParamsStale)));
    assert_eq!(queue(&sim, &lan, "Kitchen"), original);
    let log = sim.soap_log();
    let adds: Vec<_> = log[before..]
        .iter()
        .filter(|entry| entry.action == "AddURIToQueue")
        .collect();
    assert_eq!(adds.len(), 2);
    assert!(
        adds.iter()
            .all(|entry| entry.result.as_ref().is_err_and(|code| *code == 800))
    );
}

#[test]
fn a_lost_enqueue_reply_is_not_retried_as_parameter_drift() {
    let sim = sim();
    let lan = sim.lan();
    let houses = households(&sim, &lan);
    let original = seed(&sim, &lan, &houses, "Kitchen");
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let transport = FavoritesReply {
        lan: &lan,
        host: sim.player("Kitchen").unwrap().ip,
        reply: favorites_reply(
            &sim.render_params(1).unwrap(),
            SpotifyContainer::Album,
            0,
            true,
        ),
        lose_add_reply: Cell::new(true),
    };
    let before = sim.soap_log().len();
    let result = control::enqueue_spotify(
        &transport,
        &houses,
        &target.coordinator.id,
        "spotify:album:0SimAcceptedAlbum00001",
        "Accepted but unacknowledged",
        false,
    );
    assert!(matches!(
        result,
        Err(CoreError::Proto(ProtoError::Network { .. }))
    ));
    let after = queue(&sim, &lan, "Kitchen");
    assert!(after.len() > original.len());
    assert_eq!(&after[..original.len()], original.as_slice());
    assert_eq!(
        sim.soap_log()[before..]
            .iter()
            .filter(|entry| entry.action == "AddURIToQueue")
            .count(),
        1
    );
}
