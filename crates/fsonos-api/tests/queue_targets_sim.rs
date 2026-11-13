//! Multi-group queue additions retain each queue's exact range, leave playback
//! alone, and record one action. All speaker I/O uses real simulator sockets.

use fsonos_api::source::{SourceSelection, SourceSelectionKind, SpotifySources};
use fsonos_api::surface::queue::{QueueAddOutcome, QueueAddedSetDto, QueueRequest};
use fsonos_api::{ErrorCode, Failure, Surface};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::rooms::ControlTarget;
use fsonos_core::store::{ActionFilter, LoggedAction, MemStore};
use fsonos_core::{HouseholdState, favorites, inventory, playlists, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const GROUPS: [&str; 4] = ["Lead@S1", "Guest@S1", "Lead@S2", "Guest@S2"];
const ORIGINAL: [&str; 3] = [
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/last.flac",
];
const TRACK: &str = "spotify:track:1000000000000000000001";
const ARTIST: &str = "spotify:artist:2000000000000000000001";
const SHOW: &str = "spotify:show:3000000000000000000001";
const EXTERNAL: &str = "x-file-cifs://example.invalid/external.flac";

struct House {
    sim: Arc<SimHandle>,
    households: Vec<HouseholdState>,
    directory: tempfile::TempDir,
}

impl House {
    fn new() -> Self {
        let sim = Arc::new(
            SimHousehold::builder()
                .s1(["Lead", "Member", "Guest"]
                    .map(|room| SimPlayerSpec::new(room, SimModel::Play5Gen1)))
                .s2(["Lead", "Member", "Guest"].map(|room| SimPlayerSpec::new(room, SimModel::One)))
                .spawn()
                .unwrap(),
        );
        for generation in [1, 2] {
            let player = |room| {
                sim.players()
                    .iter()
                    .find(|player| player.generation == generation && player.room == room)
                    .unwrap()
            };
            soap::join_group(
                &sim.lan(),
                player("Member").ip,
                &PlayerId(player("Lead").uuid.clone()),
            )
            .unwrap();
        }
        let households = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("aliases.toml"),
            "[aliases]\nsingle = [\"Member@S1\", \"Lead@S1\"]\nselected = [\"Member@S1\", \"Lead@S1\", \"Member@S2\", \"Lead@S2\"]\n",
        ).unwrap();
        let house = Self {
            sim,
            households,
            directory,
        };
        for zone in GROUPS {
            let target = house.target(zone);
            for uri in ORIGINAL {
                soap::add_uri_to_queue(&house.sim.lan(), target.coordinator.ip, uri, "", false)
                    .unwrap();
            }
            soap::play_from_queue(
                &house.sim.lan(),
                target.coordinator.ip,
                &target.coordinator.id,
            )
            .unwrap();
            soap::seek_track(&house.sim.lan(), target.coordinator.ip, 2).unwrap();
            soap::seek_position(&house.sim.lan(), target.coordinator.ip, 17).unwrap();
            soap::play(&house.sim.lan(), target.coordinator.ip).unwrap();
            soap::pause(&house.sim.lan(), target.coordinator.ip).unwrap();
            set_play_mode(
                &house.sim.lan(),
                target.coordinator.ip,
                PlayMode::ShuffleRepeatOne,
            )
            .unwrap();
        }
        for player in house.sim.players() {
            soap::set_volume(&house.sim.lan(), player.ip, 80).unwrap();
        }
        house
    }

    fn target(&self, zone: &str) -> ControlTarget<'_> {
        resolve_room(&self.households, zone).unwrap()
    }

    fn surface(&self) -> Surface {
        self.over(Box::new(self.sim.lan()))
    }

    fn over(&self, transport: Box<dyn Transport + Send + Sync>) -> Surface {
        let known = self.households.clone();
        Surface::new(
            transport,
            Box::new(move |_| Ok(known.clone())),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_aliases_file(self.directory.path().join("aliases.toml"))
        .with_action_log(Box::new(MemStore::default()), "queue-targets")
    }

    fn queue(&self, zone: &str) -> Vec<String> {
        browse_all(&self.sim.lan(), self.target(zone).coordinator.ip, QUEUE)
            .unwrap()
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn writes(&self, start: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
            .collect()
    }

    fn playback_unchanged(&self) {
        for zone in GROUPS {
            let target = self.target(zone);
            let position = soap::get_position_info(&self.sim.lan(), target.coordinator.ip).unwrap();
            assert_eq!(
                (position.track, position.position_secs),
                (2, Some(17)),
                "{zone}"
            );
            assert_eq!(position.uri, ORIGINAL[1]);
            assert_eq!(
                soap::get_transport_info(&self.sim.lan(), target.coordinator.ip)
                    .unwrap()
                    .state,
                TransportState::Paused,
            );
            assert_eq!(
                get_play_mode(&self.sim.lan(), target.coordinator.ip).unwrap(),
                PlayMode::ShuffleRepeatOne
            );
        }
        for player in self.sim.players() {
            assert_eq!(soap::get_volume(&self.sim.lan(), player.ip).unwrap(), 80);
        }
    }
}

fn request(zone: &str, next: bool) -> QueueRequest {
    QueueRequest {
        zone: zone.into(),
        source_uri: Some(TRACK.into()),
        next,
        ..QueueRequest::default()
    }
}

fn batch(outcome: QueueAddOutcome) -> QueueAddedSetDto {
    let QueueAddOutcome::Groups(batch) = outcome else {
        panic!("expected an explicit set receipt")
    };
    batch
}

fn logged(surface: &Surface) -> LoggedAction {
    let mut actions = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    actions.pop().unwrap()
}

fn undo(surface: &Surface) {
    let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
    let report = surface.undo_checked(&Client::McpStdio, preview.id).unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
}

fn argument<'a>(call: &'a SoapLogEntry, name: &str) -> &'a str {
    &call.args.iter().find(|(key, _)| key == name).unwrap().1
}

