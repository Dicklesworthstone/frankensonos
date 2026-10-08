//! `fsonos doctor`'s Tailscale checks: does "command your speakers from
//! anywhere on your tailnet" work from this host, and if not, why and what
//! to do about it.
//!
//! * `tailscale.running`: Tailscale is installed, up and logged in, and this
//!   host has a tailnet address;
//! * `tailscale.magicdns`: this host's MagicDNS name, the stable one to
//!   share, resolves to its tailnet address;
//! * `tailscale.reach`: the daemon answers `/health` over the tailnet; on
//!   success the detail lists the connect URLs for the HTTP API and the MCP
//!   server (the `fsonos serve` banner's block).
//!
//! Tailscale is detected once per doctor run, shared by the three checks. A
//! host without a working tailnet warns, with the fix, rather than fails:
//! the speakers still answer from this machine. `--tailscale off`
//! (`FSONOS_TAILSCALE=off`) turns detection off, and these checks skip.

use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult, Runner};
use fsonos_tailscale::{BindReason, Listener, Reach, Source, Tailnet, TailnetStatus, Unavailable};
use serde_json::json;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::probe_health;
use crate::config::ServeArgs;

pub const RUNNING: CheckId = CheckId("tailscale.running");
pub const MAGICDNS: CheckId = CheckId("tailscale.magicdns");
pub const REACH: CheckId = CheckId("tailscale.reach");

/// Probing one address: the daemon's version, or why not.
type HealthProbe = dyn Fn(SocketAddr) -> Result<String, String> + Send + Sync;
/// Resolving a name: its addresses, or why not.
type Resolve = dyn Fn(&str) -> Result<Vec<IpAddr>, String> + Send + Sync;

/// This host's tailnet, detected when a check first asks.
struct Detected {
    status: OnceLock<TailnetStatus>,
    detect: Box<dyn Fn() -> TailnetStatus + Send + Sync>,
}

impl Detected {
    fn new(detect: impl Fn() -> TailnetStatus + Send + Sync + 'static) -> Self {
        Self {
            status: OnceLock::new(),
            detect: Box::new(detect),
        }
    }

    fn get(&self) -> &TailnetStatus {
        self.status.get_or_init(|| (self.detect)())
    }
}

/// `tailscale.running`'s verdict on what detection found.
fn running(status: &TailnetStatus) -> CheckResult {
    let evidence = serde_json::to_value(status).unwrap_or_default();
    let result = match status {
        TailnetStatus::Unavailable(Unavailable::Disabled) => {
            CheckResult::skip("Tailscale detection is off (FSONOS_TAILSCALE=off)")
        }
        TailnetStatus::Unavailable(Unavailable::NotInstalled) => CheckResult::warn(
            "Tailscale is not installed",
            "Install Tailscale (https://tailscale.com/download) and log in to command the \
             speakers from anywhere on your tailnet. Everything else works without it.",
        ),
        TailnetStatus::Unavailable(Unavailable::CliFailed { detail }) => CheckResult::warn(
            "Tailscale is installed but did not answer",
            "Start Tailscale (open the app, or start tailscaled), then run `tailscale up`.",
        )
        .with_detail(detail.clone()),
        TailnetStatus::Unavailable(Unavailable::TimedOut) => CheckResult::warn(
            "`tailscale status` did not answer in time",
            "tailscaled may be stuck: restart Tailscale (quit and reopen the app, or restart \
             tailscaled), then run `fsonos doctor` again.",
        ),
        TailnetStatus::Unavailable(Unavailable::Unparseable { detail }) => CheckResult::warn(
            "`tailscale status --json` answered in a form FrankenSonos cannot read",
            "Update Tailscale to a current release.",
        )
        .with_detail(detail.clone()),
        TailnetStatus::Available(t) if !t.logged_in => CheckResult::warn(
            format!("Tailscale is logged out ({})", state(t)),
            "Run `tailscale up` (or log in from the Tailscale app) to join your tailnet.",
        ),
        TailnetStatus::Available(t) if !t.running => CheckResult::warn(
            format!("Tailscale is not connected ({})", state(t)),
            "Run `tailscale up` (or connect from the Tailscale app).",
        ),
        TailnetStatus::Available(t) if t.ipv4.is_empty() && t.ipv6.is_empty() => CheckResult::warn(
            "Tailscale is up, but this host has no tailnet address",
            "Check this machine in the Tailscale admin console (disabled, or its key \
                 expired), then run `tailscale up`.",
        ),
        TailnetStatus::Available(t) => {
            let found = CheckResult::pass(format!(
                "up as {} ({}){}",
                t.magic_dns_name.as_deref().unwrap_or("this host"),
                addresses(t).join(", "),
                t.tailnet
                    .as_deref()
                    .map(|n| format!(" on {n}"))
                    .unwrap_or_default()
            ));
            if t.source == Source::Interfaces {
                found.with_detail(
                    "Found on this host's interfaces: the `tailscale` CLI did not answer, so \
                     the name and state are unknown.",
                )
            } else {
                found
            }
        }
    };
    result.with_evidence(evidence)
}

