//! Authored selections through real CLI and MCP processes on both households.

use super::{call, e2e, json_out, text};
use e2e::Scenario;
use fsonos_api::UndoDto;
use fsonos_api::surface::scenes::{SceneApplyDto, SceneDefinition, SceneDto};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control::{self as soap, MediaInfo, PositionInfo, TransportInfo};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use serde_json::{Value, json};
use std::time::Duration;

const TRACK: &str = "spotify:track:1000000000000000000001";
const EPISODE: &str = "spotify:episode:1000000000000000000002";
const ROOMS: [&str; 2] = ["Kitchen", "Den"];

fn authored() -> Value {
    let source = json!({"kind":"tracks", "tracks":[
        {"source_uri":"https://open.spotify.com/track/1000000000000000000001?si=scene", "title":"  Opening  "},
        {"source_uri":EPISODE, "title":"Interlude"},
        {"source_uri":TRACK, "title":"Reprise"}
    ]});
    json!({
        "groups":[
            {"coordinator":"Kitchen", "source":source, "playing":false},
            {"coordinator":"Den", "source":source, "playing":false}
        ],
        "volumes":{"Kitchen":20, "Den":20}
    })
}

fn expected() -> Vec<(String, String)> {
    [
        (TRACK, "Opening"),
        (EPISODE, "Interlude"),
        (TRACK, "Reprise"),
    ]
    .into_iter()
    .map(|(uri, title)| (uri.into(), title.into()))
    .collect()
}

