//! `lan.isolation`: when no player answers, is this host on a network that
//! lets it reach anything but the router? On guest Wi-Fi with client
//! isolation the neighbor (ARP) table holds only the default gateway, and
//! discovery finding nothing is not a FrankenSonos fault (GitHub issue #5).
//!
//! The table is read, never probed: `arp -an` and `route -n get default` on
//! macOS, `/proc/net/arp` and `/proc/net/route` on Linux. Only the default
//! route's interface counts, and only complete unicast entries: never this
//! host's own address (macOS lists it as `permanent`), multicast or broadcast.

use super::super::CheckResult;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};

/// What this host's neighbor table shows on the default route's interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Neighbors {
    pub gateway: Option<IpAddr>,
    pub interface: Option<String>,
    /// Hosts with a resolved link-layer address, the gateway included.
    pub hosts: Vec<IpAddr>,
}

const REMEDY: &str = "This looks like guest Wi-Fi or a network with client isolation: join \
     the network the speakers are on. On macOS, also check that the terminal (or the \
     launchd agent) has Local Network access (System Settings > Privacy & Security > Local \
     Network).";

/// The verdict: `players_answer` when any found player served its
/// description; `neighbors` as read, if it could be.
pub(crate) fn result(players_answer: bool, neighbors: Option<&Neighbors>) -> CheckResult {
    if players_answer {
        return CheckResult::pass("players answer: this host is on their network");
    }
    let Some(n) = neighbors else {
        return CheckResult::skip("the neighbor table could not be read");
    };
    let others = n.hosts.iter().filter(|h| Some(**h) != n.gateway).count();
    let evidence = json!({
        "interface": n.interface,
        "gateway": n.gateway,
        "neighbors": n.hosts,
    });
    if others > 0 {
        return CheckResult::pass(format!(
            "{others} other host(s) on this network answer, so it is not client isolation"
        ))
        .with_detail("Why no player answers: see lan.ssdp, lan.seeds and lan.players.")
        .with_evidence(evidence);
    }
    let summary = if n.gateway.is_some_and(|g| n.hosts.contains(&g)) {
        "no player answers, and the router is the only host this machine has reached on its network"
    } else {
        "no player answers, and this machine has reached no host on its network"
    };
    CheckResult::warn(summary, REMEDY).with_evidence(evidence)
}

/// This host's neighbors, or `None` where they cannot be read.
#[must_use]
pub fn system_neighbors() -> Option<Neighbors> {
    #[cfg(target_os = "macos")]
    {
        let run = |program: &str, args: &[&str]| {
            std::process::Command::new(program)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let route = run("/sbin/route", &["-n", "get", "default"])?;
        let (gateway, interface) = parse_route_get(&route);
        let arp = run("/usr/sbin/arp", &["-an"])?;
        let hosts = parse_arp_an(&arp, interface.as_deref());
        Some(Neighbors {
            gateway,
            interface,
            hosts,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let route = std::fs::read_to_string("/proc/net/route").ok()?;
        let (gateway, interface) = parse_proc_route(&route);
        let arp = std::fs::read_to_string("/proc/net/arp").ok()?;
        let hosts = parse_proc_arp(&arp, interface.as_deref());
        Some(Neighbors {
            gateway,
            interface,
            hosts,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// A neighbor worth counting: unicast, not unspecified.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn countable(ip: Ipv4Addr) -> bool {
    !(ip.is_multicast() || ip.is_broadcast() || ip.is_unspecified())
}

/// `route -n get default`: the gateway and the interface.
#[cfg(any(target_os = "macos", test))]
fn parse_route_get(text: &str) -> (Option<IpAddr>, Option<String>) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()))
    };
    (
        field("gateway:").and_then(|g| g.parse().ok()),
        field("interface:"),
    )
}

/// `arp -an`: `? (IP) at MAC on IFACE [ifscope] [permanent] [ethernet]`.
#[cfg(any(target_os = "macos", test))]
fn parse_arp_an(text: &str, interface: Option<&str>) -> Vec<IpAddr> {
    text.lines()
        .filter_map(|line| {
            let ip: Ipv4Addr = line.split_once('(')?.1.split_once(')')?.0.parse().ok()?;
            let mut words = line.split_whitespace();
            let mac = words.by_ref().skip_while(|w| *w != "at").nth(1)?;
            let on = words.skip_while(|w| *w != "on").nth(1);
            let complete = mac != "(incomplete)" && mac != "ff:ff:ff:ff:ff:ff";
            let here = interface.is_none_or(|i| on == Some(i));
            (complete && here && !line.contains(" permanent") && countable(ip))
                .then_some(IpAddr::V4(ip))
        })
        .collect()
}

/// `/proc/net/route`: the default route's gateway and interface.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_route(text: &str) -> (Option<IpAddr>, Option<String>) {
    text.lines()
        .skip(1)
        .find_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            (cols.len() > 2 && cols[1] == "00000000").then(|| {
                let gateway = u32::from_str_radix(cols[2], 16)
                    .ok()
                    .map(|g| IpAddr::V4(Ipv4Addr::from(g.to_le_bytes())));
                (gateway, Some(cols[0].to_string()))
            })
        })
        .unwrap_or_default()
}

