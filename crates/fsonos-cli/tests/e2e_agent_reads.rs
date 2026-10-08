//! What an agent reads over MCP (stdio, `fsonos mcp`) against the virtual
//! households: the zones and the DJ as resources (`sonos://zones`, a room's
//! state through the `sonos://zones/{room}` template, `sonos://dj`), the
//! favorites, a library search whose hit plays and shows in the room's state
//! within a second, the play then in `recent_plays`, and relative volume
//! held between 0 and the house policy's cap.

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::get_volume;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// A `tools/call` answer.
fn call(mcp: &mut e2e::McpSession, tool: &str, arguments: &Value) -> Value {
    mcp.request(
        "tools/call",
        &json!({ "name": tool, "arguments": arguments }),
    )
}

/// A `tools/call` result's text.
fn text(call: &Value) -> String {
    call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The JSON document a `resources/read` answered.
fn document(read: &Value) -> Value {
    read["result"]["contents"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

#[test]
fn an_agent_reads_the_house_searches_and_plays() {
    let mut s = Scenario::start("agent-reads");
    s.sim(SimHousehold::standard());
    let mut mcp = s.mcp();
    let init = mcp.initialize();
    s.check(
        "initialize",
        "mcp",
        "the server offers resources",
        init["result"]["capabilities"]["resources"].is_object(),
        &init,
    );

    let tools = mcp.request("tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|x| x["name"].as_str()).collect())
        .unwrap_or_default();
    s.check(
        "tools",
        "mcp",
        "search_library and recent_plays are listed",
        names.contains(&"search_library") && names.contains(&"recent_plays"),
        names.join(", "),
    );
    check_resources(&mut s, &mut mcp);

    check_dj_and_favorites(&mut s, &mut mcp);

    let found = call(
        &mut mcp,
        "search_library",
        &json!({ "query": "sim radio", "zone": "Living Room" }),
    );
    // The 2024-11-05 era carries no structured content: an agent reads the
    // text, which says how to play each hit.
    let hits = text(&found);
    let favorite = hits
        .lines()
        .next()
        .and_then(|first| first.split_once("favorite="))
        .map(|(_, id)| id.trim().to_string())
        .unwrap_or_default();
    s.check(
        "search",
        "mcp",
        "search_library finds the household's favorite, with how to play it",
        hits.contains("Sim Radio") && favorite.starts_with("FV:2/"),
        &found,
    );
    let played = call(
        &mut mcp,
        "play_favorite",
        &json!({ "zone": "Living Room", "favorite": favorite }),
    );
    s.check(
        "play-hit",
        "mcp",
        "the hit plays",
        played["result"].is_object() && played["result"]["isError"] != true,
        &played,
    );
    check_state_follows_the_play(&mut s, &mut mcp);
    let history = call(&mut mcp, "recent_plays", &json!({ "zone": "Living Room" }));
    let latest = text(&history)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    s.check(
        "history",
        "mcp",
        "recent_plays shows that play first, under Living Room",
        latest.contains(" in Living Room at "),
        &history,
    );
    drop(mcp);
    s.finish();
}

#[test]
fn relative_volume_stays_between_zero_and_the_policy_cap() {
    let mut s = Scenario::start("agent-volume");
    s.sim(SimHousehold::standard());
    let policy = s.dir().join("data").join("policy.toml");
    let written = std::fs::write(&policy, "[defaults]\nmax_volume = 60\nmax_step = 100\n");
    s.check(
        "policy",
        "store",
        "a policy.toml capping every room at 60",
        written.is_ok(),
        format!("{written:?}"),
    );
    let kitchen = s.ip("Kitchen");
    let mut mcp = s.mcp();
    mcp.initialize();

    let mut level = |s: &mut Scenario, step: &str, args: Value, want: u8, what: &str| {
        let answer = call(&mut mcp, "set_volume", &args);
        let now = get_volume(&s.lan(), kitchen).ok();
        s.check(
            step,
            "mcp",
            what,
            answer["result"]["isError"] != true && now == Some(want),
            format!("{now:?} {answer}"),
        );
    };
    level(
        &mut s,
        "set",
        json!({ "zone": "Kitchen", "volume": 30 }),
        30,
        "set_volume 30 sets Kitchen to 30",
    );
    level(
        &mut s,
        "down",
        json!({ "zone": "Kitchen", "delta": -100 }),
        0,
        "a delta of -100 stops at 0",
    );
    level(
        &mut s,
        "up",
        json!({ "zone": "Kitchen", "delta": 100 }),
        60,
        "a delta of +100 stops at the policy's cap of 60",
    );
    drop(mcp);
    s.finish();
}

/// The resources and the template are listed, and the zones read back.
fn check_resources(s: &mut Scenario, mcp: &mut e2e::McpSession) {
    let listed = mcp.request("resources/list", &json!({}));
    let templates = mcp.request("resources/templates/list", &json!({}));
    s.check(
        "resources",
        "mcp",
        "sonos://zones and sonos://dj are resources and sonos://zones/{room} a template",
        listed.to_string().contains("\"sonos://zones\"")
            && listed.to_string().contains("\"sonos://dj\"")
            && templates.to_string().contains("sonos://zones/{room}"),
        format!("{listed} {templates}"),
    );

    let zones = mcp.request("resources/read", &json!({ "uri": "sonos://zones" }));
    let count = document(&zones)["zones"].as_array().map(Vec::len);
    s.check(
        "read-zones",
        "mcp",
        "sonos://zones lists the four sim zones",
        count == Some(4),
        &zones,
    );
    let room = mcp.request(
        "resources/read",
        &json!({ "uri": "sonos://zones/Living%20Room" }),
    );
    s.check(
        "read-room",
        "mcp",
        "sonos://zones/Living%20Room is that room's zone state",
        document(&room)["zone"]["coordinator_room"] == "Living Room"
            && document(&room)["transport_state"].is_string(),
        &room,
    );
}

/// `sonos://dj` reads as every zone's DJ, and the favorites are listed.
fn check_dj_and_favorites(s: &mut Scenario, mcp: &mut e2e::McpSession) {
    let dj = mcp.request("resources/read", &json!({ "uri": "sonos://dj" }));
    let doc = document(&dj);
    let zones = doc["zones"].as_array().cloned().unwrap_or_default();
    s.check(
        "read-dj",
        "mcp",
        "sonos://dj has the DJ of each of the four zones (idle) and its moods",
        zones.len() == 4
            && zones
                .iter()
                .all(|z| z["running"] == false && z["zone"].is_string())
            && doc["moods"]["moods"].is_array(),
        &dj,
    );

    let favorites = call(mcp, "list_favorites", &json!({ "zone": "Living Room" }));
    s.check(
        "favorites",
        "mcp",
        "list_favorites lists the household's favorites, Sim Radio among them",
        text(&favorites).contains("Sim Radio"),
        &favorites,
    );
}

/// Within a second of the play, the room's state says it plays.
fn check_state_follows_the_play(s: &mut Scenario, mcp: &mut e2e::McpSession) {
    let played_at = Instant::now();
    let mut state = Value::Null;
    while played_at.elapsed() < Duration::from_secs(1) {
        let read = mcp.request(
            "resources/read",
            &json!({ "uri": "sonos://zones/Living%20Room" }),
        );
        state = document(&read);
        if state["transport_state"] == "playing" {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    s.check(
        "state-after-play",
        "mcp",
        "within a second, the room's state says it plays",
        state["transport_state"] == "playing" && played_at.elapsed() < Duration::from_secs(1),
        format!("{state} after {:?}", played_at.elapsed()),
    );
}
