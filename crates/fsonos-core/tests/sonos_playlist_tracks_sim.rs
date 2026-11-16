//! Native saved-order edits through real localhost S1/S2 SOAP. An edit cannot
//! rebuild the playing queue or disturb its selected duplicate occurrence.

use fsonos_core::playlists::{PlaylistContents, PlaylistTrackChange};
use fsonos_core::{CoreError, HouseholdState, control, favorites, playlists};
use fsonos_proto::content::{self, BrowseSnapshot, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::DidlObject;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, queue};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const PLAYLIST: &str = "SQ:7";
const ACTION: &str = "ReorderTracksInSavedQueue";

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Alpha").unwrap().ip;
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        house.id = Some(HouseholdId(sim.player("Alpha").unwrap().household.clone()));
        let houses = vec![house];
        let selected = playlists::list(&sim.lan(), &houses, &coordinator)
            .unwrap()
            .into_iter()
            .find(|playlist| playlist.id == PLAYLIST)
            .unwrap();
        favorites::play(&sim.lan(), &houses, &coordinator, &selected).unwrap();
        soap::seek_track(&sim.lan(), host, 3).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::RepeatAll).unwrap();
        soap::set_volume(&sim.lan(), host, 28).unwrap();
        Self {
            sim,
            houses,
            coordinator,
            host,
        }
    }

    fn contents(&self) -> PlaylistContents {
        playlists::contents(
            &self.sim.lan(),
            &self.houses,
            &self.coordinator,
            PLAYLIST,
            0,
            100,
            None,
        )
        .unwrap()
    }

    fn playback(&self) -> (BrowseSnapshot, control::Playback, PlayMode, u8, bool) {
        let lan = self.sim.lan();
        (
            content::browse_snapshot(&lan, self.host, QUEUE).unwrap(),
            control::playback(&lan, &self.houses, &self.coordinator).unwrap(),
            get_play_mode(&lan, self.host).unwrap(),
            soap::get_volume(&lan, self.host).unwrap(),
            soap::get_mute(&lan, self.host).unwrap(),
        )
    }

    fn change<T: Transport + ?Sized>(
        &self,
        transport: &T,
        remove: bool,
        revision: &str,
    ) -> Result<PlaylistTrackChange, CoreError> {
        if remove {
            playlists::remove_tracks(
                transport,
                &self.houses,
                &self.coordinator,
                PLAYLIST,
                revision,
                1,
                2,
            )
        } else {
            playlists::move_tracks(
                transport,
                &self.houses,
                &self.coordinator,
                PLAYLIST,
                revision,
                1,
                2,
                4,
            )
        }
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action.clone())
            .collect()
    }
}

fn normalized(items: &[DidlObject]) -> Vec<DidlObject> {
    items
        .iter()
        .cloned()
        .map(|mut item| {
            item.id.clear();
            item
        })
        .collect()
}

#[test]
fn native_blocks_move_both_ways_and_remove_duplicates_without_touching_playback() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let playing = f.playback();
        let original = f.contents();
        let original_order = normalized(&original.items);
        assert_eq!(
            original_order[0], original_order[2],
            "fixture retains duplicates"
        );
        let start = f.sim.soap_log().len();
        let moved = f.change(&lan, false, &original.revision).unwrap();
        assert!(moved.changed);
        assert_eq!((moved.previous_total, moved.total), (3, 3));
        let after = f.contents();
        assert_eq!(after.revision, moved.revision);
        assert_ne!(after.revision, original.revision);
        assert_eq!(
            normalized(&after.items),
            [
                original_order[2].clone(),
                original_order[0].clone(),
                original_order[1].clone()
            ]
        );
        assert_eq!(f.playback(), playing);
        assert_eq!(f.writes_since(start), [ACTION]);

        let start = f.sim.soap_log().len();
        let moved_back = playlists::move_tracks(
            &lan,
            &f.houses,
            &f.coordinator,
            PLAYLIST,
            &after.revision,
            2,
            2,
            1,
        )
        .unwrap();
        assert!(moved_back.changed);
        assert_eq!(normalized(&f.contents().items), original_order);
        assert_eq!(f.playback(), playing);
        assert_eq!(f.writes_since(start), [ACTION]);

        let removed = f.change(&lan, true, &moved_back.revision).unwrap();
        assert_eq!((removed.previous_total, removed.total), (3, 1));
        assert_eq!(normalized(&f.contents().items), [original_order[2].clone()]);
        assert_eq!(f.playback(), playing);

        let empty = playlists::remove_tracks(
            &lan,
            &f.houses,
            &f.coordinator,
            PLAYLIST,
            &removed.revision,
            1,
            1,
        )
        .unwrap();
        assert_eq!((empty.previous_total, empty.total), (1, 0));
        assert!(f.contents().items.is_empty());
        assert_eq!(
            empty.playlist, original.playlist,
            "an empty playlist retains its identity"
        );
        assert_eq!(f.playback(), playing);
        assert!(
            playlists::list(&lan, &f.houses, &f.coordinator)
                .unwrap()
                .iter()
                .any(|playlist| playlist.id == "SQ:1")
        );
    }
}

