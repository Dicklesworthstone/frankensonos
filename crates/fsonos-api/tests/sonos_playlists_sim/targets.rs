//! One reviewed native source can feed several independent queues without
//! replacing its revision with each destination's unrelated source authority.

use super::*;
use fsonos_api::surface::sonos_playlists::SonosPlaylistTracksRequest;
use fsonos_core::actions::BeforeState;
use fsonos_core::rooms::ControlTarget;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_sim::SoapLogEntry;
use fsonos_types::{PlayMode, PlayerId};

struct House {
    sim: Arc<SimHandle>,
    households: Vec<HouseholdState>,
    directory: tempfile::TempDir,
}

impl House {
    fn new() -> Self {
        let sim = Arc::new(
            SimHousehold::builder()
                .s1(["Den", "Member", "Office"]
                    .map(|room| SimPlayerSpec::new(room, SimModel::Play5Gen1)))
                .s2(["Den", "Member", "Office"].map(|room| SimPlayerSpec::new(room, SimModel::One)))
                .spawn()
                .unwrap(),
        );
        for generation in [1, 2] {
            sim.retain_favorites(generation, |_| false).unwrap();
            let player = |room| {
                sim.players()
                    .iter()
                    .find(|player| player.generation == generation && player.room == room)
                    .unwrap()
            };
            soap::join_group(
                &sim.lan(),
                player("Member").ip,
                &PlayerId(player("Den").uuid.clone()),
            )
            .unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("aliases.toml"),
            "[aliases]\nselected_s1 = [\"Member@S1\", \"Den@S1\", \"Office@S1\"]\nselected_s2 = [\"Member@S2\", \"Den@S2\", \"Office@S2\"]\n",
        )
        .unwrap();
        let house = Self {
            households: households(&sim),
            sim,
            directory,
        };
        let reader = house.surface(Box::new(house.sim.lan()), Policy::default());
        for generation in ["S1", "S2"] {
            for (room, track) in [("Den", 2), ("Office", 1)] {
                let zone = format!("{room}@{generation}");
                seed_native_queue(&reader, &zone);
                let target = house.target(&zone);
                control::play_queue_from(
                    &house.sim.lan(),
                    &house.households,
                    &target.coordinator.id,
                    track,
                )
                .unwrap();
                soap::pause(&house.sim.lan(), target.coordinator.ip).unwrap();
                soap::seek_position(&house.sim.lan(), target.coordinator.ip, 17).unwrap();
                set_play_mode(
                    &house.sim.lan(),
                    target.coordinator.ip,
                    PlayMode::ShuffleRepeatOne,
                )
                .unwrap();
            }
        }
        for player in house.sim.players() {
            soap::set_volume(&house.sim.lan(), player.ip, 80).unwrap();
        }
        house
    }

    fn target(&self, zone: &str) -> ControlTarget<'_> {
        resolve_room(&self.households, zone).unwrap()
    }

    fn surface(&self, transport: Box<dyn Transport + Send + Sync>, policy: Policy) -> Surface {
        surface(transport, &self.households, policy)
            .with_aliases_file(self.directory.path().join("aliases.toml"))
    }

    fn queue(&self, zone: &str) -> Vec<String> {
        queue(&self.sim, &self.households, zone)
    }

    fn request(
        &self,
        surface: &Surface,
        generation: &str,
        target: &str,
        mode: SonosPlaylistTrackMode,
    ) -> SonosPlaylistTracksRequest {
        let zone = format!("Den@{generation}");
        let page = surface
            .sonos_playlist_contents(&Client::Cli, &contents(&zone, "SQ:7", 100))
            .unwrap();
        SonosPlaylistTracksRequest {
            zone,
            target: Some(target.into()),
            expected_target_revision: Some(
                surface
                    .resolve_rooms(&Client::Cli, target)
                    .unwrap()
                    .revision,
            ),
            playlist: page.playlist.id,
            positions: vec![3, 2, 3],
            expected_revision: page.revision,
            mode,
            expected_queue_revision: None,
        }
    }

    fn writes(&self, start: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
            .collect()
    }

    fn undo(&self, surface: &Surface) {
        let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
        assert!(preview.undoable);
        let undone = surface.undo_checked(&Client::McpStdio, preview.id).unwrap();
        assert!(undone.failures.is_empty(), "{undone:?}");
    }
}

