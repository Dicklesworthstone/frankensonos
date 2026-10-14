//! Scenes end to end against the virtual households. With the CLI: save the
//! house, change it, apply the scene, apply it again (nothing to do), undo
//! the apply. Then the same scenes over HTTP (`fsonos serve`) and MCP
//! (`fsonos mcp`), which share the data directory's store: one surface at a
//! time, so the store has one writer.

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::{get_position_info, get_transport_info, get_volume};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::time::Duration;

const STREAM: &str = "x-rincon-mp3radio://stream.example.org/scenes.mp3";

fn json_out(run: &e2e::Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or(Value::Null)
}

/// Kitchen as the sim has it: its volume, whether Office plays in its
/// group, what its group does, and on what.
#[derive(Debug, PartialEq)]
struct Kitchen {
    volume: Option<u8>,
    with_office: bool,
    state: Option<TransportState>,
    uri: String,
}

fn kitchen(s: &Scenario) -> Kitchen {
    let (lan, ip) = (s.lan(), s.ip("Kitchen"));
    let with_office = get_zone_group_state(&lan, ip).is_ok_and(|z| {
        z.groups.iter().any(|g| {
            let named = |room: &str| g.members.iter().any(|m| m.zone_name == room);
            named("Kitchen") && named("Office")
        })
    });
    Kitchen {
        volume: get_volume(&lan, ip).ok(),
        with_office,
        state: get_transport_info(&lan, ip).map(|t| t.state).ok(),
        uri: get_position_info(&lan, ip)
            .map(|p| p.uri)
            .unwrap_or_default(),
    }
}

/// The house the scene keeps: Office in Kitchen's group, playing the
/// stream at 30.
fn evening() -> Kitchen {
    Kitchen {
        volume: Some(30),
        with_office: true,
        state: Some(TransportState::Playing),
        uri: STREAM.to_string(),
    }
}

/// Set the house up, save it as Evening, and read it back.
fn save(s: &mut Scenario) {
    for (step, args) in [
        ("play", &["play", "Kitchen", STREAM]),
        ("volume", &["volume", "Kitchen", "30"]),
        ("group", &["group", "Office", "Kitchen"]),
    ] {
        let run = s.cli(step, args);
        s.check(step, "cli", "the house is set up", run.ok(), &run.stderr);
    }
    let run = s.cli("save", &["scene", "save", "Evening"]);
    s.check(
        "save",
        "cli",
        "fsonos scene save Evening",
        run.ok() && run.stdout.starts_with("saved scene Evening"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli("list", &["--json", "scene", "list"]);
    let listed = json_out(&run);
    s.check(
        "list",
        "cli",
        "fsonos scene list shows Evening",
        listed[0]["name"] == "Evening" && listed.as_array().map(Vec::len) == Some(1),
        &run.stdout,
    );
    let run = s.cli("show", &["--json", "scene", "show", "evening"]);
    let shown = json_out(&run);
    let kept = shown["groups"].as_array().is_some_and(|groups| {
        groups.iter().any(|g| {
            g["coordinator"] == "Kitchen"
                && g["members"] == json!(["Office"])
                && g["source"] == json!({ "kind": "uri", "uri": STREAM })
                && g["playing"] == true
        })
    });
    s.check(
        "show",
        "cli",
        "the scene keeps the group, its stream, and Kitchen at 30 (name case ignored)",
        kept && shown["volumes"]["Kitchen"] == 30,
        &run.stdout,
    );
}

/// Change the house, apply Evening, apply it again, and undo the apply.
fn apply_and_undo(s: &mut Scenario) {
    for (step, args) in [
        ("perturb-ungroup", &["ungroup", "Office"][..]),
        ("perturb-volume", &["volume", "Kitchen", "60"][..]),
        ("perturb-pause", &["pause", "Kitchen"][..]),
    ] {
        let run = s.cli(step, args);
        s.check(step, "cli", "the house changes", run.ok(), &run.stderr);
    }
    let changed = kitchen(s);
    let run = s.cli("apply", &["--json", "scene", "apply", "Evening"]);
    let applied = json_out(&run);
    let now = kitchen(s);
    s.check(
        "apply",
        "sim",
        "apply regroups Office, sets Kitchen to 30 and plays the stream",
        run.ok()
            && applied["changed"] == true
            && applied["complete"] == true
            && changed != evening()
            && now == evening(),
        format!(
            "before {changed:?}; after {now:?}; {}{}",
            run.stdout, run.stderr
        ),
    );
    let run = s.cli("apply-again", &["--json", "scene", "apply", "Evening"]);
    let again = json_out(&run);
    s.check(
        "apply-again",
        "cli",
        "applying a scene the house matches sends nothing",
        run.ok() && again["changed"] == false && again["steps"] == json!([]),
        &run.stdout,
    );
    let run = s.cli("undo", &["undo"]);
    let undone = kitchen(s);
    s.check(
        "undo",
        "sim",
        "undo puts back Kitchen at 60, paused, and Office on its own",
        run.ok()
            && undone.volume == Some(60)
            && !undone.with_office
            && undone.state != Some(TransportState::Playing),
        format!("{undone:?}; {}{}", run.stdout, run.stderr),
    );
    let run = s.cli("unknown", &["scene", "apply", "Evenin"]);
    s.check(
        "unknown",
        "cli",
        "an unknown scene is UNKNOWN_SCENE (exit 3), suggesting Evening",
        run.code == Some(3)
            && run.stderr.contains("error[UNKNOWN_SCENE]")
            && run.stderr.contains("Evening"),
        &run.stderr,
    );
}

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// The scene routes on `fsonos serve`.
fn over_http(s: &mut Scenario) {
    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_owned();
    s.check(
        "serve",
        "daemon",
        "serve is up with its live model",
        !api.is_empty() && live.is_some(),
        daemon.seen.join("\n"),
    );
    http_scenes(s, &api);
    http_refusals(s, &api);
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "serve-stop",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("exit {code:?}"),
    );
}

/// List, save, and apply over HTTP.
fn http_scenes(s: &mut Scenario, api: &str) {
    let listed = http(api, "GET", "/scenes", &[], "");
    s.check(
        "http-list",
        "http",
        "GET /scenes lists the CLI's Evening",
        listed.as_ref().is_ok_and(|(code, _, body)| {
            *code == 200
                && serde_json::from_str::<Value>(body).is_ok_and(|v| v[0]["name"] == "Evening")
        }),
        format!("{listed:?}"),
    );
    let saved = http(api, "PUT", "/scenes/Morning%20Light", &[], "");
    s.check(
        "http-save",
        "http",
        "PUT /scenes/Morning%20Light saves the house as it is",
        saved
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("\"Morning Light\"")),
        format!("{saved:?}"),
    );
    let applied = http(
        api,
        "POST",
        "/scenes/Evening/apply",
        &[("Content-Type", "application/json")],
        "{}",
    );
    let now = kitchen(s);
    s.check(
        "http-apply",
        "sim",
        "POST /scenes/Evening/apply puts the house in Evening",
        applied
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("\"changed\":true"))
            && now == evening(),
        format!("{now:?}; {applied:?}"),
    );
}

