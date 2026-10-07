//! The e2e harness self-test: needs no product features beyond the binary
//! starting. It spawns the virtual households, runs `fsonos --version`, opens
//! an MCP session over stdio and lists the tools, and fetches every virtual
//! player's device description over real HTTP. See `tests/e2e/mod.rs` for the
//! scenario API and the log layout.

mod e2e;

use e2e::Scenario;
use fsonos_proto::Transport;
use fsonos_sim::SimHousehold;
use serde_json::json;

#[test]
fn harness_self_test() {
    let mut s = Scenario::start("harness-self-test");
    let players = s.sim(SimHousehold::standard()).players().to_vec();
    s.check(
        "sim",
        "sim",
        "two households of virtual players",
        players.len() == 5 && players.iter().any(|p| p.generation == 1),
        format!("{} players", players.len()),
    );

    let seeds = std::fs::read_to_string(s.dir().join("seeds.toml")).unwrap_or_default();
    let listed = players.iter().all(|p| seeds.contains(&p.ip.to_string()));
    s.check(
        "seeds",
        "sim",
        "the seeds file lists every advertised address",
        listed,
        &seeds,
    );

    let version = s.cli("version", &["--version"]);
    s.check(
        "version",
        "cli",
        "fsonos --version prints the crate version",
        version.ok() && version.stdout.contains(env!("CARGO_PKG_VERSION")),
        &version.stdout,
    );

    let mut mcp = s.mcp();
    let init = mcp.initialize();
    s.check(
        "mcp-initialize",
        "mcp",
        "the server introduces itself as fsonos",
        init["result"]["serverInfo"]["name"] == "fsonos",
        &init,
    );
    let list = mcp.request("tools/list", &json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    s.check(
        "mcp-tools",
        "mcp",
        "the control tools are listed",
        ["list_zones", "play", "pause", "set_volume", "group"]
            .iter()
            .all(|t| names.contains(t)),
        names.join(", "),
    );
    drop(mcp);

    let sim = s.sim_handle().expect("sim running");
    let fetched: Vec<(String, Result<String, String>)> = players
        .iter()
        .map(|p| {
            let body = sim
                .transport(&p.room)
                .ok_or_else(|| format!("no transport for {}", p.room))
                .and_then(|t| {
                    t.http_get(&format!("{}/xml/device_description.xml", p.base_url))
                        .map_err(|e| e.to_string())
                });
            (p.uuid.clone(), body)
        })
        .collect();
    for (uuid, body) in fetched {
        let ok = body.as_ref().is_ok_and(|b| b.contains(&uuid));
        s.check(
            &format!("description-{uuid}"),
            "http",
            "the device description names the player",
            ok,
            body.err().unwrap_or_default(),
        );
    }

    s.pending(
        "cli-control",
        "CLI control against the sim needs the routed LAN transport (d-e2e-core-scenarios)",
    );
    let summary = s.finish();
    assert_eq!(summary.failed, 0);
    assert!(summary.passed >= 10, "{summary:?}");
    assert!(
        summary.pending >= 1,
        "pending steps are counted, not passed"
    );
}

#[test]
fn pending_steps_are_never_passes() {
    let mut s = Scenario::start("harness-pending");
    s.pending("future", "not built yet");
    s.check("now", "cli", "something that holds", true, "");
    let summary = s.finish();
    // Tripwires another process holds add pending steps of their own.
    assert_eq!(summary.failed, 0);
    assert!(summary.pending >= 1, "{summary:?}");
    let steps = std::fs::read_to_string(summary.dir.join("steps.jsonl")).unwrap();
    let future = steps
        .lines()
        .find(|l| l.contains("\"step\":\"future\""))
        .expect("the pending step is logged");
    assert!(future.contains("\"status\":\"pending\"") && future.contains("\"pass\":false"));
}