#[test]
fn native_selection_plays_once_per_alias_group_with_caps_and_one_complete_undo() {
    for generation in ["S1", "S2"] {
        let house = House::new();
        let mut policy = Policy::default();
        policy.defaults.max_volume = 25;
        let surface = house.surface(Box::new(house.sim.lan()), policy);
        let alias = format!("selected_{}", generation.to_lowercase());
        let request = house.request(&surface, generation, &alias, SonosPlaylistTrackMode::Play);
        let original = house.queue(&format!("Den@{generation}"));
        let start = house.sim.soap_log().len();
        let selected = surface
            .sonos_playlist_tracks(&Client::McpStdio, &request)
            .unwrap();
        assert!(selected.complete() && selected.changed);
        let batch = selected.batch.as_ref().unwrap();
        assert_eq!(
            batch.results.len(),
            2,
            "the grouped alias member is not a third target"
        );
        assert!(batch.complete);
        assert_eq!(selected.positions, [3, 2, 3]);
        assert_eq!(selected.titles[0], selected.titles[2]);
        assert!(selected.queue_batch.is_none());
        for room in ["Den", "Office"] {
            let zone = format!("{room}@{generation}");
            let target = house.target(&zone);
            assert_eq!(
                house.queue(&zone),
                [
                    original[2].clone(),
                    original[1].clone(),
                    original[2].clone()
                ]
            );
            assert_eq!(
                get_play_mode(&house.sim.lan(), target.coordinator.ip).unwrap(),
                PlayMode::Normal
            );
            assert_eq!(
                soap::get_position_info(&house.sim.lan(), target.coordinator.ip)
                    .unwrap()
                    .track,
                1
            );
            assert_eq!(
                soap::get_transport_info(&house.sim.lan(), target.coordinator.ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
        }
        for room in ["Den", "Member", "Office"] {
            let target = house.target(&format!("{room}@{generation}"));
            assert_eq!(
                soap::get_volume(&house.sim.lan(), target.player.ip).unwrap(),
                25
            );
        }
        let writes = house.writes(start);
        assert_eq!(
            writes
                .iter()
                .filter(|call| call.action == "AddURIToQueue")
                .count(),
            6
        );
        assert!(
            !writes
                .iter()
                .any(|call| call.action == "RemoveAllTracksFromQueue")
        );
        let actions = surface
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        let before =
            BeforeState::parse(actions[0].action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(before.queue_undo.len(), 2);
        house.undo(&surface);
        for (room, track) in [("Den", 2), ("Office", 1)] {
            let zone = format!("{room}@{generation}");
            let host = house.target(&zone).coordinator.ip;
            assert_eq!(house.queue(&zone), original);
            let position = soap::get_position_info(&house.sim.lan(), host).unwrap();
            assert_eq!((position.track, position.position_secs), (track, Some(17)));
            assert_eq!(
                get_play_mode(&house.sim.lan(), host).unwrap(),
                PlayMode::ShuffleRepeatOne
            );
        }
    }
}

#[test]
fn native_next_and_append_keep_each_playhead_silent_and_undo_both_exact_ranges() {
    for generation in ["S1", "S2"] {
        for mode in [SonosPlaylistTrackMode::Next, SonosPlaylistTrackMode::Append] {
            let house = House::new();
            let surface = house.surface(Box::new(house.sim.lan()), Policy::default());
            let request = house.request(&surface, generation, &format!("all@{generation}"), mode);
            let original = house.queue(&format!("Den@{generation}"));
            let before = ["Den", "Office"].map(|room| {
                let zone = format!("{room}@{generation}");
                control::playback(
                    &house.sim.lan(),
                    &house.households,
                    &house.target(&zone).coordinator.id,
                )
                .unwrap()
            });
            let start = house.sim.soap_log().len();
            let selected = surface
                .sonos_playlist_tracks(&Client::McpStdio, &request)
                .unwrap();
            assert!(selected.complete() && selected.changed);
            assert_eq!(
                (selected.first, selected.count, selected.playing_queue),
                (None, None, None)
            );
            assert!(selected.batch.is_none());
            let batch = selected.queue_batch.as_ref().unwrap();
            assert!(batch.complete && batch.logged && batch.undoable);
            assert_eq!(batch.added.len(), 2);
            assert!(batch.failed.is_empty());
            for row in &batch.added {
                let zone = format!("{}@{}", row.queue.zone, row.household);
                let first = if mode == SonosPlaylistTrackMode::Append {
                    4
                } else if row.queue.zone == "Den" {
                    3
                } else {
                    2
                };
                assert_eq!(
                    (row.queue.first, row.queue.count, row.queue.playing_queue),
                    (first, 3, true)
                );
                let mut expected = original.clone();
                expected.splice(
                    (first - 1) as usize..(first - 1) as usize,
                    [
                        original[2].clone(),
                        original[1].clone(),
                        original[2].clone(),
                    ],
                );
                assert_eq!(house.queue(&zone), expected);
            }
            assert!(
                house
                    .writes(start)
                    .iter()
                    .all(|call| call.action == "AddURIToQueue")
            );
            for (i, room) in ["Den", "Office"].into_iter().enumerate() {
                let zone = format!("{room}@{generation}");
                let after = control::playback(
                    &house.sim.lan(),
                    &house.households,
                    &house.target(&zone).coordinator.id,
                )
                .unwrap();
                assert_eq!(after, before[i]);
            }
            for player in house.sim.players() {
                assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 80);
            }
            let actions = surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(actions.len(), 1);
            let before =
                BeforeState::parse(actions[0].action.before_state.as_deref().unwrap()).unwrap();
            assert_eq!(before.queued.len(), 2);
            assert!(before.queued.iter().all(|range| range.proof.is_some()));
            house.undo(&surface);
            for room in ["Den", "Office"] {
                assert_eq!(house.queue(&format!("{room}@{generation}")), original);
            }
        }
    }
}

#[test]
fn another_destination_uses_the_original_source_revision_and_its_own_queue_revision() {
    for mode in [
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ] {
        let house = House::new();
        let surface = house.surface(Box::new(house.sim.lan()), Policy::default());
        let mut request = house.request(&surface, "S2", "Office@S2", mode);
        request.expected_queue_revision = surface
            .queue(&Client::Cli, "Office@S2", 0, 1)
            .unwrap()
            .revision;
        let destination_page = surface
            .sonos_playlist_contents(&Client::Cli, &contents("Office@S2", "SQ:7", 100))
            .unwrap();
        assert_ne!(request.expected_revision, destination_page.revision);
        let source = house.target("Den@S2");
        let source_before =
            control::playback(&house.sim.lan(), &house.households, &source.coordinator.id).unwrap();
        let source_queue = house.queue("Den@S2");
        let selected = surface
            .sonos_playlist_tracks(&Client::McpStdio, &request)
            .unwrap();
        assert!(selected.complete() && selected.changed);
        assert_eq!(selected.zone, "Office");
        assert_eq!(house.queue("Den@S2"), source_queue);
        assert_eq!(
            control::playback(&house.sim.lan(), &house.households, &source.coordinator.id).unwrap(),
            source_before
        );
        house.undo(&surface);
        assert_eq!(house.queue("Office@S2"), source_queue);
    }
}

#[test]
fn invalid_source_audience_and_scalar_queue_proofs_reject_every_group_before_writes() {
    let house = House::new();
    let surface = house.surface(Box::new(house.sim.lan()), Policy::default());
    for mode in [
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ] {
        let reviewed = house.request(&surface, "S2", "all@S2", mode);
        for case in 0..6 {
            let mut request = reviewed.clone();
            match case {
                0 => request.expected_revision = "stale-source-revision".into(),
                1 => request.expected_target_revision = Some("0".repeat(64)),
                2 => {
                    request.expected_queue_revision = surface
                        .queue(&Client::Cli, "Den@S2", 0, 1)
                        .unwrap()
                        .revision
                }
                3 => {
                    request.target = Some("all".into());
                    request.expected_target_revision = None;
                }
                4 => {
                    request.target = Some("Office@S1".into());
                    request.expected_target_revision = None;
                }
                _ => {
                    request.zone = "Office@S2".into();
                }
            }
            let start = house.sim.soap_log().len();
            let failure = surface
                .sonos_playlist_tracks(&Client::McpStdio, &request)
                .unwrap_err();
            assert_eq!(
                failure.code,
                match case {
                    1 => ErrorCode::NotCoordinator,
                    2 => ErrorCode::InvalidArgument,
                    3 | 4 => ErrorCode::CrossHouseholdGroup,
                    _ => ErrorCode::QueueChanged,
                }
            );
            assert!(house.writes(start).is_empty(), "case {case}: {failure:?}");
            if matches!(case, 1..=4) {
                assert!(
                    !house.sim.soap_log()[start..]
                        .iter()
                        .any(|call| call.action == "Browse")
                );
            }
        }
    }
}

struct ChangeDestinationDuringSourceRead {
    inner: SimLan,
    source: IpAddr,
    destination: IpAddr,
    changed: AtomicUsize,
}

impl Transport for ChangeDestinationDuringSourceRead {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if host == self.source
            && action.ends_with("#Browse\"")
            && body.contains(">SQ:7<")
            && self.changed.fetch_add(1, Ordering::SeqCst) == 0
        {
            soap::add_uri_to_queue(
                &self.inner,
                self.destination,
                "x-file-cifs://example.invalid/external.flac",
                "",
                false,
            )?;
        }
        Ok(response)
    }
}

#[test]
fn all_destination_proofs_are_rechecked_after_native_source_reads_before_the_first_write() {
    for mode in [
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ] {
        let house = House::new();
        let reader = house.surface(Box::new(house.sim.lan()), Policy::default());
        let request = house.request(&reader, "S2", "all@S2", mode);
        let original = house.queue("Den@S2");
        let surface = house.surface(
            Box::new(ChangeDestinationDuringSourceRead {
                inner: house.sim.lan(),
                source: house.target("Den@S2").coordinator.ip,
                destination: house.target("Office@S2").coordinator.ip,
                changed: AtomicUsize::new(0),
            }),
            Policy::default(),
        );
        let start = house.sim.soap_log().len();
        let failure = surface
            .sonos_playlist_tracks(&Client::McpStdio, &request)
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::QueueChanged);
        let writes = house.writes(start);
        assert_eq!(writes.len(), 1, "only the external queue change is allowed");
        assert_eq!(writes[0].action, "AddURIToQueue");
        assert_eq!(house.queue("Den@S2"), original);
        let mut destination = original;
        destination.push("x-file-cifs://example.invalid/external.flac".into());
        assert_eq!(house.queue("Office@S2"), destination);
    }
}

