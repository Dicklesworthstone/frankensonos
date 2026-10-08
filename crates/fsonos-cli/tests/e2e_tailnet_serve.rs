//! `fsonos tailscale setup / status / teardown` against this host's real
//! Tailscale Serve (ts-serve). Off by default and in CI, and doubly opt-in:
//! besides the `tailscale-live` feature it needs `FSONOS_LIVE_SERVE=1`,
//! because it changes this host's Serve config for the run, and the HTTPS
//! certificate Serve obtains puts the host's MagicDNS name in public
//! Certificate Transparency logs. Run it on the daemon host, deliberately:
//!
//! ```text
//! FSONOS_LIVE_SERVE=1 cargo test -p fsonos-cli --features tailscale-live \
//!     --test e2e_tailnet_serve -- --nocapture
//! ```
//!
//! It touches Serve only when ports 443 and 8443 are free and Funnel is off,
//! and tears down what it added. The daemon serves the virtual households.
#![cfg(feature = "tailscale-live")]

mod e2e;

use e2e::{Run, Scenario};
use fsonos_sim::SimHousehold;
use fsonos_tailscale::serve::PortState;
use std::process::Command;
use std::time::Duration;

/// `key=value` out of the ready line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// `GET <url>` over HTTPS with curl: the status code, or why not.
fn https_status(url: &str) -> Result<u16, String> {
    let out = Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "20",
            url,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The host's MagicDNS name when this run may touch Serve: opted in, on a
/// tailnet with MagicDNS, ports 443/8443 free and Funnel off. Otherwise the
/// reason is recorded as pending.
fn ready_to_touch(s: &mut Scenario) -> Option<String> {
    if std::env::var("FSONOS_LIVE_SERVE").as_deref() != Ok("1") {
        s.pending(
            "opt-in",
            "set FSONOS_LIVE_SERVE=1: this changes the host's Serve config for the run and \
             publishes its MagicDNS name in Certificate Transparency logs",
        );
        return None;
    }
    let status = fsonos_tailscale::detect();
    let Some(name) = status.running().and_then(|t| t.magic_dns_name.clone()) else {
        s.pending(
            "tailnet",
            &format!("no running tailnet with MagicDNS: {status:?}"),
        );
        return None;
    };
    let before = fsonos_tailscale::Probe::default()
        .serve_config()
        .expect("read Serve's config");
    if before.port(443) != PortState::Free
        || before.port(8443) != PortState::Free
        || before.funnel(443)
        || before.funnel(8443)
    {
        s.pending(
            "serve",
            "ports 443/8443 are in use (or Funnel is on): not touching them",
        );
        return None;
    }
    Some(name)
}

/// `fsonos tailscale --http <api> --mcp-http <mcp> <action>`, as `step`.
fn tailscale(s: &mut Scenario, step: &str, listeners: &[&str], action: &str) -> Run {
    let mut args = vec!["tailscale"];
    args.extend_from_slice(listeners);
    args.push(action);
    s.cli(step, &args)
}

/// Setup (twice), HTTPS through Serve, status, teardown.
fn round_trip(s: &mut Scenario, name: &str, listeners: &[&str]) {
    let setup = tailscale(s, "setup", listeners, "setup");
    eprintln!("tailscale-live: setup\n{}{}", setup.stdout, setup.stderr);
    s.check(
        "setup",
        "cli",
        "fsonos tailscale setup adds both mappings",
        setup.ok(),
        format!("{}{}", setup.stdout, setup.stderr),
    );
    let again = tailscale(s, "setup-again", listeners, "setup");
    s.check(
        "setup-again",
        "cli",
        "running setup again changes nothing",
        again.ok() && again.stdout.matches("already as wanted").count() == 2,
        &again.stdout,
    );
    let health = https_status(&format!("https://{name}/health"));
    s.check(
        "https",
        "serve",
        "GET https://<name>/health through Serve is 200",
        health == Ok(200),
        format!("{health:?}"),
    );
    let status = tailscale(s, "status", listeners, "status");
    s.check(
        "status",
        "cli",
        "status shows both mappings active",
        status.ok() && status.stdout.matches("active").count() == 2,
        &status.stdout,
    );
    let teardown = tailscale(s, "teardown", listeners, "teardown");
    let after = fsonos_tailscale::Probe::default()
        .serve_config()
        .expect("read Serve's config");
    s.check(
        "teardown",
        "cli",
        "teardown leaves ports 443 and 8443 as they were",
        teardown.ok() && after.port(443) == PortState::Free && after.port(8443) == PortState::Free,
        format!("{}{} {after:?}", teardown.stdout, teardown.stderr),
    );
}

#[test]
fn setup_status_and_teardown_on_this_hosts_serve() {
    let mut s = Scenario::start("tailnet_serve");
    let Some(name) = ready_to_touch(&mut s) else {
        s.finish();
        return;
    };
    s.on_the_tailnet();
    s.sim(SimHousehold::standard());
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
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_owned();
    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_owned();
    round_trip(&mut s, &name, &["--http", &api, "--mcp-http", &mcp]);
    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    s.finish();
}
