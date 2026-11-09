//! Native saved-playlist inspection through real S1/S2 loopback SOAP.
//! The live queue and playback remain independent of the inspected catalog.

use fsonos_core::playlists::PlaylistContents;
use fsonos_core::{CoreError, HouseholdState, control, playlists, resolve_room};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayerId};
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

fn read(
    transport: &impl Transport,
    houses: &[HouseholdState],
    room: &str,
    playlist: &str,
    offset: u32,
    limit: u32,
    expected: Option<&str>,
) -> Result<PlaylistContents, CoreError> {
    let target = resolve_room(houses, room).unwrap();
    playlists::contents(
        transport,
        houses,
        &target.coordinator.id,
        playlist,
        offset,
        limit,
        expected,
    )
}

#[test]
fn both_households_browse_ordered_occurrences_without_touching_playback() {
    let (sim, houses) = fixture();
    for (room, generation) in [("Kitchen", 1), ("Den", 2)] {
        let target = resolve_room(&houses, room).unwrap();
        let lan = sim.lan();
        soap::add_uri_to_queue(
            &lan,
            target.coordinator.ip,
            "x-file-cifs://example.invalid/current.flac",
            "",
            false,
        )
        .unwrap();
        control::play_queue_from(&lan, &houses, &target.coordinator.id, 1).unwrap();
        let before = control::playback(&lan, &houses, &target.coordinator.id).unwrap();
        let queue = content::browse_snapshot(&lan, target.coordinator.ip, QUEUE).unwrap();
        let start = sim.soap_log().len();

        let first = read(&lan, &houses, room, "SQ:7", 0, 2, None).unwrap();
        assert_eq!(first.playlist.id, "SQ:7");
        assert_eq!(
            (first.offset, first.total, first.next_offset),
            (0, 3, Some(2))
        );
        assert_eq!(first.items.len(), 2);
        assert_ne!(first.items[0].res, first.items[1].res);
        assert_eq!(first.items[0].creator.as_deref(), Some("Sim Saved Artist"));
        assert_eq!(
            first.items[0].album,
            Some(format!("S{generation} Saved Album"))
        );
        assert_eq!(
            first.items[0].res.as_ref().unwrap().duration_secs(),
            Some(180)
        );
        assert!(first.items[0].album_art_uri.is_some());
        assert!(first.items[0].desc.is_some());
        let last = read(&lan, &houses, room, "SQ:7", 2, 100, Some(&first.revision)).unwrap();
        assert_eq!((last.offset, last.total, last.next_offset), (2, 3, None));
        assert_eq!(last.items.len(), 1);
        assert_eq!(first.revision, last.revision);
        assert_eq!(first.items[0].res, last.items[0].res);
        assert_ne!(first.items[0].id, last.items[0].id);
        assert_eq!(last.items[0].id, "SQ:7/3");
        assert!(
            sim.soap_log()[start..]
                .iter()
                .all(|entry| { matches!(entry.action.as_str(), "GetZoneGroupState" | "Browse") })
        );
        assert_eq!(sim.soap_log().len() - start, 12, "six reads per page");
        assert_eq!(
            content::browse_snapshot(&lan, target.coordinator.ip, QUEUE).unwrap(),
            queue
        );
        assert_eq!(
            control::playback(&lan, &houses, &target.coordinator.id).unwrap(),
            before
        );
    }
}

#[test]
fn terminal_and_past_end_pages_remain_empty_at_the_same_revision() {
    let (sim, houses) = fixture();
    let lan = sim.lan();
    let first = read(&lan, &houses, "Kitchen", "SQ:7", 0, 1, None).unwrap();
    for offset in [3, 100] {
        let page = read(
            &lan,
            &houses,
            "Kitchen",
            "SQ:7",
            offset,
            100,
            Some(&first.revision),
        )
        .unwrap();
        assert_eq!(page.offset, offset);
        assert_eq!(page.total, 3);
        assert_eq!(page.revision, first.revision);
        assert!(page.items.is_empty());
        assert!(page.next_offset.is_none());
    }
}

