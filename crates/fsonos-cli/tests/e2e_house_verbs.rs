//! The house verbs end to end against the virtual households, in direct
//! mode: the owner's room aliases (`fsonos rooms alias add|rm` editing
//! aliases.toml, and every command taking an alias for a room).

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::{get_transport_info, get_volume};
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::Value;

fn json(run: &e2e::Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or(Value::Null)
}

#[test]
fn aliases_name_rooms_for_every_command() {
    let mut s = Scenario::start("house-aliases");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let file = s.dir().join("data").join("aliases.toml");
    let kept = std::fs::write(&file, "# the owner's own notes\n[aliases]\n");
    s.check(
        "seed",
        "store",
        "an aliases.toml with a comment",
        kept.is_ok(),
        format!("{kept:?}"),
    );

    let run = s.cli(
        "alias-add",
        &["rooms", "alias", "add", "cook-room", "Kitchen"],
    );
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    s.check(
        "alias-add",
        "cli",
        "the alias is added and the owner's comment kept",
        run.ok()
            && text.contains("cook-room = \"Kitchen\"")
            && text.starts_with("# the owner's own notes\n"),
        &text,
    );

    let run = s.cli("rooms", &["rooms", "--json"]);
    let listed = json(&run);
    let named = listed.as_array().is_some_and(|rooms| {
        rooms
            .iter()
            .any(|r| r["name"] == "Kitchen" && r["aliases"] == serde_json::json!(["cook-room"]))
    });
    s.check(
        "rooms",
        "cli",
        "fsonos rooms shows the alias on Kitchen",
        named,
        &run.stdout,
    );

    let run = s.cli("volume", &["volume", "cook-room", "30"]);
    let level = get_volume(&s.lan(), kitchen).ok();
    s.check(
        "volume",
        "sim",
        "volume through the alias sets Kitchen to 30",
        run.ok() && level == Some(30),
        format!("{}; {level:?}", run.stderr),
    );
    let stream = "x-rincon-mp3radio://stream.example.org/alias.mp3";
    let run = s.cli("play", &["play", "cook-room", stream]);
    let state = get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
    s.check(
        "play",
        "sim",
        "play through the alias plays in Kitchen",
        run.ok() && state == Some(TransportState::Playing),
        format!("{}; {state:?}", run.stderr),
    );

    let run = s.cli("alias-rm", &["rooms", "alias", "rm", "cook-room"]);
    s.check(
        "alias-rm",
        "cli",
        "the alias is removed",
        run.ok(),
        &run.stderr,
    );
    let run = s.cli("gone", &["volume", "cook-room", "20"]);
    s.check(
        "gone",
        "cli",
        "a removed alias is an unknown room (exit 3)",
        run.code == Some(3) && run.stderr.contains("error[UNKNOWN_ROOM]"),
        &run.stderr,
    );
    let run = s.cli("reserved", &["rooms", "alias", "add", "here", "Kitchen"]);
    s.check(
        "reserved",
        "cli",
        "here, all and everywhere cannot be aliases (exit 2)",
        run.code == Some(2) && run.stderr.contains("reserved"),
        &run.stderr,
    );
    s.finish();
}
