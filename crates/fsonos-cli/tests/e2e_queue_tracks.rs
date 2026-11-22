//! Ordered selections through real CLI/MCP processes and loopback S1/S2 players.

mod e2e;

use e2e::Scenario;
use fsonos_api::surface::queue::{QueueAddOutcome, QueueAddedDto, QueueDto};
use fsonos_api::{ActionDto, UndoDto};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use serde_json::json;
use std::time::Duration;

const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
const EPISODE: &str = "https://open.spotify.com/episode/512ojhOuo1ktJprKbVcKyQ?si=test";

fn setup(name: &str, s2: bool) -> Scenario {
    let mut s = Scenario::start(name);
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Lead", model),
        SimPlayerSpec::new("Member", model),
    ];
    let builder = SimHousehold::builder();
    s.sim(if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    });
    let lan = s.lan();
    let lead = s.ip("Lead");
    for uri in [
        "x-file-cifs://example.invalid/first.flac",
        "x-file-cifs://example.invalid/last.flac",
    ] {
        soap::enqueue_uri(&lan, lead, uri, "", 0, false).unwrap();
    }
    let uuid = s.sim_handle().unwrap().player("Lead").unwrap().uuid.clone();
    soap::set_av_transport_uri(&lan, lead, &format!("x-rincon-queue:{uuid}#0"), "").unwrap();
    soap::seek_track(&lan, lead, 1).unwrap();
    soap::play(&lan, lead).unwrap();
    soap::pause(&lan, lead).unwrap();
    soap::set_av_transport_uri(&lan, s.ip("Member"), &format!("x-rincon:{uuid}"), "").unwrap();
    s
}