fn queue(s: &Scenario, room: &str) -> Vec<(String, String)> {
    browse_all(&s.lan(), s.ip(room), QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| {
            let uri = item.res.unwrap().uri;
            (
                spotify_uri_from_renderer_uri(&uri).unwrap_or(uri),
                item.title,
            )
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct Playback {
    media: MediaInfo,
    position: PositionInfo,
    transport: TransportInfo,
    mode: PlayMode,
    volume: u8,
}

fn playback(s: &Scenario, room: &str) -> Playback {
    let (lan, host) = (s.lan(), s.ip(room));
    Playback {
        media: soap::get_media_info(&lan, host).unwrap(),
        position: soap::get_position_info(&lan, host).unwrap(),
        transport: soap::get_transport_info(&lan, host).unwrap(),
        mode: get_play_mode(&lan, host).unwrap(),
        volume: soap::get_volume(&lan, host).unwrap(),
    }
}

fn writes(s: &Scenario, from: usize) -> Vec<SoapLogEntry> {
    s.sim_handle()
        .unwrap()
        .soap_log()
        .into_iter()
        .skip(from)
        .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
        .collect()
}

fn setup(name: &str) -> Scenario {
    let mut s = Scenario::start(name);
    s.sim(
        SimHousehold::builder()
            .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
            .s2([SimPlayerSpec::new("Den", SimModel::One)]),
    );
    for room in ROOMS {
        let (lan, host) = (s.lan(), s.ip(room));
        for name in ["repeated", "repeated", "last"] {
            soap::add_uri_to_queue(
                &lan,
                host,
                &format!("x-file-cifs://example.invalid/{name}.flac"),
                "",
                false,
            )
            .unwrap();
        }
        let player = s.sim_handle().unwrap().player(room).unwrap();
        soap::play_from_queue(&lan, host, &PlayerId(player.uuid.clone())).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::set_volume(&lan, host, 40).unwrap();
        soap::play(&lan, host).unwrap();
    }
    s.sim_handle().unwrap().advance(Duration::from_secs(17));
    s
}

fn start_daemon(s: &mut Scenario, via_daemon: bool) -> Option<e2e::Daemon> {
    via_daemon.then(|| {
        let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
        assert!(
            daemon
                .wait_line("fsonos serve: live", Duration::from_secs(30))
                .is_some(),
            "{}",
            daemon.seen.join("\n")
        );
        daemon
    })
}

fn assert_saved(scene: &SceneDto) {
    assert_eq!(scene.groups.len(), 2);
    for group in &scene.groups {
        assert_eq!(group.playing, Some(false));
        assert_eq!(group.source.kind, "tracks");
        assert_eq!(
            group
                .source
                .tracks
                .as_ref()
                .unwrap()
                .iter()
                .map(|track| { (track.source_uri.clone(), track.title.clone().unwrap()) })
                .collect::<Vec<_>>(),
            expected()
        );
    }
}

fn assert_selection(s: &Scenario, from: usize) {
    let writes = writes(s, from);
    assert_eq!(
        writes
            .iter()
            .filter(|entry| entry.action == "AddURIToQueue")
            .count(),
        6
    );
    assert!(writes.iter().all(|entry| entry.action != "Play"));
    for room in ROOMS {
        assert_eq!(queue(s, room), expected(), "{room}");
        let selected = playback(s, room);
        assert_ne!(selected.transport.state, TransportState::Playing);
        assert_eq!(selected.position.track, 1);
        assert_eq!(selected.position.position_secs, Some(0));
        assert_eq!(selected.mode, PlayMode::Normal);
        assert_eq!(selected.volume, 20);
    }
}

fn assert_partial(s: &Scenario, receipt: &SceneApplyDto, from: usize) {
    assert!(receipt.changed && !receipt.complete, "{receipt:?}");
    assert_eq!(receipt.failed.len(), 1, "{receipt:?}");
    assert!(receipt.failed[0].step.contains("Den"), "{receipt:?}");
    assert!(
        receipt
            .steps
            .iter()
            .any(|step| step.contains("Kitchen") && step.contains("3 selected")),
        "{receipt:?}"
    );
    assert_eq!(queue(s, "Kitchen"), expected());
    let writes = writes(s, from);
    for (room, attempts) in [("Kitchen", 3), ("Den", 1)] {
        let id = &s.sim_handle().unwrap().player(room).unwrap().uuid;
        assert_eq!(
            writes
                .iter()
                .filter(|entry| entry.player == *id && entry.action == "AddURIToQueue")
                .count(),
            attempts,
            "{room} must receive one selection dispatch without fallback or replay"
        );
    }
    assert!(writes.iter().all(|entry| entry.action != "Play"));
}

#[test]
fn cli_authored_tracks_round_trip_silently_restore_and_report_partial_delivery() {
    for via_daemon in [false, true] {
        let mut s = setup(if via_daemon {
            "scene-tracks-cli-daemon"
        } else {
            "scene-tracks-cli-direct"
        });
        let before: Vec<_> = ROOMS
            .into_iter()
            .map(|room| (room, queue(&s, room), playback(&s, room)))
            .collect();
        let daemon = start_daemon(&mut s, via_daemon);
        let mode = if via_daemon { "--daemon" } else { "--direct" };
        let definition: SceneDefinition = serde_json::from_value(authored()).unwrap();
        let path = s.dir().join("ordered-scene.toml");
        std::fs::write(&path, toml::to_string(&definition).unwrap()).unwrap();
        let path = path.to_str().unwrap();
        let saved = s.cli(
            "save-selection",
            &[mode, "--json", "scene", "save", "Selection", "--from", path],
        );
        assert!(saved.ok(), "{saved:?}");
        assert_saved(&serde_json::from_str(&saved.stdout).unwrap());
        let read = s.cli(
            "read-selection",
            &[mode, "--json", "scene", "show", "selection"],
        );
        assert!(read.ok(), "{read:?}");
        assert_eq!(json_out(&saved), json_out(&read));
        let displayed = s.cli("show-selection", &[mode, "scene", "show", "Selection"]);
        assert!(displayed.ok(), "{displayed:?}");
        for fragment in [
            "1. Opening",
            "2. Interlude",
            "3. Reprise",
            "replaces the queue",
            "ready without playback",
        ] {
            assert!(displayed.stdout.contains(fragment), "{displayed:?}");
        }
        let marker = s.sim_handle().unwrap().soap_log().len();
        let applied = s.cli(
            "apply-selection",
            &[mode, "--json", "scene", "apply", "Selection"],
        );
        assert!(applied.ok(), "{applied:?}");
        let receipt: SceneApplyDto = serde_json::from_str(&applied.stdout).unwrap();
        assert!(receipt.complete && receipt.changed, "{receipt:?}");
        assert_selection(&s, marker);
        let undone = s.cli("undo-selection", &[mode, "--json", "undo"]);
        assert!(undone.ok(), "{undone:?}");
        let undo: UndoDto = serde_json::from_str(&undone.stdout).unwrap();
        assert!(
            undo.undone.is_some() && undo.failures.is_empty(),
            "{undo:?}"
        );
        for (room, original_queue, original_playback) in before {
            assert_eq!(queue(&s, room), original_queue);
            assert_eq!(playback(&s, room), original_playback);
        }
        s.sim_handle()
            .unwrap()
            .upnp_fault("Den", "AddURIToQueue", 701)
            .unwrap();
        let marker = s.sim_handle().unwrap().soap_log().len();
        let partial = s.cli(
            "partial-selection",
            &[mode, "--json", "scene", "apply", "Selection"],
        );
        assert!(!partial.ok(), "{partial:?}");
        assert!(
            partial.stderr.contains("step(s) of scene Selection failed"),
            "{partial:?}"
        );
        let receipt: SceneApplyDto = serde_json::from_str(&partial.stdout).unwrap();
        assert_partial(&s, &receipt, marker);
        let marker = s.sim_handle().unwrap().soap_log().len();
        let refused = s.cli("partial-undo-refused", &[mode, "undo", "--force"]);
        assert!(
            !refused.ok() && refused.stderr.contains("restoration proof"),
            "{refused:?}"
        );
        assert!(writes(&s, marker).is_empty());
        s.check("ordered-scenes", "cli+speakers", "both generations retain authored occurrence order, silent playback, full queue undo and a terminal partial receipt", true, receipt.done);
        drop(daemon);
        s.finish();
    }
}

#[test]
fn mcp_authored_tracks_preserve_both_protocols_through_direct_and_daemon_backends() {
    for via_daemon in [false, true] {
        for modern in [false, true] {
            let mut s = setup(&format!(
                "scene-tracks-mcp-{}-{}",
                if via_daemon { "daemon" } else { "direct" },
                if modern { "modern" } else { "legacy" }
            ));
            let before: Vec<_> = ROOMS
                .into_iter()
                .map(|room| (room, queue(&s, room), playback(&s, room)))
                .collect();
            let daemon = start_daemon(&mut s, via_daemon);
            let mode = if via_daemon { "--daemon" } else { "--direct" };
            let mut mcp = s.mcp_with(&[mode]);
            if modern {
                mcp.use_modern_protocol();
            } else {
                mcp.initialize();
            }
            let saved = call(
                &mut mcp,
                "save_scene",
                &json!({"name":"Selection", "scene":authored()}),
            );
            assert!(
                saved.get("error").is_none() && saved["result"]["isError"] != true,
                "{saved}"
            );
            let read = call(&mut mcp, "get_scene", &json!({"name":"Selection"}));
            assert!(
                read.get("error").is_none() && read["result"]["isError"] != true,
                "{read}"
            );
            for fragment in [
                "1. Opening",
                "2. Interlude",
                "3. Reprise",
                "ready without playback",
            ] {
                assert!(text(&read).contains(fragment), "{read}");
            }
            if modern {
                assert_saved(
                    &serde_json::from_value(read["result"]["structuredContent"].clone()).unwrap(),
                );
            }
            let marker = s.sim_handle().unwrap().soap_log().len();
            let applied = call(&mut mcp, "apply_scene", &json!({"name":"Selection"}));
            assert!(
                applied.get("error").is_none() && applied["result"]["isError"] != true,
                "{applied}"
            );
            if modern {
                let receipt: SceneApplyDto =
                    serde_json::from_value(applied["result"]["structuredContent"].clone()).unwrap();
                assert!(receipt.complete && receipt.changed, "{receipt:?}");
            }
            assert_selection(&s, marker);
            let undone = call(&mut mcp, "undo_last", &json!({}));
            assert!(
                undone.get("error").is_none() && undone["result"]["isError"] != true,
                "{undone}"
            );
            for (room, original_queue, original_playback) in before {
                assert_eq!(queue(&s, room), original_queue);
                assert_eq!(playback(&s, room), original_playback);
            }
            s.sim_handle()
                .unwrap()
                .upnp_fault("Den", "AddURIToQueue", 701)
                .unwrap();
            let marker = s.sim_handle().unwrap().soap_log().len();
            let partial = call(&mut mcp, "apply_scene", &json!({"name":"Selection"}));
            assert!(partial.get("error").is_none(), "{partial}");
            assert_eq!(partial["result"]["isError"], true, "{partial}");
            let receipt: SceneApplyDto = if modern {
                serde_json::from_value(partial["result"]["structuredContent"].clone()).unwrap()
            } else {
                serde_json::from_str(text(&partial).split_once("\n\nReceipt: ").unwrap().1).unwrap()
            };
            assert_partial(&s, &receipt, marker);
            let marker = s.sim_handle().unwrap().soap_log().len();
            let refused = call(&mut mcp, "undo_last", &json!({}));
            assert_eq!(refused["result"]["isError"], true, "{refused}");
            assert!(text(&refused).contains("restoration proof"), "{refused}");
            assert!(writes(&s, marker).is_empty());
            s.check("agent-ordered-scenes", "mcp+speakers", "the real MCP adapter keeps silent ordered playback, original queue undo and full partial receipts with its error flag", true, &partial);
            drop(mcp);
            drop(daemon);
            s.finish();
        }
    }
}