#[test]
fn next_and_append_report_each_actual_range_and_one_undo_preserves_paused_playback() {
    for next in [false, true] {
        let house = House::new();
        let surface = house.surface();
        let start = house.sim.soap_log().len();
        let added = batch(
            surface
                .queue_request_targets(&Client::McpStdio, &request("everywhere", next))
                .unwrap(),
        );
        assert!(added.complete && added.logged && added.undoable);
        assert_eq!(added.added.len(), 4);
        assert!(added.failed.is_empty());
        let writes = house.writes(start);
        assert_eq!(writes.len(), 4);
        assert!(writes.iter().all(|call| call.action == "AddURIToQueue"));
        for row in &added.added {
            let zone = format!("{}@{}", row.queue.zone, row.household);
            assert_eq!(row.coordinator, house.target(&zone).coordinator.id.0);
            assert_eq!(
                (row.queue.first, row.queue.count),
                (if next { 3 } else { 4 }, 1)
            );
            assert!(row.queue.playing_queue);
            let mut contents = house.queue(&zone);
            let inserted = contents.remove(if next { 2 } else { 3 });
            assert_eq!(spotify_uri_from_renderer_uri(&inserted).unwrap(), TRACK);
            assert_eq!(contents, ORIGINAL);
        }
        house.playback_unchanged();
        let action = logged(&surface);
        let before = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(before.queued.len(), 4);
        assert!(before.queued.iter().all(|queued| queued.proof.is_some()));
        undo(&surface);
        for zone in GROUPS {
            assert_eq!(house.queue(zone), ORIGINAL);
        }
        house.playback_unchanged();
    }
}

#[test]
fn mixed_sources_and_playheads_return_different_positions_without_switching_music() {
    let house = House::new();
    let radio = house.target("Guest@S1").coordinator.ip;
    let first = house.target("Lead@S2").coordinator.ip;
    let stream = "x-rincon-mp3radio://example.invalid/current.mp3";
    soap::set_av_transport_uri(&house.sim.lan(), radio, stream, "").unwrap();
    soap::play(&house.sim.lan(), radio).unwrap();
    soap::seek_track(&house.sim.lan(), first, 1).unwrap();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let result = batch(
        surface
            .queue_request_targets(&Client::McpStdio, &request("everywhere", true))
            .unwrap(),
    );
    assert!(result.complete && result.undoable);
    for (zone, position, playing_queue) in [
        ("Lead@S1", 3, true),
        ("Guest@S1", 4, false),
        ("Lead@S2", 2, true),
        ("Guest@S2", 3, true),
    ] {
        let receipt = result
            .added
            .iter()
            .find(|row| row.coordinator == house.target(zone).coordinator.id.0)
            .unwrap();
        assert_eq!(
            (receipt.queue.first, receipt.queue.playing_queue),
            (position, playing_queue),
            "{zone}"
        );
    }
    assert!(
        house
            .writes(start)
            .iter()
            .all(|call| call.action == "AddURIToQueue")
    );
    assert_eq!(
        soap::get_media_info(&house.sim.lan(), radio).unwrap().uri,
        stream
    );
    assert_eq!(
        soap::get_transport_info(&house.sim.lan(), radio)
            .unwrap()
            .state,
        TransportState::Playing
    );
    assert_eq!(
        soap::get_position_info(&house.sim.lan(), first)
            .unwrap()
            .track,
        1
    );
    undo(&surface);
    for zone in GROUPS {
        assert_eq!(house.queue(zone), ORIGINAL);
    }
    assert_eq!(
        soap::get_media_info(&house.sim.lan(), radio).unwrap().uri,
        stream
    );
}