fn current(s: &Scenario) -> Vec<String> {
    browse_snapshot(&s.lan(), s.ip("Lead"), QUEUE)
        .unwrap()
        .objects
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn revision(s: &mut Scenario, mode: &str, room: &str) -> String {
    let read = s.cli("queue-revision", &[mode, "--json", "queue", room]);
    assert!(read.ok(), "{}", read.stderr);
    serde_json::from_str::<QueueDto>(&read.stdout)
        .unwrap()
        .revision
        .unwrap()
}

fn ordered_selection(s: &mut Scenario, mode: &str, room: &str) {
    let before = current(s);
    let lead = s.ip("Lead");
    let transport = soap::get_transport_info(&s.lan(), lead).unwrap();
    let position = soap::get_position_info(&s.lan(), lead).unwrap();
    let media = soap::get_media_info(&s.lan(), lead).unwrap();
    let revision = revision(s, mode, room);
    let run = s.cli(
        "ordered-next",
        &[
            mode,
            "--json",
            "queue-tracks",
            room,
            TRACK,
            EPISODE,
            TRACK,
            "--next",
            "--expected-revision",
            &revision,
        ],
    );
    assert!(run.ok(), "{}", run.stderr);
    let added: QueueAddedDto = serde_json::from_str(&run.stdout).unwrap();
    let after = current(s);
    s.check(
        "ordered-next", "cli",
        "insert the complete selection after the paused item, retaining order and duplicate occurrences",
        added.zone == "Lead" && added.first == 2 && added.count == 3 && added.playing_queue
            && after.len() == 5 && after[0] == before[0] && after[4] == before[1]
            && after[1].contains("4uLU6hMCjMI75M1A2tKUQC")
            && after[2].contains("512ojhOuo1ktJprKbVcKyQ") && after[1] == after[3],
        format!("{added:?} {after:?}"),
    );
    s.check(
        "quiet-selection",
        "speaker",
        "selected source, paused state and current occurrence are unchanged",
        soap::get_transport_info(&s.lan(), lead).unwrap() == transport
            && soap::get_position_info(&s.lan(), lead).unwrap() == position
            && soap::get_media_info(&s.lan(), lead).unwrap().uri == media.uri,
        "before/after player reads",
    );
    let log = s.cli("selection-log", &[mode, "--json", "log"]);
    assert!(log.ok(), "{}", log.stderr);
    let actions: Vec<ActionDto> = serde_json::from_str(&log.stdout).unwrap();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(actions[0].undoable && actions[0].intent.starts_with("add_to_queue:"));
    let undo = s.cli("undo-selection", &[mode, "--json", "undo"]);
    assert!(undo.ok(), "{}", undo.stderr);
    let undone: UndoDto = serde_json::from_str(&undo.stdout).unwrap();
    s.check(
        "undo-selection",
        "cli",
        "one undo restores all selected occurrences together",
        undone.undone == Some(actions[0].id) && undone.failures.is_empty() && current(s) == before,
        &undo.stdout,
    );
    let stale = s.cli(
        "stale-destination",
        &[
            mode,
            "queue-tracks",
            room,
            TRACK,
            EPISODE,
            "--expected-revision",
            &revision,
        ],
    );
    s.check(
        "stale-destination",
        "cli",
        "a stale destination revision refuses the complete selection",
        !stale.ok() && stale.stderr.contains("QUEUE_CHANGED") && current(s) == before,
        &stale.stderr,
    );
}

#[test]
fn direct_ordered_batches_are_quiet_and_undoable_on_s1_and_s2() {
    for s2 in [false, true] {
        let mut s = setup(
            if s2 {
                "queue-tracks-direct-s2"
            } else {
                "queue-tracks-direct-s1"
            },
            s2,
        );
        ordered_selection(
            &mut s,
            "--direct",
            if s2 { "Member@S2" } else { "Member@S1" },
        );
        s.finish();
    }
}

#[test]
fn daemon_and_mcp_share_the_same_ordered_queue_operation() {
    let mut s = setup("queue-tracks-daemon", true);
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    ordered_selection(&mut s, "--daemon", "Member@S2");
    let before = current(&s);
    let revision = revision(&mut s, "--daemon", "Member@S2");
    let transport = soap::get_transport_info(&s.lan(), s.ip("Lead")).unwrap();
    let mut mcp = s.mcp_with(&["--daemon"]);
    mcp.initialize();
    let answer = mcp.request(
        "tools/call",
        &json!({
            "name": "add_tracks_to_queue", "arguments": {
                "zone": "Member@S2", "expected_revision": revision,
                "tracks": [
                    {"source_uri": TRACK, "title": "Selected first"},
                    {"source_uri": EPISODE, "title": "Selected episode"},
                    {"source_uri": TRACK, "title": "Selected first again"}
                ]
            }
        }),
    );
    let after = current(&s);
    s.check(
        "agent-batch",
        "mcp",
        "the agent appends the selection in order without starting playback",
        answer["result"].is_object()
            && answer["result"]["isError"] != true
            && after.len() == 5
            && after[..2] == before[..]
            && after[2].contains("4uLU6hMCjMI75M1A2tKUQC")
            && after[3].contains("512ojhOuo1ktJprKbVcKyQ")
            && after[2] == after[4]
            && soap::get_transport_info(&s.lan(), s.ip("Lead")).unwrap() == transport,
        &answer,
    );
    drop(mcp);
    drop(daemon);
    s.finish();
}

fn selected_queue(s: &Scenario, room: &str) -> Vec<String> {
    browse_snapshot(&s.lan(), s.ip(room), QUEUE)
        .unwrap()
        .objects
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn room_set_scenario(name: &str) -> Scenario {
    let mut s = Scenario::start(name);
    s.sim(SimHousehold::standard());
    std::fs::write(
        s.dir().join("data/aliases.toml"),
        "[aliases]\nselected = [\"Kitchen\", \"Living Room\"]\n",
    )
    .unwrap();
    for room in ["Kitchen", "Living Room"] {
        for uri in [
            "x-file-cifs://example.invalid/first.flac",
            "x-file-cifs://example.invalid/last.flac",
        ] {
            soap::add_uri_to_queue(&s.lan(), s.ip(room), uri, "", false).unwrap();
        }
        let uuid = s.sim_handle().unwrap().player(room).unwrap().uuid.clone();
        soap::set_av_transport_uri(
            &s.lan(),
            s.ip(room),
            &format!("x-rincon-queue:{uuid}#0"),
            "",
        )
        .unwrap();
        soap::seek_track(&s.lan(), s.ip(room), 1).unwrap();
        soap::play(&s.lan(), s.ip(room)).unwrap();
        soap::pause(&s.lan(), s.ip(room)).unwrap();
    }
    s
}

fn assert_partial_queue(outcome: &QueueAddOutcome) {
    let QueueAddOutcome::Groups(batch) = outcome else {
        panic!("both groups need their own receipt")
    };
    assert!(!batch.complete);
    assert_eq!(batch.added.len(), 1);
    assert_eq!(batch.added[0].household, "S1");
    assert_eq!(batch.added[0].queue.zone, "Kitchen");
    assert_eq!(batch.added[0].queue.count, 3);
    assert_eq!(batch.failed.len(), 1);
    assert_eq!(batch.failed[0].household, "S2");
    assert_eq!(batch.failed[0].zone, "Living Room");
}

fn assert_one_selection_dispatch(s: &Scenario, start: usize) {
    let log = s.sim_handle().unwrap().soap_log();
    let writes: Vec<_> = log[start..]
        .iter()
        .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
        .collect();
    for (room, expected) in [("Kitchen", 3), ("Living Room", 1)] {
        assert_eq!(
            writes
                .iter()
                .filter(|call| call.room == room && call.action == "AddURIToQueue")
                .count(),
            expected,
            "{room}: each selected occurrence is sent at most once"
        );
    }
    assert!(writes.iter().all(|call| call.action == "AddURIToQueue"));
}

#[test]
fn room_set_selections_are_quiet_undo_together_and_keep_partial_cli_and_mcp_receipts() {
    for via_daemon in [false, true] {
        let mut s = room_set_scenario(if via_daemon {
            "queue-tracks-room-set-daemon"
        } else {
            "queue-tracks-room-set-direct"
        });
        let daemon = via_daemon.then(|| {
            let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
            assert!(
                daemon
                    .wait_line("fsonos serve: live", Duration::from_secs(30))
                    .is_some()
            );
            daemon
        });
        let mode = if via_daemon { "--daemon" } else { "--direct" };
        let original: Vec<_> = ["Kitchen", "Living Room"]
            .into_iter()
            .map(|room| {
                (
                    room,
                    selected_queue(&s, room),
                    soap::get_transport_info(&s.lan(), s.ip(room)).unwrap(),
                    soap::get_position_info(&s.lan(), s.ip(room)).unwrap(),
                )
            })
            .collect();
        let run = s.cli(
            "selected-next",
            &[
                mode, "--json", "queue-tracks", "selected", TRACK, EPISODE, TRACK, "--next",
            ],
        );
        assert!(run.ok(), "{run:?}");
        let outcome: QueueAddOutcome = serde_json::from_str(&run.stdout).unwrap();
        let QueueAddOutcome::Groups(batch) = outcome else {
            panic!("a room set returns every group's range")
        };
        assert!(batch.complete && batch.logged && batch.undoable);
        assert_eq!(batch.added.len(), 2);
        assert!(batch.failed.is_empty());
        for (room, before, transport, position) in &original {
            let after = selected_queue(&s, room);
            assert_eq!(after.len(), before.len() + 3, "{room}");
            assert_eq!(after[0], before[0]);
            assert_eq!(after[4], before[1]);
            assert!(after[1].contains("4uLU6hMCjMI75M1A2tKUQC"));
            assert!(after[2].contains("512ojhOuo1ktJprKbVcKyQ"));
            assert_eq!(after[1], after[3]);
            assert_eq!(
                soap::get_transport_info(&s.lan(), s.ip(room)).unwrap(),
                *transport
            );
            assert_eq!(
                soap::get_position_info(&s.lan(), s.ip(room)).unwrap(),
                *position
            );
        }
        let log = s.cli("selection-log", &[mode, "--json", "log"]);
        assert!(log.ok(), "{log:?}");
        let actions: Vec<ActionDto> = serde_json::from_str(&log.stdout).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].undoable);
        let undone = s.cli("undo-room-set", &[mode, "--json", "undo"]);
        assert!(undone.ok(), "{undone:?}");
        let undo: UndoDto = serde_json::from_str(&undone.stdout).unwrap();
        assert_eq!(undo.undone, Some(actions[0].id));
        assert!(undo.failures.is_empty());
        for (room, before, _, _) in &original {
            assert_eq!(selected_queue(&s, room), *before);
        }

        s.sim_handle()
            .unwrap()
            .upnp_fault("Living Room", "AddURIToQueue", 701)
            .unwrap();
        let start = s.sim_handle().unwrap().soap_log().len();
        let partial = s.cli(
            "partial-selection",
            &[mode, "--json", "queue-tracks", "selected", TRACK, EPISODE, TRACK],
        );
        assert!(!partial.ok(), "{partial:?}");
        assert!(partial.stderr.contains("queue addition incomplete"));
        let receipt: QueueAddOutcome = serde_json::from_str(&partial.stdout).unwrap();
        assert_partial_queue(&receipt);
        assert_one_selection_dispatch(&s, start);

        for modern in [false, true] {
            let mut mcp = s.mcp_with(&[mode]);
            if modern {
                mcp.use_modern_protocol();
            } else {
                mcp.initialize();
            }
            let start = s.sim_handle().unwrap().soap_log().len();
            let answer = mcp.request(
                "tools/call",
                &json!({
                    "name": "add_tracks_to_queue",
                    "arguments": {
                        "zone": "selected",
                        "tracks": [
                            {"source_uri": TRACK},
                            {"source_uri": EPISODE},
                            {"source_uri": TRACK}
                        ]
                    }
                }),
            );
            assert!(answer.get("error").is_none(), "{answer}");
            assert_eq!(answer["result"]["isError"], true, "{answer}");
            let receipt: QueueAddOutcome = if modern {
                serde_json::from_value(answer["result"]["structuredContent"].clone()).unwrap()
            } else {
                let text = answer["result"]["content"][0]["text"].as_str().unwrap();
                serde_json::from_str(text.split_once("\n\nReceipt: ").unwrap().1).unwrap()
            };
            assert_partial_queue(&receipt);
            assert_one_selection_dispatch(&s, start);
        }
        s.check(
            "room-set-selection",
            "cli+mcp",
            "ordered multi-room queueing preserves playback, aggregate undo and every partial receipt",
            true,
            "direct and daemon paths send each selected occurrence at most once",
        );
        drop(daemon);
        s.finish();
    }
}