#[test]
fn block_internal_destinations_are_verified_noops_but_stale_revisions_are_not() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let before = f.contents();
        let start = f.sim.soap_log().len();
        for destination in 1..=3 {
            let unchanged = playlists::move_tracks(
                &f.sim.lan(),
                &f.houses,
                &f.coordinator,
                PLAYLIST,
                &before.revision,
                1,
                2,
                destination,
            )
            .unwrap();
            assert!(!unchanged.changed);
            assert_eq!(unchanged.revision, before.revision);
            assert_eq!((unchanged.previous_total, unchanged.total), (3, 3));
        }
        assert!(f.writes_since(start).is_empty());
        content::rename_object(
            &f.sim.lan(),
            f.host,
            PLAYLIST,
            &before.playlist.title,
            "Changed elsewhere",
        )
        .unwrap();
        let start = f.sim.soap_log().len();
        assert!(matches!(
            playlists::move_tracks(
                &f.sim.lan(),
                &f.houses,
                &f.coordinator,
                PLAYLIST,
                &before.revision,
                1,
                2,
                1
            ),
            Err(CoreError::PlaylistChanged(_))
        ));
        assert!(matches!(
            f.change(&f.sim.lan(), true, &before.revision),
            Err(CoreError::PlaylistChanged(_))
        ));
        assert!(f.writes_since(start).is_empty());
    }
}

struct NoRequests;
impl Transport for NoRequests {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid saved-track edit must fail before speaker I/O");
    }
}

#[test]
fn malformed_edit_inputs_never_reach_a_speaker() {
    let coordinator = PlayerId("missing".into());
    for (id, revision, first, count, destination) in [
        ("Q:0", "revision", 1, 1, 2),
        ("SQ:7/1", "revision", 1, 1, 2),
        (PLAYLIST, " ", 1, 1, 2),
        (PLAYLIST, "bad\nrevision", 1, 1, 2),
        (PLAYLIST, "revision", 0, 1, 2),
        (PLAYLIST, "revision", 1, 0, 2),
        (PLAYLIST, "revision", 1, 1001, 2),
        (PLAYLIST, "revision", u32::MAX, 2, 2),
    ] {
        assert!(
            playlists::remove_tracks(&NoRequests, &[], &coordinator, id, revision, first, count)
                .is_err()
        );
        assert!(
            playlists::move_tracks(
                &NoRequests,
                &[],
                &coordinator,
                id,
                revision,
                first,
                count,
                destination
            )
            .is_err()
        );
    }
    assert!(
        playlists::move_tracks(
            &NoRequests,
            &[],
            &coordinator,
            PLAYLIST,
            "revision",
            1,
            1,
            0
        )
        .is_err()
    );
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    LostReply,
    MalformedReply,
    Unconfirmed,
    UnreadableAfter,
    RaceAtWrite,
    WrongOrder,
}

struct FailedEdit {
    lan: SimLan,
    mode: Failure,
    writes: Cell<usize>,
    title: String,
}