fn state(t: &Tailnet) -> &str {
    t.backend_state.as_deref().unwrap_or("unknown state")
}

fn addresses(t: &Tailnet) -> Vec<String> {
    t.ipv4
        .iter()
        .map(ToString::to_string)
        .chain(t.ipv6.iter().map(ToString::to_string))
        .collect()
}

/// `tailscale.magicdns`'s verdict: the name, and what it resolves to here.
fn magicdns(status: &TailnetStatus, resolve: &Resolve) -> CheckResult {
    let Some(t) = status.running() else {
        return CheckResult::skip("Tailscale is not running here (see tailscale.running)");
    };
    let Some(name) = t.magic_dns_name.as_deref() else {
        return if t.source == Source::Interfaces {
            CheckResult::warn(
                "this host's MagicDNS name is unknown: the `tailscale` CLI did not answer",
                "Make sure `tailscale status` works for the user that runs fsonos; the name \
                 comes from it.",
            )
        } else {
            CheckResult::warn(
                "MagicDNS is off: this host has no stable tailnet name",
                format!(
                    "Turn on MagicDNS in the Tailscale admin console (DNS page). Until then, \
                     use the tailnet address ({}), which changes if the machine is re-added.",
                    addresses(t).join(", ")
                ),
            )
        };
    };
    let evidence = |resolved: &[IpAddr]| json!({ "name": name, "resolved": resolved });
    match resolve(name) {
        Ok(resolved) => {
            let ours: Vec<IpAddr> = resolved
                .iter()
                .copied()
                .filter(|ip| is_ours(t, *ip))
                .collect();
            if let Some(ip) = ours.first() {
                CheckResult::pass(format!("{name} resolves to {ip}"))
                    .with_evidence(evidence(&resolved))
            } else {
                CheckResult::warn(
                    format!(
                        "{name} resolves to {}, not this host's tailnet address",
                        list(&resolved)
                    ),
                    "This host is not using Tailscale's DNS: let Tailscale manage it \
                     (`tailscale set --accept-dns=true`).",
                )
                .with_evidence(evidence(&resolved))
            }
        }
        Err(why) => CheckResult::warn(
            format!("{name} does not resolve on this host"),
            "Let Tailscale manage DNS here (`tailscale set --accept-dns=true`), and check \
             that MagicDNS is on in the admin console. Other tailnet devices may still \
             resolve it.",
        )
        .with_detail(why)
        .with_evidence(evidence(&[])),
    }
}

fn is_ours(t: &Tailnet, ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => t.ipv4.contains(&v4),
        IpAddr::V6(v6) => t.ipv6.contains(&v6),
    }
}