/// What the scene routes refuse, and deleting one.
fn http_refusals(s: &mut Scenario, api: &str) {
    let bare = http(api, "POST", "/scenes/Evening/apply", &[], "");
    s.check(
        "http-apply-media",
        "http",
        "an apply that is not JSON is 415 UNSUPPORTED_MEDIA_TYPE",
        bare.as_ref()
            .is_ok_and(|(code, _, body)| *code == 415 && body.contains("UNSUPPORTED_MEDIA_TYPE")),
        format!("{bare:?}"),
    );
    let foreign = http(
        api,
        "DELETE",
        "/scenes/Evening",
        &[("Origin", "http://evil.example")],
        "",
    );
    s.check(
        "http-origin",
        "http",
        "a foreign web page cannot delete a scene (403 UNTRUSTED_ORIGIN)",
        foreign
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 403 && body.contains("UNTRUSTED_ORIGIN")),
        format!("{foreign:?}"),
    );
    let deleted = http(api, "DELETE", "/scenes/morning%20light", &[], "");
    let gone = http(api, "GET", "/scenes/Morning%20Light", &[], "");
    s.check(
        "http-delete",
        "http",
        "DELETE /scenes/{name} forgets it; then GET is 404 UNKNOWN_SCENE",
        deleted.as_ref().is_ok_and(|(code, _, _)| *code == 200)
            && gone
                .as_ref()
                .is_ok_and(|(code, _, body)| *code == 404 && body.contains("UNKNOWN_SCENE")),
        format!("{deleted:?}; {gone:?}"),
    );
}

/// A `tools/call` answer.
fn call(mcp: &mut e2e::McpSession, tool: &str, arguments: &Value) -> Value {
    mcp.request(
        "tools/call",
        &json!({ "name": tool, "arguments": arguments }),
    )
}

fn text(call: &Value) -> String {
    call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The scene tools on `fsonos mcp`.
fn over_mcp(s: &mut Scenario) {
    let mut mcp = s.mcp();
    mcp.initialize();
    let listed = call(&mut mcp, "list_scenes", &json!({}));
    // The 2024-11-05 era carries the text only, a scene a line.
    let lines: Vec<String> = text(&listed).lines().map(str::to_string).collect();
    s.check(
        "mcp-list",
        "mcp",
        "list_scenes has Evening only, with its group and stream",
        lines.len() == 1
            && lines[0].starts_with("Evening: Kitchen + Office: ")
            && lines[0].contains(STREAM),
        &listed,
    );
    let applied = call(&mut mcp, "apply_scene", &json!({ "name": "evening" }));
    s.check(
        "mcp-apply",
        "mcp",
        "apply_scene on the house HTTP already put in Evening changes nothing",
        applied["result"]["isError"] != true
            && text(&applied) == "the house already matches scene Evening",
        &applied,
    );
    let saved = call(&mut mcp, "save_scene", &json!({ "name": "Late" }));
    s.check(
        "mcp-save",
        "mcp",
        "save_scene saves Late",
        saved["result"]["isError"] != true && text(&saved).starts_with("Saved Late:"),
        &saved,
    );
    let unknown = call(&mut mcp, "apply_scene", &json!({ "name": "Nope" }));
    s.check(
        "mcp-unknown",
        "mcp",
        "an unknown scene is a tool error with UNKNOWN_SCENE",
        unknown["result"]["isError"] == true && text(&unknown).starts_with("UNKNOWN_SCENE: "),
        &unknown,
    );
    drop(mcp);
    let run = s.cli("rm", &["scene", "rm", "late"]);
    let run2 = s.cli("list-after", &["--json", "scene", "list"]);
    s.check(
        "rm",
        "cli",
        "the CLI sees MCP's Late and removes it",
        run.ok()
            && run.stdout == "deleted scene Late\n"
            && json_out(&run2).as_array().map(Vec::len) == Some(1),
        format!("{}{}; {}", run.stdout, run.stderr, run2.stdout),
    );
}

#[test]
fn scenes_save_apply_and_undo_on_every_surface() {
    let mut s = Scenario::start("scenes");
    s.sim(SimHousehold::standard());
    save(&mut s);
    apply_and_undo(&mut s);
    over_http(&mut s);
    over_mcp(&mut s);
    s.finish();
}
