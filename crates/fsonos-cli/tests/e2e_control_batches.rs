//! A partial house command is visible through real CLI/MCP processes, with
//! each simulator target accounted for and no automatic second dispatch.

mod e2e;

use e2e::Scenario;
use fsonos_api::{ControlResultDto, ErrorCode, OutcomeDto};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::json;
use std::time::Duration;

fn assert_partial(outcome: &OutcomeDto) {
    assert!(!outcome.complete());
    assert_eq!(outcome.incomplete_targets(), 1);
    let batch = outcome.batch.as_ref().expect("a two-group receipt");
    assert!(!batch.complete);
    assert_eq!(batch.results.len(), 2);
    for result in &batch.results {
        match result.zone.as_str() {
            "Kitchen" => {
                assert_eq!(result.household, "S1");
                assert!(matches!(result.result, ControlResultDto::Succeeded { .. }));
            }
            "Living Room" => {
                assert_eq!(result.household, "S2");
                assert!(matches!(&result.result, ControlResultDto::Failed { error }
                    if error.code == ErrorCode::UpnpFault && error.upnp_code == Some(702)));
            }
            other => panic!("unselected target {other}"),
        }
        assert!(!result.target.is_empty());
    }
}

fn assert_dispatched_once(s: &Scenario, start: usize) {
    let mut paused: Vec<_> = s.sim_handle().unwrap().soap_log()[start..]
        .iter()
        .filter(|call| call.action == "Pause")
        .map(|call| call.room.clone())
        .collect();
    paused.sort();
    assert_eq!(paused, ["Kitchen", "Living Room"]);
    assert_eq!(
        soap::get_transport_info(&s.lan(), s.ip("Kitchen"))
            .unwrap()
            .state,
        TransportState::Paused
    );
    assert_eq!(
        soap::get_transport_info(&s.lan(), s.ip("Living Room"))
            .unwrap()
            .state,
        TransportState::Playing
    );
}

fn partial_commands(via_daemon: bool) {
    let mut s = Scenario::start(if via_daemon {
        "control-batches-daemon"
    } else {
        "control-batches-direct"
    });
    s.sim(SimHousehold::standard());
    std::fs::write(
        s.dir().join("data/aliases.toml"),
        "[aliases]\nselected = [\"Kitchen\", \"Living Room\"]\n",
    )
    .unwrap();
    for room in ["Kitchen", "Living Room"] {
        soap::set_av_transport_uri(
            &s.lan(),
            s.ip(room),
            "x-rincon-mp3radio://example.invalid/original.mp3",
            "",
        )
        .unwrap();
        soap::play(&s.lan(), s.ip(room)).unwrap();
    }
    // A stream refusing Pause with 701 may successfully fall back to Stop.
    // Use a definite refusal that the transport must report as a failure.
    s.sim_handle()
        .unwrap()
        .upnp_fault("Living Room", "Pause", 702)
        .unwrap();
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
        assert!(
            daemon
                .wait_line("fsonos serve: ready", Duration::from_secs(30))
                .is_some(),
            "{:?}",
            daemon.seen
        );
        assert!(
            daemon
                .wait_line("fsonos serve: live", Duration::from_secs(30))
                .is_some(),
            "{:?}",
            daemon.seen
        );
        daemon
    });
    let mode = if via_daemon { "--daemon" } else { "--direct" };
    let start = s.sim_handle().unwrap().soap_log().len();
    let paused = s.cli("partial-pause", &[mode, "--json", "pause", "selected"]);
    let receipt: OutcomeDto =
        serde_json::from_str(&paused.stdout).expect("retain JSON even on nonzero exit");
    assert_partial(&receipt);
    assert_dispatched_once(&s, start);
    s.check(
        "partial-pause",
        "cli",
        "partial pause retains both results and exits nonzero",
        !paused.ok() && paused.stderr.contains("do not replay the batch"),
        format!("{paused:?}"),
    );

    for protocol in ["2024-11-05", "2026-07-28"] {
        soap::play(&s.lan(), s.ip("Kitchen")).unwrap();
        let mut mcp = s.mcp();
        if protocol == "2024-11-05" {
            let initialized = mcp.initialize();
            assert_eq!(initialized["result"]["protocolVersion"], protocol);
        } else {
            mcp.use_modern_protocol();
        }
        let start = s.sim_handle().unwrap().soap_log().len();
        let paused = mcp.request(
            "tools/call",
            &json!({"name":"pause", "arguments":{"zone":"selected"}}),
        );
        assert!(paused.get("error").is_none(), "{protocol}: {paused}");
        let text = paused["result"]["content"][0]["text"]
            .as_str()
            .expect("partial results remain readable in either era");
        let receipt: OutcomeDto = if protocol == "2024-11-05" {
            assert!(paused["result"].get("structuredContent").is_none());
            serde_json::from_str(text.split_once("\n\nReceipt: ").unwrap().1).unwrap()
        } else {
            serde_json::from_value(paused["result"]["structuredContent"].clone())
                .expect("modern MCP retains the typed batch receipt")
        };
        assert_partial(&receipt);
        assert_dispatched_once(&s, start);
        s.check(
            "partial-pause",
            protocol,
            "partial pause sets isError without losing successful targets",
            paused["result"]["isError"] == true
                && text.contains("Kitchen")
                && text.contains("Living Room"),
            &paused,
        );
        drop(mcp);
    }

    s.sim_handle().unwrap().clear_faults("Living Room").unwrap();
    let complete = s.cli("complete-pause", &[mode, "--json", "pause", "selected"]);
    let receipt: OutcomeDto = serde_json::from_str(&complete.stdout).unwrap();
    s.check(
        "complete-pause",
        "cli",
        "all-success batch remains successful",
        complete.ok()
            && receipt.complete()
            && receipt.batch.as_ref().is_some_and(|batch| batch.complete),
        &complete.stdout,
    );
    let single = s.cli("single-pause", &[mode, "--json", "pause", "Kitchen"]);
    let receipt: OutcomeDto = serde_json::from_str(&single.stdout).unwrap();
    s.check(
        "single-pause",
        "cli",
        "single-group response keeps its established shape",
        single.ok() && receipt.batch.is_none(),
        &single.stdout,
    );
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
fn direct_cli_and_standalone_mcp_report_partial_house_commands() {
    partial_commands(false);
}

#[test]
fn daemon_cli_and_forwarded_mcp_report_partial_house_commands() {
    partial_commands(true);
}
