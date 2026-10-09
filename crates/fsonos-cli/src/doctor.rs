//! `fsonos doctor`: what is wrong with this setup, and how to fix it.
//!
//! The report comes from the shared surface (the core's checks), plus the
//! checks only this layer can make:
//!
//! * `store.open` and `lan.*` ([`lan_checks`]): the core's LAN checks over
//!   this host's LAN as `--routes` and the seeds shape it (the store, SSDP,
//!   the seeds, reading each player, the households, a GENA round trip, the
//!   topology);
//! * `daemon.bind`: the bind guard's verdict on the HTTP and MCP addresses;
//! * `daemon.health`: whether a daemon answers on the HTTP address, and
//!   which version;
//! * `spotify.taste`: whether the cached Spotify grant carries the taste
//!   scopes, so the DJ learns beyond the owner's saved library;
//! * `tailscale.*` ([`tailscale`]): whether the daemon is reachable over the
//!   tailnet, with the connect URLs, or why not.
//!
//! Exit codes: 0 all passed, 6 warnings only, 7 something failed (outside
//! the CLI's 1-5 error codes and clap's 2).

use fsonos_api::Failure;
use fsonos_core::doctor::lan::LanProbe;
use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult, Report, Runner};
use fsonos_core::policy::Client;
use fsonos_spotify::client::TokenCache;
use serde_json::json;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{self, GlobalArgs, ServeArgs};
use crate::direct::Direct;

pub mod tailscale;

/// `fsonos doctor` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct DoctorArgs {
    /// The daemon settings to check (the same flags and env as `serve`).
    #[command(flatten)]
    pub serve: ServeArgs,
    /// Only checks whose id starts with this (e.g. `spotify`, `daemon.`).
    #[arg(long, value_name = "PREFIX")]
    pub only: Option<String>,
}

const BIND: CheckId = CheckId("daemon.bind");
const HEALTH: CheckId = CheckId("daemon.health");
const TASTE: CheckId = CheckId("spotify.taste");

/// The bind guard's verdict for both control listeners.
struct BindCheck {
    http: SocketAddr,
    mcp: SocketAddr,
    allow_unsafe: bool,
}

impl Check for BindCheck {
    fn id(&self) -> CheckId {
        BIND
    }

    fn title(&self) -> &'static str {
        "Listener addresses"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        let evidence = json!({ "http": self.http.to_string(), "mcp": self.mcp.to_string() });
        let mut warnings = Vec::new();
        for (listener, addr) in [("HTTP API", self.http), ("MCP server", self.mcp)] {
            match config::check_control_bind(listener, addr, self.allow_unsafe) {
                Ok(None) => {}
                Ok(Some(warning)) => warnings.push(warning),
                Err(refusal) => {
                    return CheckResult::fail(refusal.detail, refusal.hint).with_evidence(evidence);
                }
            }
        }
        if warnings.is_empty() {
            CheckResult::pass(format!(
                "HTTP {} and MCP {} are loopback or tailnet addresses",
                self.http, self.mcp
            ))
            .with_evidence(evidence)
        } else {
            CheckResult::warn(
                warnings.join("; "),
                "Bind 127.0.0.1 and front it with Tailscale Serve (docs/DEPLOY.md).",
            )
            .with_evidence(evidence)
        }
    }
}

/// `GET /health` on `addr`: the daemon's version, or why there is none.
fn probe_health(addr: SocketAddr, timeout: Duration) -> Result<String, String> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let request = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .map_err(|e| e.to_string())?;
    let body = answer.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let health: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| format!("not a FrankenSonos answer: {answer:.80}"))?;
    health["version"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "no version in /health".to_string())
}

/// Whether `fsonos serve` answers `/health` on the HTTP address.
struct HealthCheck {
    http: SocketAddr,
}

impl Check for HealthCheck {
    fn id(&self) -> CheckId {
        HEALTH
    }

    fn title(&self) -> &'static str {
        "Daemon"
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        if self.http.port() == 0 {
            return CheckResult::skip("the HTTP address has no fixed port to probe");
        }
        let timeout = ctx.remaining().min(Duration::from_secs(3));
        match probe_health(self.http, timeout) {
            Ok(version) => {
                CheckResult::pass(format!("fsonos serve {version} answers on {}", self.http))
                    .with_evidence(json!({ "version": version }))
            }
            Err(why) => CheckResult::warn(
                format!("no daemon answers on {}", self.http),
                "Start it with `fsonos serve`, or under launchd (docs/DEPLOY.md).",
            )
            .with_detail(why),
        }
    }
}

/// Whether the cached Spotify grant carries the taste scopes, so the DJ can
/// learn beyond the owner's saved library (followed and top artists, top
/// tracks, recent plays, playlists). Without them the DJ runs library-only,
/// which this warns about; it never fails.
struct TasteScopesCheck {
    data_dir: PathBuf,
}

impl Check for TasteScopesCheck {
    fn id(&self) -> CheckId {
        TASTE
    }

