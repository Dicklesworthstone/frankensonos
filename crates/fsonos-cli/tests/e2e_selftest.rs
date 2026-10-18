//! `fsonos selftest` against the virtual households: every inaudible step
//! passes on both generations, the report names no room, address or player
//! id, `fsonos doctor` reports the kept run, and the house is exactly as it
//! was afterwards (every room's volume, mute, transport state and play mode,
//! and the grouping). `--transcript` keeps every exchange raw in a local
//! file while the report stays scrubbed.

mod e2e;

use e2e::Scenario;
use fsonos_proto::{control, play_mode};
use fsonos_sim::{NotifyDrop, SimHousehold};
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

/// The room names, addresses and player ids of the sim that appear in
/// `text`: a shareable report has none.
fn leaks(s: &Scenario, text: &str) -> Vec<String> {
    let mut secrets: Vec<String> = ROOMS.iter().map(|room| (*room).to_string()).collect();
    if let Some(sim) = s.sim_handle() {
        for p in sim.players() {
            secrets.push(p.uuid.clone());
            secrets.push(p.ip.to_string());
        }
    }
    secrets.retain(|secret| text.contains(secret.as_str()));
    secrets
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
    let leaked = leaks(&s, &run.stdout);
    s.check(
        "scrubbed",
        "report",
        "the report names no room, address or player id",
        leaked.is_empty(),
        leaked.join(", "),
    );

    // Kept for doctor, as scrubbed as the printed report.
    let kept = std::fs::read_to_string(s.dir().join("data/selftest-last.json")).unwrap_or_default();
    let leaked = leaks(&s, &kept);
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

/// `--transcript`: every exchange of the run, raw, in a local JSON-lines
/// file (SOAP and GENA alike, as the players sent them), while the printed
/// report stays scrubbed; with no path it lands in the data directory.
#[test]
fn a_transcript_keeps_every_exchange_raw_while_the_report_stays_scrubbed() {
    let mut s = Scenario::start("selftest-transcript");
    s.sim(SimHousehold::standard());
    let path = s.dir().join("transcript.jsonl");
    let path_arg = path.to_string_lossy().into_owned();
    let run = s.cli(
        "selftest",
        &["selftest", "--json", "--transcript", &path_arg],
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let soap = |action: &str| {
        lines.iter().any(|x| {
            x["method"] == "POST" && x["headers"].to_string().contains(action) && x["status"] == 200
        })
    };
    s.check(
        "transcript",
        "cli",
        "the run passes; its transcript has SOAP reads and writes and GENA subscriptions, answered",
        run.ok()
            && soap("GetZoneGroupState")
            && soap("SetVolume")
            && lines
                .iter()
                .any(|x| x["method"] == "SUBSCRIBE" && x["status"] == 200)
            && lines.iter().any(|x| x["method"] == "UNSUBSCRIBE")
            && run.stderr.contains("not scrubbed"),
        format!("{} lines\n{}", lines.len(), run.stderr),
    );
    let printed = leaks(&s, &run.stdout);
    s.check(
        "transcript-raw",
        "cli",
        "the transcript is raw (ids and rooms as sent); the printed report still names none",
        !leaks(&s, &raw).is_empty() && printed.is_empty(),
        printed.join(", "),
    );
    let bare = s.cli(
        "selftest-default",
        &["selftest", "--transcript", "--only", "selftest.house"],
    );
    let kept: Vec<String> = std::fs::read_dir(s.dir().join("data"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|x| x == "jsonl")
                        && p.file_name().is_some_and(|n| {
                            n.to_string_lossy().starts_with("selftest-transcript-")
                        })
                })
                .map(|p| p.display().to_string())
                .collect()
        })
        .unwrap_or_default();
    s.check(
        "transcript-default",
        "cli",
        "with no path, the transcript lands in the data directory",
        bare.ok() && kept.len() == 1,
        format!("{kept:?}\n{}", bare.stderr),
    );
    s.finish();
}

/// `fsonos selftest --json` on the scenario's house: its exit code, the
/// JSON report and the raw output.
fn selftest(s: &mut Scenario, step: &str) -> (Option<i32>, Value, String) {
    let run = s.cli(step, &["selftest", "--json"]);
    let report = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    (run.code, report, format!("{}\n{}", run.stdout, run.stderr))
}

