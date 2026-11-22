//! Real CLI and MCP processes retain every bedtime result without replay.

use super::*;
use fsonos_api::surface::schedules::{SleepOutcome, SleepSetDto, SleepTimerDto};
use fsonos_api::{ControlResultDto, OutcomeDto};
use fsonos_proto::control as soap;

fn batch(value: SleepOutcome) -> SleepSetDto {
    match value {
        SleepOutcome::Groups(batch) => batch,
        SleepOutcome::Single(timer) => panic!("two independent groups need two results: {timer:?}"),
    }
}

fn applied_once(s: &Scenario, start: usize, expected: &[&str]) {
    let mut writes: Vec<_> = s.sim_handle().unwrap().soap_log()[start..]
        .iter()
        .filter(|entry| entry.action == "ConfigureSleepTimer")
        .map(|entry| entry.room.clone())
        .collect();
    writes.sort();
    let mut expected: Vec<_> = expected.iter().copied().collect();
    expected.sort_unstable();
    assert_eq!(writes, expected);
}

fn partial(batch: &SleepSetDto) {
    assert!(!batch.complete);
    assert_eq!(batch.applied.len(), 1);
    assert_eq!(batch.failed.len(), 1);
    assert_eq!(batch.applied[0].household, "S1");
    assert_eq!(batch.applied[0].timer.zone, "Kitchen");
    assert_eq!(batch.failed[0].household, "S2");
    assert_eq!(batch.failed[0].zone, "Living Room");
    assert!(!batch.applied[0].coordinator.is_empty());
    assert!(!batch.failed[0].coordinator.is_empty());
}

