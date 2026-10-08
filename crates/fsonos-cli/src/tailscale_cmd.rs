//! `fsonos tailscale`: put Tailscale Serve in front of the daemon, or take it
//! away. Serve gives tailnet devices and agents `https://<host>.<tailnet>.ts.net/`
//! for the HTTP API and `https://<host>.<tailnet>.ts.net:8443/mcp` for MCP,
//! with certificates Tailscale manages, while the daemon itself listens on
//! loopback only.
//!
//! * `setup`: add both mappings; running it again changes nothing. It
//!   refuses while Funnel is on for either port and never replaces Serve
//!   config it did not make.
//! * `status`: what Serve does on those ports, and the URLs to use.
//! * `teardown`: remove the daemon's mappings, and only those.
//!
//! Funnel is never used: it would publish the API and MCP server, which have
//! no authentication of their own, to the public internet.

use anyhow::{Context as _, anyhow, bail};
use fsonos_tailscale::serve::{self, Mapping, MappingStatus, ServeConfig, Step};
use fsonos_tailscale::{Probe, StepError, TailnetStatus};
use serde_json::json;
use std::time::Duration;

use crate::config::{GlobalArgs, ServeArgs};

/// The HTTPS port Serve gives the HTTP API.
pub const API_PORT: u16 = 443;
/// The HTTPS port Serve gives the MCP server.
pub const MCP_PORT: u16 = 8443;
/// How long one `tailscale serve` step may take (the first HTTPS mapping
/// waits for a certificate).
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// `fsonos tailscale` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct TailscaleArgs {
    #[command(subcommand)]
    pub action: Action,
    /// The daemon's settings (the same flags and env as `serve`): which local
    /// listeners Serve fronts.
    #[command(flatten)]
    pub serve: ServeArgs,
}

