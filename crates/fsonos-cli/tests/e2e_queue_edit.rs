//! Real CLI -> shared API -> loopback S1/S2 queue edits. Nothing reaches a real LAN.

mod e2e;

use e2e::Scenario;
use fsonos_api::surface::queue::QueueDto;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    for name in ["a", "duplicate", "duplicate", "d", "e"] {
        soap::enqueue_uri(&lan, lead, &uri(name), "", 0, false).unwrap();
    }
    let uuid = s.sim_handle().unwrap().player("Lead").unwrap().uuid.clone();
    soap::set_av_transport_uri(&lan, lead, &format!("x-rincon-queue:{uuid}#0"), "").unwrap();
    soap::seek_track(&lan, lead, 1).unwrap();
    soap::play(&lan, lead).unwrap();
    soap::set_av_transport_uri(&lan, s.ip("Member"), &format!("x-rincon:{uuid}"), "").unwrap();
    s
}

fn uri(name: &str) -> String {
    format!("x-file-cifs://example.invalid/{name}.flac")
}

fn current(s: &Scenario) -> Vec<String> {
    browse_snapshot(&s.lan(), s.ip("Lead"), QUEUE)
        .unwrap()
        .objects
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn snapshot(s: &mut Scenario, mode: &str, room: &str) -> (PathBuf, QueueDto) {
    let run = s.cli(
        "queue-snapshot",
        &[mode, "--json", "queue", room, "--limit", "100"],
    );
    assert!(run.ok(), "{}", run.stderr);
    let page: QueueDto = serde_json::from_str(&run.stdout).unwrap();
    let path = s.dir().join("queue.json");
    std::fs::write(&path, &run.stdout).unwrap();
    (path, page)
}

fn edits(s: &mut Scenario, mode: &str, room: &str) {
    let (path, before) = snapshot(s, mode, room);
    remove_range(s, mode, room, &path);
    let (path, after) = snapshot(s, mode, room);
    assert_ne!(before.revision, after.revision);
    move_range(s, mode, room, &path);
    play_snapshot(s, mode, room, &before);
}

fn remove_range(s: &mut Scenario, mode: &str, room: &str, path: &Path) {
    let removed = s.cli(
        "remove-range",
        &[
            mode,
            "--json",
            "queue-remove",
            room,
            "2",
            "--count",
            "2",
            "--snapshot",
            path.to_str().unwrap(),
        ],
    );
    let result: Value = serde_json::from_str(&removed.stdout).unwrap_or(Value::Null);
    s.check(
        "remove-range",
        "cli",
        "remove exactly two duplicate entries on the coordinator",
        removed.ok()
            && result["count"] == 2
            && result["remaining"] == 3
            && result["undoable"] == false
            && current(s) == [uri("a"), uri("d"), uri("e")],
        format!("{} {}", removed.stdout, removed.stderr),
    );
    let stale = s.cli(
        "stale-selection",
        &[
            mode,
            "queue-remove",
            room,
            "2",
            "--snapshot",
            path.to_str().unwrap(),
        ],
    );
    s.check(
        "stale-selection",
        "cli",
        "an old snapshot cannot remove the item shifted into its position",
        !stale.ok() && current(s) == [uri("a"), uri("d"), uri("e")],
        &stale.stderr,
    );
}

fn move_range(s: &mut Scenario, mode: &str, room: &str, path: &Path) {
    let moved = s.cli(
        "move-range",
        &[
            mode,
            "--json",
            "queue-move",
            room,
            "2",
            "1",
            "--count",
            "2",
            "--snapshot",
            path.to_str().unwrap(),
        ],
    );
    let result: Value = serde_json::from_str(&moved.stdout).unwrap_or(Value::Null);
    s.check(
        "move-range",
        "cli",
        "move the block before its original destination without rebuilding",
        moved.ok()
            && result["position"] == 1
            && result["count"] == 2
            && current(s) == [uri("d"), uri("e"), uri("a")],
        format!("{} {}", moved.stdout, moved.stderr),
    );
    let position = soap::get_position_info(&s.lan(), s.ip("Lead")).unwrap();
    s.check(
        "playback",
        "speaker",
        "moving other entries preserves the current track",
        position.uri == uri("a"),
        format!("{position:?}"),
    );

    let (_, page) = snapshot(s, mode, room);
    let revision = page.revision.unwrap();
    let no_op = s.cli(
        "no-op",
        &[
            mode,
            "--json",
            "queue-move",
            room,
            "1",
            "2",
            "--expected-uri",
            &uri("d"),
            "--expected-revision",
            &revision,
        ],
    );
    let result: Value = serde_json::from_str(&no_op.stdout).unwrap_or(Value::Null);
    s.check(
        "no-op",
        "cli",
        "a native no-op returns the shared unchanged result",
        no_op.ok() && result["changed"] == false && result["revision"] == revision,
        format!("{} {}", no_op.stdout, no_op.stderr),
    );
}

fn play_snapshot(s: &mut Scenario, mode: &str, room: &str, before: &QueueDto) {
    let (latest, _) = snapshot(s, mode, room);
    let played = s.cli(
        "play-snapshot",
        &[
            mode,
            "queue-play",
            room,
            "1",
            "--snapshot",
            latest.to_str().unwrap(),
        ],
    );
    s.check(
        "play-snapshot",
        "cli",
        "the same reviewed page selects playback without changing queue contents",
        played.ok()
            && soap::get_position_info(&s.lan(), s.ip("Lead")).unwrap().uri == uri("d")
            && current(s) == [uri("d"), uri("e"), uri("a")],
        format!("{} {}", played.stdout, played.stderr),
    );
    let old = s.dir().join("old-queue.json");
    std::fs::write(&old, serde_json::to_vec(before).unwrap()).unwrap();
    let refused = s.cli(
        "stale-playback",
        &[
            mode,
            "queue-play",
            room,
            "1",
            "--snapshot",
            old.to_str().unwrap(),
        ],
    );
    s.check(
        "stale-playback",
        "cli",
        "a stale playback snapshot cannot select a different queue entry",
        !refused.ok()
            && soap::get_position_info(&s.lan(), s.ip("Lead")).unwrap().uri == uri("d"),
        &refused.stderr,
    );
}

#[test]
fn direct_queue_edits_work_on_s1_and_s2() {
    for s2 in [false, true] {
        let mut s = setup(
            if s2 {
                "queue-edit-direct-s2"
            } else {
                "queue-edit-direct-s1"
            },
            s2,
        );
        edits(
            &mut s,
            "--direct",
            if s2 { "Member@S2" } else { "Member@S1" },
        );
        s.finish();
    }
}

#[test]
fn daemon_queue_edits_reach_the_same_shared_actions() {
    let mut s = setup("queue-edit-daemon", true);
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    edits(&mut s, "--daemon", "Member@S2");
    let log = s.cli(
        "action-log",
        &["--daemon", "--json", "log", "--limit", "20"],
    );
    s.check(
        "action-log",
        "cli",
        "the daemon logs the same queue removal and movement actions",
        log.ok()
            && log.stdout.contains("remove_from_queue")
            && log.stdout.contains("move_in_queue"),
        format!("{} {}", log.stdout, log.stderr),
    );
    drop(daemon);
    s.finish();
}