struct RefuseDestinationAdd {
    inner: SimLan,
    destination: IpAddr,
    after_prefix: bool,
    adds: AtomicUsize,
}

impl Transport for RefuseDestinationAdd {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if host == self.destination && action.ends_with("#AddURIToQueue\"") {
            let at = self.adds.fetch_add(1, Ordering::SeqCst);
            if !self.after_prefix || at == 1 {
                return Err(ProtoError::SoapFault {
                    code: 711,
                    reason: "selected destination refused this native occurrence".into(),
                });
            }
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn native_target_write_failures_keep_success_receipts_without_claiming_uncertain_undo() {
    for after_prefix in [false, true] {
        let house = House::new();
        let reader = house.surface(Box::new(house.sim.lan()), Policy::default());
        let request = house.request(&reader, "S2", "all@S2", SonosPlaylistTrackMode::Append);
        let original = house.queue("Den@S2");
        let surface = house.surface(
            Box::new(RefuseDestinationAdd {
                inner: house.sim.lan(),
                destination: house.target("Office@S2").coordinator.ip,
                after_prefix,
                adds: AtomicUsize::new(0),
            }),
            Policy::default(),
        );
        let start = house.sim.soap_log().len();
        let selected = surface
            .sonos_playlist_tracks(&Client::McpStdio, &request)
            .unwrap();
        assert!(!selected.complete());
        let batch = selected.queue_batch.unwrap();
        assert!(!batch.complete && batch.logged);
        assert_eq!(batch.added.len(), 1);
        assert_eq!(batch.added[0].queue.zone, "Den");
        assert_eq!(batch.failed.len(), 1);
        assert_eq!(batch.failed[0].zone, "Office");
        assert!(!batch.failed[0].error.retryable);
        // The reviewed-audience fence conservatively treats any attempted
        // write with a failed outcome as uncertain, including a first SOAP fault.
        assert_eq!(batch.failed[0].error.code, ErrorCode::OutcomeUncertain);
        assert_eq!(house.queue("Den@S2").len(), original.len() + 3);
        assert_eq!(
            house.queue("Office@S2").len(),
            original.len() + usize::from(after_prefix)
        );
        assert_eq!(
            house
                .writes(start)
                .iter()
                .filter(|call| call.action == "AddURIToQueue")
                .count(),
            3 + usize::from(after_prefix)
        );
        assert!(!batch.undoable);
        let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
        assert!(!preview.undoable);
    }
}

#[test]
fn an_uncertain_single_target_without_confirmed_rows_still_reports_attempted_changes() {
    let house = House::new();
    let surface = house.surface(Box::new(house.sim.lan()), Policy::default());
    let request = house.request(&surface, "S2", "Office@S2", SonosPlaylistTrackMode::Append);
    let original = house.queue("Office@S2");
    house
        .sim
        .upnp_fault(
            &house.target("Office@S2").coordinator.id.0,
            "AddURIToQueue",
            711,
        )
        .unwrap();
    let start = house.sim.soap_log().len();
    let selected = surface
        .sonos_playlist_tracks(&Client::McpStdio, &request)
        .unwrap();
    assert!(!selected.complete());
    assert!(
        selected.changed,
        "an uncertain attempted write requires a fresh queue read"
    );
    assert_eq!((selected.first, selected.count), (None, None));
    let batch = selected.queue_batch.unwrap();
    assert!(!batch.complete && batch.logged && !batch.undoable);
    assert!(batch.added.is_empty());
    assert_eq!(batch.failed.len(), 1);
    assert_eq!(batch.failed[0].zone, "Office");
    assert_eq!(batch.failed[0].error.code, ErrorCode::OutcomeUncertain);
    assert!(!batch.failed[0].error.retryable);
    assert_eq!(
        house
            .writes(start)
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        1,
        "the refused write is never replayed"
    );
    assert_eq!(house.queue("Office@S2"), original);
    let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
    assert!(!preview.undoable);
}

struct RenameSourceAfterFirstGroup {
    inner: SimLan,
    source: IpAddr,
    title: String,
    adds: AtomicUsize,
}

impl Transport for RenameSourceAfterFirstGroup {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#AddURIToQueue\"") && self.adds.fetch_add(1, Ordering::SeqCst) == 2 {
            fsonos_proto::content::rename_object(
                &self.inner,
                self.source,
                "SQ:7",
                &self.title,
                "Changed by another controller",
            )?;
        }
        Ok(response)
    }
}

