//! Tailscale presence for the FrankenSonos daemon host.
//!
//! FrankenSonos is commanded from anywhere on the owner's tailnet: the daemon
//! host sits on both the speaker LAN and the tailnet, and its HTTP API and MCP
//! server are reached at the host's tailnet address or MagicDNS name. (The
//! speakers never run Tailscale and never leave the LAN.) This crate answers
//! "is this host on a tailnet, at which addresses, under which name?" so that
//! binding, connect URLs and diagnostics above it can be zero-config.
//!
//! [`detect`] asks the local `tailscale` CLI (`tailscale status --json`, with a
//! short timeout) and, when the CLI is missing or cannot answer, falls back to
//! scanning this host's interfaces for tailnet addresses. Parsing
//! ([`parse_status`]) and the interface fallback ([`from_addresses`]) are pure.
//! [`reach`] and [`describe`] turn the status into the URLs a listener is
//! reachable at from the tailnet; [`WhoIs`] says who is on the other end of a
//! tailnet connection.

pub mod connect;
mod exec;
mod status;
pub mod whois;

pub use connect::{Listener, Reach, describe, reach};
pub use status::parse_status;
pub use whois::{Identity, WhoIs, parse_whois};

use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::time::Duration;

/// The Tailscale IPv6 ULA prefix, `fd7a:115c:a1e0::/48`.
const TAILNET_V6_PREFIX: [u16; 3] = [0xfd7a, 0x115c, 0xa1e0];

/// Is `ip` in Tailscale's IPv4 range, the CGNAT block `100.64.0.0/10`?
#[must_use]
pub const fn is_tailnet_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && b & 0xc0 == 64
}

/// Is `ip` in Tailscale's IPv6 range, `fd7a:115c:a1e0::/48`?
#[must_use]
pub fn is_tailnet_v6(ip: Ipv6Addr) -> bool {
    ip.segments()[..3] == TAILNET_V6_PREFIX
}

/// Is `ip` a tailnet address? IPv4-mapped IPv6 is judged as its IPv4.
#[must_use]
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_tailnet_v4(v4),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| is_tailnet_v6(v6), is_tailnet_v4),
    }
}

/// What this host knows about its tailnet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TailnetStatus {
    /// Tailscale reported its state, or tailnet addresses are on an interface.
    Available(Tailnet),
    /// No tailnet could be found, and why.
    Unavailable(Unavailable),
}

impl TailnetStatus {
    /// The tailnet, when this host is on one and it is up.
    #[must_use]
    pub fn running(&self) -> Option<&Tailnet> {
        match self {
            Self::Available(t) if t.running => Some(t),
            _ => None,
        }
    }
}

/// This host's view of its tailnet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tailnet {
    /// Where the facts came from.
    pub source: Source,
    /// Tailscale's `BackendState` (`Running`, `Stopped`, `NeedsLogin`, …);
    /// `None` when found by interface scan.
    pub backend_state: Option<String>,
    /// The tailnet is up on this host.
    pub running: bool,
    /// The node is logged in. An interface scan infers it from the presence
    /// of tailnet addresses, which Tailscale only assigns after login.
    pub logged_in: bool,
    /// This host's tailnet IPv4 addresses.
    pub ipv4: Vec<Ipv4Addr>,
    /// This host's tailnet IPv6 addresses.
    pub ipv6: Vec<Ipv6Addr>,
    /// The MagicDNS name without its trailing dot (e.g.
    /// `host.tailnet-name.ts.net`); `None` when MagicDNS is off or unknown.
    pub magic_dns_name: Option<String>,
    /// The tailnet's name, when the CLI reports it.
    pub tailnet: Option<String>,
}

/// Where a [`Tailnet`] was learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `tailscale status --json`.
    Cli,
    /// Tailnet addresses found on this host's interfaces (the CLI could not
    /// answer).
    Interfaces,
}

/// Why no tailnet was found. Carries the CLI's failure, since the interface
/// scan found nothing either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Unavailable {
    /// No `tailscale` CLI on this host.
    NotInstalled,
    /// The CLI ran but reported an error (e.g. `tailscaled` is not running).
    CliFailed { detail: String },
    /// The CLI did not answer within the probe's timeout.
    TimedOut,
    /// The CLI's output was not the expected JSON.
    Unparseable { detail: String },
}

/// How to look for Tailscale on this host.
#[derive(Debug, Clone)]
pub struct Probe {
    /// CLI programs to try, in order; a bare name is looked up on `PATH`.
    pub cli_candidates: Vec<PathBuf>,
    /// How long `tailscale status --json` may take.
    pub timeout: Duration,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            cli_candidates: vec![
                PathBuf::from("tailscale"),
                // The macOS app bundles the CLI without putting it on PATH.
                PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
            ],
            timeout: Duration::from_secs(3),
        }
    }
}

impl Probe {
    /// Detect this host's tailnet: ask the CLI, then fall back to an
    /// interface scan.
    #[must_use]
    pub fn detect(&self) -> TailnetStatus {
        let why = match self.query_cli() {
            Ok(tailnet) => return TailnetStatus::Available(tailnet),
            Err(why) => why,
        };
        // getifaddrs failing leaves nothing to fall back on; report the CLI.
        let addrs: Vec<IpAddr> = if_addrs::get_if_addrs()
            .map(|ifaces| ifaces.iter().map(if_addrs::Interface::ip).collect())
            .unwrap_or_default();
        from_addresses(addrs).map_or(TailnetStatus::Unavailable(why), TailnetStatus::Available)
    }

