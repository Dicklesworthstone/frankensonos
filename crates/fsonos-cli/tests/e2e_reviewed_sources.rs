//! The agent's read/review/write workflow over real MCP stdio processes,
//! ordinary daemon forwarding and independent S1/S2 simulator groups.

mod e2e;

use e2e::{McpSession, Scenario};
use fsonos_api::surface::queue::QueueAddOutcome;
use fsonos_api::{ControlResultDto, ErrorCode, OutcomeDto};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::{Value, json};
use std::time::Duration;

const ALIASES: &str = "[aliases]\nselected = ['Kitchen', 'Office', 'Living Room']\n";
const TRACK: &str = "spotify:track:1000000000000000000001";

fn tool(mcp: &mut McpSession, name: &str, arguments: Value) -> Value {
    mcp.request("tools/call", &json!({"name": name, "arguments": arguments}))
}

fn successful(reply: &Value) {
    assert!(
        reply.get("error").is_none() && reply["result"].is_object(),
        "{reply}"
    );
    assert_ne!(reply["result"]["isError"], true, "{reply}");
}

fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"]
        .as_str()
        .expect("a readable MCP result")
}

fn review(mcp: &mut McpSession, modern: bool) -> String {
    let reply = tool(mcp, "resolve_rooms", json!({"target":"selected"}));
    successful(&reply);
    let revision = text(&reply)
        .lines()
        .find_map(|line| line.strip_prefix("Revision: "))
        .expect("legacy agents need the exact revision too")
        .to_owned();
    assert_eq!(revision.len(), 64);
    for room in ["Kitchen", "Office", "Living Room"] {
        assert!(text(&reply).contains(room), "{reply}");
    }
    if modern {
        let observed = &reply["result"]["structuredContent"];
        assert_eq!(observed["revision"], revision);
        assert_eq!(observed["rooms"].as_array().unwrap().len(), 3);
        assert_eq!(observed["groups"].as_array().unwrap().len(), 2);
    } else {
        assert!(reply["result"].get("structuredContent").is_none());
    }
    revision
}

fn source(mcp: &mut McpSession, revision: &str, mode: &str, uri: &str) -> Value {
    tool(
        mcp,
        "control_source",
        json!({
            "zone":"selected", "expected_revision":revision,
            "mode":mode, "source_uri":uri,
        }),
    )
}

fn partial_receipt(reply: &Value, modern: bool) -> Value {
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    if modern {
        reply["result"]["structuredContent"].clone()
    } else {
        assert!(reply["result"].get("structuredContent").is_none());
        serde_json::from_str(
            text(reply)
                .split_once("\n\nReceipt: ")
                .expect("legacy partial results retain their full receipt")
                .1,
        )
        .unwrap()
    }
}

fn calls_since(s: &Scenario, from: usize, action: &str) -> Vec<String> {
    let mut rooms: Vec<_> = s.sim_handle().unwrap().soap_log()[from..]
        .iter()
        .filter(|entry| entry.action == action)
        .map(|entry| entry.room.clone())
        .collect();
    rooms.sort();
    rooms
}