/// What to do with Tailscale Serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum Action {
    /// Front the daemon with Tailscale Serve: HTTPS on 443 (the HTTP API) and
    /// 8443 (MCP). Running it again changes nothing.
    Setup {
        /// Print what would run; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// What Serve does on the daemon's ports, and the URLs to use.
    Status,
    /// Remove the daemon's Serve mappings, and only those.
    Teardown {
        /// Print what would run; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// The daemon's two Serve mappings for `serve`'s settings: Serve proxies to
/// the listeners this machine reaches.
#[must_use]
pub fn mappings(serve: &ServeArgs) -> [Mapping; 2] {
    [
        Mapping {
            label: "HTTP API".into(),
            https_port: API_PORT,
            target: format!("http://{}", serve.http_local()),
        },
        Mapping {
            label: "MCP server".into(),
            https_port: MCP_PORT,
            target: format!("http://{}", serve.mcp_local()),
        },
    ]
}

/// The URL tailnet clients use for `mapping` on the MagicDNS `name`.
#[must_use]
pub fn url(name: &str, mapping: &Mapping) -> String {
    let path = if mapping.https_port == MCP_PORT {
        "/mcp"
    } else {
        "/"
    };
    if mapping.https_port == 443 {
        format!("https://{name}{path}")
    } else {
        format!("https://{name}:{}{path}", mapping.https_port)
    }
}

/// What every action starts from: this host's MagicDNS name, a probe for
/// the `tailscale` CLI, and Serve's current config.
struct Serving {
    name: String,
    probe: Probe,
    config: ServeConfig,
}

/// Check Tailscale is up with MagicDNS, and read Serve's config.
fn prepare(serve: &ServeArgs, tailnet: &TailnetStatus) -> anyhow::Result<Serving> {
    let Some(t) = tailnet.running() else {
        let hint = match fsonos_tailscale::reach(tailnet, serve.http_local(), "") {
            fsonos_tailscale::Reach::NoTailnet { hint } => hint,
            _ => String::new(),
        };
        bail!("Tailscale is not running here. {hint}");
    };
    // Serve's HTTPS certificate is issued for the MagicDNS name.
    let name = t.magic_dns_name.clone().context(
        "Tailscale Serve's HTTPS needs MagicDNS and HTTPS certificates: turn both on in the \
         Tailscale admin console (DNS page), then run this again",
    )?;
    let probe = Probe {
        timeout: STEP_TIMEOUT,
        ..Probe::default()
    };
    let config = probe
        .serve_config()
        .map_err(|why| anyhow!("could not read Tailscale Serve's config: {why:?}"))?;
    Ok(Serving {
        name,
        probe,
        config,
    })
}

/// Run `fsonos tailscale <action>`.
pub fn run(global: &GlobalArgs, args: &TailscaleArgs) -> anyhow::Result<()> {
    let Serving {
        name,
        probe,
        config,
    } = prepare(&args.serve, &args.serve.tailnet())?;
    let ours = mappings(&args.serve);
    match args.action {
        Action::Status => {
            report_status(global, &name, &serve::status(&config, &ours));
            Ok(())
        }
        Action::Setup { dry_run } => {
            let steps =
                serve::plan_setup(&config, &ours).map_err(|refusal| anyhow!("{refusal}"))?;
            execute(global, &probe, &steps, dry_run)?;
            if !dry_run && !global.json {
                for m in &ours {
                    let label = format!("{}:", m.label);
                    println!("{label:<11} {}", url(&name, m));
                }
                println!(
                    "Agents: claude mcp add --transport http fsonos {}",
                    url(&name, &ours[1])
                );
                println!(
                    "Note: Tailscale's HTTPS certificate for {name} is recorded in public \
                     Certificate Transparency logs."
                );
            }
            Ok(())
        }
        Action::Teardown { dry_run } => execute(
            global,
            &probe,
            &serve::plan_teardown(&config, &ours),
            dry_run,
        ),
    }
}

/// `fsonos serve --tailscale-serve`: the setup, at daemon startup. One line
/// for the log saying where the daemon is now reachable, or why not.
///
/// # Errors
/// Tailscale is down or without MagicDNS, Serve refuses (Funnel, a port in
/// use), or a step fails.
pub fn setup_at_startup(serve: &ServeArgs, tailnet: &TailnetStatus) -> anyhow::Result<String> {
    let Serving {
        name,
        probe,
        config,
    } = prepare(serve, tailnet)?;
    let ours = mappings(serve);
    let steps = serve::plan_setup(&config, &ours).map_err(|refusal| anyhow!("{refusal}"))?;
    let mut added = 0;
    for step in &steps {
        if let Step::Run { mapping, argv } = step {
            probe.run_step(argv).map_err(|why| {
                anyhow!(
                    "{}: `tailscale {}` failed: {}",
                    mapping.label,
                    argv.join(" "),
                    explain(&why)
                )
            })?;
            added += 1;
        }
    }
    Ok(format!(
        "Tailscale Serve: {} and {} ({added} added, {} already set up)",
        url(&name, &ours[0]),
        url(&name, &ours[1]),
        steps.len() - added
    ))
}

/// Run each step's argv (or only print them with `dry_run`).
fn execute(
    global: &GlobalArgs,
    probe: &Probe,
    steps: &[Step],
    dry_run: bool,
) -> anyhow::Result<()> {
    if global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "dry_run": dry_run, "steps": steps }))?
        );
    }
    for step in steps {
        match step {
            Step::Keep { mapping } => {
                if !global.json {
                    println!(
                        "{}: already as wanted (port {})",
                        mapping.label, mapping.https_port
                    );
                }
            }
            Step::Leave { mapping, current } => {
                if !global.json {
                    println!(
                        "{}: port {} serves {current}, not the daemon; left alone",
                        mapping.label, mapping.https_port
                    );
                }
            }
            Step::Run { mapping, argv } => {
                let command = format!("tailscale {}", argv.join(" "));
                if dry_run {
                    if !global.json {
                        println!("{}: would run `{command}`", mapping.label);
                    }
                    continue;
                }
                probe.run_step(argv).map_err(|why| {
                    anyhow!("{}: `{command}` failed: {}", mapping.label, explain(&why))
                })?;
                if !global.json {
                    println!("{}: ran `{command}`", mapping.label);
                }
            }
        }
    }
    Ok(())
}

/// A step's failure, with the likely fix.
fn explain(why: &StepError) -> String {
    const HTTPS: &str = "Serve's HTTPS needs HTTPS certificates turned on in the Tailscale admin \
                         console (DNS page), or the consent URL it printed";
    match why {
        StepError::NotInstalled => "the `tailscale` CLI is not installed".into(),
        StepError::TimedOut { output } => format!(
            "it did not finish within {} s{}. {HTTPS}",
            STEP_TIMEOUT.as_secs(),
            if output.is_empty() {
                String::new()
            } else {
                format!(" ({output})")
            }
        ),
        StepError::Failed { detail } => {
            let lower = detail.to_ascii_lowercase();
            if lower.contains("https") || lower.contains("cert") || lower.contains("not enabled") {
                format!("{detail}. {HTTPS}")
            } else {
                detail.clone()
            }
        }
    }
}

