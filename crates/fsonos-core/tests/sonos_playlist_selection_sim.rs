//! Exact saved-playlist occurrences keep native metadata through S1/S2 SOAP.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::{CoreError, HouseholdState, control, favorite_queue, playlists, resolve_room};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::parse_didl;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::time::Duration;

fn fixture() -> (SimHandle, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let houses = households(&sim);
    (sim, houses)
}

fn households(sim: &SimHandle) -> Vec<HouseholdState> {
    ["Kitchen", "Den"]
        .iter()
        .map(|room| {
            let mut house = HouseholdState::default();
            house.apply_topology(
                &get_zone_group_state(&sim.lan(), sim.player(room).unwrap().ip).unwrap(),
            );
            house
        })
        .collect()
}

fn revision(t: &impl Transport, houses: &[HouseholdState], room: &str) -> String {
    let target = resolve_room(houses, room).unwrap();
    playlists::contents(t, houses, &target.coordinator.id, "SQ:7", 0, 3, None)
        .unwrap()
        .revision
}

fn assert_read_only(sim: &SimHandle, start: usize) {
    assert!(
        sim.soap_log()[start..]
            .iter()
            .all(|entry| { matches!(entry.action.as_str(), "GetZoneGroupState" | "Browse") })
    );
}

#[test]
fn duplicate_occurrences_keep_their_positions_and_native_metadata_in_both_households() {
    let (sim, houses) = fixture();
    for (room, generation) in [("Kitchen", 1), ("Den", 2)] {
        sim.retain_favorites(generation, |_| false).unwrap();
        let lan = sim.lan();
        let target = resolve_room(&houses, room).unwrap();
        let page =
            playlists::contents(&lan, &houses, &target.coordinator.id, "SQ:7", 0, 3, None).unwrap();
        assert_eq!(page.items.len(), 3);
        let start = sim.soap_log().len();
        let mut selected = Vec::new();
        for (index, item) in page.items.iter().enumerate() {
            let position = u32::try_from(index + 1).unwrap();
            let track = playlists::select_track(
                &lan,
                &houses,
                &target.coordinator.id,
                "SQ:7",
                position,
                &page.revision,
            )
            .unwrap();
            assert_eq!(track.id, format!("SQ:7/{position}"));
            assert_eq!(track.kind, FavoriteKind::Track);
            assert_eq!(
                track.uri.as_deref(),
                Some(item.res.as_ref().unwrap().uri.as_str())
            );
            assert_eq!(parse_didl(&track.metadata).unwrap(), [item.clone()]);
            assert!(
                item.desc.is_some(),
                "the native service account is retained"
            );
            assert_eq!(track.art_uri, item.album_art_uri);
            selected.push(track);
        }
        assert_eq!(selected[0].uri, selected[2].uri);
        assert_ne!(selected[0].id, selected[2].id);
        assert_ne!(selected[0].metadata, selected[2].metadata);
        assert_ne!(selected[0].uri, selected[1].uri);
        assert_read_only(&sim, start);
        assert_eq!(
            sim.soap_log().len() - start,
            18,
            "six bounded reads per selection"
        );
    }
}

#[test]
fn selected_tracks_use_existing_play_next_and_append_without_expanding_the_playlist() {
    let (sim, houses) = fixture();
    for room in ["Kitchen", "Den"] {
        let lan = sim.lan();
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        let host = target.coordinator.ip;
        for uri in [
            "x-file-cifs://example.invalid/owner-a.flac",
            "x-file-cifs://example.invalid/owner-b.flac",
        ] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        control::play_queue_from(&lan, &houses, coordinator, 1).unwrap();
        let before = control::playback(&lan, &houses, coordinator).unwrap();
        let revision = revision(&lan, &houses, room);
        let selected =
            playlists::select_track(&lan, &houses, coordinator, "SQ:7", 3, &revision).unwrap();
        for (next, expected_position) in [(true, 2), (false, 4)] {
            let added =
                favorite_queue::enqueue(&lan, &houses, coordinator, &selected, next).unwrap();
            assert_eq!(added.first, expected_position);
            assert_eq!(added.uris, [selected.uri.clone().unwrap()]);
            assert!(added.playing_queue);
            assert_eq!(
                control::playback(&lan, &houses, coordinator).unwrap(),
                before
            );
        }
        let queue = content::browse_snapshot(&lan, host, QUEUE).unwrap();
        assert_eq!(
            queue.objects.len(),
            4,
            "only the two selected occurrences were added"
        );
        let start = sim.soap_log().len();
        favorites::play(&lan, &houses, coordinator, &selected).unwrap();
        let now = control::playback(&lan, &houses, coordinator).unwrap();
        assert_eq!(now.transport.state, TransportState::Playing);
        assert_eq!(now.position.uri, selected.uri.unwrap());
        assert_eq!(content::browse_snapshot(&lan, host, QUEUE).unwrap(), queue);
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| entry.action.clone())
            .collect();
        assert_eq!(writes, ["SetAVTransportURI", "Play"]);
    }
}

