//! `fsonos serve`: the long-lived daemon.
//!
//! One shared [`Surface`] (the LAN, the household survey, the house policy)
//! backs both control surfaces: the HTTP API and the MCP server over
//! streamable HTTP (`/mcp`). Each listens on its own thread and runtime.
//! Once both are bound, a ready line on stderr names the actual addresses (so
//! ephemeral `:0` ports are usable), then the daemon runs until SIGINT or
//! SIGTERM. It keeps running when discovery finds nothing: calls answer
//! `NOT_READY` until the speakers do (TN3179: macOS shows the Local Network
//! prompt only to a process that stays alive).
//!
//! Callers are identified per listener. A loopback listener's callers are
//! local processes (`loopback-http`, which is how Tailscale Serve arrives);
//! any other bind answers as `unknown`, which the default policy keeps
//! read-only until tailnet identity reaches the HTTP layer.

use anyhow::Context as _;
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Failure, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::SqliteStore;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::config::{self, GlobalArgs, ServeArgs};

/// The data directory, or `INVALID_ARGUMENT` when none can be determined.
pub fn data_dir(global: &GlobalArgs) -> Result<PathBuf, Failure> {
    global.data_dir().ok_or_else(|| {
        Failure::invalid("no data directory is configured and HOME is unset")
            .with_hint("Set FSONOS_DATA_DIR (or HOME) and start again.")
    })
}

/// The house policy from `policy.toml` in `data_dir` (defaults when absent).
pub fn policy(data_dir: &Path) -> Result<Policy, Failure> {
    Policy::load(data_dir).map_err(|e| {
        Failure::invalid(e.to_string()).with_hint("Fix policy.toml in the data directory.")
    })
}

/// The surface the control servers share: the LAN (with any `--routes`),
/// surveyed with the global seeds and SSDP wait.
pub fn surface(global: &GlobalArgs, policy: Policy) -> Result<Surface, Failure> {
    let seeds = global.seed_addrs()?;
    let wait = global.wait();
    let survey: fsonos_api::surface::Survey = Box::new(move |transport| {
        Ok(fsonos_core::inventory::survey(transport, &seeds, wait)?.households)
    });
    Ok(Surface::new(
        Box::new(global.lan()?),
        survey,
        policy,
        Box::new(SystemClock),
    ))
}

/// The store's file in the data directory (the name core's store uses).
pub const DB_FILE: &str = "fsonos.db";

/// `surface` with the action log kept in the data directory's store
/// (`fsonos.db`), recorded as `label`. A store that cannot open is a warning,
/// not a refusal: control still works, only undo and the log do not.
#[must_use]
pub fn with_action_log(surface: Surface, data_dir: &Path, label: &str) -> Surface {
    let opened = std::fs::create_dir_all(data_dir)
        .map_err(|e| e.to_string())
        .and_then(|()| SqliteStore::open(&data_dir.join(DB_FILE)).map_err(|e| e.to_string()));
    match opened {
        Ok(store) => surface.with_action_log(Box::new(store), label),
        Err(e) => {
            tracing::warn!(
                "no action log (undo unavailable): cannot open the store in {}: {e}",
                data_dir.display()
            );
            surface
        }
    }
}

/// Who the callers of a listener bound to `addr` are, for the house policy.
#[must_use]
pub fn listener_client(addr: SocketAddr) -> Client {
    if addr.ip().is_loopback() {
        Client::LoopbackHttp
    } else {
        Client::Unknown
    }
}

/// Vet the configuration, start both servers, report readiness, and run
/// until SIGINT / SIGTERM.
pub fn run(global: &GlobalArgs, args: &ServeArgs) -> anyhow::Result<()> {
    for (listener, addr) in [("HTTP API", args.http), ("MCP server", args.mcp_http)] {
        match config::check_control_bind(listener, addr, args.allow_unsafe_bind) {
            Ok(None) => {}
            Ok(Some(warning)) => tracing::warn!("{warning}"),
            Err(refusal) => return Err(refusal.into()),
        }
    }
    let data_dir = data_dir(global)?;
    let checks = args.clone();
    let surface = Arc::new(
        with_action_log(surface(global, policy(&data_dir)?)?, &data_dir, "serve")
            .with_doctor_checks(Box::new(move |runner| {
                crate::doctor::register(runner, &checks);
            })),
    );

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .context("install the SIGINT/SIGTERM handler")?;
    }

    let names = tailnet_names();
    let (http_server, http_addr) = start_http(&surface, args.http, &names)?;
    let mcp_addr = start_mcp(&surface, args.mcp_http)?;
    eprintln!(
        "fsonos serve: ready http=http://{http_addr} mcp=http://{mcp_addr}/mcp data={}",
        data_dir.display()
    );

    while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!("fsonos serve: stopping");
    http_server.shutdown();
    // Wake the accept loop so it sees the shutdown.
    drop(std::net::TcpStream::connect(http_addr));
    Ok(())
}