#[test]
fn a_grouped_room_alias_keeps_the_single_group_wire_shape_and_adds_only_once() {
    let house = House::new();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let req = QueueRequest {
        zone: "single".into(),
        favorite: Some("Nocturne".into()),
        next: true,
        ..QueueRequest::default()
    };
    let result = surface
        .queue_request_targets(&Client::McpStdio, &req)
        .unwrap();
    let QueueAddOutcome::Single(added) = &result else {
        panic!("one group must keep the legacy response")
    };
    assert_eq!(added.zone, "Lead");
    assert!(!added.done.contains("RINCON_"));
    assert_eq!((added.first, added.count), (3, 1));
    let value = serde_json::to_value(&result).unwrap();
    assert!(value.get("first").is_some() && value.get("added").is_none());
    let writes = house.writes(start);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].player, house.target("Lead@S1").coordinator.id.0);
    for zone in ["Guest@S1", "Lead@S2", "Guest@S2"] {
        assert_eq!(house.queue(zone), ORIGINAL);
    }
    house.playback_unchanged();
    logged(&surface);
}

#[test]
fn native_catalogs_and_metadata_stay_household_local_and_group_members_deduplicate() {
    for saved in [false, true] {
        let house = House::new();
        let surface = house.surface();
        let chosen: Vec<_> = ["Lead@S1", "Lead@S2"]
            .iter()
            .map(|zone| {
                let target = house.target(zone);
                let list = if saved {
                    playlists::list(&house.sim.lan(), &house.households, &target.coordinator.id)
                } else {
                    favorites::list(&house.sim.lan(), &house.households, &target.coordinator.id)
                }
                .unwrap();
                favorites::find(&list, if saved { "SQ:7" } else { "Nocturne" })
                    .unwrap()
                    .clone()
            })
            .collect();
        assert_ne!(chosen[0].metadata, chosen[1].metadata);
        let req = QueueRequest {
            zone: "selected".into(),
            favorite: (!saved).then(|| "Nocturne".into()),
            playlist: saved.then(|| "SQ:7".into()),
            next: true,
            ..QueueRequest::default()
        };
        let start = house.sim.soap_log().len();
        let result = batch(
            surface
                .queue_request_targets(&Client::McpStdio, &req)
                .unwrap(),
        );
        assert!(result.complete && result.undoable);
        assert_eq!(result.added.len(), 2);
        let writes = house.writes(start);
        assert_eq!(writes.len(), 2);
        for (zone, favorite) in ["Lead@S1", "Lead@S2"].iter().zip(chosen) {
            let call = writes
                .iter()
                .find(|call| call.player == house.target(zone).coordinator.id.0)
                .unwrap();
            assert_eq!(call.action, "AddURIToQueue");
            assert_eq!(
                argument(call, "EnqueuedURI"),
                favorite.uri.as_deref().unwrap()
            );
            assert_eq!(argument(call, "EnqueuedURIMetaData"), favorite.metadata);
        }
        house.playback_unchanged();
        undo(&surface);
        for zone in GROUPS {
            assert_eq!(house.queue(zone), ORIGINAL);
        }
    }
}