fn run_bedtime(via_daemon: bool) {
    let mut s = Scenario::start(if via_daemon {
        "bedtime-daemon"
    } else {
        "bedtime-direct"
    });
    s.sim(SimHousehold::standard());
    std::fs::write(
        s.dir().join("data/aliases.toml"),
        "[aliases]\nselected = [\"Kitchen\", \"Living Room\"]\n",
    )
    .unwrap();
    let clock = clock_file(&s);
    set_clock(&clock, now());
    let mut daemon = via_daemon.then(|| serve(&mut s, &clock).0);
    let mode = if via_daemon { "--daemon" } else { "--direct" };
    let start = s.sim_handle().unwrap().soap_log().len();
    let set = s.cli("set-bedtime", &[mode, "--json", "sleep", "selected", "5m"]);
    assert!(set.ok(), "{set:?}");
    let receipt = batch(serde_json::from_str(&set.stdout).unwrap());
    assert!(receipt.complete);
    assert!(receipt.failed.is_empty());
    assert_eq!(receipt.applied.len(), 2);
    for applied in &receipt.applied {
        assert_eq!(applied.timer.fades, via_daemon);
        assert_eq!(applied.timer.remaining_secs, Some(300));
    }
    applied_once(&s, start, &["Kitchen", "Living Room"]);

    let show = s.cli("show-bedtime", &[mode, "--json", "sleep", "selected"]);
    assert!(show.ok(), "{show:?}");
    let timers: Vec<SleepTimerDto> = serde_json::from_str(&show.stdout).unwrap();
    assert_eq!(timers.len(), 2);
    assert!(timers.iter().all(|timer| timer.fades == via_daemon));

    let extended = s.cli(
        "extend-bedtime",
        &[mode, "--json", "sleep", "selected", "--extend", "2m"],
    );
    assert!(extended.ok(), "{extended:?}");
    let receipt = batch(serde_json::from_str(&extended.stdout).unwrap());
    assert!(receipt.complete);
    assert!(receipt.applied.iter().all(|applied| {
        applied
            .timer
            .remaining_secs
            .is_some_and(|left| (410..=420).contains(&left))
    }));

    let cancelled = s.cli(
        "cancel-bedtime",
        &[mode, "--json", "sleep", "selected", "--cancel"],
    );
    assert!(cancelled.ok(), "{cancelled:?}");
    let receipt = batch(serde_json::from_str(&cancelled.stdout).unwrap());
    assert!(receipt.complete);
    assert!(
        receipt
            .applied
            .iter()
            .all(|applied| applied.timer.remaining_secs.is_none())
    );
    for room in ["Kitchen", "Living Room"] {
        assert_eq!(
            soap::get_remaining_sleep_timer(&s.lan(), s.ip(room)).unwrap(),
            None
        );
    }

    s.sim_handle()
        .unwrap()
        .upnp_fault("Living Room", "ConfigureSleepTimer", 702)
        .unwrap();
    let start = s.sim_handle().unwrap().soap_log().len();
    let set = s.cli(
        "partial-bedtime",
        &[mode, "--json", "sleep", "selected", "5m"],
    );
    assert!(!set.ok(), "{set:?}");
    assert!(set.stderr.contains("sleep timer change incomplete"));
    partial(&batch(serde_json::from_str(&set.stdout).unwrap()));
    applied_once(&s, start, &["Kitchen", "Living Room"]);

    for protocol in ["2024-11-05", "2026-07-28"] {
        let mut mcp = s.mcp();
        if protocol == "2024-11-05" {
            mcp.initialize();
        } else {
            mcp.use_modern_protocol();
        }
        let start = s.sim_handle().unwrap().soap_log().len();
        let result = mcp.request(
            "tools/call",
            &json!({
                "name": "set_sleep_timer", "arguments": {"zone": "selected", "duration": "5m"}
            }),
        );
        assert!(result.get("error").is_none(), "{protocol}: {result}");
        assert_eq!(result["result"]["isError"], true, "{protocol}: {result}");
        let text = result["result"]["content"][0]["text"].as_str().unwrap();
        let receipt: SleepOutcome = if protocol == "2024-11-05" {
            serde_json::from_str(text.split_once("\n\nReceipt: ").unwrap().1).unwrap()
        } else {
            serde_json::from_value(result["result"]["structuredContent"].clone()).unwrap()
        };
        partial(&batch(receipt));
        assert!(text.contains("Kitchen") && text.contains("Living Room"));
        applied_once(&s, start, &["Kitchen", "Living Room"]);
        drop(mcp);
    }

    s.sim_handle().unwrap().clear_faults("Living Room").unwrap();
    let start = s.sim_handle().unwrap().soap_log().len();
    let extended = s.cli(
        "extend-missing-timer",
        &[mode, "--json", "sleep", "selected", "--extend", "2m"],
    );
    assert!(!extended.ok(), "{extended:?}");
    partial(&batch(serde_json::from_str(&extended.stdout).unwrap()));
    applied_once(&s, start, &["Kitchen"]);
    assert_eq!(
        soap::get_remaining_sleep_timer(&s.lan(), s.ip("Living Room")).unwrap(),
        None
    );

    let cancel = s.cli(
        "clear-bedtime",
        &[mode, "--json", "sleep", "selected", "--cancel"],
    );
    assert!(cancel.ok(), "{cancel:?}");
    for room in ["Kitchen", "Living Room"] {
        soap::set_av_transport_uri(&s.lan(), s.ip(room), RADIO, "").unwrap();
        soap::play(&s.lan(), s.ip(room)).unwrap();
    }
    let paused = s.cli(
        "fade-house",
        &[mode, "--json", "pause", "selected", "--fade", "1s"],
    );
    assert!(paused.ok(), "{paused:?}");
    let outcome: OutcomeDto = serde_json::from_str(&paused.stdout).unwrap();
    assert!(outcome.complete());
    let results = outcome.batch.unwrap().results;
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|result| matches!(result.result, ControlResultDto::Succeeded { .. }))
    );
    for room in ["Kitchen", "Living Room"] {
        assert_eq!(
            soap::get_transport_info(&s.lan(), s.ip(room))
                .unwrap()
                .state,
            TransportState::Paused
        );
    }
    s.check(
        "bedtime",
        "cli/mcp",
        "room sets set, read, extend, cancel and fade with per-group receipts and no replay",
        true,
        "completed all assertions",
    );
    if let Some(daemon) = &mut daemon {
        assert_eq!(daemon.interrupt(Duration::from_secs(10)), Some(0));
    }
    s.finish();
}

#[test]
fn direct_cli_and_mcp_apply_bedtime_to_every_selected_group() {
    run_bedtime(false);
}

#[test]
fn daemon_cli_and_mcp_apply_bedtime_to_every_selected_group() {
    run_bedtime(true);
}