    fn query_cli(&self) -> Result<Tailnet, Unavailable> {
        let stdout = self.run_cli(&["status", "--json"])?;
        parse_status(&stdout).map_err(|e| Unavailable::Unparseable {
            detail: e.to_string(),
        })
    }

    /// Run the first CLI candidate that exists with `args`; its stdout.
    pub(crate) fn run_cli(&self, args: &[&str]) -> Result<Vec<u8>, Unavailable> {
        for program in &self.cli_candidates {
            match exec::run(program, args, self.timeout) {
                Err(exec::ExecError::NotFound) => {}
                Err(exec::ExecError::TimedOut) => return Err(Unavailable::TimedOut),
                Err(exec::ExecError::Failed { detail }) => {
                    return Err(Unavailable::CliFailed { detail });
                }
                Ok(stdout) => return Ok(stdout),
            }
        }
        Err(Unavailable::NotInstalled)
    }
}

/// Detect this host's tailnet with the default [`Probe`].
#[must_use]
pub fn detect() -> TailnetStatus {
    Probe::default().detect()
}

/// Build a [`Tailnet`] from this host's interface addresses, keeping the
/// tailnet ones; `None` when there are none.
#[must_use]
pub fn from_addresses(addrs: impl IntoIterator<Item = IpAddr>) -> Option<Tailnet> {
    let (mut ipv4, mut ipv6) = (Vec::new(), Vec::new());
    for ip in addrs {
        match ip {
            IpAddr::V4(v4) if is_tailnet_v4(v4) => ipv4.push(v4),
            IpAddr::V6(v6) if is_tailnet_v6(v6) => ipv6.push(v6),
            _ => {}
        }
    }
    ipv4.sort_unstable();
    ipv4.dedup();
    ipv6.sort_unstable();
    ipv6.dedup();
    if ipv4.is_empty() && ipv6.is_empty() {
        return None;
    }
    Some(Tailnet {
        source: Source::Interfaces,
        backend_state: None,
        running: true,
        logged_in: true,
        ipv4,
        ipv6,
        magic_dns_name: None,
        tailnet: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn tailnet_ranges() {
        for inside in ["100.64.0.0", "100.100.100.100", "100.127.255.255"] {
            assert!(is_tailnet_ip(ip(inside)), "{inside}");
        }
        for outside in ["100.63.255.255", "100.128.0.0", "10.0.0.1", "192.168.1.2"] {
            assert!(!is_tailnet_ip(ip(outside)), "{outside}");
        }
        assert!(is_tailnet_ip(ip("fd7a:115c:a1e0::1")));
        assert!(is_tailnet_ip(ip("fd7a:115c:a1e0:ab12::ff")));
        assert!(!is_tailnet_ip(ip("fd7a:115c:a1e1::1")));
        assert!(!is_tailnet_ip(ip("fe80::1")));
        assert!(is_tailnet_ip(ip("::ffff:100.101.102.103")));
        assert!(!is_tailnet_ip(ip("::ffff:192.168.1.2")));
    }

    #[test]
    fn interface_fallback_keeps_only_tailnet_addresses() {
        let t = from_addresses([
            ip("127.0.0.1"),
            ip("192.168.1.20"),
            ip("100.101.102.103"),
            ip("fe80::1"),
            ip("fd7a:115c:a1e0::6501:6667"),
            ip("100.101.102.103"),
        ])
        .expect("tailnet addresses present");
        assert_eq!(t.source, Source::Interfaces);
        assert!(t.running && t.logged_in);
        assert_eq!(t.ipv4, [Ipv4Addr::new(100, 101, 102, 103)]);
        assert_eq!(
            t.ipv6,
            ["fd7a:115c:a1e0::6501:6667".parse::<Ipv6Addr>().unwrap()]
        );
        assert_eq!(t.magic_dns_name, None);
    }

    #[test]
    fn interface_fallback_without_tailnet_is_none() {
        assert_eq!(from_addresses([ip("127.0.0.1"), ip("10.1.2.3")]), None);
    }

    #[test]
    fn missing_cli_reports_not_installed_or_falls_back() {
        let probe = Probe {
            cli_candidates: vec![PathBuf::from("/nonexistent/fsonos-test/tailscale")],
            timeout: Duration::from_secs(1),
        };
        match probe.detect() {
            TailnetStatus::Unavailable(why) => assert_eq!(why, Unavailable::NotInstalled),
            // A host that really is on a tailnet still finds it by interface.
            TailnetStatus::Available(t) => assert_eq!(t.source, Source::Interfaces),
        }
    }

    #[test]
    fn serializes_with_status_tags() {
        let unavailable = TailnetStatus::Unavailable(Unavailable::CliFailed {
            detail: "tailscaled is not running".into(),
        });
        assert_eq!(
            serde_json::to_value(&unavailable).unwrap(),
            serde_json::json!({
                "status": "unavailable",
                "reason": "cli_failed",
                "detail": "tailscaled is not running"
            })
        );
        let available = TailnetStatus::Available(
            from_addresses([ip("100.101.102.103")]).expect("tailnet address"),
        );
        let v = serde_json::to_value(&available).unwrap();
        assert_eq!(v["status"], "available");
        assert_eq!(v["source"], "interfaces");
        assert_eq!(v["ipv4"][0], "100.101.102.103");
    }
}
