//! `fsonos serve --bridge-compat <addr>`: the legacy Sonos bridge's HTTP API
//! on a loopback address ([`fsonos_api::http::bridge`], vg3s.9), so a caller
//! of the old bridge moves over by changing nothing and comes under the
//! house policy as the `bridge-compat` client.

use anyhow::Context as _;
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Surface, WebPolicy};
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

/// The most connections it holds open at once (a bridge caller makes one
/// request at a time).
const MAX_CONNECTIONS: usize = 64;

/// Serve the bridge's API on `addr`, when it is set: the listener, ready to
/// be stopped like the HTTP API's ([`super::stop_http`]). A non-loopback
/// address is refused: the API has no authentication.
pub(super) fn start(
    surface: &Arc<Surface>,
    addr: Option<SocketAddr>,
) -> anyhow::Result<Vec<(Arc<TcpServer>, SocketAddr)>> {
    let Some(addr) = addr else {
        return Ok(Vec::new());
    };
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "--bridge-compat binds loopback only ({addr} is not): the bridge's API has no \
         authentication"
    );
    let config = ServerConfig::new(addr.to_string())
        .with_allowed_hosts(WebPolicy::for_listener(addr, &[]).hosts().to_vec())
        .with_max_connections(MAX_CONNECTIONS);
    let server = Arc::new(TcpServer::new(config));
    let (bound_tx, bound_rx) = mpsc::channel();
    let (serving, surface) = (Arc::clone(&server), Arc::clone(surface));
    thread::Builder::new()
        .name("fsonos-bridge-compat".into())
        .spawn(move || {
            let ready_tx = bound_tx.clone();
            let result = super::listener_runtime().and_then(|rt| {
                rt.block_on(async move {
                    let cx = asupersync::Cx::current().context("ambient Cx")?;
                    let listener = asupersync::net::TcpListener::bind(addr)
                        .await
                        .with_context(|| format!("bind the bridge-compatible API on {addr}"))?;
                    let local = listener
                        .local_addr()
                        .context("bridge-compatible API address")?;
                    let app = Arc::new(fsonos_api::http::bridge::app(&surface));
                    let _ = ready_tx.send(Ok(local));
                    serving
                        .serve_on_app_concurrent(&cx, listener, app)
                        .await
                        .map_err(|e| anyhow::anyhow!("bridge-compatible API: {e}"))
                })
            });
            if let Err(e) = result {
                let _ = bound_tx.send(Err(e));
            }
        })
        .context("start the bridge-compatible API thread")?;
    let bound = bound_rx
        .recv_timeout(Duration::from_secs(10))
        .context("the bridge-compatible API did not start within 10 s")??;
    eprintln!("fsonos serve: bridge-compatible API on http://{bound} (as client bridge-compat)");
    Ok(vec![(server, bound)])
}