#[test]
fn continuations_are_bound_to_household_player_playlist_boot_and_group() {
    let (sim, houses) = fixture();
    let lan = sim.lan();
    let first = read(&lan, &houses, "Kitchen", "SQ:7", 0, 1, None).unwrap();
    for (room, playlist) in [("Den", "SQ:7"), ("Office", "SQ:7"), ("Kitchen", "SQ:1")] {
        assert!(matches!(
            read(&lan, &houses, room, playlist, 1, 1, Some(&first.revision)),
            Err(CoreError::PlaylistChanged(_))
        ));
    }
    let mut changed_house = houses.clone();
    changed_house[0].id = Some(HouseholdId("synthetic-replacement-household".into()));
    assert!(matches!(
        read(
            &lan,
            &changed_house,
            "Kitchen",
            "SQ:7",
            1,
            1,
            Some(&first.revision)
        ),
        Err(CoreError::PlaylistChanged(_))
    ));
    sim.reboot("Kitchen", Duration::ZERO).unwrap();
    assert!(matches!(
        read(
            &lan,
            &houses,
            "Kitchen",
            "SQ:7",
            1,
            1,
            Some(&first.revision)
        ),
        Err(CoreError::PlaylistChanged(_))
    ));
    let rebooted = read(&lan, &houses, "Kitchen", "SQ:7", 0, 1, None).unwrap();
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    control::join(&lan, &houses, &office.player.id, &kitchen.coordinator.id).unwrap();
    assert!(matches!(
        read(
            &lan,
            &households(&sim),
            "Kitchen",
            "SQ:7",
            1,
            1,
            Some(&rebooted.revision)
        ),
        Err(CoreError::PlaylistChanged(_))
    ));
}

#[test]
fn a_new_catalog_revision_invalidates_an_old_contents_continuation() {
    let (sim, houses) = fixture();
    let lan = sim.lan();
    let first = read(&lan, &houses, "Kitchen", "SQ:7", 0, 1, None).unwrap();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    soap::add_uri_to_queue(&lan, target.coordinator.ip, "test:one", "", false).unwrap();
    playlists::save_queue(&lan, &houses, &target.coordinator.id, "Another saved mix").unwrap();
    assert!(matches!(
        read(
            &lan,
            &houses,
            "Kitchen",
            "SQ:7",
            1,
            1,
            Some(&first.revision)
        ),
        Err(CoreError::PlaylistChanged(_))
    ));
    let fresh = read(&lan, &houses, "Kitchen", "SQ:7", 0, 1, None).unwrap();
    assert_ne!(fresh.revision, first.revision);
    assert_eq!(fresh.items, first.items);
}

#[derive(Clone, Copy)]
enum Change {
    ShortPages,
    Empty,
    Stalled,
    BadCount,
    HugeTotal,
    MissingResource,
    ForeignMetadata,
    CatalogTitle,
    ContentCounter,
    ContentTitle,
    LostRead,
}

struct Altered {
    lan: SimLan,
    change: Change,
    children: Cell<u32>,
    metadata: Cell<u32>,
}

impl Altered {
    fn new(sim: &SimHandle, change: Change) -> Self {
        Self {
            lan: sim.lan(),
            change,
            children: Cell::new(0),
            metadata: Cell::new(0),
        }
    }
}

fn field(mut xml: String, name: &str, value: &str) -> String {
    let open = format!("<{name}>");
    let start = xml.find(&open).unwrap() + open.len();
    let end = start + xml[start..].find(&format!("</{name}>")).unwrap();
    xml.replace_range(start..end, value);
    xml
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
        let child = body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>");
        let metadata = body.contains("<BrowseFlag>BrowseMetadata</BrowseFlag>");
        let request = if child && matches!(self.change, Change::ShortPages) {
            field(body.into(), "RequestedCount", "1")
        } else {
            body.into()
        };
        let mut response = self.lan.soap_post(host, path, action, &request)?;
        if child {
            let call = self.children.get() + 1;
            self.children.set(call);
            match self.change {
                Change::Empty | Change::Stalled => {
                    response = field(response, "Result", "");
                    response = field(response, "NumberReturned", "0");
                    if matches!(self.change, Change::Empty) {
                        response = field(response, "TotalMatches", "0");
                    }
                }
                Change::BadCount => response = field(response, "NumberReturned", "99"),
                Change::HugeTotal => response = field(response, "TotalMatches", "100001"),
                Change::MissingResource => {
                    response = response
                        .replace("&lt;res ", "&lt;unavailable ")
                        .replace("&lt;/res&gt;", "&lt;/unavailable&gt;");
                }
                Change::ContentCounter if call == 2 => response = field(response, "UpdateID", "42"),
                Change::ContentTitle if call == 2 => {
                    response = response.replace("Saved", "Changed")
                }
                Change::LostRead if call == 2 => {
                    return Err(ProtoError::Network {
                        target: host.to_string(),
                        detail: "second contents read unavailable".into(),
                    });
                }
                _ => {}
            }
        }
        if metadata {
            let call = self.metadata.get() + 1;
            self.metadata.set(call);
            match self.change {
                Change::ForeignMetadata => response = response.replace("SQ:7", "SQ:1"),
                Change::CatalogTitle if call == 2 => {
                    response = response.replace("Evening", "Changed")
                }
                _ => {}
            }
        }
        Ok(response)
    }
}