#[test]
fn stale_household_playlist_boot_and_group_proofs_never_select_a_track() {
    let (sim, houses) = fixture();
    let lan = sim.lan();
    let first = revision(&lan, &houses, "Kitchen");
    let start = sim.soap_log().len();
    for (room, playlist) in [("Den", "SQ:7"), ("Office", "SQ:7"), ("Kitchen", "SQ:1")] {
        let target = resolve_room(&houses, room).unwrap();
        assert!(matches!(
            playlists::select_track(&lan, &houses, &target.coordinator.id, playlist, 1, &first),
            Err(CoreError::PlaylistChanged(_))
        ));
    }
    assert_read_only(&sim, start);
    sim.reboot("Kitchen", Duration::ZERO).unwrap();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let start = sim.soap_log().len();
    assert!(matches!(
        playlists::select_track(&lan, &houses, &target.coordinator.id, "SQ:7", 1, &first),
        Err(CoreError::PlaylistChanged(_))
    ));
    assert_read_only(&sim, start);
    let rebooted = revision(&lan, &houses, "Kitchen");
    let office = resolve_room(&houses, "Office").unwrap();
    control::join(&lan, &houses, &office.player.id, &target.coordinator.id).unwrap();
    let current = households(&sim);
    let start = sim.soap_log().len();
    assert!(matches!(
        playlists::select_track(&lan, &current, &target.coordinator.id, "SQ:7", 1, &rebooted),
        Err(CoreError::PlaylistChanged(_))
    ));
    assert_read_only(&sim, start);
}

#[test]
fn changed_catalog_revisions_and_positions_past_the_end_cannot_select_another_track() {
    let (sim, houses) = fixture();
    let lan = sim.lan();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let observed = revision(&lan, &houses, "Kitchen");
    let start = sim.soap_log().len();
    assert!(matches!(
        playlists::select_track(&lan, &houses, &target.coordinator.id, "SQ:7", 4, &observed),
        Err(CoreError::Proto(ProtoError::Malformed(_)))
    ));
    assert_read_only(&sim, start);
    soap::add_uri_to_queue(
        &lan,
        target.coordinator.ip,
        "x-file-cifs://example.invalid/new.flac",
        "",
        false,
    )
    .unwrap();
    playlists::save_queue(&lan, &houses, &target.coordinator.id, "New catalog entry").unwrap();
    let start = sim.soap_log().len();
    assert!(matches!(
        playlists::select_track(&lan, &houses, &target.coordinator.id, "SQ:7", 1, &observed),
        Err(CoreError::PlaylistChanged(_))
    ));
    assert_read_only(&sim, start);
}

#[derive(Clone, Copy)]
enum Unavailable {
    Resource,
    Class,
    Container,
    Sentinel,
}

struct Altered {
    lan: SimLan,
    unavailable: Unavailable,
}

impl Transport for Altered {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        assert!(action.ends_with("#Browse\"") || action.ends_with("#GetZoneGroupState\""));
        let response = self.lan.soap_post(host, path, action, body)?;
        if !body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>") {
            return Ok(response);
        }
        Ok(match self.unavailable {
            Unavailable::Resource => response
                .replace("&lt;res ", "&lt;unavailable ")
                .replace("&lt;/res&gt;", "&lt;/unavailable&gt;"),
            Unavailable::Class => {
                response.replace("object.item.audioItem.musicTrack", "object.item.audioItem")
            }
            Unavailable::Container => response
                .replace("&lt;item ", "&lt;container ")
                .replace("&lt;/item&gt;", "&lt;/container&gt;"),
            Unavailable::Sentinel => {
                let mut response = response;
                let resource = response.find("&lt;res ").unwrap();
                let start = resource + response[resource..].find("&gt;").unwrap() + "&gt;".len();
                let end = start + response[start..].find("&lt;/res&gt;").unwrap();
                response.replace_range(start..end, "NOT_IMPLEMENTED");
                response
            }
        })
    }
}

#[test]
fn unavailable_occurrences_stay_visible_but_cannot_be_selected_as_tracks() {
    let (sim, houses) = fixture();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    for unavailable in [
        Unavailable::Resource,
        Unavailable::Class,
        Unavailable::Container,
        Unavailable::Sentinel,
    ] {
        let transport = Altered {
            lan: sim.lan(),
            unavailable,
        };
        let page = playlists::contents(
            &transport,
            &houses,
            &target.coordinator.id,
            "SQ:7",
            0,
            3,
            None,
        )
        .unwrap();
        assert_eq!(
            page.items.len(),
            3,
            "unavailable rows keep their occurrence positions"
        );
        let start = sim.soap_log().len();
        assert!(matches!(
            playlists::select_track(
                &transport,
                &houses,
                &target.coordinator.id,
                "SQ:7",
                1,
                &page.revision
            ),
            Err(CoreError::Proto(ProtoError::Malformed(_)))
        ));
        assert_read_only(&sim, start);
    }
}