/// The names this host has on its tailnet (MagicDNS name and addresses),
/// which tailnet clients and Tailscale Serve send as the Host. Empty off a
/// tailnet.
fn tailnet_names() -> Vec<String> {
    let status = fsonos_tailscale::detect();
    let Some(tailnet) = status.running() else {
        return Vec::new();
    };
    tailnet
        .magic_dns_name
        .iter()
        .cloned()
        .chain(tailnet.ipv4.iter().map(ToString::to_string))
        .chain(tailnet.ipv6.iter().map(ToString::to_string))
        .collect()
}

/// Bind the HTTP API on its own thread; returns the server and the bound
/// address once it listens. Only the listener's own Host names are admitted
/// (DNS-rebinding defense), and every route applies the browser rules.
fn start_http(
    surface: &Arc<Surface>,
    addr: SocketAddr,
    names: &[String],
) -> anyhow::Result<(Arc<TcpServer>, SocketAddr)> {
    let web = WebPolicy::for_listener(addr, names);
    let app = Arc::new(fsonos_api::app(surface, &listener_client(addr), &web));
    let config = ServerConfig::new(addr.to_string()).with_allowed_hosts(web.hosts().to_vec());
    let server = Arc::new(TcpServer::new(config));
    let (bound_tx, bound_rx) = mpsc::channel();
    let serving = Arc::clone(&server);
    thread::Builder::new()
        .name("fsonos-http".into())
        .spawn(move || {
            let ready_tx = bound_tx.clone();
            let result = crate::runtime().and_then(|rt| {
                rt.block_on(async move {
                    let cx = asupersync::Cx::current().context("ambient Cx")?;
                    let listener = asupersync::net::TcpListener::bind(addr)
                        .await
                        .with_context(|| format!("bind the HTTP API on {addr}"))?;
                    let local = listener.local_addr().context("HTTP API address")?;
                    let _ = ready_tx.send(Ok(local));
                    serving
                        .serve_on_app(&cx, listener, app)
                        .await
                        .map_err(|e| anyhow::anyhow!("HTTP API: {e}"))
                })
            });
            if let Err(e) = result {
                let _ = bound_tx.send(Err(e));
            }
        })
        .context("start the HTTP API thread")?;
    let bound = bound_rx
        .recv_timeout(Duration::from_secs(10))
        .context("the HTTP API did not start within 10 s")??;
    Ok((server, bound))
}

/// Bind the MCP server (streamable HTTP) on its own thread; returns the bound
/// address once it listens.
fn start_mcp(surface: &Arc<Surface>, addr: SocketAddr) -> anyhow::Result<SocketAddr> {
    let backend = fsonos_mcp::tools::Backend::shared(Arc::clone(surface), listener_client(addr));
    if !fsonos_mcp::tools::install(backend) {
        anyhow::bail!("the MCP backend is already installed");
    }
    let (bound_tx, bound_rx) = mpsc::channel();
    thread::Builder::new()
        .name("fsonos-mcp".into())
        .spawn(move || {
            let ready_tx = bound_tx.clone();
            let result = crate::runtime().and_then(|rt| {
                rt.block_on(async move {
                    let cx = asupersync::Cx::current().context("ambient Cx")?;
                    let bound = fsonos_mcp::server()
                        .bind_http(&cx, addr.to_string())
                        .await
                        .map_err(|e| anyhow::anyhow!("bind the MCP server on {addr}: {e}"))?;
                    let local = bound
                        .local_addr()
                        .map_err(|e| anyhow::anyhow!("MCP address: {e}"))?;
                    let _ = ready_tx.send(Ok(local));
                    bound
                        .serve(&cx)
                        .await
                        .map(drop)
                        .map_err(|e| anyhow::anyhow!("MCP server: {e}"))
                })
            });
            if let Err(e) = result {
                let _ = bound_tx.send(Err(e));
            }
        })
        .context("start the MCP thread")?;
    bound_rx
        .recv_timeout(Duration::from_secs(10))
        .context("the MCP server did not start within 10 s")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_listeners_serve_local_callers() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(listener_client(at("127.0.0.1:8099")), Client::LoopbackHttp);
        assert_eq!(listener_client(at("[::1]:8099")), Client::LoopbackHttp);
        assert_eq!(listener_client(at("100.70.1.2:8099")), Client::Unknown);
    }
}
