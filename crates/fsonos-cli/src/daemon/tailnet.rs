//! Keep automatic HTTP listeners in step with Tailscale after startup.
//!
//! Loopback belongs to the daemon for its whole lifetime. This worker owns
//! only the automatic tailnet listeners: a late login or reconnect adds
//! them, a reported disconnect retires them, and failed binds are retried.
//! A changed name or address set also rebuilds their Host, browser-origin,
//! and own-address identity rules. Configured HTTP and all MCP binds stay
//! outside this worker.

use fastapi::TcpServer;
use fsonos_api::Surface;
use fsonos_api::web::ServeOrigin;
use fsonos_tailscale::{Probe, TailnetStatus};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, ServeArgs, TailscaleMode};

const INTERVAL: Duration = Duration::from_secs(30);
/// A failed CLI probe can fall back to interface addresses, without the
/// MagicDNS name. Keep a working listener through brief misses, but drop
/// stale names after four unanswered looks (about two minutes).
const UNANSWERED_LIMIT: u32 = 4;

pub(super) fn enabled(args: &ServeArgs) -> bool {
    args.http.is_none() && args.tailscale == TailscaleMode::Auto
}

/// A live listener owns its shutdown, including when the worker cannot be
/// spawned or unwinds. Dropping an Arc alone does not stop a serving thread.
struct Http((Arc<TcpServer>, SocketAddr));

impl Drop for Http {
    fn drop(&mut self) {
        tracing::info!(address = %self.0.1, "retiring automatic tailnet HTTP listener");
        super::stop_http(std::slice::from_ref(&self.0));
    }
}

/// The listener owner is generic so lifecycle tests can exercise retries
/// and retirement without a tailnet or a live speaker network.
struct Listeners<T> {
    held: Vec<(SocketAddr, T)>,
    names: Vec<String>,
}

impl<T> Listeners<T> {
    fn new(held: Vec<(SocketAddr, T)>, names: Vec<String>) -> Self {
        Self {
            held,
            names: canonical_names(names),
        }
    }

    /// Drop obsolete listeners before rebinding the same address with new
    /// names. A failed bind is left absent, so the next observation retries
    /// it; an unchanged listener keeps its connections and WhoIs cache.
    fn reconcile(
        &mut self,
        desired: &[SocketAddr],
        names: Vec<String>,
        stopped: impl Fn() -> bool,
        mut bind: impl FnMut(SocketAddr, &[String]) -> anyhow::Result<T>,
    ) {
        let names = canonical_names(names);
        if self.names != names {
            self.held.clear();
            self.names = names;
        }
        self.held.retain(|(addr, _)| desired.contains(addr));
        for &addr in desired {
            if stopped() {
                break;
            }
            if self.held.iter().any(|(bound, _)| *bound == addr) {
                continue;
            }
            match bind(addr, &self.names) {
                Ok(listener) => self.held.push((addr, listener)),
                Err(error) => {
                    tracing::warn!("HTTP API: not listening on {addr}: {error:#}; will retry");
                }
            }
        }
    }
}

fn canonical_names(names: Vec<String>) -> Vec<String> {
    let mut names: Vec<_> = names
        .into_iter()
        .map(|name| name.trim_end_matches('.').to_ascii_lowercase())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Never let automatic discovery widen a listener to a LAN, wildcard, or
/// public address, even if a malformed CLI answer includes one.
fn addresses(status: &TailnetStatus) -> Vec<SocketAddr> {
    let mut desired: Vec<_> = fsonos_tailscale::bind_plan(status, config::HTTP_PORT, None)
        .addrs
        .into_iter()
        .filter(|addr| fsonos_tailscale::is_tailnet_ip(addr.ip()))
        .collect();
    desired.sort_unstable();
    desired.dedup();
    desired
}

/// Authoritative CLI answers take effect immediately. Interface discovery
/// can bring up the first listener even without a CLI; once listening, a
/// brief CLI failure must not discard the host's known MagicDNS identity.
fn accept(status: &TailnetStatus, listening: bool, unanswered: &mut u32) -> bool {
    if status.answered() {
        *unanswered = 0;
        true
    } else {
        *unanswered = unanswered.saturating_add(1);
        !listening || *unanswered >= UNANSWERED_LIMIT
    }
}

/// Follow only the automatic tailnet HTTP listeners until the daemon stops.
/// The default probe bounds each CLI attempt; waiting between probes also
/// checks shutdown so a late-starting Tailscale cannot delay daemon exit.
pub(super) fn watch(
    initial: Vec<(Arc<TcpServer>, SocketAddr)>,
    names: Vec<String>,
    surface: Arc<Surface>,
    cli_token: String,
    behind_serve: bool,
    serve: ServeOrigin,
    stop: Arc<AtomicBool>,
) -> Option<thread::JoinHandle<()>> {
    // Wrap before spawning: a spawn error must stop these listeners too.
    let mut listeners = Listeners::new(
        initial
            .into_iter()
            .map(|listener| (listener.1, Http(listener)))
            .collect(),
        names,
    );
    let spawned = thread::Builder::new()
        .name("fsonos-tailnet-http".into())
        .spawn(move || {
            let probe = Probe::default();
            let mut unanswered = 0;
            while !stop.load(Ordering::Acquire) {
                let status = probe.detect();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                listeners
                    .held
                    .retain(|(_, listener)| !listener.0.0.is_shutting_down());
                if accept(&status, !listeners.held.is_empty(), &mut unanswered) {
                    let desired = addresses(&status);
                    let ours = config::own_tailnet_addrs(&status);
                    listeners.reconcile(
                        &desired,
                        super::tailnet_names(&status),
                        || stop.load(Ordering::Acquire),
                        |addr, names| {
                            if let Some(warning) =
                                config::check_control_bind("HTTP API", addr, false, &ours)?
                            {
                                tracing::warn!("{warning}");
                            }
                            let listener = super::start_http(
                                &surface,
                                addr,
                                names,
                                behind_serve,
                                &cli_token,
                                &serve,
                            )?;
                            eprintln!(
                                "fsonos serve: HTTP API now listening on http://{} (Tailscale)",
                                listener.1
                            );
                            Ok(Http(listener))
                        },
                    );
                } else {
                    tracing::debug!(unanswered, "Tailscale did not answer; HTTP listeners kept");
                }
                let until = Instant::now() + INTERVAL;
                while Instant::now() < until && !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            // `listeners` drops here, shutting every owned listener down.
        });
    match spawned {
        Ok(watching) => Some(watching),
        Err(error) => {
            tracing::warn!("not watching automatic tailnet HTTP listeners: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests;