impl Transport for FailedEdit {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if matches!(self.mode, Failure::UnreadableAfter)
            && self.writes.get() > 0
            && name == "Browse"
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "saved contents unavailable after mutation".into(),
            });
        }
        if name != ACTION {
            return self.lan.soap_post(host, path, action, body);
        }
        self.writes.set(self.writes.get() + 1);
        if matches!(self.mode, Failure::RaceAtWrite) {
            content::rename_object(
                &self.lan,
                host,
                PLAYLIST,
                &self.title,
                "Changed before admission",
            )
            .unwrap();
        }
        if matches!(self.mode, Failure::Unconfirmed) {
            return Ok(fsonos_proto::soap::envelope(
                &fsonos_proto::soap::AV_TRANSPORT,
                &format!("{ACTION}Response"),
                "<QueueLengthChange>0</QueueLengthChange><NewQueueLength>3</NewQueueLength><NewUpdateID>2</NewUpdateID>",
            ));
        }
        let response = if matches!(self.mode, Failure::WrongOrder) {
            // A syntactically valid reply and advancing revision do not prove
            // the desired order. Apply a different native move at that revision.
            let wrong = body.replace(
                "<NewPositionList>2,1</NewPositionList>",
                "<NewPositionList>1,0</NewPositionList>",
            );
            self.lan.soap_post(host, path, action, &wrong)?
        } else {
            self.lan.soap_post(host, path, action, body)?
        };
        match self.mode {
            Failure::LostReply => Err(ProtoError::Network {
                target: host.to_string(),
                detail: "native edit applied but reply lost".into(),
            }),
            Failure::MalformedReply => Ok("invalid native acknowledgement".into()),
            _ => Ok(response),
        }
    }
}

#[test]
fn uncertain_edits_never_replay_and_cannot_become_false_success() {
    for generation in [1, 2] {
        for mode in [
            Failure::LostReply,
            Failure::MalformedReply,
            Failure::Unconfirmed,
            Failure::UnreadableAfter,
            Failure::WrongOrder,
        ] {
            let f = Fixture::new(generation);
            let observed = f.contents();
            let playing = f.playback();
            let failed = FailedEdit {
                lan: f.sim.lan(),
                mode,
                writes: Cell::new(0),
                title: observed.playlist.title,
            };
            let error = f.change(&failed, false, &observed.revision).unwrap_err();
            assert!(
                matches!(error, CoreError::Uncertain(_)),
                "{generation}/{mode:?}: {error}"
            );
            assert_eq!(failed.writes.get(), 1);
            assert_eq!(f.playback(), playing);
            let actual = f.contents();
            if matches!(mode, Failure::Unconfirmed | Failure::WrongOrder) {
                assert_eq!(normalized(&actual.items), normalized(&observed.items));
            } else {
                assert_eq!(
                    normalized(&actual.items),
                    [
                        normalized(&observed.items)[2].clone(),
                        normalized(&observed.items)[0].clone(),
                        normalized(&observed.items)[1].clone()
                    ]
                );
            }
        }
    }
}

#[test]
fn native_revision_guard_fences_a_race_after_complete_observation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let observed = f.contents();
        let playing = f.playback();
        let transport = FailedEdit {
            lan: f.sim.lan(),
            mode: Failure::RaceAtWrite,
            writes: Cell::new(0),
            title: observed.playlist.title,
        };
        assert!(matches!(
            f.change(&transport, true, &observed.revision),
            Err(CoreError::PlaylistChanged(_))
        ));
        assert_eq!(transport.writes.get(), 1);
        assert_eq!(normalized(&f.contents().items), normalized(&observed.items));
        assert_eq!(f.playback(), playing);
    }
}

#[test]
fn explicit_native_refusal_is_not_recovered_as_another_edit() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let observed = f.contents();
        let playing = f.playback();
        f.sim.upnp_fault("Alpha", ACTION, 800).unwrap();
        let start = f.sim.soap_log().len();
        assert!(matches!(
            f.change(&f.sim.lan(), true, &observed.revision),
            Err(CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
        ));
        assert_eq!(f.writes_since(start), [ACTION]);
        assert_eq!(f.contents(), observed);
        assert_eq!(f.playback(), playing);
    }
}

struct Drift<'a> {
    fixture: &'a Fixture,
    reboot: bool,
    reads: Cell<u32>,
    writes: Cell<u32>,
}

impl Transport for Drift<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == ACTION {
            self.writes.set(self.writes.get() + 1);
        }
        let response = self.fixture.sim.lan().soap_post(host, path, action, body)?;
        if name == "GetZoneGroupState" {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 2 {
                if self.reboot {
                    self.fixture.sim.reboot("Alpha", Duration::ZERO).unwrap();
                } else {
                    soap::join_group(
                        &self.fixture.sim.lan(),
                        self.fixture.sim.player("Beta").unwrap().ip,
                        &self.fixture.coordinator,
                    )
                    .unwrap();
                }
            }
        }
        Ok(response)
    }
}

