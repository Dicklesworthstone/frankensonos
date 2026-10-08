//! The CLI through the daemon, end to end against the virtual households:
//! with `fsonos serve` running, commands answer from the daemon (found via
//! `daemon.json` in the data directory) with the same JSON they print
//! directly; a control goes through as the CLI (the daemon's log names
//! `cli`); `--direct` skips the daemon and `--daemon` requires it; once the
//! daemon is gone, commands run directly again at once.
//!
//! Through the daemon a command never reads the seeds: given a seeds file
//! that does not exist, it still answers, which a direct run cannot.

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::get_volume;
use fsonos_sim::SimHousehold;
use serde_json::Value;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[test]
fn commands_go_through_a_running_daemon_with_the_same_answers() {
    let mut s = Scenario::start("daemon-client");
    s.sim(SimHousehold::standard());

    // Directly first: no daemon has run in this data directory.
    let zones_direct = s.cli("zones-direct", &["--json", "zones"]);
    let status_direct = s.cli("status-direct", &["--json", "status", "Kitchen"]);
    let no_seeds = s.dir().join("no-such-seeds.toml");
    let no_seeds = no_seeds.to_str().unwrap_or_default().to_string();
    let blind = s.cli("blind-direct", &["--seeds", &no_seeds, "--json", "zones"]);
    s.check(
        "blind-direct",
        "cli",
        "directly, with a seeds file that does not exist, zones cannot answer",
        !blind.ok(),
        &blind.stderr,
    );

    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    let file =
        std::fs::read_to_string(s.dir().join("data").join("daemon.json")).unwrap_or_default();
    s.check(
        "daemon-file",
        "daemon",
        "serve is live and wrote daemon.json with its loopback address and a CLI token",
        live.is_some()
            && json(&file)["http"]
                .as_str()
                .is_some_and(|a| a.starts_with("127.0.0.1:"))
            && json(&file)["cli_token"]
                .as_str()
                .is_some_and(|t| t.len() == 32),
        daemon.seen.join("\n"),
    );

    let zones = s.cli(
        "zones-daemon",
        &["--daemon", "--seeds", &no_seeds, "--json", "zones"],
    );
    s.check(
        "zones",
        "cli",
        "zones answers through the daemon (no seeds needed) with the same JSON as directly",
        zones.ok() && zones_direct.ok() && json(&zones.stdout) == json(&zones_direct.stdout),
        format!(
            "{} ms through the daemon, {} ms directly\n{}\n{}",
            zones.duration_ms, zones_direct.duration_ms, zones.stdout, zones.stderr
        ),
    );
    let status = s.cli(
        "status-daemon",
        &["--seeds", &no_seeds, "--json", "status", "Kitchen"],
    );
    s.check(
        "status",
        "cli",
        "status answers through the daemon with the same JSON as directly",
        status.ok() && json(&status.stdout) == json(&status_direct.stdout),
        format!("{}\n{}", status.stdout, status.stderr),
    );
    let forced = s.cli("zones-forced-direct", &["--direct", "--json", "zones"]);
    s.check(
        "direct-flag",
        "cli",
        "--direct gives the same answer without the daemon",
        forced.ok() && json(&forced.stdout) == json(&zones.stdout),
        &forced.stderr,
    );

    // A control, then the daemon's log: the CLI's own entry, logged by serve.
    let kitchen = s.ip("Kitchen");
    let volume = s.cli("volume-daemon", &["volume", "Kitchen", "33"]);
    let level = get_volume(&s.lan(), kitchen).ok();
    s.check(
        "volume",
        "sim",
        "volume through the daemon sets Kitchen to 33",
        volume.ok() && level == Some(33),
        format!("{level:?} {}", volume.stderr),
    );
    let log = s.cli(
        "log-daemon",
        &["--seeds", &no_seeds, "--json", "log", "--limit", "1"],
    );
    let newest = json(&log.stdout)[0].clone();
    s.check(
        "log",
        "cli",
        "the daemon logged that volume as the CLI's, through serve",
        newest["client"] == "cli" && newest["surface"] == "serve",
        &log.stdout,
    );

    // Killed, not stopped: its daemon.json is left behind, stale.
    drop(daemon);
    let after = s.cli("zones-after-kill", &["--json", "zones"]);
    s.check(
        "after-kill",
        "cli",
        "with the daemon killed (a stale daemon.json), zones runs directly and still answers",
        after.ok()
            && json(&after.stdout)
                .as_array()
                .is_some_and(|z| !z.is_empty()),
        &after.stderr,
    );
    s.finish();
}

#[test]
fn daemon_and_direct_say_what_they_mean() {
    let mut s = Scenario::start("daemon-client-flags");
    s.sim(SimHousehold::standard());
    let required = s.cli("daemon-required", &["--daemon", "--json", "zones"]);
    s.check(
        "daemon-required",
        "cli",
        "--daemon with no daemon is NOT_READY (exit 4)",
        required.code == Some(4) && required.stderr.contains("NOT_READY"),
        &required.stderr,
    );
    let both = s.cli("both-flags", &["--daemon", "--direct", "zones"]);
    s.check(
        "both-flags",
        "cli",
        "--daemon and --direct together are a usage error (exit 2)",
        both.code == Some(2),
        &both.stderr,
    );
    s.finish();
}
