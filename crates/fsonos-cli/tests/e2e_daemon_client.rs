//! The CLI through the daemon, end to end against the virtual households:
//! with `fsonos serve` running, commands answer from the daemon (found via
//! `daemon.json` in the data directory) with the same JSON they print
//! directly; a control goes through as the CLI (the daemon's log names
//! `cli`); `--direct` skips the daemon and `--daemon` requires it; once the
//! daemon is gone, commands run directly again at once.
//!
//! Through the daemon a command never reads the seeds: given a seeds file
//! that does not exist, it still answers, which a direct run cannot. That
//! holds for `play --search` too: the daemon searches and plays. From
//! another machine (`--daemon` with the daemon's address), scenes, schedules
//! and the policy are the daemon's.

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::{get_position_info, get_volume};
use fsonos_sim::SimHousehold;
use serde_json::Value;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// `fsonos discover` through the daemon (it needs no seeds) prints what a
/// direct one (`--direct`, its own survey) prints.
fn same_discover(s: &mut Scenario, no_seeds: &str) {
    let direct = s
        .cli("discover-direct", &["--direct", "--json", "discover"])
        .stdout;
    let run = s.cli(
        "discover-daemon",
        &["--daemon", "--seeds", no_seeds, "--json", "discover"],
    );
    s.check(
        "discover",
        "cli",
        "discover answers through the daemon with the same JSON as directly",
        run.ok()
            && json(&run.stdout)["players"]
                .as_array()
                .is_some_and(|p| !p.is_empty())
            && json(&run.stdout) == json(&direct),
        format!(
            "through the daemon:\n{}\ndirectly:\n{direct}\n{}",
            run.stdout, run.stderr
        ),
    );
}

/// `fsonos play --search` through the daemon (it needs no seeds): a bare
/// --pick lists the daemon's matches, and the search plays the best.
fn search_through_daemon(s: &mut Scenario, no_seeds: &str) {
    let found = s.cli(
        "search-cmd-daemon",
        &[
            "--daemon", "--seeds", no_seeds, "search", "sim", "radio", "--zone", "Kitchen",
        ],
    );
    s.check(
        "search",
        "cli",
        "fsonos search lists the daemon's matches, numbered, best first",
        found.ok()
            && found.stdout.starts_with("1. Sim Radio")
            && found.stdout.contains("[favorite]"),
        format!("{}\n{}", found.stdout, found.stderr),
    );
    let pick = s.cli(
        "search-pick-daemon",
        &[
            "--daemon",
            "--seeds",
            no_seeds,
            "play",
            "Kitchen",
            "--search",
            "sim radio",
            "--pick",
        ],
    );
    s.check(
        "search-pick",
        "cli",
        "play --search --pick lists the daemon's matches, as directly",
        pick.ok() && pick.stdout.starts_with("1. Sim Radio") && pick.stdout.contains("[favorite]"),
        format!("{}\n{}", pick.stdout, pick.stderr),
    );
    let run = s.cli(
        "search-daemon",
        &[
            "--daemon",
            "--seeds",
            no_seeds,
            "play",
            "Kitchen",
            "--search",
            "sim radio",
        ],
    );
    let uri = get_position_info(&s.lan(), s.ip("Kitchen"))
        .map(|p| p.uri)
        .unwrap_or_default();
    s.check(
        "search-play",
        "cli",
        "play --search through the daemon plays the best match: the Sim Radio station",
        run.ok() && uri.starts_with("x-rincon-mp3radio:"),
        format!("{}; now on {uri}", run.stderr),
    );
}

/// The data directory's daemon.json as text (empty when there is none).
fn daemon_file(s: &Scenario) -> String {
    std::fs::read_to_string(s.dir().join("data").join("daemon.json")).unwrap_or_default()
}

