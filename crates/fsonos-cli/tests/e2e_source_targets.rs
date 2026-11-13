//! Source playback through real CLI and MCP processes, retaining the owner's
//! existing groups while aliases span both virtual Sonos generations.

mod e2e;

use e2e::Scenario;
use fsonos_api::surface::queue::QueueAddOutcome;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::json;

fn current(s: &Scenario, room: &str) -> (TransportState, String) {
    let host = s.ip(room);
    (
        soap::get_transport_info(&s.lan(), host).unwrap().state,
        soap::get_position_info(&s.lan(), host).unwrap().uri,
    )
}

fn starts_since(s: &Scenario, from: usize) -> Vec<String> {
    let mut rooms: Vec<_> = s.sim_handle().unwrap().soap_log()[from..]
        .iter()
        .filter(|entry| entry.action == "Play")
        .map(|entry| entry.room.clone())
        .collect();
    rooms.sort();
    rooms
}

fn queued(s: &Scenario, room: &str) -> Vec<String> {
    browse_all(&s.lan(), s.ip(room), QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn queue_house(name: &str) -> Scenario {
    let mut s = Scenario::start(name);
    s.sim(SimHousehold::standard());
    std::fs::write(
        s.dir().join("data/aliases.toml"),
        "[aliases]\ndownstairs = [\"Kitchen\", \"Office\", \"Living Room\"]\n",
    )
    .unwrap();
    let grouped = s.cli("group", &["--direct", "group", "Office", "Kitchen"]);
    assert!(grouped.ok(), "{grouped:?}");
    for (room, count) in [("Kitchen", 1), ("Living Room", 2)] {
        for index in 0..count {
            soap::add_uri_to_queue(
                &s.lan(),
                s.ip(room),
                &format!("x-file-cifs://example.invalid/original-{index}.flac"),
                "",
                false,
            )
            .unwrap();
        }
        let player = s.sim_handle().unwrap().player(room).unwrap();
        soap::play_from_queue(
            &s.lan(),
            player.ip,
            &fsonos_types::PlayerId(player.uuid.clone()),
        )
        .unwrap();
        soap::play(&s.lan(), player.ip).unwrap();
        if room == "Kitchen" {
            soap::pause(&s.lan(), player.ip).unwrap();
        }
    }
    s
}

#[test]
fn direct_cli_alias_plays_once_per_group_and_one_undo_restores_both_households() {
    let mut s = Scenario::start("source-targets-direct");
    s.sim(SimHousehold::standard());
    std::fs::write(
        s.dir().join("data/aliases.toml"),
        "[aliases]\ndownstairs = [\"Kitchen\", \"Office\", \"Living Room\"]\n",
    )
    .unwrap();
    let grouped = s.cli("group", &["--direct", "group", "Office", "Kitchen"]);
    assert!(grouped.ok(), "{grouped:?}");

    for (room, source) in [
        (
            "Kitchen",
            "x-rincon-mp3radio://stream.example.org/kitchen.mp3",
        ),
        (
            "Living Room",
            "x-rincon-mp3radio://stream.example.org/living.mp3",
        ),
        (
            "Bedroom",
            "x-rincon-mp3radio://stream.example.org/bedroom.mp3",
        ),
    ] {
        soap::set_av_transport_uri(&s.lan(), s.ip(room), source, "").unwrap();
        soap::play(&s.lan(), s.ip(room)).unwrap();
    }
    let before: Vec<_> = ["Kitchen", "Living Room", "Bedroom"]
        .into_iter()
        .map(|room| (room, current(&s, room)))
        .collect();
    let offset = s.sim_handle().unwrap().soap_log().len();
    let stream = "x-rincon-mp3radio://stream.example.org/downstairs.mp3";
    let played = s.cli("play-alias", &["--direct", "play", "downstairs", stream]);
    let starts = starts_since(&s, offset);
    s.check(
        "play-alias",
        "cli",
        "a cross-household alias starts each existing group once and leaves other groups alone",
        played.ok()
            && starts == ["Kitchen", "Living Room"]
            && current(&s, "Kitchen") == (TransportState::Playing, stream.into())
            && current(&s, "Living Room") == (TransportState::Playing, stream.into())
            && current(&s, "Bedroom") == before[2].1,
        format!("{played:?}; starts: {starts:?}"),
    );

    let undone = s.cli("undo-alias", &["--direct", "undo"]);
    let restored = before
        .iter()
        .all(|(room, original)| current(&s, room) == *original);
    s.check(
        "undo-alias",
        "cli",
        "one undo restores both original streams while preserving the untouched bedroom",
        undone.ok() && restored,
        format!("{undone:?}; restored: {restored}"),
    );
    s.finish();
}

#[test]
fn standalone_mcp_qualified_everywhere_plays_only_the_selected_household() {
    let mut s = Scenario::start("source-targets-mcp");
    s.sim(SimHousehold::standard());
    let kitchen = current(&s, "Kitchen");
    let office = current(&s, "Office");
    let offset = s.sim_handle().unwrap().soap_log().len();
    let stream = "x-rincon-mp3radio://stream.example.org/s2.mp3";
    let mut mcp = s.mcp();
    mcp.initialize();
    let played = mcp.request(
        "tools/call",
        &json!({
            "name": "play",
            "arguments": {"zone": "everywhere@S2", "source_uri": stream},
        }),
    );
    let starts = starts_since(&s, offset);
    s.check(
        "play-s2",
        "mcp",
        "standalone MCP starts each S2 group once without touching S1",
        played["result"].is_object()
            && played["result"]["isError"] != true
            && starts == ["Bedroom", "Living Room"]
            && current(&s, "Living Room") == (TransportState::Playing, stream.into())
            && current(&s, "Bedroom") == (TransportState::Playing, stream.into())
            && current(&s, "Kitchen") == kitchen
            && current(&s, "Office") == office,
        format!("{played}; starts: {starts:?}"),
    );
    drop(mcp);
    s.finish();
}

#[test]
fn cli_queue_sets_return_distinct_ranges_preserve_playback_and_undo_once() {
    let mut s = queue_house("queue-targets-direct");
    let before: Vec<_> = ["Kitchen", "Living Room"]
        .into_iter()
        .map(|room| (room, current(&s, room), queued(&s, room)))
        .collect();
    let offset = s.sim_handle().unwrap().soap_log().len();
    let added = s.cli(
        "queue-alias",
        &[
            "--direct",
            "--json",
            "play",
            "downstairs",
            "spotify:track:1000000000000000000001",
            "--queue",
        ],
    );
    assert!(added.ok(), "{added:?}");
    let QueueAddOutcome::Groups(receipt) = serde_json::from_str(&added.stdout).unwrap() else {
        panic!("multiple groups need their own insertion receipts")
    };
    let mut positions: Vec<_> = receipt
        .added
        .iter()
        .map(|group| {
            (
                group.queue.zone.as_str(),
                group.queue.first,
                group.queue.count,
            )
        })
        .collect();
    positions.sort();
    s.check(
        "queue-alias",
        "cli",
        "a multi-room queue add reports each insertion position and preserves paused/playing intent",
        receipt.complete && receipt.logged && receipt.undoable
            && receipt.failed.is_empty()
            && positions == [("Kitchen", 2, 1), ("Living Room", 3, 1)]
            && starts_since(&s, offset).is_empty()
            && before.iter().all(|(room, state, original)| {
                current(&s, room) == *state && queued(&s, room).len() == original.len() + 1
            }),
        &added.stdout,
    );
    let undo = s.cli("undo-queue-alias", &["--direct", "undo"]);
    s.check(
        "undo-queue-alias",
        "cli",
        "one undo removes both proved additions and retains original playback",
        undo.ok()
            && before.iter().all(|(room, state, original)| {
                current(&s, room) == *state && queued(&s, room) == *original
            }),
        format!("{undo:?}"),
    );
    s.finish();
}

#[test]
fn incomplete_cli_queue_sets_print_the_receipt_exit_nonzero_and_never_retry() {
    let mut s = queue_house("queue-targets-partial");
    let original_kitchen = queued(&s, "Kitchen");
    let original_living = queued(&s, "Living Room");
    s.sim_handle()
        .unwrap()
        .upnp_fault("Living Room", "AddURIToQueue", 701)
        .unwrap();
    let offset = s.sim_handle().unwrap().soap_log().len();
    let added = s.cli(
        "queue-partial",
        &[
            "--direct",
            "--json",
            "play",
            "downstairs",
            "spotify:track:1000000000000000000001",
            "--queue",
        ],
    );
    let QueueAddOutcome::Groups(receipt) = serde_json::from_str(&added.stdout).unwrap() else {
        panic!("partial delivery must retain every destination's receipt")
    };
    let writes = s.sim_handle().unwrap().soap_log()[offset..]
        .iter()
        .filter(|call| call.action == "AddURIToQueue")
        .count();
    s.check(
        "queue-partial",
        "cli",
        "an incomplete batch prints confirmed and failed groups, exits nonzero and sends each add once",
        !added.ok() && added.stderr.contains("incomplete")
            && !receipt.complete && receipt.added.len() == 1 && receipt.failed.len() == 1
            && receipt.added[0].queue.zone == "Kitchen" && writes == 2
            && queued(&s, "Kitchen").len() == original_kitchen.len() + 1
            && queued(&s, "Living Room") == original_living,
        format!("{added:?}"),
    );
    let undo = s.cli("undo-partial", &["--direct", "undo"]);
    s.check(
        "undo-partial",
        "cli",
        "a definite refusal leaves the other group's confirmed addition undoable",
        undo.ok()
            && queued(&s, "Kitchen") == original_kitchen
            && queued(&s, "Living Room") == original_living,
        format!("{undo:?}"),
    );
    s.finish();
}
