//! What an agent reads over MCP (stdio, `fsonos mcp`) against the virtual
//! households: the zones as resources (`sonos://zones`, and a room's state
//! through the `sonos://zones/{room}` template), a library search whose hit
//! plays, and the play then in `recent_plays`.

mod e2e;

use e2e::Scenario;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};

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
    let listed = mcp.request("resources/list", &json!({}));
    let templates = mcp.request("resources/templates/list", &json!({}));
    s.check(
        "resources",
        "mcp",
        "sonos://zones is a resource and sonos://zones/{room} a template",
        listed.to_string().contains("\"sonos://zones\"")
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