fn list(ips: &[IpAddr]) -> String {
    if ips.is_empty() {
        return "nothing".into();
    }
    ips.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `tailscale.reach`'s verdict: does the daemon answer over the tailnet
/// where `serve`'s settings put it?
fn reach(status: &TailnetStatus, serve: &ServeArgs, probe: &HealthProbe) -> CheckResult {
    if status.running().is_none() {
        return CheckResult::skip("Tailscale is not running here (see tailscale.running)");
    }
    let plan = serve.http_plan(status);
    let tailnet: Vec<SocketAddr> = plan
        .addrs
        .iter()
        .copied()
        .filter(|a| fsonos_tailscale::is_tailnet_ip(a.ip()))
        .collect();
    if tailnet.is_empty() {
        let bound = plan.addrs[0];
        let hint = match fsonos_tailscale::reach(status, bound, "") {
            Reach::NotOnTailnet { hint } | Reach::NoTailnet { hint } => hint,
            Reach::Tailnet { .. } => String::new(),
        };
        return CheckResult::skip(format!(
            "the HTTP API is configured on {bound}, not on the tailnet"
        ))
        .with_detail(hint);
    }
    if tailnet.iter().any(|a| a.port() == 0) {
        return CheckResult::skip("the HTTP address has no fixed port to probe");
    }
    // A host may not reach its own tailnet IPv6 address (Tailscale's macOS
    // app drops that traffic), so probe IPv4 where the host has it.
    let (probed, unprobed): (Vec<SocketAddr>, Vec<SocketAddr>) =
        if tailnet.iter().any(SocketAddr::is_ipv4) {
            tailnet.iter().partition(|a| a.is_ipv4())
        } else {
            (tailnet, Vec::new())
        };
    let answers: Vec<(SocketAddr, Result<String, String>)> =
        probed.iter().map(|a| (*a, probe(*a))).collect();
    let evidence = json!({
        "probed": answers.iter().map(|(a, r)| json!({
            "addr": a.to_string(),
            "version": r.as_ref().ok(),
            "error": r.as_ref().err(),
        })).collect::<Vec<_>>(),
        "not_probed": unprobed.iter().map(ToString::to_string).collect::<Vec<_>>(),
    });
    let failed: Vec<String> = answers
        .iter()
        .filter_map(|(a, r)| r.as_ref().err().map(|why| format!("{a}: {why}")))
        .collect();
    if failed.is_empty() {
        let first = probed[0];
        let url = fsonos_tailscale::reach(status, first, "")
            .best_url()
            .map_or_else(|| format!("http://{first}"), str::to_string);
        let mut detail = connect_block(status, serve, first);
        if !unprobed.is_empty() {
            let _ = writeln!(
                detail,
                "Not probed from this host (it may not reach its own tailnet IPv6 address): {}",
                unprobed
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return CheckResult::pass(format!("the daemon answers on the tailnet at {url}"))
            .with_detail(detail)
            .with_evidence(evidence);
    }
    if failed.len() < answers.len() {
        return CheckResult::warn(
            format!(
                "the daemon answers on part of the tailnet; not on {}",
                failed.join("; ")
            ),
            "It could not bind that address when it started; its log says why \
             (`grep 'fsonos serve' ~/Library/Logs/fsonos/fsonos.log`). Restart it once the \
             address is up.",
        )
        .with_evidence(evidence);
    }
    unreached(&plan, serve, &failed, probe).with_evidence(evidence)
}

/// No tailnet address answered: why, as far as this host can tell.
fn unreached(
    plan: &fsonos_tailscale::BindPlan,
    serve: &ServeArgs,
    failed: &[String],
    probe: &HealthProbe,
) -> CheckResult {
    if plan.reason == BindReason::Configured {
        // That address is also what daemon.health probes.
        return CheckResult::skip(format!(
            "no daemon answers on {} (see daemon.health)",
            plan.addrs[0]
        ));
    }
    let local = serve.http_local();
    match probe(local) {
        Ok(version) => CheckResult::warn(
            format!(
                "fsonos serve {version} answers on {local} but not on the tailnet ({})",
                failed.join("; ")
            ),
            "It listens on this machine only. Either it started before Tailscale was up \
             (restart it: `sudo launchctl kickstart -k system/io.github.dicklesworthstone.fsonos` \
             under launchd, see docs/DEPLOY.md; or restart `fsonos serve`), or it runs with \
             FSONOS_HTTP_ADDR set to loopback behind Tailscale Serve, where this is expected \
             (run the doctor with the same setting).",
        ),
        Err(_) => CheckResult::skip(format!("no daemon answers on {local} (see daemon.health)")),
    }
}

/// The connect URLs for both listeners, as `fsonos serve` prints them.
pub(crate) fn connect_block(status: &TailnetStatus, serve: &ServeArgs, http: SocketAddr) -> String {
    let mcp = serve
        .mcp_plan(status)
        .addrs
        .iter()
        .copied()
        .find(|a| fsonos_tailscale::is_tailnet_ip(a.ip()))
        .unwrap_or_else(|| serve.mcp_local());
    fsonos_tailscale::describe(
        status,
        &[
            Listener {
                label: "HTTP API",
                bind: http,
                path: "",
            },
            Listener {
                label: "MCP server",
                bind: mcp,
                path: "/mcp",
            },
        ],
    )
}

struct RunningCheck(Arc<Detected>);

impl Check for RunningCheck {
    fn id(&self) -> CheckId {
        RUNNING
    }

    fn title(&self) -> &'static str {
        "Tailscale"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        running(self.0.get())
    }
}

struct MagicDnsCheck {
    detected: Arc<Detected>,
    resolve: Box<Resolve>,
}

impl Check for MagicDnsCheck {
    fn id(&self) -> CheckId {
        MAGICDNS
    }

    fn title(&self) -> &'static str {
        "MagicDNS name"
    }

    fn requires(&self) -> &[CheckId] {
        &[RUNNING]
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        magicdns(self.detected.get(), &self.resolve)
    }
}

struct ReachCheck {
    detected: Arc<Detected>,
    serve: ServeArgs,
    probe: Box<HealthProbe>,
}

impl Check for ReachCheck {
    fn id(&self) -> CheckId {
        REACH
    }

    fn title(&self) -> &'static str {
        "Daemon on the tailnet"
    }

    fn requires(&self) -> &[CheckId] {
        &[RUNNING]
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        reach(self.detected.get(), &self.serve, &self.probe)
    }
}