#[test]
fn native_source_changes_between_groups_keep_the_first_receipt_and_refuse_the_second() {
    let house = House::new();
    let reader = house.surface(Box::new(house.sim.lan()), Policy::default());
    let request = house.request(&reader, "S2", "all@S2", SonosPlaylistTrackMode::Append);
    let title = reader
        .sonos_playlist_contents(&Client::Cli, &contents("Den@S2", "SQ:7", 100))
        .unwrap()
        .playlist
        .title;
    let original = house.queue("Den@S2");
    let surface = house.surface(
        Box::new(RenameSourceAfterFirstGroup {
            inner: house.sim.lan(),
            source: house.target("Den@S2").coordinator.ip,
            title,
            adds: AtomicUsize::new(0),
        }),
        Policy::default(),
    );
    let start = house.sim.soap_log().len();
    let selected = surface
        .sonos_playlist_tracks(&Client::McpStdio, &request)
        .unwrap();
    assert!(!selected.complete());
    let batch = selected.queue_batch.unwrap();
    assert_eq!(batch.added.len(), 1);
    assert_eq!(batch.failed.len(), 1);
    assert_eq!(batch.failed[0].error.code, ErrorCode::QueueChanged);
    assert!(batch.logged && batch.undoable);
    let successful = format!("{}@S2", batch.added[0].queue.zone);
    let refused = format!("{}@S2", batch.failed[0].zone);
    assert_ne!(successful, refused);
    assert_eq!(house.queue(&successful).len(), original.len() + 3);
    assert_eq!(house.queue(&refused), original);
    assert_eq!(
        house
            .writes(start)
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        3
    );
    house.undo(&surface);
    assert_eq!(house.queue("Den@S2"), original);
    assert_eq!(house.queue("Office@S2"), original);
}

mod queue_selection;