#[test]
fn an_unavailable_source_in_one_household_changes_no_queue_or_playback() {
    for source in ["favorite", "playlist", "spotify"] {
        let house = House::new();
        let mut req = request("everywhere", true);
        match source {
            "favorite" => {
                house
                    .sim
                    .retain_favorites(2, |uri| !uri.contains("TrackA"))
                    .unwrap();
                req.source_uri = None;
                req.favorite = Some("Nocturne".into());
            }
            "playlist" => {
                req.source_uri = None;
                req.playlist = Some("Saved Mix S1".into());
            }
            _ => {
                house.sim.retain_favorites(2, |_| false).unwrap();
            }
        }
        let surface = house.surface();
        let start = house.sim.soap_log().len();
        surface
            .queue_request_targets(&Client::McpStdio, &req)
            .unwrap_err();
        assert!(house.writes(start).is_empty());
        for zone in GROUPS {
            assert_eq!(house.queue(zone), ORIGINAL);
        }
        house.playback_unchanged();
        assert!(
            surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap()
                .is_empty()
        );
    }
}

struct Catalog {
    calls: Arc<AtomicUsize>,
    change: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Catalog {
    fn selection(&self, kind: SourceSelectionKind) -> SourceSelection {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(change) = &self.change {
            change();
        }
        let prefix = if kind == SourceSelectionKind::ShowEpisodes {
            "episode"
        } else {
            "track"
        };
        SourceSelection {
            tracks: [1, 2, 3]
                .map(|index| {
                    (
                        format!("spotify:{prefix}:100000000000000000000{index}"),
                        format!("Selected {index}"),
                    )
                })
                .to_vec(),
            kind,
        }
    }
}

impl SpotifySources for Catalog {
    fn artist_tracks(&self, uri: &str) -> Result<SourceSelection, Failure> {
        assert_eq!(uri, ARTIST);
        Ok(self.selection(SourceSelectionKind::TopTracks))
    }