/// Check `id` of a self-test report (null when it is not there).
fn entry(report: &Value, id: &str) -> Value {
    report["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|x| x["id"] == id))
        .cloned()
        .unwrap_or(Value::Null)
}

/// `check` failed, its detail naming `action` and with a remedy.
fn failed_with(check: &Value, action: &str) -> bool {
    check["status"] == "fail"
        && check["detail"].as_str().is_some_and(|d| d.contains(action))
        && check["remedy"].as_str().is_some_and(|r| !r.is_empty())
}

/// Clear every fault in the sim.
fn clear_faults(s: &Scenario) -> bool {
    let sim = s.sim_handle().expect("the sim");
    sim.players()
        .iter()
        .all(|p| sim.clear_faults(&p.room).is_ok())
}

/// A house that refuses a read and drops every event, then one that refuses
/// a (no-op) write: each run reports its failures with their evidence (still
/// naming no room, address or player id) and exits 7 rather than crashing;
/// writes are not attempted when the reads failed; doctor reports the kept
/// run as not passed; and the house is as it was once the faults clear.
#[test]
fn a_faulty_house_fails_the_selftest_with_evidence_and_is_left_as_it_was() {
    let mut s = Scenario::start("selftest-faults");
    s.sim(SimHousehold::standard());
    let before = house(&mut s, "zones-before");

    let sim = s.sim_handle().expect("the sim");
    let injected = sim.upnp_fault("Kitchen", "GetPositionInfo", 501).is_ok()
        && sim.players().iter().all(|p| {
            sim.drop_notifies(&p.room, Some(NotifyDrop::Probability(1.0)))
                .is_ok()
        });
    let (code, report, out) = selftest(&mut s, "selftest-reads");
    let events = entry(&report, "selftest.events");
    s.check(
        "reads",
        "cli",
        "a refused read fails the reads with its evidence, the lost events do not pass, \
         the writes are not attempted, exit 7",
        injected
            && code == Some(7)
            && failed_with(&entry(&report, "selftest.reads"), "GetPositionInfo")
            && !events["status"].is_null()
            && events["status"] != "pass"
            && entry(&report, "selftest.inaudible_writes")["status"] == "skip",
        &out,
    );
    let leaked = leaks(&s, &out);
    s.check(
        "scrubbed-reads",
        "report",
        "the failure report names no room, address or player id",
        leaked.is_empty(),
        leaked.join(", "),
    );

    let sim = s.sim_handle().expect("the sim");
    let injected = clear_faults(&s) && sim.upnp_fault("Bedroom", "SetVolume", 501).is_ok();
    let (code, report, out) = selftest(&mut s, "selftest-writes");
    s.check(
        "writes",
        "cli",
        "a refused no-op write fails the writes with its evidence, the reads pass, exit 7",
        injected
            && code == Some(7)
            && entry(&report, "selftest.reads")["status"] == "pass"
            && failed_with(&entry(&report, "selftest.inaudible_writes"), "SetVolume"),
        &out,
    );
    let leaked = leaks(&s, &out);
    s.check(
        "scrubbed-writes",
        "report",
        "the failure report names no room, address or player id",
        leaked.is_empty(),
        leaked.join(", "),
    );

    let doctor = s.cli("doctor", &["doctor", "--json", "--only", "selftest.last"]);
    let last: Value = serde_json::from_str(&doctor.stdout).unwrap_or(Value::Null);
    let kept = &last["checks"][0];
    s.check(
        "kept",
        "doctor",
        "doctor reports the kept run as not passed, naming the failed check",
        kept["status"] == "warn"
            && kept["summary"]
                .as_str()
                .is_some_and(|t| t.contains("selftest.inaudible_writes")),
        format!("{}\n{}", doctor.stdout, doctor.stderr),
    );

    let cleared = clear_faults(&s);
    let after = house(&mut s, "zones-after");
    s.check(
        "unchanged",
        "sim",
        "every room and group is exactly as before",
        cleared && before == after,
        format!("before {before:?}\nafter {after:?}"),
    );
    s.finish();
}