fn queue(s: &Scenario, room: &str) -> Vec<String> {
    browse_all(&s.lan(), s.ip(room), QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn playback(s: &Scenario, room: &str) -> (TransportState, String) {
    (
        soap::get_transport_info(&s.lan(), s.ip(room))
            .unwrap()
            .state,
        soap::get_position_info(&s.lan(), s.ip(room)).unwrap().uri,
    )
}

fn stale_review(mcp: &mut McpSession, s: &Scenario, revision: &str) {
    let from = s.sim_handle().unwrap().soap_log().len();
    let refused = source(
        mcp,
        revision,
        "play",
        "https://audio.example.invalid/unreviewed.mp3",
    );
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(text(&refused).contains("NOT_COORDINATOR"), "{refused}");
    let writes: Vec<_> = s.sim_handle().unwrap().soap_log()[from..]
        .iter()
        .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
        .map(|entry| (entry.room.clone(), entry.action.clone()))
        .collect();
    assert!(
        writes.is_empty(),
        "stale review sent speaker writes: {writes:?}"
    );
}

fn reviewed_sources(via_daemon: bool) {
    let mut s = Scenario::start(if via_daemon {
        "reviewed-sources-daemon"
    } else {
        "reviewed-sources-standalone"
    });
    s.sim(SimHousehold::standard());
    std::fs::write(s.dir().join("data/aliases.toml"), ALIASES).unwrap();
    let grouped = s.cli("initial-group", &["--direct", "group", "Office", "Kitchen"]);
    assert!(grouped.ok(), "{grouped:?}");
    let mut daemon = via_daemon.then(|| {
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
        for line in ["fsonos serve: ready", "fsonos serve: live"] {
            assert!(
                daemon.wait_line(line, Duration::from_secs(30)).is_some(),
                "{:?}",
                daemon.seen
            );
        }
        daemon
    });

    for modern in [false, true] {
        let era = if modern { "2026-07-28" } else { "2024-11-05" };
        let mut mcp = s.mcp();
        if modern {
            mcp.use_modern_protocol();
        } else {
            mcp.initialize();
        }
        let listed = mcp.request("tools/list", &json!({}));
        let tools = listed["result"]["tools"].as_array().unwrap();
        for name in ["resolve_rooms", "control_source"] {
            assert!(tools.iter().any(|tool| tool["name"] == name), "{listed}");
        }
        let revision = review(&mut mcp, modern);
        let untouched = playback(&s, "Bedroom");
        let stream = format!("x-rincon-mp3radio://audio.example.invalid/reviewed-{era}.mp3");
        let from = s.sim_handle().unwrap().soap_log().len();
        let played = source(&mut mcp, &revision, "play", &stream);
        successful(&played);
        s.check(
            "reviewed-play",
            era,
            "one start per reviewed S1/S2 coordinator and no unselected playback",
            calls_since(&s, from, "Play") == ["Kitchen", "Living Room"]
                && playback(&s, "Kitchen") == (TransportState::Playing, stream.clone())
                && playback(&s, "Living Room") == (TransportState::Playing, stream.clone())
                && playback(&s, "Bedroom") == untouched,
            &played,
        );

        let before = [queue(&s, "Kitchen"), queue(&s, "Living Room")];
        let from = s.sim_handle().unwrap().soap_log().len();
        let appended = source(&mut mcp, &revision, "append", TRACK);
        successful(&appended);
        s.check(
            "reviewed-append",
            era,
            "reviewed queue insertion leaves streams playing and dispatches once per group",
            calls_since(&s, from, "AddURIToQueue") == ["Kitchen", "Living Room"]
                && calls_since(&s, from, "Play").is_empty()
                && queue(&s, "Kitchen").len() == before[0].len() + 1
                && queue(&s, "Living Room").len() == before[1].len() + 1
                && playback(&s, "Kitchen") == (TransportState::Playing, stream.clone())
                && playback(&s, "Living Room") == (TransportState::Playing, stream.clone()),
            &appended,
        );

        s.sim_handle()
            .unwrap()
            .upnp_fault("Living Room", "AddURIToQueue", 702)
            .unwrap();
        let before = [queue(&s, "Kitchen"), queue(&s, "Living Room")];
        let from = s.sim_handle().unwrap().soap_log().len();
        let partial = source(&mut mcp, &revision, "next", TRACK);
        let QueueAddOutcome::Groups(receipt) =
            serde_json::from_value(partial_receipt(&partial, modern)).unwrap()
        else {
            panic!("a multi-group queue addition needs both destinations")
        };
        s.check(
            "partial-queue",
            era,
            "a tool error retains confirmed and refused groups without replay",
            !receipt.complete
                && receipt.added.len() == 1
                && receipt.failed.len() == 1
                && receipt.added[0].queue.zone == "Kitchen"
                && receipt.failed[0].zone == "Living Room"
                && calls_since(&s, from, "AddURIToQueue") == ["Kitchen", "Living Room"]
                && queue(&s, "Kitchen").len() == before[0].len() + 1
                && queue(&s, "Living Room") == before[1],
            &partial,
        );
        s.sim_handle().unwrap().clear_faults("Living Room").unwrap();

        s.sim_handle()
            .unwrap()
            .upnp_fault("Living Room", "Play", 702)
            .unwrap();
        let from = s.sim_handle().unwrap().soap_log().len();
        let partial = source(
            &mut mcp,
            &revision,
            "play",
            "https://audio.example.invalid/partial.mp3",
        );
        let receipt: OutcomeDto =
            serde_json::from_value(partial_receipt(&partial, modern)).unwrap();
        let batch = receipt.batch.as_ref().unwrap();
        s.check("partial-play", era, "a failed start after source selection retains the uncertain target and never repeats Play",
            !receipt.complete() && batch.results.len() == 2
                && batch.results.iter().any(|target| target.zone == "Kitchen"
                    && matches!(target.result, ControlResultDto::Succeeded { .. }))
                && batch.results.iter().any(|target| target.zone == "Living Room"
                    && matches!(&target.result, ControlResultDto::Failed { error }
                        if error.code == ErrorCode::OutcomeUncertain))
                && calls_since(&s, from, "Play") == ["Kitchen", "Living Room"],
            &partial);
        s.sim_handle().unwrap().clear_faults("Living Room").unwrap();

        std::fs::write(
            s.dir().join("data/aliases.toml"),
            "[aliases]\nselected = ['Kitchen', 'Office', 'Living Room', 'Bedroom']\n",
        )
        .unwrap();
        stale_review(&mut mcp, &s, &revision);
        std::fs::write(s.dir().join("data/aliases.toml"), ALIASES).unwrap();
        let revision = review(&mut mcp, modern);
        let grouped = tool(
            &mut mcp,
            "group",
            json!({"zone":"Bedroom", "to":"Living Room"}),
        );
        successful(&grouped);
        stale_review(&mut mcp, &s, &revision);
        let separated = tool(&mut mcp, "ungroup", json!({"zone":"Bedroom"}));
        successful(&separated);
        // The second protocol era has its own fresh review; the old one is
        // never silently renewed by control_source itself.
        s.check(
            "stale-reviews",
            era,
            "alias expansion and physical regrouping refuse the retained review without writes",
            true,
            "no speaker mutations after either refused review",
        );
        drop(mcp);
    }
    if let Some(daemon) = &mut daemon {
        let code = daemon.interrupt(Duration::from_secs(10));
        s.check(
            "exit",
            "daemon",
            "daemon stops cleanly",
            code == Some(0),
            format!("{code:?}"),
        );
    }
    s.finish();
}

#[test]
fn standalone_mcp_reviews_and_controls_exact_group_sources_in_both_protocol_eras() {
    reviewed_sources(false);
}

#[test]
fn daemon_forwarded_mcp_reviews_and_controls_exact_group_sources_in_both_protocol_eras() {
    reviewed_sources(true);
}