    fn show_episodes(&self, uri: &str) -> Result<SourceSelection, Failure> {
        assert_eq!(uri, SHOW);
        Ok(self.selection(SourceSelectionKind::ShowEpisodes))
    }
}

#[test]
fn one_artist_or_show_lookup_supplies_every_queue_without_starting_any_group() {
    for uri in [ARTIST, SHOW] {
        let house = House::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let surface = house.surface().with_spotify_sources(Box::new(Catalog {
            calls: Arc::clone(&calls),
            change: None,
        }));
        let mut req = request("everywhere", true);
        req.source_uri = Some(uri.into());
        let start = house.sim.soap_log().len();
        let result = batch(
            surface
                .queue_request_targets(&Client::McpStdio, &req)
                .unwrap(),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(result.complete && result.undoable);
        for row in &result.added {
            assert_eq!((row.queue.first, row.queue.count), (3, 3));
            let queue = house.queue(&format!("{}@{}", row.queue.zone, row.household));
            assert_eq!(&queue[..2], &ORIGINAL[..2]);
            assert_eq!(queue[5], ORIGINAL[2]);
            let observed: Vec<_> = queue[2..5]
                .iter()
                .map(|uri| spotify_uri_from_renderer_uri(uri).unwrap())
                .collect();
            let prefix = if uri == SHOW { "episode" } else { "track" };
            assert_eq!(
                observed,
                [1, 2, 3].map(|index| format!("spotify:{prefix}:100000000000000000000{index}"))
            );
        }
        assert!(
            house
                .writes(start)
                .iter()
                .all(|call| call.action == "AddURIToQueue")
        );
        house.playback_unchanged();
        logged(&surface);
        undo(&surface);
        for zone in GROUPS {
            assert_eq!(house.queue(zone), ORIGINAL);
        }
    }
}

#[test]
fn all_queue_destinations_are_retained_before_slow_catalog_work() {
    for change in ["queue", "source", "group"] {
        let house = House::new();
        let sim = Arc::clone(&house.sim);
        let guest = house.target("Guest@S2").player.ip;
        let lead = house.target("Lead@S2").coordinator.id.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let surface = house.surface().with_spotify_sources(Box::new(Catalog {
            calls: Arc::clone(&calls),
            change: Some(Box::new(move || match change {
                "queue" => {
                    soap::add_uri_to_queue(&sim.lan(), guest, EXTERNAL, "", false).unwrap();
                }
                "source" => {
                    soap::set_av_transport_uri(&sim.lan(), guest, EXTERNAL, "").unwrap();
                }
                "group" => {
                    soap::join_group(&sim.lan(), guest, &lead).unwrap();
                }
                _ => unreachable!(),
            })),
        }));
        let mut req = request("everywhere", true);
        req.source_uri = Some(ARTIST.into());
        let start = house.sim.soap_log().len();
        surface
            .queue_request_targets(&Client::McpStdio, &req)
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            house.writes(start).len(),
            1,
            "only the external {change} write is allowed"
        );
        for zone in ["Lead@S1", "Guest@S1", "Lead@S2"] {
            assert_eq!(house.queue(zone), ORIGINAL);
        }
        assert!(
            surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn a_definite_refusal_has_a_per_group_receipt_and_other_confirmed_adds_share_one_undo() {
    let house = House::new();
    let surface = house.surface();
    let refused = house.target("Guest@S2").coordinator.id.clone();
    house
        .sim
        .upnp_fault(&refused.0, "AddURIToQueue", 701)
        .unwrap();
    let outcome = surface
        .queue_request_targets(&Client::McpStdio, &request("everywhere", true))
        .unwrap();
    assert!(outcome.text().contains("Guest@S2: UPNP_FAULT"));
    let result = batch(outcome);
    assert!(!result.complete);
    assert!(result.logged && result.undoable);
    assert_eq!((result.added.len(), result.failed.len()), (3, 1));
    assert_eq!(result.failed[0].coordinator, refused.0);
    assert!(!result.failed[0].error.retryable);
    assert_eq!(house.queue("Guest@S2"), ORIGINAL);
    let action = logged(&surface);
    let before = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
    assert_eq!(before.queued.len(), 3);
    house.playback_unchanged();
    house.sim.clear_faults(&refused.0).unwrap();
    undo(&surface);
    for zone in GROUPS {
        assert_eq!(house.queue(zone), ORIGINAL);
    }
}

struct LostReply {
    lan: SimLan,
    host: IpAddr,
    adds: Arc<AtomicUsize>,
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
        if action.contains("#AddURIToQueue") {
            self.adds.fetch_add(1, Ordering::SeqCst);
            if host == self.host {
                return Err(ProtoError::Network {
                    target: "simulated queue".into(),
                    detail: "lost add reply".into(),
                });
            }
        }
        Ok(response)
    }
}

#[test]
fn an_uncertain_container_is_not_retried_and_blocks_partial_undo_of_the_batch() {
    let house = House::new();
    let adds = Arc::new(AtomicUsize::new(0));
    let lost = house.target("Guest@S2").coordinator;
    let surface = house.over(Box::new(LostReply {
        lan: house.sim.lan(),
        host: lost.ip,
        adds: Arc::clone(&adds),
    }));
    let req = QueueRequest {
        zone: "everywhere".into(),
        playlist: Some("SQ:7".into()),
        ..QueueRequest::default()
    };
    let result = batch(
        surface
            .queue_request_targets(&Client::McpStdio, &req)
            .unwrap(),
    );
    assert!(!result.complete && !result.undoable);
    assert!(result.logged);
    assert_eq!((result.added.len(), result.failed.len()), (3, 1));
    assert_eq!(result.failed[0].error.code, ErrorCode::OutcomeUncertain);
    assert!(!result.failed[0].error.retryable);
    assert_eq!(
        adds.load(Ordering::SeqCst),
        4,
        "one add per group, including the lost reply"
    );
    for zone in GROUPS {
        assert_eq!(house.queue(zone).len(), 6);
    }
    house.playback_unchanged();
    let action = logged(&surface);
    assert!(!fsonos_core::actions::undoable(
        action.action.before_state.as_deref()
    ));
    assert!(surface.preview_undo(&Client::McpStdio).is_err());
}

#[test]
fn all_failed_dispatch_still_returns_each_target_and_no_undo_claim() {
    let house = House::new();
    let surface = house.surface();
    for zone in GROUPS {
        house
            .sim
            .upnp_fault(&house.target(zone).coordinator.id.0, "AddURIToQueue", 701)
            .unwrap();
    }
    let outcome = surface
        .queue_request_targets(&Client::McpStdio, &request("everywhere", false))
        .unwrap();
    let text = outcome.text();
    for zone in GROUPS {
        assert!(text.contains(zone), "{text}");
    }
    assert!(text.contains("0 of 4 groups") && text.contains("incomplete"));
    let result = batch(outcome);
    assert!(!result.complete && !result.undoable);
    assert!(result.added.is_empty() && result.failed.len() == 4);
    assert!(result.failed.iter().all(|failed| !failed.error.retryable));
    assert!(logged(&surface).action.before_state.is_none());
    for zone in GROUPS {
        assert_eq!(house.queue(zone), ORIGINAL);
    }
    house.playback_unchanged();
}