/// Print `fsonos tailscale status`.
fn report_status(global: &GlobalArgs, name: &str, status: &[MappingStatus]) {
    if global.json {
        let rows: Vec<_> = status
            .iter()
            .map(|s| json!({ "status": s, "url": url(name, &s.mapping) }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return;
    }
    for s in status {
        let state = if s.active {
            format!("active  {}", url(name, &s.mapping))
        } else {
            format!(
                "not set up (port {} serves {})",
                s.mapping.https_port, s.current
            )
        };
        let label = format!("{}:", s.mapping.label);
        println!("{label:<11} {state}");
        if s.funnel {
            println!(
                "  WARNING: Funnel is ON for port {}: it is reachable from the public internet, \
                 with no authentication. Turn it off: `tailscale funnel --https={} off`",
                s.mapping.https_port, s.mapping.https_port
            );
        }
    }
    if status.iter().any(|s| !s.active) {
        println!("Run `fsonos tailscale setup` to add what is missing.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    const NAME: &str = "sonos-host.example-tailnet.ts.net";

    #[derive(Parser)]
    struct Harness {
        #[command(subcommand)]
        command: Commands,
    }

    #[derive(clap::Subcommand)]
    enum Commands {
        Tailscale(TailscaleArgs),
    }

    fn parse(args: &[&str]) -> Result<TailscaleArgs, clap::Error> {
        Harness::try_parse_from(args).map(|h| match h.command {
            Commands::Tailscale(args) => args,
        })
    }

    #[test]
    fn serve_fronts_the_daemons_local_listeners() {
        let args = parse(&["fsonos", "tailscale", "setup"]).unwrap();
        assert_eq!(args.action, Action::Setup { dry_run: false });
        let [api, mcp] = mappings(&args.serve);
        assert_eq!(
            (api.https_port, api.target.as_str()),
            (443, "http://127.0.0.1:8099")
        );
        assert_eq!(
            (mcp.https_port, mcp.target.as_str()),
            (8443, "http://127.0.0.1:8098")
        );
        let args = parse(&[
            "fsonos",
            "tailscale",
            "--http",
            "127.0.0.1:9099",
            "--mcp-http",
            "[::1]:9098",
            "teardown",
            "--dry-run",
        ])
        .unwrap();
        assert_eq!(args.action, Action::Teardown { dry_run: true });
        let [api, mcp] = mappings(&args.serve);
        assert_eq!(api.target, "http://127.0.0.1:9099");
        assert_eq!(mcp.target, "http://[::1]:9098");
        // There is no way to ask for Funnel.
        assert!(parse(&["fsonos", "tailscale", "funnel"]).is_err());
        assert!(parse(&["fsonos", "tailscale", "setup", "--funnel"]).is_err());
    }

    #[test]
    fn the_api_is_at_the_root_and_mcp_on_8443() {
        let [api, mcp] = mappings(&parse(&["fsonos", "tailscale", "status"]).unwrap().serve);
        assert_eq!(url(NAME, &api), format!("https://{NAME}/"));
        assert_eq!(url(NAME, &mcp), format!("https://{NAME}:8443/mcp"));
    }

    #[test]
    fn a_failed_step_says_how_to_fix_it() {
        let waiting = explain(&StepError::TimedOut {
            output: "To enable, visit: https://login.example/f/serve".into(),
        });
        eprintln!("timed out: {waiting}");
        assert!(
            waiting.contains("https://login.example/f/serve"),
            "{waiting}"
        );
        assert!(waiting.contains("HTTPS certificates"), "{waiting}");
        let https = explain(&StepError::Failed {
            detail: "serve: HTTPS is not enabled for this tailnet".into(),
        });
        assert!(https.contains("admin console"), "{https}");
        assert_eq!(
            explain(&StepError::Failed {
                detail: "permission denied".into()
            }),
            "permission denied"
        );
        assert!(explain(&StepError::NotInstalled).contains("not installed"));
    }
}