/// `/proc/net/arp`: complete entries (flags 0x2) on `interface`.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_arp(text: &str, interface: Option<&str>) -> Vec<IpAddr> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let [ip, _, flags, mac, _, device, ..] = cols.as_slice() else {
                return None;
            };
            let ip: Ipv4Addr = ip.parse().ok()?;
            let complete = u32::from_str_radix(flags.trim_start_matches("0x"), 16)
                .is_ok_and(|f| f & 0x2 != 0)
                && *mac != "00:00:00:00:00:00";
            let here = interface.is_none_or(|i| *device == i);
            (complete && here && countable(ip)).then_some(IpAddr::V4(ip))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const ROUTE: &str = "   route to: default\ndestination: default\n       mask: default\n    \
                         gateway: 192.0.2.1\n  interface: en1\n      flags: <UP,GATEWAY,DONE>\n";

    #[test]
    fn route_get_names_the_gateway_and_interface() {
        assert_eq!(
            parse_route_get(ROUTE),
            (Some(ip("192.0.2.1")), Some("en1".into()))
        );
        assert_eq!(
            parse_route_get("route: writing to routing socket: not in table\n"),
            (None, None)
        );
    }

    #[test]
    fn arp_counts_complete_unicast_neighbors_on_the_interface_only() {
        let arp = "? (192.0.2.1) at 0:11:22:33:44:55 on en1 ifscope [ethernet]\n\
                   ? (192.0.2.7) at 0:11:22:33:44:66 on en1 ifscope permanent [ethernet]\n\
                   ? (192.0.2.9) at (incomplete) on en1 ifscope [ethernet]\n\
                   ? (192.0.2.255) at ff:ff:ff:ff:ff:ff on en1 ifscope [ethernet]\n\
                   ? (224.0.0.251) at 1:0:5e:0:0:fb on en1 ifscope permanent [ethernet]\n\
                   ? (198.51.100.4) at 0:11:22:33:44:77 on en0 ifscope [ethernet]\n\
                   ? (192.0.2.20) at 0:11:22:33:44:88 on en1 ifscope [ethernet]\n";
        assert_eq!(
            parse_arp_an(arp, Some("en1")),
            [ip("192.0.2.1"), ip("192.0.2.20")]
        );
        assert_eq!(parse_arp_an(arp, None).len(), 3, "every interface");
    }

    #[test]
    fn proc_route_and_arp_read_as_on_linux() {
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\n\
                     wlan0\t000200C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\n";
        assert_eq!(
            parse_proc_route(route),
            (Some(ip("192.0.2.1")), Some("wlan0".into()))
        );
        let arp = "IP address       HW type     Flags       HW address            Mask     Device\n\
                   192.0.2.1        0x1         0x2         00:11:22:33:44:55     *        wlan0\n\
                   192.0.2.9        0x1         0x0         00:00:00:00:00:00     *        wlan0\n\
                   198.51.100.4     0x1         0x2         00:11:22:33:44:77     *        eth0\n";
        assert_eq!(parse_proc_arp(arp, Some("wlan0")), [ip("192.0.2.1")]);
    }

    #[test]
    fn only_the_router_in_reach_warns_of_client_isolation() {
        let isolated = Neighbors {
            gateway: Some(ip("192.0.2.1")),
            interface: Some("en1".into()),
            hosts: vec![ip("192.0.2.1")],
        };
        let warned = result(false, Some(&isolated));
        assert_eq!(warned.status, Status::Warn);
        assert!(
            warned.summary.contains("router is the only host"),
            "{warned:?}"
        );
        assert!(
            warned
                .remedy
                .as_deref()
                .is_some_and(|r| r.contains("guest Wi-Fi"))
        );

        let alone = Neighbors {
            hosts: Vec::new(),
            ..isolated.clone()
        };
        assert_eq!(result(false, Some(&alone)).status, Status::Warn);

        let busy = Neighbors {
            hosts: vec![ip("192.0.2.1"), ip("192.0.2.20")],
            ..isolated.clone()
        };
        assert_eq!(result(false, Some(&busy)).status, Status::Pass);
        // Players answering settle it, whatever the table shows.
        assert_eq!(result(true, Some(&isolated)).status, Status::Pass);
        assert_eq!(result(false, None).status, Status::Skip);
    }
}
