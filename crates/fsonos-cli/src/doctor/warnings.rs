//! `daemon.warnings`: whether the daemon's own warnings are storming
//! (co55.7), from its `GET /health`. A storm is one kind logged at least 20
//! times in the last hour and ten times its usual hour: the renewals that
//! fail while the Wi-Fi to the speakers drops, the event listener re-opening
//! as the route flaps, a full disk. It names the kind and what usually
//! causes it.

use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;

const WARNINGS: CheckId = CheckId("daemon.warnings");

/// The storms in the daemon's `/health` on `http`.
pub struct WarningsCheck {
    pub http: SocketAddr,
}

impl Check for WarningsCheck {
    fn id(&self) -> CheckId {
        WARNINGS
    }

    fn title(&self) -> &'static str {
        "Daemon warnings"
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        if self.http.port() == 0 {
            return CheckResult::skip("the HTTP address has no fixed port to probe");
        }
        let timeout = ctx.remaining().min(Duration::from_secs(3));
        match super::probe_health_json(self.http, timeout) {
            Ok(health) => result(&health["warnings"]),
            Err(_) => CheckResult::skip("no daemon answers to count its warnings"),
        }
    }
}

/// The verdict on `/health`'s `warnings`: absent when none storms.
fn result(warnings: &Value) -> CheckResult {
    let Some(storming) = warnings["storming"].as_array().filter(|s| !s.is_empty()) else {
        return CheckResult::pass("no warning is storming in the last hour");
    };
    let named: Vec<String> = storming
        .iter()
        .map(|s| {
            format!(
                "\"{}\" {} times (storm at {})",
                s["sample"].as_str().unwrap_or("?"),
                s["last_hour"],
                s["threshold"]
            )
        })
        .collect();
    let remedy = storming
        .iter()
        .filter_map(|s| s["sample"].as_str())
        .find_map(remedy)
        .unwrap_or(
            "Read the daemon log for these messages: each is logged far more often than usual.",
        );
    CheckResult::warn(
        format!(
            "{} warning(s) storming in the last hour: {}",
            storming.len(),
            named.join("; ")
        ),
        remedy,
    )
    .with_evidence(json!({ "storming": storming }))
}

/// What usually causes the warning whose message is `sample`.
fn remedy(sample: &str) -> Option<&'static str> {
    if sample.contains("re-opening the event listener") {
        Some(
            "The route from this host to the speakers keeps changing: a link dropping beside \
             another interface (Wi-Fi beside a wired one). Steady the speakers' Wi-Fi, or give \
             this host one path to them.",
        )
    } else if sample.contains("renewal failed") || sample.contains("delivered no initial state") {
        Some(
            "The speakers keep missing event renewals: the network between this host and them \
             is dropping (Wi-Fi signal or interference, a DHCP change).",
        )
    } else if sample.contains("left its household's topology") || sample.contains("player missing")
    {
        Some(
            "Players keep dropping off and rejoining their household: their own Wi-Fi link is \
             weak. Move them, or the access point, closer, or wire one.",
        )
    } else if sample.contains("No space left") {
        Some("Make room on the data directory's volume (`fsonos doctor --only store`).")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::Status;

    #[test]
    fn a_daemon_with_no_storm_passes_and_a_storm_is_named_with_its_cause() {
        assert_eq!(result(&Value::Null).status, Status::Pass);
        assert_eq!(result(&json!({ "storming": [] })).status, Status::Pass);
        let flapping = json!({ "storming": [{
            "kind": "fsonos_core::live@crates/fsonos-core/src/live.rs:640",
            "level": "warn",
            "sample": "re-opening the event listener",
            "last_hour": 31,
            "threshold": 20,
        }]});
        let r = result(&flapping);
        assert_eq!(r.status, Status::Warn);
        assert!(
            r.summary
                .contains("\"re-opening the event listener\" 31 times"),
            "{}",
            r.summary
        );
        assert!(
            r.remedy.as_deref().is_some_and(|m| m.contains("route")),
            "{r:?}"
        );
        let unknown = json!({ "storming": [{ "sample": "something new", "last_hour": 90, "threshold": 20 }] });
        assert!(
            result(&unknown)
                .remedy
                .as_deref()
                .is_some_and(|m| m.contains("daemon log"))
        );
    }
}