/// `serve` leaves daemon.json for the CLI, and a second daemon for the same
/// data directory refuses to start: it would take that file over while the
/// first still holds the event port.
fn check_the_daemon_file(s: &mut Scenario, daemon: &e2e::Daemon, live: bool) {
    let file = daemon_file(s);
    s.check(
        "daemon-file",
        "daemon",
        "serve is live and wrote daemon.json with its loopback address and a CLI token",
        live && json(&file)["http"]
            .as_str()
            .is_some_and(|a| a.starts_with("127.0.0.1:"))
            && json(&file)["cli_token"]
                .as_str()
                .is_some_and(|t| t.len() == 32),
        daemon.seen.join("\n"),
    );
    let mut second = s.spawn("serve-twice", &["serve", "--http", "127.0.0.1:0"]);
    let refused = second.wait_line(
        "already runs for this data directory",
        Duration::from_secs(10),
    );
    let still = daemon_file(s);
    s.check(
        "serve-twice",
        "daemon",
        "a second serve on the same data directory refuses, and daemon.json stays the first's",
        refused.is_some() && still == file,
        second.seen.join("\n"),
    );
}

#[test]
fn commands_go_through_a_running_daemon_with_the_same_answers() {
    let mut s = Scenario::start("daemon-client");
    s.sim(SimHousehold::standard());

    // Directly first: no daemon has run in this data directory.
    let zones_direct = s.cli("zones-direct", &["--json", "zones"]);
    let room_direct = s.cli("zone-direct", &["--json", "zones", "Kitchen"]);
    let status_direct = s.cli("status-direct", &["--json", "status", "Kitchen"]);
    let no_seeds = s.dir().join("no-such-seeds.toml").display().to_string();
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
    check_the_daemon_file(&mut s, &daemon, live.is_some());

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
    let zone = s.cli(
        "zone-daemon",
        &["--seeds", &no_seeds, "--json", "zones", "Kitchen"],
    );
    s.check(
        "zone",
        "cli",
        "zones Kitchen answers through the daemon with the same JSON as directly: one zone",
        zone.ok()
            && json(&zone.stdout) == json(&room_direct.stdout)
            && json(&zone.stdout).is_object(),
        format!("{}\n{}\n{}", zone.stdout, zone.stderr, room_direct.stderr),
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
    same_discover(&mut s, &no_seeds);
    let forced = s.cli("zones-forced-direct", &["--direct", "--json", "zones"]);
    s.check(
        "direct-flag",
        "cli",
        "--direct gives the same answer without the daemon",
        forced.ok() && json(&forced.stdout) == json(&zones.stdout),
        &forced.stderr,
    );

    // Searches and controls change what plays, so they come after the
    // same-answer comparisons above.
    search_through_daemon(&mut s, &no_seeds);
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

/// `fsonos --daemon <args>` as another machine runs it: a data directory of
/// its own, the daemon's address in FSONOS_HTTP_ADDR, and no routes file (it
/// asks only the daemon, never the speakers).
fn elsewhere(s: &mut Scenario, http: &str, step: &str, args: &[&str]) -> e2e::Run {
    let dir = s.dir().join("elsewhere");
    std::fs::create_dir_all(&dir).expect("create the other machine's data directory");
    let dir = dir.display().to_string();
    let env = [
        ("FSONOS_DATA_DIR", dir.as_str()),
        ("FSONOS_HTTP_ADDR", http),
    ];
    let args: Vec<&str> = std::iter::once("--daemon")
        .chain(args.iter().copied())
        .collect();
    s.cli_env(step, &args, &env, &["FSONOS_ROUTES", "FSONOS_SEEDS"])
}

/// Scenes, schedules and the policy from another machine are the daemon's,
/// not that machine's empty store's.
#[test]
fn scenes_schedules_and_the_policy_are_the_daemons_from_elsewhere() {
    let mut s = Scenario::start("daemon-client-elsewhere");
    s.sim(SimHousehold::standard());
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    let http = json(&daemon_file(&s))["http"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let saved = s.cli("scene-save", &["scene", "save", "Evening"]);
    let listed = elsewhere(
        &mut s,
        &http,
        "scene-list-elsewhere",
        &["--json", "scene", "list"],
    );
    let shown = elsewhere(
        &mut s,
        &http,
        "scene-show-elsewhere",
        &["scene", "show", "Evening"],
    );
    s.check(
        "scenes",
        "cli",
        "scene list and show from elsewhere answer with the daemon's scene",
        live.is_some() && saved.ok() && json(&listed.stdout)[0]["name"] == "Evening" && shown.ok(),
        format!("{listed:?}\n{shown:?}"),
    );
    let added = elsewhere(
        &mut s,
        &http,
        "schedule-add-elsewhere",
        &["schedule", "add", "daily 22:00", "pause", "Kitchen"],
    );
    let here = s.cli(
        "schedule-list-here",
        &["--direct", "--json", "schedule", "list"],
    );
    s.check(
        "schedules",
        "cli",
        "a schedule added from elsewhere is in the daemon's store",
        added.ok()
            && json(&here.stdout)
                .as_array()
                .is_some_and(|all| all.len() == 1),
        format!("{added:?}\n{here:?}"),
    );
    let removed = elsewhere(
        &mut s,
        &http,
        "scene-rm-elsewhere",
        &["scene", "rm", "Evening"],
    );
    let after = s.cli("scene-list-here", &["--direct", "--json", "scene", "list"]);
    s.check(
        "scene-rm",
        "cli",
        "scene rm from elsewhere removes the daemon's scene",
        removed.ok() && json(&after.stdout).as_array().is_some_and(Vec::is_empty),
        format!("{removed:?}\n{after:?}"),
    );
    // Capped as loopback-http, Kitchen goes up one step at most: the daemon
    // notes it, and the CLI prints the note.
    let clamped = elsewhere(
        &mut s,
        &http,
        "volume-elsewhere",
        &["volume", "Kitchen", "90"],
    );
    s.check(
        "notes",
        "cli",
        "a control through the daemon prints its notes (the volume the policy lowered)",
        clamped.ok() && clamped.stdout.contains("\nnote: "),
        format!("{clamped:?}"),
    );
    let policy = elsewhere(
        &mut s,
        &http,
        "policy-elsewhere",
        &["--json", "policy", "show"],
    );
    s.check(
        "policy",
        "cli",
        "policy show from elsewhere is the daemon's, naming the caller as the daemon sees it",
        json(&policy.stdout)["you"] == "loopback-http",
        format!("{policy:?}"),
    );
    s.finish();
}

/// From another machine by URL (`--daemon-url`, as Tailscale Serve's
/// `https://<mac>.<tailnet>.ts.net` would be): commands answer from that
/// daemon, with no daemon.json, routes or seeds of their own.
#[test]
fn a_daemon_url_reaches_the_daemon_from_elsewhere() {
    let mut s = Scenario::start("daemon-client-url");
    s.sim(SimHousehold::standard());
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    let http = json(&daemon_file(&s))["http"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let dir = s.dir().join("elsewhere");
    std::fs::create_dir_all(&dir).expect("create the other machine's data directory");
    let dir = dir.display().to_string();
    let env = [("FSONOS_DATA_DIR", dir.as_str())];
    let unset = ["FSONOS_ROUTES", "FSONOS_SEEDS"];
    let url = format!("http://{http}/");
    let zones = s.cli_env(
        "zones-by-url",
        &["--daemon-url", &url, "--json", "zones"],
        &env,
        &unset,
    );
    s.check(
        "by-url",
        "cli",
        "zones by the daemon's URL answers with the daemon's house",
        live.is_some()
            && zones.ok()
            && json(&zones.stdout)
                .as_array()
                .is_some_and(|z| !z.is_empty()),
        format!("{zones:?}"),
    );
    let bad = s.cli_env(
        "bad-url",
        &["--daemon-url", "ftp://nowhere", "zones"],
        &env,
        &unset,
    );
    s.check(
        "bad-url",
        "cli",
        "a URL that is not http(s) is an invalid argument (exit 2)",
        bad.code == Some(2) && bad.stderr.contains("INVALID_ARGUMENT"),
        format!("{bad:?}"),
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
