//! `fsonos doctor`: what is wrong with this setup, and how to fix it.
//!
//! The report comes from the shared surface (the core's checks), plus the
//! checks only this layer can make:
//!
//! * `daemon.bind`: the bind guard's verdict on the HTTP and MCP addresses;
//! * `daemon.health`: whether a daemon answers on the HTTP address, and
//!   which version.
//!
//! Exit codes: 0 all passed, 6 warnings only, 7 something failed (outside
//! the CLI's 1-5 error codes and clap's 2).

use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult, Report, Runner};
use fsonos_core::policy::Client;
use serde_json::json;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use crate::config::{self, GlobalArgs, ServeArgs};
use crate::direct::Direct;

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

/// Whether `fsonos serve` answers `/health` on the HTTP address.
struct HealthCheck {
    http: SocketAddr,
}

impl HealthCheck {
    fn probe(&self, timeout: Duration) -> Result<String, String> {
        let mut stream =
            TcpStream::connect_timeout(&self.http, timeout).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let request = format!(
            "GET /health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.http
        );
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
        match self.probe(timeout) {
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

/// Register the checks this layer owns for `serve`'s settings.
pub fn register(runner: &mut Runner, serve: &ServeArgs) {
    runner.register(BindCheck {
        http: serve.http,
        mcp: serve.mcp_http,
        allow_unsafe: serve.allow_unsafe_bind,
    });
    runner.register(HealthCheck { http: serve.http });
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

/// Run `fsonos doctor` and print the report; the exit code is the report's.
pub fn run(global: &GlobalArgs, args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    let serve = args.serve.clone();
    let direct = Direct::open(
        global,
        Some(Box::new(move |runner| register(runner, &serve))),
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
                http: "127.0.0.1:0".parse().unwrap(),
                mcp_http: "127.0.0.1:0".parse().unwrap(),
                spotify_client_id: None,
                spotify_redirect_uri: String::new(),
                allow_unsafe_bind: false,
            },
        );
        let report = only(runner.run().unwrap(), Some("daemon.b"));
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].id, BIND);
    }
}