#[test]
fn changed_group_or_boot_after_reading_contents_prevents_the_write() {
    for reboot in [false, true] {
        let f = Fixture::new(2);
        let observed = f.contents();
        let drift = Drift {
            fixture: &f,
            reboot,
            reads: Cell::new(0),
            writes: Cell::new(0),
        };
        assert!(f.change(&drift, true, &observed.revision).is_err());
        assert_eq!(drift.writes.get(), 0);
    }
}

struct Oversized<'a>(&'a SimLan);
impl Transport for Oversized<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.0.soap_post(host, path, action, body)?;
        if body.contains("<ObjectID>SQ:7</ObjectID>")
            && body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>")
        {
            return Ok(response.replace(
                "<TotalMatches>3</TotalMatches>",
                "<TotalMatches>1001</TotalMatches>",
            ));
        }
        assert!(
            !action.contains(ACTION),
            "oversized observation must not mutate"
        );
        Ok(response)
    }
}

#[test]
fn whole_playlist_bounds_apply_even_to_a_small_selected_range() {
    let f = Fixture::new(2);
    let observed = f.contents();
    let lan = f.sim.lan();
    let start = f.sim.soap_log().len();
    let error = f
        .change(&Oversized(&lan), true, &observed.revision)
        .unwrap_err();
    assert!(error.to_string().contains("at most 1000"), "{error}");
    assert!(f.writes_since(start).is_empty());
}

#[test]
fn direct_native_saved_revision_protects_both_removal_and_reordering() {
    let f = Fixture::new(2);
    let lan = f.sim.lan();
    let page = content::browse(
        &lan,
        f.host,
        PLAYLIST,
        content::BrowseFlag::DirectChildren,
        0,
        100,
    )
    .unwrap();
    let result = queue::reorder_saved_tracks(
        &lan,
        f.host,
        PLAYLIST,
        page.update_id,
        &[1, 0],
        Some(&[2, 1]),
    )
    .unwrap();
    assert_eq!((result.length_change, result.total), (0, 3));
    let stale = queue::reorder_saved_tracks(&lan, f.host, PLAYLIST, page.update_id, &[0], None)
        .unwrap_err();
    assert!(matches!(
        stale,
        ProtoError::SoapFault {
            code: queue::STALE_QUEUE_FAULT,
            ..
        }
    ));
    let removed =
        queue::reorder_saved_tracks(&lan, f.host, PLAYLIST, result.update_id, &[2, 1, 0], None)
            .unwrap();
    assert_eq!((removed.length_change, removed.total), (-3, 0));
}

#[test]
fn an_edit_across_native_pages_preserves_the_complete_saved_order() {
    let f = Fixture::new(2);
    let lan = f.sim.lan();
    let source = f.contents().playlist;
    // Every native container append adds three retained occurrences. The
    // resulting saved playlist crosses the real 100-item Browse page limit.
    for _ in 0..34 {
        soap::enqueue_uri(
            &lan,
            f.host,
            source.uri.as_deref().unwrap(),
            &source.metadata,
            0,
            false,
        )
        .unwrap();
    }
    let saved =
        playlists::save_queue(&lan, &f.houses, &f.coordinator, "Paged saved order").unwrap();
    let before = content::browse_snapshot(&lan, f.host, &saved.id).unwrap();
    assert_eq!(before.objects.len(), 105);
    let playing = f.playback();
    let observed =
        playlists::contents(&lan, &f.houses, &f.coordinator, &saved.id, 0, 100, None).unwrap();
    assert_eq!(observed.next_offset, Some(100));
    let start = f.sim.soap_log().len();
    let edited = playlists::move_tracks(
        &lan,
        &f.houses,
        &f.coordinator,
        &saved.id,
        &observed.revision,
        99,
        4,
        1,
    )
    .unwrap();
    assert_eq!((edited.previous_total, edited.total), (105, 105));
    let after = content::browse_snapshot(&lan, f.host, &saved.id).unwrap();
    let mut expected = normalized(&before.objects);
    let moved: Vec<_> = expected.drain(98..102).collect();
    expected.splice(0..0, moved);
    assert_eq!(normalized(&after.objects), expected);
    assert_eq!(f.writes_since(start), [ACTION]);
    assert_eq!(f.playback(), playing);
}
