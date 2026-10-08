//! `fsonos serve`: the long-lived daemon.
//!
//! One shared [`Surface`] (the LAN, the house policy, and the live model)
//! backs both control surfaces: the HTTP API and the MCP server over
//! streamable HTTP (`/mcp`). Each listens on its own thread and runtime.
//! Once both are bound, a ready line on stderr names the actual addresses (so
//! ephemeral `:0` ports are usable), then the daemon runs until SIGINT or
//! SIGTERM. It keeps running when discovery finds nothing: calls answer
//! `NOT_READY` until the speakers do (TN3179: macOS shows the Local Network
//! prompt only to a process that stays alive).
//!
//! The live model ([`Live`]) surveys in the background and keeps the zones
//! current from the players' GENA events, which arrive on the events port
//! (`--events-port`). A second line, `fsonos serve: live ...`, says when its
//! first survey found the households and where events arrive. Stopping the
//! daemon ends every subscription.
//!
//! Unconfigured, the HTTP API listens on loopback plus this host's tailnet
//! addresses (ts-autobind) and MCP on loopback; a third line,
//! `fsonos serve: listening (...)`, and the connect URLs follow the ready
//! line.
//!
//! Callers are identified per listener. A loopback listener's callers are
//! local processes (`loopback-http`, which is how Tailscale Serve arrives);
//! any other bind answers as `unknown`, which the default policy keeps
//! read-only until tailnet identity reaches the HTTP layer.

use anyhow::Context as _;
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Failure, Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::live::{Live, LiveConfig, LiveEvent};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::SqliteStore;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak, mpsc};
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
    Ok(
        Surface::new(global.lan()?, survey, policy, Box::new(SystemClock))
            .with_dj(Box::new(crate::dj::SpotifyDj::default())),
    )
}

/// The daemon's surface over a live model of the speakers: surveys and
/// reads through the (confined) transport, events through the LAN on
/// `events_port`. The caller owns the model: dropping the returned `Arc`
/// ends every subscription.
pub fn live_surface(
    global: &GlobalArgs,
    events_port: u16,
    policy: Policy,
) -> Result<(Surface, Arc<Live>), Failure> {
    let seeds = global.seed_addrs()?;
    let wait = global.wait();
    let network = global.network()?;
    let config = LiveConfig {
        callback_port: events_port,
        ..LiveConfig::new(seeds.clone())
    };
    let live = Arc::new(Live::start_with(
        Arc::clone(&network.transport),
        network.lan,
        config,
    ));
    // Right after a regroup the surface surveys directly; see
    // `fsonos_api::surface::SETTLE`.
    let survey: fsonos_api::surface::Survey = Box::new(move |transport| {
        Ok(fsonos_core::inventory::survey(transport, &seeds, wait)?.households)
    });
    let surface = Surface::new(
        Box::new(network.transport),
        survey,
        policy,
        Box::new(SystemClock),
    )
    .with_live(&live)
    .with_dj(Box::new(crate::dj::SpotifyDj::default()));
    Ok((surface, live))
}

/// Print `fsonos serve: live ...` once the model's first survey has found
/// the households (at once if it already has). Ends with the model.
fn announce_live(live: &Arc<Live>) {
    // Subscribe before looking, so a survey finishing in between is seen.
    let changes = live.subscribe();
    if announce_if_found(live) {
        return;
    }
    let live: Weak<Live> = Arc::downgrade(live);
    let _ = thread::Builder::new()
        .name("fsonos-live-ready".into())
        .spawn(move || {
            while let Ok(change) = changes.recv() {
                if change != LiveEvent::Topology {
                    continue;
                }
                let Some(live) = live.upgrade() else { return };
                if announce_if_found(&live) {
                    return;
                }
            }
        });
}

