//! Refreshing the DJ's library (scenario `dj_sync`), without Spotify:
//! `fsonos serve` with a Spotify app but no sign-in waits quietly (its
//! daily check finds nothing to do), `GET /dj/sync` says the library has
//! not been refreshed, and asking for a refresh, over HTTP or with
//! `fsonos dj sync` through the daemon, is SPOTIFY_AUTH_REQUIRED, with
//! nothing read. Without a daemon, `fsonos dj sync` needs a client id, and
//! signed out it refuses the same way. A real refresh is fsonos-spotify's
//! `sync_library`, tested there against a fake Spotify.

mod e2e;

use e2e::{Scenario, http};
use fsonos_sim::SimHousehold;
use serde_json::Value;
use std::time::Duration;

/// A placeholder Spotify app client id (the format, not a real app).
const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// The daemon's answers: the state, then a refused start.
fn over_http(s: &mut Scenario, api: &str) {
    let state = http(api, "GET", "/dj/sync", &[], "");
    s.check(
        "status",
        "http",
        "GET /dj/sync: not running, never refreshed, no error (signed out, the check waits)",
        state.as_ref().is_ok_and(|(code, _, body)| {
            let v = json(body);
            *code == 200
                && v["running"] == false
                && v.get("last").is_none()
                && v.get("error").is_none()
                && v["done"]
                    .as_str()
                    .is_some_and(|d| d.contains("not been refreshed"))
        }),
        format!("{state:?}"),
    );
    let start = http(
        api,
        "POST",
        "/dj/sync",
        &[("Content-Type", "application/json")],
        "{}",
    );
    s.check(
        "start-signed-out",
        "http",
        "POST /dj/sync signed out is 409 SPOTIFY_AUTH_REQUIRED, naming the fix",
        start.as_ref().is_ok_and(|(code, _, body)| {
            *code == 409 && body.contains("SPOTIFY_AUTH_REQUIRED") && body.contains("fsonos setup")
        }),
        format!("{start:?}"),
    );
}

#[test]
fn a_signed_out_daemon_refreshes_nothing_and_says_why() {
    let mut s = Scenario::start("dj_sync");
    s.sim(SimHousehold::standard());

    // No daemon: a refresh needs the Spotify app's client id, then a sign-in.
    let no_app = s.cli("direct-no-app", &["--direct", "dj", "sync"]);
    s.check(
        "direct-no-app",
        "cli",
        "without a daemon or a client id, dj sync is INVALID_ARGUMENT (exit 2)",
        no_app.code == Some(2) && no_app.stderr.contains("FSONOS_SPOTIFY_CLIENT_ID"),
        &no_app.stderr,
    );
    let direct = s.cli(
        "direct-signed-out",
        &["--direct", "dj", "sync", "--spotify-client-id", CLIENT_ID],
    );
    s.check(
        "direct-signed-out",
        "cli",
        "without a daemon, signed out, dj sync is SPOTIFY_AUTH_REQUIRED (exit 1)",
        direct.code == Some(1) && direct.stderr.contains("SPOTIFY_AUTH_REQUIRED"),
        &direct.stderr,
    );

    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--spotify-client-id",
            CLIENT_ID,
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_owned();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "ready",
        "daemon",
        "serve is up with a Spotify app and no sign-in",
        api.starts_with("127.0.0.1:") && live.is_some(),
        daemon.seen.join("\n"),
    );

    over_http(&mut s, &api);
    let cli = s.cli("cli-through-daemon", &["--daemon", "dj", "sync"]);
    s.check(
        "cli-through-daemon",
        "cli",
        "fsonos dj sync through the daemon, signed out, is SPOTIFY_AUTH_REQUIRED (exit 1)",
        cli.code == Some(1) && cli.stderr.contains("SPOTIFY_AUTH_REQUIRED"),
        &cli.stderr,
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "quiet",
        "daemon",
        "the daemon never claimed a refresh, and SIGINT stops it cleanly",
        code == Some(0) && !daemon.seen.iter().any(|l| l.contains("Library refreshed")),
        format!("exit {code:?}\n{}", daemon.seen.join("\n")),
    );
    s.finish();
}
