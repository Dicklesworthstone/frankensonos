//! Captured scenes recall an existing queue without editing it. Source,
//! occurrence, seek, mode and transport form one guarded operation on every
//! surface, including when the saved scene's writer has narrower permissions.

use fsonos_api::surface::scenes::SceneDto;
use fsonos_api::{NoteCode, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::content::{BrowseSnapshot, browse_snapshot};
use fsonos_proto::control::{self as soap, MediaInfo, PositionInfo, TransportInfo};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::Duration;

const RADIO: &str = "x-rincon-mp3radio://example.invalid/between-scenes.mp3";

fn setup(generation: u8, policy: Policy) -> (SimHandle, Surface) {
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
    let lan = sim.lan();
    let host = host(&sim);
    let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
    for name in ["repeated", "repeated", "last"] {
        soap::enqueue_uri(
            &lan,
            host,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    soap::set_av_transport_uri(
        &lan,
        sim.player("Beta").unwrap().ip,
        &format!("x-rincon:{}", coordinator.0),
        "",
    )
    .unwrap();
    soap::play_from_queue(&lan, host, &coordinator).unwrap();
    set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
    soap::seek_track(&lan, host, 2).unwrap();
    soap::play(&lan, host).unwrap();
    sim.advance(Duration::from_secs(17));
    let mut household = HouseholdState {
        id: Some(HouseholdId(sim.player("Alpha").unwrap().household.clone())),
        ..HouseholdState::default()
    };
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let surface = Surface::new(
        Box::new(lan),
        Box::new(move |_| Ok(vec![household.clone()])),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    (sim, surface)
}

fn host(sim: &SimHandle) -> IpAddr {
    sim.player("Alpha").unwrap().ip
}

fn queue(sim: &SimHandle) -> BrowseSnapshot {
    browse_snapshot(&sim.lan(), host(sim), "Q:0").unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct Playback {
    media: MediaInfo,
    position: PositionInfo,
    transport: TransportInfo,
}

fn playback(sim: &SimHandle) -> Playback {
    let lan = sim.lan();
    let host = host(sim);
    Playback {
        media: soap::get_media_info(&lan, host).unwrap(),
        position: soap::get_position_info(&lan, host).unwrap(),
        transport: soap::get_transport_info(&lan, host).unwrap(),
    }
}

fn radio(sim: &SimHandle, playing: bool) {
    let lan = sim.lan();
    let host = host(sim);
    set_play_mode(&lan, host, PlayMode::Normal).unwrap();
    soap::set_av_transport_uri(&lan, host, RADIO, "").unwrap();
    if playing {
        soap::play(&lan, host).unwrap();
    } else {
        soap::stop(&lan, host).unwrap();
    }
}

fn assert_reads_only(sim: &SimHandle, start: usize) {
    let calls = sim.soap_log();
    assert!(
        calls[start..]
            .iter()
            .all(|call| call.action.starts_with("Get") || call.action == "Browse"),
        "unexpected write: {:?}",
        &calls[start..]
    );
}

fn assert_captured_queue(sim: &SimHandle, scene: &SceneDto, update_id: u32) {
    assert_eq!(scene.groups.len(), 1);
    let group = &scene.groups[0];
    assert_eq!(group.coordinator, "Alpha");
    assert_eq!(group.members, ["Beta"]);
    assert_eq!(group.playing, Some(true));
    assert_eq!(group.source.kind, "queue");
    let saved = group
        .source
        .queue
        .as_ref()
        .expect("a guarded queue reference");
    let alpha = sim.player("Alpha").unwrap();
    assert_eq!(saved.coordinator, alpha.uuid);
    assert_eq!(saved.household.as_deref(), Some(alpha.household.as_str()));
    assert_eq!(saved.update_id, update_id);
    assert_eq!((saved.track, saved.position_secs), (2, 17));
    assert_eq!(saved.play_mode.as_deref(), Some("REPEAT_ALL"));
    assert_eq!(saved.boot_seq, Some(1));
    let json = serde_json::to_value(scene).unwrap();
    let source = &json["groups"][0]["source"];
    assert_eq!(source["kind"], "queue");
    assert_eq!(source["queue"]["track"], 2);
    assert_eq!(source["queue"]["position_secs"], 17);
    assert_eq!(source["queue"]["update_id"], update_id);
    assert!(source.get("uri").is_none());
    assert!(source.get("name").is_none());
    assert!(source.get("mood").is_none());
}

#[test]
fn captured_queue_scenes_round_trip_recall_and_undo_on_both_generations() {
    for generation in [1, 2] {
        let (sim, surface) = setup(generation, Policy::default());
        let before_queue = queue(&sim);
        let original = playback(&sim);
        let start = sim.soap_log().len();
        let saved = surface.save_scene(&Client::Cli, "Queue evening").unwrap();
        assert_captured_queue(&sim, &saved, before_queue.update_id);
        assert_eq!(
            surface.scene(&Client::Unknown, "QUEUE EVENING").unwrap(),
            saved
        );
        assert_eq!(surface.scenes(&Client::Unknown).unwrap(), vec![saved]);
        assert_reads_only(&sim, start);

        radio(&sim, true);
        let radio_state = playback(&sim);
        assert_eq!(radio_state.media.uri, RADIO);
        let applied = surface.apply_scene(&Client::Cli, "Queue evening").unwrap();
        assert!(applied.changed && applied.complete, "{applied:?}");
        assert!(applied.failed.is_empty(), "{applied:?}");
        assert_eq!(playback(&sim), original);
        assert_eq!(
            get_play_mode(&sim.lan(), host(&sim)).unwrap(),
            PlayMode::RepeatAll
        );
        assert_eq!(
            queue(&sim),
            before_queue,
            "recall must not rebuild the queue"
        );

        let undone = surface
            .undo(&Client::Cli, false)
            .unwrap()
            .expect("scene apply undo");
        assert!(undone.failures.is_empty(), "{undone:?}");
        assert_eq!(
            playback(&sim),
            radio_state,
            "undo restores the intervening radio"
        );
        assert_eq!(
            queue(&sim),
            before_queue,
            "undo also leaves the queue alone"
        );
        sim.shutdown();
    }
}

fn denied_recall(tool: &str, denied_writer: bool) {
    let policy = Policy::from_toml(&format!(
        "[clients.\"tag:restricted\"]\ndeny = [\"{tool}\"]\n"
    ))
    .unwrap();
    let (sim, surface) = setup(2, policy);
    let restricted = Client::Tailnet("tag:restricted".into());
    let cli = Client::Cli;
    if tool == "pause" {
        soap::pause(&sim.lan(), host(&sim)).unwrap();
    }
    let (writer, caller) = if denied_writer {
        (&restricted, &cli)
    } else {
        (&cli, &restricted)
    };
    surface.save_scene(writer, "Restricted queue").unwrap();
    radio(&sim, false);
    let before = playback(&sim);
    let before_queue = queue(&sim);
    let start = sim.soap_log().len();
    let applied = surface.apply_scene(caller, "Restricted queue").unwrap();
    assert!(
        !applied.changed,
        "{tool}, writer={denied_writer}: {applied:?}"
    );
    assert!(applied.steps.is_empty(), "{applied:?}");
    assert!(
        applied
            .notes
            .iter()
            .any(|note| note.code == NoteCode::Skipped && note.detail.contains(tool)),
        "{tool}, writer={denied_writer}: {applied:?}"
    );
    assert_eq!(playback(&sim), before, "no fallback resumes the radio");
    assert_eq!(queue(&sim), before_queue);
    assert_reads_only(&sim, start);
    sim.shutdown();
}

#[test]
fn caller_and_writer_permissions_gate_the_entire_queue_recall() {
    for tool in ["play_queue", "seek", "set_play_mode", "play", "pause"] {
        for denied_writer in [false, true] {
            denied_recall(tool, denied_writer);
        }
    }
}

#[test]
fn a_changed_queue_reports_source_failure_without_resuming_the_radio() {
    for generation in [1, 2] {
        let (sim, surface) = setup(generation, Policy::default());
        surface.save_scene(&Client::Cli, "Original queue").unwrap();
        radio(&sim, false);
        soap::enqueue_uri(
            &sim.lan(),
            host(&sim),
            "x-file-cifs://example.invalid/new-track.flac",
            "",
            0,
            false,
        )
        .unwrap();
        let before = playback(&sim);
        assert_ne!(before.transport.state, TransportState::Playing);
        let changed_queue = queue(&sim);
        let start = sim.soap_log().len();
        let applied = surface.apply_scene(&Client::Cli, "Original queue").unwrap();
        assert!(!applied.complete && !applied.changed, "{applied:?}");
        assert!(applied.steps.is_empty(), "{applied:?}");
        assert!(
            applied
                .failed
                .iter()
                .any(|failure| failure.step.contains("queue")
                    && failure.error.contains("queue changed")),
            "{applied:?}"
        );
        assert_eq!(
            playback(&sim),
            before,
            "the stale source must not fall through to Play"
        );
        assert_eq!(queue(&sim), changed_queue);
        assert_reads_only(&sim, start);
        sim.shutdown();
    }
}
