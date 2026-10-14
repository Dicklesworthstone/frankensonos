//! `fsonos setup --yes --skip-spotify` against the virtual households, from a
//! fresh data directory. Every step but the skipped sign-in passes, one JSON
//! line each, in order. Multicast works here, so no seeds.toml is written,
//! and a second run reports the same (setup is idempotent). The text output
//! marks each step. Without --skip-spotify and no terminal, the sign-in
//! step says what is missing (an app client id, then a terminal) and only
//! warns: exit 6.

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::{get_position_info, get_transport_info, get_volume};
use fsonos_sim::SimHousehold;
use serde_json::Value;

/// The JSON lines of a `--json` run.
fn steps(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// `(step, status)` for each line.
fn verdicts(steps: &[Value]) -> Vec<(String, String)> {
    steps
        .iter()
        .map(|s| {
            (
                s["step"].as_str().unwrap_or("?").to_owned(),
                s["status"].as_str().unwrap_or("?").to_owned(),
            )
        })
        .collect()
}

#[test]
fn setup_takes_a_fresh_data_dir_through_every_step() {
    let mut s = Scenario::start("setup");
    s.sim(SimHousehold::standard());

    let run = s.cli("setup", &["setup", "--yes", "--skip-spotify", "--json"]);
    let first = steps(&run.stdout);
    let expected: Vec<(String, String)> = [
        ("data", "pass"),
        ("discovery", "pass"),
        ("households", "pass"),
        ("spotify-login", "skip"),
        ("spotify-households", "pass"),
        ("test-play", "skip"),
        ("tailscale", "skip"),
        ("next-steps", "pass"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
    .collect();
    s.check(
        "setup",
        "cli",
        "every step but the skipped sign-in (and Tailscale, off here) passes, in order: exit 0",
        run.code == Some(0) && verdicts(&first) == expected,
        format!("exit {:?}\n{}{}", run.code, run.stdout, run.stderr),
    );
    let households = first
        .iter()
        .find(|step| step["step"] == "households")
        .and_then(|step| step["summary"].as_str())
        .unwrap_or_default()
        .to_owned();
    s.check(
        "households",
        "cli",
        "the households step names both households and their rooms",
        households.contains("S1: ")
            && households.contains("S2: ")
            && households.contains("Kitchen"),
        &households,
    );
    let next = first
        .last()
        .and_then(|step| step["summary"].as_str())
        .unwrap_or_default()
        .to_owned();
    s.check(
        "next-steps",
        "cli",
        "the last step says what to run next: serve, launchd, Tailscale, the MCP line",
        next.contains("fsonos serve")
            && next.contains("docs/DEPLOY.md")
            && next.contains("fsonos tailscale setup")
            && next.contains("claude mcp add fsonos -- fsonos mcp"),
        &next,
    );
    let seeds = s.dir().join("data").join("seeds.toml");
    s.check(
        "no-seeds",
        "store",
        "multicast finds the players here, so setup writes no seeds.toml",
        !seeds.exists(),
        seeds.display().to_string(),
    );

    let run = s.cli(
        "setup-again",
        &["setup", "--yes", "--skip-spotify", "--json"],
    );
    s.check(
        "setup-again",
        "cli",
        "a second run reports the same steps the same way",
        run.code == Some(0) && verdicts(&steps(&run.stdout)) == expected,
        format!("exit {:?}\n{}", run.code, run.stdout),
    );

    let run = s.cli("setup-text", &["setup", "--yes", "--skip-spotify"]);
    s.check(
        "setup-text",
        "cli",
        "without --json each step is a marked line",
        run.code == Some(0)
            && run.stdout.contains("[ok  ] Data directory and store")
            && run
                .stdout
                .contains("[skip] Spotify sign-in: skipped (--skip-spotify)"),
        &run.stdout,
    );
    check_sign_in(&mut s);
    check_test_play(&mut s);
    s.finish();
}

/// What Kitchen is doing: (volume, transport state, URI).
fn kitchen(s: &Scenario) -> (Option<u8>, Option<String>, Option<String>) {
    let (lan, ip) = (s.lan(), s.ip("Kitchen"));
    (
        get_volume(&lan, ip).ok(),
        get_transport_info(&lan, ip)
            .ok()
            .map(|t| format!("{:?}", t.state)),
        get_position_info(&lan, ip).ok().map(|p| p.uri),
    )
}

/// `fsonos setup --test-play Kitchen`: the run's code and its test-play
/// step.
fn test_play_run(s: &mut Scenario, step: &str) -> (Option<i32>, Value) {
    let run = s.cli(
        step,
        &[
            "setup",
            "--yes",
            "--skip-spotify",
            "--json",
            "--test-play",
            "Kitchen",
        ],
    );
    let found = steps(&run.stdout)
        .into_iter()
        .find(|line| line["step"] == "test-play")
        .unwrap_or_default();
    (run.code, found)
}

/// --test-play plays a favorite softly in Kitchen's group and puts it back:
/// exactly when it was playing, and stopped again when it had nothing
/// loaded (Sonos cannot unload a source).
fn check_test_play(s: &mut Scenario) {
    let before = kitchen(s);
    let (code, step) = test_play_run(s, "setup-test-play-idle");
    let after = kitchen(s);
    s.check(
        "setup-test-play-idle",
        "cli",
        "on an idle Kitchen, --test-play plays at volume 10 for 5 s, then stops again at its old volume, saying the track stays loaded",
        code == Some(0)
            && step["status"] == "pass"
            && step["summary"]
                .as_str()
                .is_some_and(|t| t.contains("at volume 10 for 5 s, then stopped it again"))
            && (before.0, &before.1) == (after.0, &after.1),
        format!("exit {code:?}\n{step}\nbefore {before:?}\nafter {after:?}"),
    );

    let stream = "x-rincon-mp3radio://stream.example.invalid/setup.mp3";
    let run = s.cli("play-stream", &["play", "Kitchen", stream]);
    let before = kitchen(s);
    let (code, step) = test_play_run(s, "setup-test-play-playing");
    let after = kitchen(s);
    s.check(
        "setup-test-play-playing",
        "cli",
        "on a Kitchen playing a stream, --test-play puts it back exactly: the stream, playing, its volume",
        run.code == Some(0)
            && code == Some(0)
            && step["status"] == "pass"
            && step["summary"]
                .as_str()
                .is_some_and(|t| t.ends_with("then put it back"))
            && before.2.as_deref() == Some(stream)
            && before == after,
        format!("exit {code:?}\n{step}\nbefore {before:?}\nafter {after:?}"),
    );
}

/// Without --skip-spotify and no terminal, the sign-in says what is missing
/// and only warns.
fn check_sign_in(s: &mut Scenario) {
    let login = |run: &e2e::Run| {
        steps(&run.stdout)
            .into_iter()
            .find(|step| step["step"] == "spotify-login")
            .unwrap_or_default()
    };
    let run = s.cli("setup-no-app", &["setup", "--yes", "--json"]);
    let step = login(&run);
    s.check(
        "setup-no-app",
        "cli",
        "without an app client id the sign-in warns with how to get one: exit 6",
        run.code == Some(6)
            && step["status"] == "warn"
            && step["remedies"]
                .to_string()
                .contains("FSONOS_SPOTIFY_CLIENT_ID"),
        format!("exit {:?}\n{}", run.code, run.stdout),
    );
    let run = s.cli(
        "setup-no-terminal",
        &[
            "setup",
            "--yes",
            "--json",
            "--spotify-client-id",
            "0123456789abcdef0123456789abcdef",
        ],
    );
    let step = login(&run);
    s.check(
        "setup-no-terminal",
        "cli",
        "with a client id but no terminal it is not signed in, says how to sign in, and reads nothing: exit 6",
        run.code == Some(6)
            && step["status"] == "warn"
            && step["summary"] == "not signed in to Spotify"
            && step["remedies"].to_string().contains("on a terminal"),
        format!("exit {:?}\n{}", run.code, run.stdout),
    );
}
