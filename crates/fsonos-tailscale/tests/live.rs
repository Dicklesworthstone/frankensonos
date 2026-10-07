//! Live smoke test against this host's real Tailscale. Off by default (and in
//! CI); run on the daemon host with:
//!
//! ```text
//! cargo test -p fsonos-tailscale --features tailscale-live -- --nocapture
//! ```
//!
//! It logs what was detected so a human can compare it with
//! `tailscale status`; the assertions only check internal consistency, since
//! the right answer depends on the host.
#![cfg(feature = "tailscale-live")]

use fsonos_tailscale::{Probe, Source, TailnetStatus, is_tailnet_v4, is_tailnet_v6};
use std::time::Instant;

#[test]
fn detects_this_hosts_tailnet() {
    let probe = Probe::default();
    let started = Instant::now();
    let status = probe.detect();
    let elapsed = started.elapsed();
    eprintln!(
        "tailscale-live: probe candidates {:?}",
        probe.cli_candidates
    );
    eprintln!("tailscale-live: detected in {elapsed:?}");
    eprintln!(
        "tailscale-live: {}",
        serde_json::to_string_pretty(&status).expect("status serializes")
    );

    // The CLI is bounded by the probe timeout; the interface scan is fast.
    assert!(elapsed < probe.timeout * 2, "detection took {elapsed:?}");
    if let TailnetStatus::Available(t) = &status {
        assert!(t.ipv4.iter().all(|ip| is_tailnet_v4(*ip)), "{t:?}");
        assert!(t.ipv6.iter().all(|ip| is_tailnet_v6(*ip)), "{t:?}");
        if t.source == Source::Interfaces {
            assert!(!t.ipv4.is_empty() || !t.ipv6.is_empty());
        }
        if t.running {
            assert!(t.logged_in, "running implies logged in: {t:?}");
        }
    }
}
