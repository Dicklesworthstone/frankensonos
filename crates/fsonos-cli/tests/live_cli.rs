//! Opt-in live check of the CLI's read path against the owner's own players:
//! `fsonos discover --json` and `fsonos zones --json`, both read-only.
//!
//! It must run on the speaker-LAN host; under rch it runs on a remote worker
//! with no speakers, which is not a live result. From a plain shell there:
//!
//! ```text
//! cargo test -p fsonos-cli --test live_cli -- --ignored --nocapture
//! ```
//!
//! Output stays on the terminal; it names real rooms and addresses, so never
//! paste it into the repository.

use serde_json::Value;
use std::process::Command;

fn fsonos_json(args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_fsonos"))
        .args(args)
        .args(["--json", "--wait", "3"])
        .output()
        .expect("run fsonos");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "fsonos {args:?} failed: {stderr}");
    serde_json::from_slice(&out.stdout).expect("JSON on stdout")
}

#[test]
#[ignore = "live LAN: needs the owner's speakers; run with -- --ignored"]
fn live_discover_and_zones() {
    let found = fsonos_json(&["discover"]);
    let players = found["players"].as_array().expect("players array");
    assert!(!players.is_empty(), "no players: {found}");
    println!("discover: {} players", players.len());

    let zones = fsonos_json(&["zones"]);
    let zones = zones.as_array().expect("zones array");
    assert!(!zones.is_empty(), "no zones");
    for z in zones {
        assert!(
            z["members"].as_array().is_some_and(|m| !m.is_empty()),
            "{z}"
        );
        assert!(z["transport_state"].is_string(), "{z}");
    }
    println!("zones: {} groups", zones.len());
}
