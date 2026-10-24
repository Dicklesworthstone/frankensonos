//! `Surface::warnings` (the `GET /diagnostics/warnings` and MCP `diagnostics`
//! read, co55.7): an agent reads the daemon's own warnings without running
//! the full `doctor`. Every kind logged in the last day is listed with its
//! per-hour counts; a kind logged past the storm floor is reported storming,
//! and its sample names no login. Its own test binary, so the process-wide
//! warning counts are this test's alone.

use fsonos_api::Surface;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};

struct NoLan;
impl Transport for NoLan {
    fn soap_post(
        &self,
        _: std::net::IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, ProtoError> {
        Err(ProtoError::NotWired("no LAN in this test"))
    }
}

fn surface(policy: Policy) -> Surface {
    Surface::new(
        Box::new(NoLan),
        Box::new(|_| Ok(Vec::new())),
        policy,
        Box::new(SystemClock),
    )
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn an_agent_reads_the_daemons_warnings_without_the_lan() {
    let quiet = "fsonos_core::events@crates/fsonos-core/src/events.rs:1";
    let storm = "fsonos_core::auth@crates/fsonos-core/src/auth.rs:1";
    let at = now();
    fsonos_core::warnings::record(quiet, false, "delivered no initial state", at);
    // A login in the message: masking (digits -> N) leaves it, so a scrubbed
    // sample proves `warnings` ran the scrub, not only the mask.
    for _ in 0..fsonos_core::warnings::STORM_FLOOR {
        fsonos_core::warnings::record(storm, true, "signed in as someone@example then failed", at);
    }

    let view = surface(Policy::default())
        .warnings(&Client::Cli)
        .expect("the read is allowed");

    // Every kind logged this day is listed, the quiet one among them.
    let quiet_kind = view
        .kinds
        .iter()
        .find(|k| k.kind == quiet)
        .expect("the quiet kind");
    assert_eq!(quiet_kind.last_day.len(), 24);
    assert_eq!(quiet_kind.last_day[23], 1, "{quiet_kind:?}");

    // The one past the floor is storming, as an error, its sample scrubbed.
    let storming = view
        .storming
        .iter()
        .find(|s| s.kind == storm)
        .expect("the storm");
    assert_eq!(storming.level, "error");
    assert_eq!(storming.last_hour, fsonos_core::warnings::STORM_FLOOR);
    assert!(
        storming.sample.contains("<login>") && !storming.sample.contains('@'),
        "login not scrubbed: {}",
        storming.sample
    );
}

#[test]
fn a_caller_denied_diagnostics_is_refused() {
    let policy = Policy::from_toml("[clients.\"mcp-stdio\"]\ndeny = [\"diagnostics\"]\n").unwrap();
    let surface = surface(policy);
    assert!(surface.warnings(&Client::Cli).is_ok(), "the owner reads it");
    let denied = surface.warnings(&Client::McpStdio).unwrap_err();
    assert_eq!(
        denied.code,
        fsonos_api::ErrorCode::PolicyDenied,
        "{denied:?}"
    );
}
