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

use fsonos_tailscale::{
    BindReason, Probe, Source, TailnetStatus, bind_plan, is_tailnet_v4, is_tailnet_v6,
};
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

/// The default bind plan for this host binds and accepts on every address it
/// picks (loopback, plus the tailnet when it is up).
#[test]
fn the_default_bind_plan_is_bindable_here() {
    let status = Probe::default().detect();
    let plan = bind_plan(&status, 0, None);
    eprintln!("tailscale-live: bind plan {plan:#?}");
    if status.running().is_some() {
        assert_eq!(plan.reason, BindReason::Tailnet);
    }
    for addr in &plan.addrs {
        let listener =
            std::net::TcpListener::bind(addr).unwrap_or_else(|e| panic!("bind {addr}: {e}"));
        let bound = listener.local_addr().unwrap();
        std::net::TcpStream::connect(bound).unwrap_or_else(|e| panic!("connect {bound}: {e}"));
        eprintln!("tailscale-live: bound and reached {bound}");
    }
}