#[test]
fn short_pages_continue_at_actual_occurrences_and_empty_playlists_stay_empty() {
    let (sim, houses) = fixture();
    let short = Altered::new(&sim, Change::ShortPages);
    let first = read(&short, &houses, "Kitchen", "SQ:7", 0, 100, None).unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.next_offset, Some(1));
    let next = read(
        &short,
        &houses,
        "Kitchen",
        "SQ:7",
        1,
        100,
        Some(&first.revision),
    )
    .unwrap();
    assert_eq!(next.items[0].id, "SQ:7/2");
    assert_eq!(next.next_offset, Some(2));
    assert_eq!(first.revision, next.revision);
    let empty = read(
        &Altered::new(&sim, Change::Empty),
        &houses,
        "Kitchen",
        "SQ:7",
        0,
        100,
        None,
    )
    .unwrap();
    assert_eq!(empty.total, 0);
    assert!(empty.items.is_empty());
    assert!(empty.next_offset.is_none());
}

#[test]
fn malformed_counters_and_foreign_metadata_never_become_a_contents_page() {
    let (sim, houses) = fixture();
    for change in [
        Change::Stalled,
        Change::BadCount,
        Change::HugeTotal,
        Change::ForeignMetadata,
    ] {
        assert!(matches!(
            read(
                &Altered::new(&sim, change),
                &houses,
                "Kitchen",
                "SQ:7",
                0,
                100,
                None
            ),
            Err(CoreError::Proto(ProtoError::Malformed(_)))
        ));
    }
}

#[test]
fn unavailable_resources_do_not_remove_occurrences() {
    let (sim, houses) = fixture();
    let page = read(
        &Altered::new(&sim, Change::MissingResource),
        &houses,
        "Kitchen",
        "SQ:7",
        0,
        100,
        None,
    )
    .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.items.len(), 3);
    assert!(page.items.iter().all(|item| item.res.is_none()));
    assert_eq!(page.items[2].id, "SQ:7/3");
}

#[test]
fn changed_catalog_or_children_are_not_published_even_when_counters_do_not_move() {
    let (sim, houses) = fixture();
    for change in [
        Change::CatalogTitle,
        Change::ContentCounter,
        Change::ContentTitle,
    ] {
        assert!(matches!(
            read(
                &Altered::new(&sim, change),
                &houses,
                "Kitchen",
                "SQ:7",
                0,
                100,
                None
            ),
            Err(CoreError::PlaylistChanged(_))
        ));
    }
    assert!(matches!(
        read(
            &Altered::new(&sim, Change::LostRead),
            &houses,
            "Kitchen",
            "SQ:7",
            0,
            100,
            None
        ),
        Err(CoreError::Proto(ProtoError::Network { .. }))
    ));
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid requests must not reach the network")
    }
}

#[test]
fn malformed_requests_are_rejected_before_any_network_io() {
    let coordinator = PlayerId("synthetic-coordinator".into());
    for (id, offset, limit, expected) in [
        ("SQ:", 0, 100, None),
        ("SQ:7/1", 0, 100, None),
        ("favorite", 0, 100, None),
        ("file:///savedqueues.rsq#7", 0, 100, None),
        ("SQ:7", 0, 0, None),
        ("SQ:7", 0, 101, None),
        ("SQ:7", u32::MAX, 1, Some("stale")),
        ("SQ:7", 1, 1, None),
        ("SQ:7", 0, 1, Some("")),
        ("SQ:7", 0, 1, Some("line\nbreak")),
    ] {
        assert!(matches!(
            playlists::contents(&NoIo, &[], &coordinator, id, offset, limit, expected),
            Err(CoreError::Proto(ProtoError::Malformed(_)))
        ));
    }
}
