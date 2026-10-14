//! Where the daemon's listeners bind by default: on the tailnet when there is
//! one.
//!
//! "Command your speakers from anywhere on your tailnet" has to be the
//! default, not a configuration chore. With no address configured, a
//! listener binds loopback (for this machine) plus every tailnet address this
//! host has, so any tailnet device reaches it out of the box; with Tailscale
//! down it binds loopback only and says why. A configured address always
//! wins. Loopback and tailnet addresses are exactly what the daemon's bind
//! guard allows, and nothing here ever binds a LAN, wildcard or public
//! address.

use crate::{TailnetStatus, connect};
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Why a listener binds where it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindReason {
    /// The address was configured (flag or environment).
    Configured,
    /// Loopback plus the tailnet, detected.
    Tailnet,
    /// Loopback only: no running tailnet.
    LoopbackOnly,
}

/// The addresses one listener binds, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindPlan {
    pub addrs: Vec<SocketAddr>,
    pub reason: BindReason,
    /// One line for the startup log.
    pub note: String,
}

/// Where a listener on `port` binds: `configured` if given, else loopback
/// plus this host's tailnet addresses when Tailscale is up, else loopback.
#[must_use]
pub fn bind_plan(status: &TailnetStatus, port: u16, configured: Option<SocketAddr>) -> BindPlan {
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    if let Some(addr) = configured {
        return BindPlan {
            addrs: vec![addr],
            reason: BindReason::Configured,
            note: format!("bound to {addr} as configured"),
        };
    }
    let Some(tailnet) = status.running() else {
        let why = match connect::reach(status, loopback, "") {
            connect::Reach::NoTailnet { hint } => hint,
            _ => String::new(),
        };
        return BindPlan {
            addrs: vec![loopback],
            reason: BindReason::LoopbackOnly,
            note: format!("loopback only ({loopback}): {why}"),
        };
    };
    let mut addrs = vec![loopback];
    addrs.extend(
        tailnet
            .ipv4
            .iter()
            .map(|ip| SocketAddr::new(IpAddr::V4(*ip), port)),
    );
    addrs.extend(
        tailnet
            .ipv6
            .iter()
            .map(|ip| SocketAddr::new(IpAddr::V6(*ip), port)),
    );
    let name = tailnet
        .magic_dns_name
        .as_deref()
        .map(|n| format!(" as {n}"))
        .unwrap_or_default();
    BindPlan {
        note: format!(
            "loopback and the tailnet{name}: {}",
            addrs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        addrs,
        reason: BindReason::Tailnet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Source, Tailnet, Unavailable, is_tailnet_ip};

    fn up(magic: bool, v6: bool) -> TailnetStatus {
        TailnetStatus::Available(Tailnet {
            source: Source::Cli,
            backend_state: Some("Running".into()),
            running: true,
            logged_in: true,
            ipv4: vec![Ipv4Addr::new(100, 101, 102, 103)],
            ipv6: if v6 {
                vec!["fd7a:115c:a1e0::6501:6667".parse().unwrap()]
            } else {
                Vec::new()
            },
            magic_dns_name: magic.then(|| "sonos-host.example-tailnet.ts.net".into()),
            tailnet: Some("example.com".into()),
        })
    }

    #[test]
    fn a_running_tailnet_adds_its_addresses_to_loopback() {
        let plan = bind_plan(&up(true, true), 8099, None);
        assert_eq!(plan.reason, BindReason::Tailnet);
        assert_eq!(
            plan.addrs,
            [
                "127.0.0.1:8099".parse::<SocketAddr>().unwrap(),
                "100.101.102.103:8099".parse().unwrap(),
                "[fd7a:115c:a1e0::6501:6667]:8099".parse().unwrap(),
            ]
        );
        assert!(plan.note.contains("as sonos-host.example-tailnet.ts.net"));
        // Only loopback and tailnet addresses, which the bind guard allows.
        assert!(
            plan.addrs
                .iter()
                .all(|a| a.ip().is_loopback() || is_tailnet_ip(a.ip()))
        );
    }

    #[test]
    fn no_tailnet_means_loopback_only_with_the_reason() {
        let plan = bind_plan(
            &TailnetStatus::Unavailable(Unavailable::NotInstalled),
            8098,
            None,
        );
        assert_eq!(plan.reason, BindReason::LoopbackOnly);
        assert_eq!(
            plan.addrs,
            ["127.0.0.1:8098".parse::<SocketAddr>().unwrap()]
        );
        assert!(plan.note.contains("not installed"), "{}", plan.note);

        let mut stopped = up(false, false);
        if let TailnetStatus::Available(t) = &mut stopped {
            t.running = false;
            t.backend_state = Some("Stopped".into());
        }
        let plan = bind_plan(&stopped, 8099, None);
        assert_eq!(plan.reason, BindReason::LoopbackOnly);
        assert!(plan.note.contains("tailscale up"), "{}", plan.note);
    }

    #[test]
    fn a_configured_address_overrides_detection() {
        let configured: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let plan = bind_plan(&up(true, true), 8099, Some(configured));
        assert_eq!(plan.reason, BindReason::Configured);
        assert_eq!(plan.addrs, [configured]);
        let plan = bind_plan(
            &TailnetStatus::Unavailable(Unavailable::TimedOut),
            8099,
            Some(configured),
        );
        assert_eq!(plan.addrs, [configured]);
    }

    #[test]
    fn ipv4_only_tailnets_and_names_are_optional() {
        let plan = bind_plan(&up(false, false), 8099, None);
        assert_eq!(plan.addrs.len(), 2);
        assert!(!plan.note.contains(" as "));
    }
}
