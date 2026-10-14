//! Parsing `tailscale status --json`.
//!
//! Only the fields FrankenSonos needs are read; everything else (notably the
//! per-peer map) is skipped. Missing or `null` fields degrade to "unknown"
//! rather than failing, since the JSON shape varies across client versions
//! and backend states.

use crate::{Source, Tailnet};
use serde::Deserialize;
use std::net::IpAddr;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStatus {
    #[serde(default)]
    backend_state: Option<String>,
    #[serde(default)]
    have_node_key: bool,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(default, rename = "Self")]
    self_node: Option<RawNode>,
    #[serde(default)]
    current_tailnet: Option<RawTailnet>,
}

#[derive(Deserialize)]
struct RawNode {
    #[serde(default, rename = "DNSName")]
    dns_name: Option<String>,
}

#[derive(Deserialize)]
struct RawTailnet {
    #[serde(default, rename = "Name")]
    name: Option<String>,
    #[serde(default, rename = "MagicDNSEnabled")]
    magic_dns_enabled: Option<bool>,
}

/// Parse the output of `tailscale status --json` into a [`Tailnet`].
pub fn parse_status(json: &[u8]) -> Result<Tailnet, serde_json::Error> {
    let raw: RawStatus = serde_json::from_slice(json)?;
    let backend_state = raw.backend_state.filter(|s| !s.is_empty());
    let (mut ipv4, mut ipv6) = (Vec::new(), Vec::new());
    for ip in raw.tailscale_ips.unwrap_or_default() {
        match ip.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => ipv4.push(v4),
            Ok(IpAddr::V6(v6)) => ipv6.push(v6),
            Err(_) => {}
        }
    }
    let magic_dns_on = raw
        .current_tailnet
        .as_ref()
        .and_then(|t| t.magic_dns_enabled)
        .unwrap_or(true);
    let magic_dns_name = raw
        .self_node
        .and_then(|n| n.dns_name)
        .map(|name| name.trim_end_matches('.').to_owned())
        .filter(|name| magic_dns_on && !name.is_empty());
    let tailnet = raw
        .current_tailnet
        .and_then(|t| t.name)
        .filter(|name| !name.is_empty());
    Ok(Tailnet {
        source: Source::Cli,
        running: backend_state.as_deref() == Some("Running"),
        logged_in: logged_in(backend_state.as_deref(), raw.have_node_key),
        backend_state,
        ipv4,
        ipv6,
        magic_dns_name,
        tailnet,
    })
}

/// Logged in = past the login step, whether or not the tunnel is up.
/// `NeedsMachineAuth` is logged in but awaiting the tailnet admin's approval.
fn logged_in(backend_state: Option<&str>, have_node_key: bool) -> bool {
    match backend_state {
        Some("Running" | "Starting" | "Stopped" | "NeedsMachineAuth") => true,
        Some("NeedsLogin" | "NoState") => false,
        _ => have_node_key,
    }
}
