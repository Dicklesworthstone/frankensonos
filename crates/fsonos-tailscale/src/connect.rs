//! Where tailnet clients should point: the connect URLs for a listener.
//!
//! "Command your speakers from anywhere on your tailnet" is only easy if the
//! owner (and their agents) are told the address. Given what this host knows
//! about its tailnet and the address a listener is actually bound to,
//! [`reach`] says whether tailnet devices can reach it and at which URLs — the
//! stable MagicDNS name first, then the raw tailnet IPs — or why not and what
//! to change. It never claims a URL the listener does not answer on: a
//! loopback-bound daemon is reported as not on the tailnet.

use crate::{Tailnet, TailnetStatus, Unavailable, is_tailnet_ip};
use serde::Serialize;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};

/// How tailnet devices can reach a listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reach", rename_all = "snake_case")]
pub enum Reach {
    /// Reachable at these URLs (MagicDNS name first when it is on, then each
    /// tailnet IP), with a hint when MagicDNS is off.
    Tailnet {
        urls: Vec<String>,
        hint: Option<String>,
    },
    /// Running on the tailnet, but bound where tailnet devices cannot reach.
    NotOnTailnet { hint: String },
    /// This host is not on a running tailnet.
    NoTailnet { hint: String },
}

impl Reach {
    /// The URL to share: the first one, when reachable.
    #[must_use]
    pub fn best_url(&self) -> Option<&str> {
        match self {
            Self::Tailnet { urls, .. } => urls.first().map(String::as_str),
            _ => None,
        }
    }
}

/// How tailnet devices reach a listener bound to `bind`, with `path` (e.g.
/// `"/mcp"`, or `""` for the root) appended to every URL.
#[must_use]
pub fn reach(status: &TailnetStatus, bind: SocketAddr, path: &str) -> Reach {
    let Some(tailnet) = status.running() else {
        return Reach::NoTailnet {
            hint: not_running_hint(status),
        };
    };
    let port = bind.port();
    let (v4, v6) = match bind.ip() {
        // 0.0.0.0 listens on every IPv4 address; :: on every address (where
        // the OS makes it dual-stack, which macOS and Linux do by default).
        ip if ip.is_unspecified() && ip.is_ipv4() => (tailnet.ipv4.clone(), Vec::new()),
        ip if ip.is_unspecified() => (tailnet.ipv4.clone(), tailnet.ipv6.clone()),
        IpAddr::V4(ip) if tailnet.ipv4.contains(&ip) => (vec![ip], Vec::new()),
        IpAddr::V6(ip) if tailnet.ipv6.contains(&ip) => (Vec::new(), vec![ip]),
        ip => {
            return Reach::NotOnTailnet {
                hint: not_bound_hint(tailnet, ip, port),
            };
        }
    };
    if v4.is_empty() && v6.is_empty() {
        return Reach::NotOnTailnet {
            hint: not_bound_hint(tailnet, bind.ip(), port),
        };
    }
    let mut urls = Vec::new();
    if let Some(name) = &tailnet.magic_dns_name {
        urls.push(format!("http://{name}:{port}{path}"));
    }
    urls.extend(v4.iter().map(|ip| format!("http://{ip}:{port}{path}")));
    urls.extend(v6.iter().map(|ip| format!("http://[{ip}]:{port}{path}")));
    let hint = tailnet.magic_dns_name.is_none().then(|| {
        "MagicDNS is off, so these addresses can change; turn it on in the Tailscale admin \
         console for a stable name."
            .to_string()
    });
    Reach::Tailnet { urls, hint }
}

/// A listener to describe: its label, bound address, and URL path.
#[derive(Debug, Clone, Copy)]
pub struct Listener<'a> {
    pub label: &'a str,
    pub bind: SocketAddr,
    pub path: &'a str,
}