/// The live line, if the model has found players; whether it was printed.
fn announce_if_found(live: &Live) -> bool {
    let snapshot = live.snapshot();
    let players: usize = snapshot.households.iter().map(|h| h.players.len()).sum();
    if players > 0 {
        eprintln!(
            "fsonos serve: live households={} players={players} events={}",
            snapshot.households.len(),
            snapshot.callback.as_deref().unwrap_or("none"),
        );
    }
    players > 0
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
    // Unconfigured listeners bind loopback plus the tailnet (ts-autobind).
    let tailnet = args.tailnet();
    let http_plan = args.http_plan(&tailnet);
    let mcp_plan = args.mcp_plan(&tailnet);
    for (listener, plan) in [("HTTP API", &http_plan), ("MCP server", &mcp_plan)] {
        for &addr in &plan.addrs {
            match config::check_control_bind(listener, addr, args.allow_unsafe_bind) {
                Ok(None) => {}
                Ok(Some(warning)) => tracing::warn!("{warning}"),
                Err(refusal) => return Err(refusal.into()),
            }
        }
    }
    let data_dir = data_dir(global)?;
    let checks = args.clone();
    let (surface, live) = live_surface(global, args.events_port, policy(&data_dir)?)?;
    let surface = Arc::new(
        with_action_log(surface, &data_dir, "serve").with_doctor_checks(Box::new(move |runner| {
            crate::doctor::register(runner, &checks);
        })),
    );
    // The live model's playback keeps each DJ queue topped up.
    fsonos_api::surface::follow(&surface);

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .context("install the SIGINT/SIGTERM handler")?;
    }

    let names = tailnet_names(&tailnet);
    let http = start_all("HTTP API", &http_plan, |addr| {
        start_http(&surface, addr, &names, args.tailscale_serve)
    })?;
    let mcp = start_all("MCP server", &mcp_plan, |addr| start_mcp(&surface, addr))?;
    // One value per key: the e2e harness and scripts parse this line.
    eprintln!(
        "fsonos serve: ready http=http://{} mcp=http://{}/mcp data={}",
        http[0].1,
        mcp[0],
        data_dir.display()
    );
    announce_tailnet(&tailnet, &http_plan, &http, &mcp);
    if args.tailscale_serve {
        // A first HTTPS certificate can take a while: not on the main thread.
        let (serve, tailnet) = (args.clone(), tailnet.clone());
        thread::spawn(
            move || match crate::tailscale_cmd::setup_at_startup(&serve, &tailnet) {
                Ok(done) => eprintln!("fsonos serve: {done}"),
                Err(e) => eprintln!("fsonos serve: Tailscale Serve not set up: {e:#}"),
            },
        );
    }
    announce_live(&live);

    while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!("fsonos serve: stopping");
    for (server, addr) in &http {
        server.shutdown();
        // Wake the accept loop so it sees the shutdown. Briefly: a host may
        // not reach its own tailnet IPv6 address (Tailscale on macOS), and
        // the loop also polls for the shutdown.
        drop(std::net::TcpStream::connect_timeout(
            addr,
            Duration::from_millis(250),
        ));
    }
    // The surfaces hold the model weakly: this ends every subscription.
    drop(live);
    Ok(())
}

/// Bind every address of `plan`. Loopback and a configured address must
/// bind; a tailnet address that fails in the automatic plan (Tailscale still
/// coming up, say) is logged and skipped rather than stopping the daemon.
fn start_all<T>(
    listener: &str,
    plan: &fsonos_tailscale::BindPlan,
    mut start: impl FnMut(SocketAddr) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<T>> {
    let mut started = Vec::new();
    for &addr in &plan.addrs {
        match start(addr) {
            Ok(bound) => started.push(bound),
            Err(e)
                if plan.reason == fsonos_tailscale::BindReason::Tailnet
                    && !addr.ip().is_loopback() =>
            {
                tracing::warn!("{listener}: not listening on {addr}: {e:#}");
            }
            Err(e) => return Err(e),
        }
    }
    anyhow::ensure!(!started.is_empty(), "{listener}: nothing to listen on");
    Ok(started)
}

/// Say where the listeners are and how tailnet devices reach them (the
/// connect URLs and the `claude mcp add` line), after the ready line.
fn announce_tailnet(
    tailnet: &fsonos_tailscale::TailnetStatus,
    plan: &fsonos_tailscale::BindPlan,
    http: &[(Arc<TcpServer>, SocketAddr)],
    mcp: &[SocketAddr],
) {
    eprintln!("fsonos serve: listening ({})", plan.note);
    // Describe each listener by its tailnet address when it has one.
    let pick = |addrs: Vec<SocketAddr>| {
        addrs
            .iter()
            .copied()
            .find(|a| fsonos_tailscale::is_tailnet_ip(a.ip()))
            .unwrap_or(addrs[0])
    };
    let listeners = [
        fsonos_tailscale::Listener {
            label: "HTTP API",
            bind: pick(http.iter().map(|(_, a)| *a).collect()),
            path: "",
        },
        fsonos_tailscale::Listener {
            label: "MCP server",
            bind: pick(mcp.to_vec()),
            path: "/mcp",
        },
    ];
    for line in fsonos_tailscale::describe(tailnet, &listeners).lines() {
        eprintln!("fsonos serve: {line}");
    }
}

/// The names this host has on its tailnet (MagicDNS name and addresses),
/// which tailnet clients and Tailscale Serve send as the Host. Empty off a
/// tailnet.
fn tailnet_names(status: &fsonos_tailscale::TailnetStatus) -> Vec<String> {
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
    behind_serve: bool,
) -> anyhow::Result<(Arc<TcpServer>, SocketAddr)> {
    let web = WebPolicy::for_listener(addr, names);
    // Behind Tailscale Serve, its login header names the tailnet user.
    let identity = if behind_serve {
        Identity::behind_serve(listener_client(addr))
    } else {
        Identity::fixed(listener_client(addr))
    };
    let app = Arc::new(fsonos_api::app(surface, &identity, &web));
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
                    // One task per connection: an idle keep-alive client or
                    // an open GET /events stream must not hold up the rest.
                    serving
                        .serve_on_app_concurrent(&cx, listener, app)
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