    fn title(&self) -> &'static str {
        "Spotify taste scopes"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        match TokenCache::in_data_dir(&self.data_dir).load() {
            Ok(None) => {
                CheckResult::skip("not signed in to Spotify yet (run `fsonos setup spotify`)")
            }
            Ok(Some(token)) => {
                let missing = token.missing_taste_scopes();
                if missing.is_empty() {
                    CheckResult::pass(
                        "the grant carries every taste scope; the DJ learns from your \
                         follows, top artists and tracks, recent plays and playlists",
                    )
                } else {
                    CheckResult::warn(
                        format!(
                            "the DJ's taste is library-only: the Spotify grant is missing {}",
                            missing.join(", ")
                        ),
                        "Re-run `fsonos setup spotify` (or sign in again) to grant the taste scopes.",
                    )
                    .with_evidence(json!({ "missing_scopes": missing }))
                }
            }
            Err(e) => CheckResult::warn(
                "could not read the Spotify token cache".to_owned(),
                "Check the data directory, or re-run `fsonos setup spotify`.",
            )
            .with_detail(e.to_string()),
        }
    }
}

/// Register the checks this layer owns for `serve`'s settings.
pub fn register(runner: &mut Runner, serve: &ServeArgs, data_dir: &Path) {
    runner.register(BindCheck {
        http: serve.http_local(),
        mcp: serve.mcp_local(),
        allow_unsafe: serve.allow_unsafe_bind,
    });
    runner.register(HealthCheck {
        http: serve.http_local(),
    });
    runner.register(TasteScopesCheck {
        data_dir: data_dir.to_path_buf(),
    });
    tailscale::register(runner, serve);
}

/// Keep only the checks whose id starts with `prefix`.
#[must_use]
pub fn only(report: Report, prefix: Option<&str>) -> Report {
    match prefix {
        None => report,
        Some(prefix) => Report {
            entries: report
                .entries
                .into_iter()
                .filter(|e| e.id.0.starts_with(prefix))
                .collect(),
        },
    }
}

/// The core's LAN checks over this host's LAN, as `--routes`, the seeds and
/// `--wait` shape it, with the store check for the data directory. Each run
/// takes a fresh probe (it discovers once and caches what it saw).
pub fn lan_checks(
    global: &GlobalArgs,
) -> Result<impl Fn(&mut Runner) + Send + Sync + 'static, Failure> {
    let network = global.network()?;
    let probe = Arc::new(
        LanProbe::new(Arc::clone(&network.lan), global.seed_addrs()?).with_ssdp_wait(global.wait()),
    );
    let data_dir = crate::daemon::data_dir(global)?;
    Ok(move |runner: &mut Runner| {
        fsonos_core::doctor::lan::register(runner, data_dir.clone(), &probe);
    })
}

/// Run `fsonos doctor` and print the report; the exit code is the report's.
pub fn run(global: &GlobalArgs, args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    let serve = args.serve.clone();
    let lan = lan_checks(global)?;
    let data_dir = crate::daemon::data_dir(global)?;
    let direct = Direct::open(
        global,
        Some(Box::new(move |runner| {
            lan(runner);
            register(runner, &serve, &data_dir);
        })),
    )?;
    let report = only(direct.doctor(&Client::Cli)?, args.only.as_deref());
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render_table());
    }
    Ok(ExitCode::from(report.exit_code()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::Status;

    /// Run one check the way `fsonos doctor` does, through a runner.
    fn run_one(check: impl Check + 'static) -> CheckResult {
        let id = check.id();
        let mut runner = Runner::new();
        runner.register(check);
        runner.run().unwrap().get(id).cloned().unwrap()
    }

    fn bind(http: &str, mcp: &str) -> CheckResult {
        run_one(BindCheck {
            http: http.parse().unwrap(),
            mcp: mcp.parse().unwrap(),
            allow_unsafe: false,
        })
    }

    #[test]
    fn bind_verdicts_follow_the_guard() {
        assert_eq!(
            bind("127.0.0.1:8099", "127.0.0.1:8098").status,
            Status::Pass
        );
        assert_eq!(
            bind("192.168.1.9:8099", "127.0.0.1:8098").status,
            Status::Warn
        );
        let wild = bind("0.0.0.0:8099", "127.0.0.1:8098");
        assert_eq!(wild.status, Status::Fail);
        assert!(wild.remedy.unwrap().contains("--allow-unsafe-bind"));
    }

    #[test]
    fn a_missing_daemon_is_a_warning_with_a_remedy() {
        // Nothing listens on a fresh ephemeral port once its listener is dropped.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let r = run_one(HealthCheck {
            http: SocketAddr::from(([127, 0, 0, 1], port)),
        });
        assert_eq!(r.status, Status::Warn);
        assert!(r.remedy.unwrap().contains("fsonos serve"));
        let skipped = run_one(HealthCheck {
            http: "127.0.0.1:0".parse().unwrap(),
        });
        assert_eq!(skipped.status, Status::Skip);
    }

    #[test]
    fn only_keeps_a_prefix() {
        let mut runner = Runner::new();
        register(
            &mut runner,
            &ServeArgs {
                http: Some("127.0.0.1:0".parse().unwrap()),
                mcp_http: Some("127.0.0.1:0".parse().unwrap()),
                spotify_client_id: None,
                spotify_redirect_uri: String::new(),
                events_port: 0,
                allow_unsafe_bind: false,
                tailscale: crate::config::TailscaleMode::Auto,
                tailscale_serve: false,
            },
            &std::env::temp_dir(),
        );
        let report = only(runner.run().unwrap(), Some("daemon.b"));
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].id, BIND);
    }

    #[test]
    fn taste_scopes_skip_when_not_signed_in() {
        let dir =
            std::env::temp_dir().join(format!("fsonos-doctor-taste-{}-absent", std::process::id()));
        assert_eq!(run_one(TasteScopesCheck { data_dir: dir }).status, Status::Skip);
    }
}
