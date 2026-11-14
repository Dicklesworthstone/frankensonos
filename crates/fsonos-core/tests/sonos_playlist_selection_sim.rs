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
use std::cell::Cell;
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

#[test]
fn batch_selection_coalesces_pages_but_retains_requested_order_and_repeated_positions() {
    let (sim, houses) = fixture();
    for room in ["Kitchen", "Den"] {
        let lan = sim.lan();
        let target = resolve_room(&houses, room).unwrap();
        let page =
            playlists::contents(&lan, &houses, &target.coordinator.id, "SQ:7", 0, 3, None).unwrap();
        let start = sim.soap_log().len();
        let selected = playlists::select_tracks(
            &lan,
            &houses,
            &target.coordinator.id,
            "SQ:7",
            &[3, 1, 3, 2],
            &page.revision,
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["SQ:7/3", "SQ:7/1", "SQ:7/3", "SQ:7/2"]
        );
        for (track, index) in selected.iter().zip([2, 0, 2, 1]) {
            assert_eq!(
                parse_didl(&track.metadata).unwrap(),
                [page.items[index].clone()]
            );
        }
        assert_eq!(selected[0], selected[2]);
        assert_ne!(selected[0], selected[1]);
        assert_eq!(selected[0].uri, selected[1].uri);
        assert_read_only(&sim, start);
        assert_eq!(
            sim.soap_log().len() - start,
            6,
            "one verified native page for all occurrences"
        );
    }
}

struct ShortPages {
    lan: SimLan,
    reads: Cell<u32>,
    change_on_reread: bool,
}

impl Transport for ShortPages {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let selected = body.contains("<ObjectID>SQ:7</ObjectID>")
            && body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>");
        if !selected {
            return self.lan.soap_post(host, path, action, body);
        }
        self.reads.set(self.reads.get() + 1);
        let mut body = body.to_string();
        let start = body.find("<RequestedCount>").unwrap() + "<RequestedCount>".len();
        let end = body[start..].find("</RequestedCount>").unwrap() + start;
        body.replace_range(start..end, "1");
        let response = self.lan.soap_post(host, path, action, &body)?;
        if self.change_on_reread && self.reads.get() == 5 {
            Ok(response.replace("&lt;dc:title&gt;", "&lt;dc:title&gt;Changed "))
        } else {
            Ok(response)
        }
    }
}

#[test]
fn short_pages_are_completed_and_every_selected_page_is_rechecked_before_publication() {
    let (sim, houses) = fixture();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let revision = revision(&sim.lan(), &houses, "Kitchen");
    for change_on_reread in [false, true] {
        let transport = ShortPages {
            lan: sim.lan(),
            reads: Cell::new(0),
            change_on_reread,
        };
        let start = sim.soap_log().len();
        let result = playlists::select_tracks(
            &transport,
            &houses,
            &target.coordinator.id,
            "SQ:7",
            &[3, 1, 2, 3],
            &revision,
        );
        if change_on_reread {
            assert!(matches!(result, Err(CoreError::PlaylistChanged(_))));
            assert_eq!(transport.reads.get(), 5);
        } else {
            assert_eq!(
                result
                    .unwrap()
                    .iter()
                    .map(|track| track.id.as_str())
                    .collect::<Vec<_>>(),
                ["SQ:7/3", "SQ:7/1", "SQ:7/2", "SQ:7/3"]
            );
            assert_eq!(transport.reads.get(), 6);
            assert_eq!(
                sim.soap_log().len() - start,
                10,
                "group and metadata shared across all pages"
            );
        }
        assert_read_only(&sim, start);
    }
}

#[test]
fn batch_with_a_missing_or_unavailable_final_occurrence_returns_no_selection() {
    let (sim, houses) = fixture();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let revision = revision(&sim.lan(), &houses, "Kitchen");
    let start = sim.soap_log().len();
    assert!(
        playlists::select_tracks(
            &sim.lan(),
            &houses,
            &target.coordinator.id,
            "SQ:7",
            &[1, 4],
            &revision,
        )
        .is_err()
    );
    let transport = Altered {
        lan: sim.lan(),
        unavailable: Unavailable::Resource,
    };
    assert!(
        playlists::select_tracks(
            &transport,
            &houses,
            &target.coordinator.id,
            "SQ:7",
            &[1, 2],
            &revision,
        )
        .is_err()
    );
    assert_read_only(&sim, start);
}

#[test]
fn sparse_selected_occurrences_read_only_their_bounded_native_pages() {
    let (sim, houses) = fixture();
    let lan = sim.lan();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    for index in 1..=103 {
        soap::add_uri_to_queue(
            &lan,
            target.coordinator.ip,
            &format!("x-file-cifs://example.invalid/library/{index}.flac"),
            "",
            false,
        )
        .unwrap();
    }
    let saved = playlists::save_queue(
        &lan,
        &houses,
        &target.coordinator.id,
        "Long native playlist",
    )
    .unwrap();
    let observed =
        playlists::contents(&lan, &houses, &target.coordinator.id, &saved.id, 0, 1, None).unwrap();
    let start = sim.soap_log().len();
    let selected = playlists::select_tracks(
        &lan,
        &houses,
        &target.coordinator.id,
        &saved.id,
        &[103, 1, 101, 103],
        &observed.revision,
    )
    .unwrap();
    for (track, index) in selected.iter().zip([103, 1, 101, 103]) {
        assert_eq!(track.id, format!("{}/{index}", saved.id));
        assert_eq!(
            track.uri.as_deref(),
            Some(format!("x-file-cifs://example.invalid/library/{index}.flac").as_str())
        );
    }
    assert_read_only(&sim, start);
    assert_eq!(
        sim.soap_log().len() - start,
        8,
        "two selected pages read twice plus one shared metadata/group proof"
    );
    let log = sim.soap_log();
    let ranges: Vec<_> = log[start..]
        .iter()
        .filter(|entry| {
            entry
                .args
                .iter()
                .any(|(name, value)| name == "BrowseFlag" && value == "BrowseDirectChildren")
        })
        .map(|entry| {
            (
                entry
                    .args
                    .iter()
                    .find(|(name, _)| name == "StartingIndex")
                    .unwrap()
                    .1
                    .clone(),
                entry
                    .args
                    .iter()
                    .find(|(name, _)| name == "RequestedCount")
                    .unwrap()
                    .1
                    .clone(),
            )
        })
        .collect();
    assert_eq!(
        ranges,
        [
            ("0".to_string(), "1".to_string()),
            ("100".to_string(), "3".to_string()),
            ("0".to_string(), "1".to_string()),
            ("100".to_string(), "3".to_string())
        ]
    );
}