/// The tailnet block `fsonos serve`, `fsonos status` and `fsonos doctor`
/// print: this host's tailnet identity, each listener's URLs (or why it is
/// not reachable), and, for an MCP listener (path `/mcp`), the line that adds
/// it to an agent.
#[must_use]
pub fn describe(status: &TailnetStatus, listeners: &[Listener<'_>]) -> String {
    let mut out = String::new();
    let Some(t) = status.running() else {
        let _ = writeln!(out, "Tailnet: unavailable. {}", not_running_hint(status));
        return out;
    };
    let addrs: Vec<String> = t
        .ipv4
        .iter()
        .map(ToString::to_string)
        .chain(t.ipv6.iter().map(ToString::to_string))
        .collect();
    let _ = writeln!(
        out,
        "Tailnet: {} ({}){}",
        t.magic_dns_name.as_deref().unwrap_or("this host"),
        addrs.join(", "),
        t.tailnet
            .as_deref()
            .map(|n| format!(" on {n}"))
            .unwrap_or_default()
    );
    let width = listeners.iter().map(|l| l.label.len()).max().unwrap_or(0) + 1;
    for l in listeners {
        let label = format!("{}:", l.label);
        match reach(status, l.bind, l.path) {
            Reach::Tailnet { urls, hint } => {
                for (i, url) in urls.iter().enumerate() {
                    let head = if i == 0 { label.as_str() } else { "" };
                    let _ = writeln!(out, "  {head:<width$} {url}");
                }
                if l.path == "/mcp" {
                    let _ = writeln!(
                        out,
                        "  {:<width$} claude mcp add --transport http fsonos {}",
                        "", urls[0]
                    );
                }
                if let Some(hint) = hint {
                    let _ = writeln!(out, "  {:<width$} {hint}", "");
                }
            }
            Reach::NotOnTailnet { hint } | Reach::NoTailnet { hint } => {
                let _ = writeln!(
                    out,
                    "  {label:<width$} not reachable from the tailnet. {hint}"
                );
            }
        }
    }
    out
}

fn not_running_hint(status: &TailnetStatus) -> String {
    match status {
        TailnetStatus::Unavailable(Unavailable::NotInstalled) => {
            "Tailscale is not installed on this host; install it and log in to command the \
             speakers from your tailnet."
                .into()
        }
        TailnetStatus::Unavailable(Unavailable::CliFailed { detail }) => {
            format!("Tailscale did not answer ({detail}); start the Tailscale app or tailscaled.")
        }
        TailnetStatus::Unavailable(Unavailable::TimedOut) => {
            "Tailscale did not answer in time; check that tailscaled is healthy.".into()
        }
        TailnetStatus::Unavailable(Unavailable::Unparseable { detail }) => {
            format!("Tailscale's status could not be read ({detail}).")
        }
        TailnetStatus::Unavailable(Unavailable::Disabled) => {
            "Tailscale is turned off for FrankenSonos (FSONOS_TAILSCALE=off); unset it to use \
             the tailnet."
                .into()
        }
        TailnetStatus::Available(t) if !t.logged_in => {
            "Tailscale is not logged in; run `tailscale up` (or log in from the app).".into()
        }
        TailnetStatus::Available(t) => format!(
            "Tailscale is {}; run `tailscale up` to connect.",
            t.backend_state.as_deref().unwrap_or("not running")
        ),
    }
}

fn not_bound_hint(tailnet: &Tailnet, bound: IpAddr, port: u16) -> String {
    let what = if bound.is_loopback() {
        "this machine only".to_string()
    } else if is_tailnet_ip(bound) {
        "a tailnet address this host does not have".to_string()
    } else {
        format!("{bound}, which tailnet devices cannot reach")
    };
    let suggestion = tailnet
        .ipv4
        .first()
        .map(|ip| format!("bind {ip}:{port}"))
        .or_else(|| tailnet.ipv6.first().map(|ip| format!("bind [{ip}]:{port}")))
        .unwrap_or_else(|| "bind this host's tailnet address".to_string());
    format!(
        "Bound to {what}. To reach it from your tailnet, {suggestion}, or front it with `tailscale serve`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const V4: Ipv4Addr = Ipv4Addr::new(100, 101, 102, 103);

    fn v6() -> Ipv6Addr {
        "fd7a:115c:a1e0::6501:6667".parse().unwrap()
    }

    fn up(magic_dns: bool) -> TailnetStatus {
        TailnetStatus::Available(Tailnet {
            source: Source::Cli,
            backend_state: Some("Running".into()),
            running: true,
            logged_in: true,
            ipv4: vec![V4],
            ipv6: vec![v6()],
            magic_dns_name: magic_dns.then(|| "sonos-host.example-tailnet.ts.net".into()),
            tailnet: Some("example.com".into()),
        })
    }

    fn at(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn tailnet_bind_gets_magic_dns_then_ip() {
        assert_eq!(
            reach(&up(true), at("100.101.102.103:8099"), ""),
            Reach::Tailnet {
                urls: vec![
                    "http://sonos-host.example-tailnet.ts.net:8099".into(),
                    "http://100.101.102.103:8099".into(),
                ],
                hint: None,
            }
        );
        let mcp = reach(&up(true), at("100.101.102.103:8098"), "/mcp");
        assert_eq!(
            mcp.best_url(),
            Some("http://sonos-host.example-tailnet.ts.net:8098/mcp")
        );
    }

    #[test]
    fn ipv6_urls_are_bracketed() {
        let r = reach(&up(false), at("[fd7a:115c:a1e0::6501:6667]:8099"), "/mcp");
        let Reach::Tailnet { urls, hint } = r else {
            panic!("{r:?}");
        };
        assert_eq!(urls, ["http://[fd7a:115c:a1e0::6501:6667]:8099/mcp"]);
        assert!(hint.unwrap().contains("MagicDNS is off"));
    }

    #[test]
    fn wildcard_binds_list_every_tailnet_address() {
        let Reach::Tailnet { urls, .. } = reach(&up(true), at("[::]:8099"), "") else {
            panic!();
        };
        assert_eq!(
            urls,
            [
                "http://sonos-host.example-tailnet.ts.net:8099",
                "http://100.101.102.103:8099",
                "http://[fd7a:115c:a1e0::6501:6667]:8099",
            ]
        );
        let Reach::Tailnet { urls, .. } = reach(&up(false), at("0.0.0.0:8099"), "") else {
            panic!();
        };
        assert_eq!(urls, ["http://100.101.102.103:8099"]);
    }

    #[test]
    fn loopback_and_lan_binds_are_not_on_the_tailnet() {
        let Reach::NotOnTailnet { hint } = reach(&up(true), at("127.0.0.1:8099"), "") else {
            panic!();
        };
        assert_eq!(
            hint,
            "Bound to this machine only. To reach it from your tailnet, bind \
             100.101.102.103:8099, or front it with `tailscale serve`."
        );
        let Reach::NotOnTailnet { hint } = reach(&up(true), at("192.168.1.20:8099"), "") else {
            panic!();
        };
        assert!(hint.starts_with("Bound to 192.168.1.20, which tailnet devices cannot reach."));
        // A tailnet address that is not this host's (stale configuration).
        let Reach::NotOnTailnet { hint } = reach(&up(true), at("100.64.0.9:8099"), "") else {
            panic!();
        };
        assert!(hint.contains("a tailnet address this host does not have"));
    }

    #[test]
    fn no_tailnet_explains_why() {
        let cases = [
            (
                TailnetStatus::Unavailable(Unavailable::NotInstalled),
                "not installed",
            ),
            (
                TailnetStatus::Unavailable(Unavailable::CliFailed {
                    detail: "failed to connect to local tailscaled".into(),
                }),
                "start the Tailscale app",
            ),
            (
                TailnetStatus::Unavailable(Unavailable::TimedOut),
                "did not answer in time",
            ),
            (
                TailnetStatus::Unavailable(Unavailable::Disabled),
                "FSONOS_TAILSCALE=off",
            ),
        ];
        for (status, needle) in cases {
            let Reach::NoTailnet { hint } = reach(&status, at("100.101.102.103:8099"), "") else {
                panic!("{status:?}");
            };
            assert!(hint.contains(needle), "{hint}");
        }
        let mut stopped = up(true);
        if let TailnetStatus::Available(t) = &mut stopped {
            t.running = false;
            t.backend_state = Some("Stopped".into());
        }
        let Reach::NoTailnet { hint } = reach(&stopped, at("0.0.0.0:8099"), "") else {
            panic!();
        };
        assert_eq!(hint, "Tailscale is Stopped; run `tailscale up` to connect.");
        let mut logged_out = stopped;
        if let TailnetStatus::Available(t) = &mut logged_out {
            t.logged_in = false;
            t.backend_state = Some("NeedsLogin".into());
        }
        let Reach::NoTailnet { hint } = reach(&logged_out, at("0.0.0.0:8099"), "") else {
            panic!();
        };
        assert!(hint.contains("not logged in"));
    }

    #[test]
    fn describe_lists_urls_and_the_agent_line() {
        let listeners = [
            Listener {
                label: "HTTP API",
                bind: at("100.101.102.103:8099"),
                path: "",
            },
            Listener {
                label: "MCP server",
                bind: at("100.101.102.103:8098"),
                path: "/mcp",
            },
        ];
        assert_eq!(
            describe(&up(true), &listeners),
            "\
Tailnet: sonos-host.example-tailnet.ts.net (100.101.102.103, fd7a:115c:a1e0::6501:6667) on example.com
  HTTP API:   http://sonos-host.example-tailnet.ts.net:8099
              http://100.101.102.103:8099
  MCP server: http://sonos-host.example-tailnet.ts.net:8098/mcp
              http://100.101.102.103:8098/mcp
              claude mcp add --transport http fsonos http://sonos-host.example-tailnet.ts.net:8098/mcp
"
        );
        let local = [Listener {
            label: "HTTP API",
            bind: at("127.0.0.1:8099"),
            path: "",
        }];
        let text = describe(&up(true), &local);
        assert!(
            text.contains("HTTP API: not reachable from the tailnet. Bound to this machine only."),
            "{text}"
        );
        assert_eq!(
            describe(
                &TailnetStatus::Unavailable(Unavailable::NotInstalled),
                &local
            ),
            "Tailnet: unavailable. Tailscale is not installed on this host; install it and log \
             in to command the speakers from your tailnet.\n"
        );
    }

    #[test]
    fn serializes_for_status_json() {
        let v = serde_json::to_value(reach(&up(true), at("100.101.102.103:8099"), "")).unwrap();
        assert_eq!(v["reach"], "tailnet");
        assert_eq!(
            v["urls"][0],
            "http://sonos-host.example-tailnet.ts.net:8099"
        );
        let v = serde_json::to_value(reach(&up(true), at("127.0.0.1:8099"), "")).unwrap();
        assert_eq!(v["reach"], "not_on_tailnet");
    }
}
