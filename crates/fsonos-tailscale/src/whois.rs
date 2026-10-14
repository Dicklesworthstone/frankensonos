//! Who is on the other end of a tailnet connection.
//!
//! The HTTP API and the MCP server are reached over the tailnet; the house
//! policy and the action log want to know which person or agent asked. Tailscale
//! already knows: `tailscale whois --json <ip>` maps a peer address to its
//! node and user. [`WhoIs::resolve`] asks it (with a short cache) and never
//! fails a request — a peer that cannot be identified is simply unknown.

use crate::{Probe, is_tailnet_ip};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The login Tailscale reports for a tagged device's owner.
const TAGGED_DEVICES: &str = "tagged-devices";

/// A tailnet peer's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// The owning user's login (e.g. `alice@example.com`); `None` for a
    /// tagged device, which acts as its tags.
    pub login: Option<String>,
    pub display_name: Option<String>,
    /// The device's MagicDNS host name (e.g. `phone`).
    pub device: String,
    /// ACL tags (e.g. `tag:agent`).
    pub tags: Vec<String>,
}

impl Identity {
    /// The house-policy principal: a tagged device's first tag, else the
    /// user's login, else the device name.
    #[must_use]
    pub fn principal(&self) -> String {
        self.tags
            .first()
            .or(self.login.as_ref())
            .unwrap_or(&self.device)
            .clone()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawWhoIs {
    node: RawNode,
    #[serde(default)]
    user_profile: Option<RawUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawNode {
    #[serde(default)]
    name: String,
    #[serde(default)]
    computed_name: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawUser {
    #[serde(default)]
    login_name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

/// Parse `tailscale whois --json` output.
pub fn parse_whois(json: &[u8]) -> Result<Identity, serde_json::Error> {
    let raw: RawWhoIs = serde_json::from_slice(json)?;
    let fqdn = raw.node.name.trim_end_matches('.');
    let device = raw
        .node
        .computed_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fqdn.split('.').next().unwrap_or(fqdn).to_string());
    let user = raw.user_profile;
    let login = user
        .as_ref()
        .and_then(|u| u.login_name.clone())
        .filter(|l| !l.is_empty() && l != TAGGED_DEVICES);
    let display_name = user
        .and_then(|u| u.display_name)
        .filter(|n| !n.is_empty() && login.is_some());
    Ok(Identity {
        login,
        display_name,
        device,
        tags: raw.node.tags.unwrap_or_default(),
    })
}

/// Resolves peer addresses to tailnet identities, caching answers (and
/// failures) for a short while so a burst of requests asks once.
#[derive(Debug)]
pub struct WhoIs {
    probe: Probe,
    ttl: Duration,
    cache: Mutex<HashMap<IpAddr, (Instant, Option<Identity>)>>,
}

impl Default for WhoIs {
    fn default() -> Self {
        Self::new(Probe::default())
    }
}

impl WhoIs {
    /// How long an answer is reused.
    pub const TTL: Duration = Duration::from_secs(60);

    #[must_use]
    pub fn new(probe: Probe) -> Self {
        Self {
            probe,
            ttl: Self::TTL,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Who is at `peer`. `None` — unknown — for addresses outside the
    /// tailnet (loopback, LAN), when Tailscale is absent or does not know
    /// the peer, or when its answer cannot be read. Never an error.
    pub fn resolve(&self, peer: IpAddr) -> Option<Identity> {
        if !is_tailnet_ip(peer) {
            return None;
        }
        if let Ok(cache) = self.cache.lock()
            && let Some((at, identity)) = cache.get(&peer)
            && at.elapsed() < self.ttl
        {
            return identity.clone();
        }
        let identity = self
            .probe
            .run_cli(&["whois", "--json", &peer.to_string()])
            .ok()
            .and_then(|out| parse_whois(&out).ok());
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(peer, (Instant::now(), identity.clone()));
        }
        identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const USER_DEVICE: &str = r#"{
        "Node": {"ID": 1, "StableID": "nEXAMPLE0002", "Name": "phone.example-tailnet.ts.net.",
                 "ComputedName": "phone", "Addresses": ["100.90.80.70/32"]},
        "UserProfile": {"ID": 7, "LoginName": "owner@example.com", "DisplayName": "Owner"},
        "CapMap": null
    }"#;

    const TAGGED_DEVICE: &str = r#"{
        "Node": {"ID": 2, "Name": "agent-box.example-tailnet.ts.net.",
                 "Tags": ["tag:agent", "tag:server"]},
        "UserProfile": {"ID": 9, "LoginName": "tagged-devices", "DisplayName": "Tagged Devices"}
    }"#;

    #[test]
    fn user_devices_are_their_user() {
        let id = parse_whois(USER_DEVICE.as_bytes()).unwrap();
        assert_eq!(
            id,
            Identity {
                login: Some("owner@example.com".into()),
                display_name: Some("Owner".into()),
                device: "phone".into(),
                tags: Vec::new(),
            }
        );
        assert_eq!(id.principal(), "owner@example.com");
    }

    #[test]
    fn tagged_devices_are_their_tags() {
        let id = parse_whois(TAGGED_DEVICE.as_bytes()).unwrap();
        assert_eq!(id.login, None);
        assert_eq!(id.display_name, None);
        assert_eq!(id.device, "agent-box");
        assert_eq!(id.tags, ["tag:agent", "tag:server"]);
        assert_eq!(id.principal(), "tag:agent");
    }

    #[test]
    fn a_device_with_no_user_or_tags_is_its_name() {
        let id = parse_whois(br#"{"Node": {"Name": "kiosk.example-tailnet.ts.net."}}"#).unwrap();
        assert_eq!(id.principal(), "kiosk");
        assert!(parse_whois(b"peer not found").is_err());
    }

    /// A fake `tailscale` that answers `whois` with `answer` (or fails with
    /// "peer not found" when `None`) and counts its runs in `count`.
    #[cfg(unix)]
    fn fake_cli(dir: &std::path::Path, answer: Option<&str>) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let count = dir.join("runs");
        let body = match answer {
            Some(json) => format!("cat <<'JSON'\n{json}\nJSON\n"),
            None => "echo 'peer not found' >&2\nexit 1\n".to_string(),
        };
        let script = dir.join("tailscale");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho run >> '{}'\n{body}", count.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, count)
    }

    #[cfg(unix)]
    fn runs(count: &std::path::Path) -> usize {
        std::fs::read_to_string(count).map_or(0, |s| s.lines().count())
    }

    #[cfg(unix)]
    fn whois_with(script: PathBuf) -> WhoIs {
        WhoIs::new(Probe {
            cli_candidates: vec![script],
            timeout: Duration::from_secs(5),
        })
    }

    #[cfg(unix)]
    #[test]
    fn resolves_and_caches_tailnet_peers() {
        let dir = tempdir();
        let (script, count) = fake_cli(&dir, Some(TAGGED_DEVICE));
        let whois = whois_with(script);
        let peer: IpAddr = "100.90.80.70".parse().unwrap();
        assert_eq!(whois.resolve(peer).unwrap().principal(), "tag:agent");
        assert_eq!(whois.resolve(peer).unwrap().principal(), "tag:agent");
        assert_eq!(runs(&count), 1, "the second lookup is cached");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_peers_are_unknown_not_errors() {
        let dir = tempdir();
        let (script, count) = fake_cli(&dir, None);
        let whois = whois_with(script);
        assert_eq!(whois.resolve("100.90.80.71".parse().unwrap()), None);
        // Outside the tailnet nothing is even asked.
        assert_eq!(whois.resolve("127.0.0.1".parse().unwrap()), None);
        assert_eq!(whois.resolve("192.168.1.9".parse().unwrap()), None);
        assert_eq!(runs(&count), 1);
        // No Tailscale at all: unknown too.
        let absent = whois_with(dir.join("no-such-tailscale"));
        assert_eq!(absent.resolve("100.90.80.72".parse().unwrap()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsonos-whois-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