/// Resolve `name` with the system resolver (what this host's tools see).
fn system_resolve(name: &str) -> Result<Vec<IpAddr>, String> {
    (name, 0)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|a| a.ip()).collect())
        .map_err(|e| e.to_string())
}

/// Register the Tailscale checks for `serve`'s settings.
pub fn register(runner: &mut Runner, serve: &ServeArgs) {
    let detecting = serve.clone();
    register_with(
        runner,
        serve,
        Detected::new(move || detecting.tailnet()),
        Box::new(system_resolve),
        Box::new(|addr| probe_health(addr, PROBE_TIMEOUT)),
    );
}

/// How long one `/health` probe may take; `tailscale.reach` makes up to one
/// per tailnet address plus one on loopback, inside the runner's timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn register_with(
    runner: &mut Runner,
    serve: &ServeArgs,
    detected: Detected,
    resolve: Box<Resolve>,
    probe: Box<HealthProbe>,
) {
    let detected = Arc::new(detected);
    runner.register(RunningCheck(Arc::clone(&detected)));
    runner.register(MagicDnsCheck {
        detected: Arc::clone(&detected),
        resolve,
    });
    runner.register(ReachCheck {
        detected,
        serve: serve.clone(),
        probe,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::{EXIT_OK, Status};
    use std::net::{Ipv4Addr, Ipv6Addr};

    const V4: Ipv4Addr = Ipv4Addr::new(100, 101, 102, 103);
    const NAME: &str = "sonos-host.example-tailnet.ts.net";

    fn v6() -> Ipv6Addr {
        "fd7a:115c:a1e0::6501:6667".parse().unwrap()
    }

    /// A running tailnet with both families and MagicDNS, then `edit`.
    fn tailnet(edit: impl FnOnce(&mut Tailnet)) -> TailnetStatus {
        let mut t = Tailnet {
            source: Source::Cli,
            backend_state: Some("Running".into()),
            running: true,
            logged_in: true,
            ipv4: vec![V4],
            ipv6: vec![v6()],
            magic_dns_name: Some(NAME.into()),
            tailnet: Some("example.com".into()),
        };
        edit(&mut t);
        TailnetStatus::Available(t)
    }

    fn up() -> TailnetStatus {
        tailnet(|_| {})
    }

    fn serve(http: Option<&str>) -> ServeArgs {
        ServeArgs {
            http: http.map(|a| a.parse().unwrap()),
            mcp_http: None,
            spotify_client_id: None,
            spotify_redirect_uri: String::new(),
            events_port: 0,
            allow_unsafe_bind: false,
            tailscale: crate::config::TailscaleMode::Auto,
            tailscale_serve: false,
        }
    }

    /// A daemon answering on exactly `up`.
    fn answering(up: &'static [&'static str]) -> impl Fn(SocketAddr) -> Result<String, String> {
        move |addr| {
            if up.iter().any(|a| a.parse::<SocketAddr>().unwrap() == addr) {
                Ok("0.1.0".into())
            } else {
                Err("Connection refused (os error 61)".into())
            }
        }
    }

    const EVERYWHERE: &[&str] = &[
        "127.0.0.1:8099",
        "100.101.102.103:8099",
        "[fd7a:115c:a1e0::6501:6667]:8099",
    ];

    /// Log a verdict as the doctor shows it, then hand it back.
    fn logged(case: &str, r: CheckResult) -> CheckResult {
        eprintln!(
            "{case}: {:?} | {} | detail: {} | remedy: {}",
            r.status,
            r.summary,
            r.detail.as_deref().unwrap_or("-"),
            r.remedy.as_deref().unwrap_or("-")
        );
        r
    }

    fn remedy(r: &CheckResult) -> &str {
        r.remedy.as_deref().unwrap_or_default()
    }

    #[test]
    fn each_tailscale_state_has_its_diagnosis_and_remedy() {
        let unavailable = TailnetStatus::Unavailable;
        let cases = [
            (
                "off",
                unavailable(Unavailable::Disabled),
                Status::Skip,
                None,
            ),
            (
                "not installed",
                unavailable(Unavailable::NotInstalled),
                Status::Warn,
                Some("tailscale.com/download"),
            ),
            (
                "cli failed",
                unavailable(Unavailable::CliFailed {
                    detail: "failed to connect to local tailscaled".into(),
                }),
                Status::Warn,
                Some("tailscale up"),
            ),
            (
                "timed out",
                unavailable(Unavailable::TimedOut),
                Status::Warn,
                Some("restart Tailscale"),
            ),
            (
                "unreadable",
                unavailable(Unavailable::Unparseable {
                    detail: "expected value at line 1".into(),
                }),
                Status::Warn,
                Some("Update Tailscale"),
            ),
            (
                "logged out",
                tailnet(|t| {
                    t.running = false;
                    t.logged_in = false;
                    t.backend_state = Some("NeedsLogin".into());
                    t.ipv4.clear();
                    t.ipv6.clear();
                }),
                Status::Warn,
                Some("tailscale up"),
            ),
            (
                "stopped",
                tailnet(|t| {
                    t.running = false;
                    t.backend_state = Some("Stopped".into());
                }),
                Status::Warn,
                Some("tailscale up"),
            ),
            (
                "no address",
                tailnet(|t| {
                    t.ipv4.clear();
                    t.ipv6.clear();
                }),
                Status::Warn,
                Some("admin console"),
            ),
            ("up", up(), Status::Pass, None),
        ];
        for (case, status, want, fix) in cases {
            let r = logged(case, running(&status));
            assert_eq!(r.status, want, "{case}");
            match fix {
                Some(fix) => assert!(remedy(&r).contains(fix), "{case}: {:?}", r.remedy),
                None => assert_eq!(r.remedy, None, "{case}"),
            }
        }
        assert_eq!(
            running(&up()).summary,
            format!("up as {NAME} ({V4}, {}) on example.com", v6())
        );
        let scanned = logged(
            "interfaces",
            running(&tailnet(|t| {
                t.source = Source::Interfaces;
                t.backend_state = None;
                t.magic_dns_name = None;
                t.tailnet = None;
            })),
        );
        assert_eq!(scanned.status, Status::Pass);
        assert!(
            scanned
                .detail
                .as_deref()
                .unwrap()
                .contains("CLI did not answer")
        );
    }

    #[test]
    fn the_magicdns_name_must_resolve_to_this_host() {
        let ours = |_: &str| Ok(vec![IpAddr::V6(v6()), IpAddr::V4(V4)]);
        let r = logged("resolves", magicdns(&up(), &ours));
        assert_eq!(r.status, Status::Pass);
        assert_eq!(r.summary, format!("{NAME} resolves to {}", v6()));

        let elsewhere = |_: &str| Ok(vec!["192.0.2.7".parse().unwrap()]);
        let r = logged("elsewhere", magicdns(&up(), &elsewhere));
        assert_eq!(r.status, Status::Warn);
        assert!(r.summary.contains("resolves to 192.0.2.7"), "{}", r.summary);
        assert!(remedy(&r).contains("--accept-dns"));

        let nowhere = |_: &str| Err("nodename nor servname provided, or not known".to_string());
        let r = logged("unresolved", magicdns(&up(), &nowhere));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.as_deref().unwrap().contains("not known"));
        assert!(remedy(&r).contains("--accept-dns"));

        let off = tailnet(|t| t.magic_dns_name = None);
        let r = logged("magicdns off", magicdns(&off, &ours));
        assert_eq!(r.status, Status::Warn);
        assert!(remedy(&r).contains("Turn on MagicDNS"));
        assert!(remedy(&r).contains(&V4.to_string()));

        let scanned = tailnet(|t| {
            t.source = Source::Interfaces;
            t.magic_dns_name = None;
        });
        let r = logged("name unknown", magicdns(&scanned, &ours));
        assert_eq!(r.status, Status::Warn);
        assert!(remedy(&r).contains("tailscale status"));

        let stopped = tailnet(|t| t.running = false);
        assert_eq!(magicdns(&stopped, &ours).status, Status::Skip);
    }

    #[test]
    fn reach_finds_the_daemon_on_the_tailnet_and_says_where() {
        let r = logged(
            "everywhere",
            reach(&up(), &serve(None), &answering(EVERYWHERE)),
        );
        assert_eq!(r.status, Status::Pass);
        assert_eq!(
            r.summary,
            format!("the daemon answers on the tailnet at http://{NAME}:8099")
        );
        let urls = r.detail.as_deref().unwrap();
        assert!(urls.contains(&format!("http://{V4}:8099")), "{urls}");
        // MCP stays on loopback by default; the block says how to reach it.
        assert!(urls.contains("MCP server"), "{urls}");
        assert!(urls.contains("tailscale serve"), "{urls}");

        // A host may not reach its own tailnet IPv6 address: IPv4 is probed,
        // and the detail says what was not.
        let r = logged(
            "ipv4 probed",
            reach(
                &up(),
                &serve(None),
                &answering(&["127.0.0.1:8099", "100.101.102.103:8099"]),
            ),
        );
        assert_eq!(r.status, Status::Pass);
        assert!(
            r.detail
                .unwrap()
                .contains("Not probed from this host (it may not reach its own tailnet IPv6 address): [fd7a:115c:a1e0::6501:6667]:8099"),
        );
        // An IPv6-only tailnet is probed over IPv6.
        let v6_only = tailnet(|t| t.ipv4.clear());
        let r = reach(&v6_only, &serve(None), &answering(EVERYWHERE));
        assert_eq!(r.status, Status::Pass, "{r:?}");

        // A configured tailnet address is probed as configured.
        let configured = serve(Some("100.101.102.103:8099"));
        let r = reach(&up(), &configured, &answering(EVERYWHERE));
        assert_eq!(r.status, Status::Pass);
    }

    #[test]
    fn reach_explains_a_daemon_the_tailnet_cannot_reach() {
        // Started before Tailscale was up: loopback only.
        let r = logged(
            "loopback only",
            reach(&up(), &serve(None), &answering(&["127.0.0.1:8099"])),
        );
        assert_eq!(r.status, Status::Warn);
        assert!(
            r.summary
                .starts_with("fsonos serve 0.1.0 answers on 127.0.0.1:8099 but not on the tailnet"),
            "{}",
            r.summary
        );
        assert!(remedy(&r).contains("launchctl kickstart -k"));

        // One of two tailnet addresses.
        let two = tailnet(|t| t.ipv4.push(Ipv4Addr::new(100, 101, 102, 104)));
        let r = logged(
            "one of two",
            reach(
                &two,
                &serve(None),
                &answering(&["127.0.0.1:8099", "100.101.102.103:8099"]),
            ),
        );
        assert_eq!(r.status, Status::Warn);
        assert!(r.summary.contains("100.101.102.104:8099"), "{}", r.summary);

        // No daemon at all is daemon.health's finding, not this one's.
        let r = logged("no daemon", reach(&up(), &serve(None), &answering(&[])));
        assert_eq!(r.status, Status::Skip);
        assert!(r.summary.contains("daemon.health"));
        let r = reach(&up(), &serve(Some("100.101.102.103:8099")), &answering(&[]));
        assert_eq!(r.status, Status::Skip);
    }

    #[test]
    fn reach_skips_where_there_is_nothing_to_probe() {
        let all = answering(EVERYWHERE);
        // Loopback behind Tailscale Serve.
        let r = logged(
            "behind serve",
            reach(&up(), &serve(Some("127.0.0.1:8099")), &all),
        );
        assert_eq!(r.status, Status::Skip);
        assert!(r.detail.as_deref().unwrap().contains("tailscale serve"));
        // An ephemeral port.
        let r = reach(&up(), &serve(Some("100.101.102.103:0")), &all);
        assert_eq!(r.status, Status::Skip);
        assert!(r.summary.contains("no fixed port"));
        // No tailnet.
        let r = reach(
            &TailnetStatus::Unavailable(Unavailable::NotInstalled),
            &serve(None),
            &all,
        );
        assert_eq!(r.status, Status::Skip);
    }

    /// The three checks through a runner, detecting `status` once.
    fn doctor(
        status: TailnetStatus,
        probe: &'static [&'static str],
    ) -> fsonos_core::doctor::Report {
        let mut runner = Runner::new();
        register_with(
            &mut runner,
            &serve(None),
            Detected::new(move || status.clone()),
            Box::new(|_: &str| Ok(vec![IpAddr::V4(V4)])),
            Box::new(answering(probe)),
        );
        let report = runner.run().unwrap();
        eprintln!("{}", report.render_table());
        report
    }

    #[test]
    fn a_working_tailnet_passes_and_detection_off_skips() {
        let report = doctor(up(), EVERYWHERE);
        for id in [RUNNING, MAGICDNS, REACH] {
            assert_eq!(report.get(id).unwrap().status, Status::Pass, "{id}");
        }
        assert_eq!(report.exit_code(), EXIT_OK);

        let report = doctor(
            TailnetStatus::Unavailable(Unavailable::Disabled),
            EVERYWHERE,
        );
        for id in [RUNNING, MAGICDNS, REACH] {
            assert_eq!(report.get(id).unwrap().status, Status::Skip, "{id}");
        }
        assert_eq!(report.exit_code(), EXIT_OK);
    }
}
