//! `fsonos selftest` against the virtual households: every inaudible step
//! passes on both generations, the report names no room, address or player
//! id, `fsonos doctor` reports the kept run, and the house is exactly as it
//! was afterwards (every room's volume, mute, transport state and play mode,
//! and the grouping).

mod e2e;

use e2e::Scenario;
use fsonos_proto::{control, play_mode};
use fsonos_sim::SimHousehold;
use serde_json::Value;

/// The standard virtual house's rooms (the bridge plays nothing).
const ROOMS: [&str; 4] = ["Kitchen", "Office", "Living Room", "Bedroom"];

/// Every room's volume, mute, transport state and play mode, read from the
/// players themselves, and `fsonos zones` for the grouping.
fn house(s: &mut Scenario, step: &str) -> Vec<String> {
    let lan = s.lan();
    let mut out: Vec<String> = ROOMS
        .iter()
        .map(|room| {
            let ip = s.ip(room);
            format!(
                "{room}: {:?} {:?} {:?} {:?}",
                control::get_volume(&lan, ip).ok(),
                control::get_mute(&lan, ip).ok(),
                control::get_transport_info(&lan, ip).ok().map(|i| i.state),
                play_mode::get_play_mode(&lan, ip).ok(),
            )
        })
        .collect();
    out.push(s.cli(step, &["zones", "--direct"]).stdout);
    out
}

#[test]
fn the_inaudible_selftest_passes_and_changes_nothing() {
    let mut s = Scenario::start("selftest");
    s.sim(SimHousehold::standard());
    // A coordinator that leads another room, so the run meets a group.
    let grouped = s.cli("group", &["group", "Office", "Kitchen", "--direct"]);
    s.check(
        "group",
        "cli",
        "Office joins Kitchen's group",
        grouped.ok(),
        grouped.stderr.clone(),
    );
    let before = house(&mut s, "zones-before");

    let run = s.cli("selftest", &["selftest", "--json"]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    let checks = report["checks"].as_array().cloned().unwrap_or_default();
    let ids: Vec<&str> = checks.iter().filter_map(|c| c["id"].as_str()).collect();
    s.check(
        "selftest",
        "cli",
        "the five inaudible checks ran and passed (exit 0)",
        run.ok()
            && ids
                == [
                    "selftest.house",
                    "selftest.reads",
                    "selftest.browse",
                    "selftest.events",
                    "selftest.inaudible_writes",
                ]
            && checks.iter().all(|c| c["status"] == "pass"),
        format!("{}\n{}", run.stdout, run.stderr),
    );
    let houses = checks
        .first()
        .and_then(|c| c["evidence"]["households"].as_array())
        .map_or(0, Vec::len);
    s.check(
        "selftest",
        "cli",
        "both households were tested",
        houses == 2,
        format!("{houses}"),
    );

    // Shareable: no room name, address or player id anywhere in the report.
    let sim = s.sim_handle().expect("the sim");
    let secrets: Vec<String> = ROOMS
        .iter()
        .map(|room| (*room).to_string())
        .chain(
            sim.players()
                .iter()
                .flat_map(|p| [p.uuid.clone(), p.ip.to_string()]),
        )
        .collect();
    let leaks = |text: &str| -> Vec<String> {
        secrets
            .iter()
            .filter(|secret| text.contains(secret.as_str()))
            .cloned()
            .collect()
    };
    let leaked = leaks(&run.stdout);
    s.check(
        "scrubbed",
        "report",
        "the report names no room, address or player id",
        leaked.is_empty(),
        leaked.join(", "),
    );

    // Kept for doctor, as scrubbed as the printed report.
    let kept = std::fs::read_to_string(s.dir().join("data/selftest-last.json")).unwrap_or_default();
    let leaked = leaks(&kept);
    let doctor = s.cli("doctor", &["doctor", "--json", "--only", "selftest.last"]);
    let last: Value = serde_json::from_str(&doctor.stdout).unwrap_or(Value::Null);
    let entry = &last["checks"][0];
    s.check(
        "kept",
        "doctor",
        "doctor reports the run, kept with no room, address or player id, as passed",
        !kept.is_empty()
            && leaked.is_empty()
            && entry["id"] == "selftest.last"
            && entry["status"] == "pass",
        format!("leaked: {leaked:?}\n{}\n{}", doctor.stdout, doctor.stderr),
    );

    let after = house(&mut s, "zones-after");
    s.check(
        "unchanged",
        "sim",
        "every room and group is exactly as before",
        before == after,
        format!("before {before:?}\nafter {after:?}"),
    );
    // The writes reached the players: the control path was exercised.
    let writes = s
        .sim_handle()
        .expect("the sim")
        .soap_log()
        .iter()
        .filter(|e| matches!(e.action.as_str(), "SetVolume" | "SetPlayMode" | "Pause"))
        .count();
    s.check(
        "writes",
        "sim",
        "SetVolume, SetPlayMode and Pause reached the players",
        writes >= ROOMS.len(),
        format!("{writes}"),
    );
    s.finish();
}
