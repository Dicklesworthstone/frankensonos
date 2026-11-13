//! Source playback through real CLI and MCP processes, retaining the owner's
//! existing groups while aliases span both virtual Sonos generations.

mod e2e;

use e2e::Scenario;
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
