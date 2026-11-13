//! Keep host ownership and automatic HTTP listeners in step with Tailscale.
//!
//! Loopback belongs to the daemon for its whole lifetime. This worker owns
//! only the automatic tailnet listeners: a late login or reconnect adds
//! them, a reported disconnect retires them, and failed binds are retried.
//! A changed name or address set also rebuilds their Host, browser-origin,
//! and own-address identity rules. Configured HTTP and all MCP binds stay
//! outside this worker; their shared policy still follows the host's owner.

use fsonos_api::Surface;
use fsonos_api::web::ServeOrigin;
use fsonos_tailscale::{Probe, TailnetStatus};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, ServeArgs, TailscaleMode};

mod owner;

const INTERVAL: Duration = Duration::from_secs(30);
/// A failed CLI probe can fall back to interface addresses, without the
/// MagicDNS name. Keep a working listener through brief misses, but drop
/// stale names after four unanswered looks (about two minutes).
const UNANSWERED_LIMIT: u32 = 4;

pub(super) fn enabled(args: &ServeArgs) -> bool {
    args.tailscale == TailscaleMode::Auto
}

pub(super) fn automatic_http(args: &ServeArgs) -> bool {
    enabled(args) && args.http.is_none()
}

pub(super) fn owner_login(status: &TailnetStatus) -> Option<String> {
    owner::initial(status)
}

/// Owner authority is valid only while its watcher can refresh or expire it.
/// Create this before spawning so a spawn failure also revokes the startup
/// owner; a panic must not leave fixed/Serve listeners with permanent proof.
struct OwnerLease(Arc<Surface>);

impl Drop for OwnerLease {
    fn drop(&mut self) {
        self.0.set_tailnet_owner(None);
    }
}

/// A live listener owns its shutdown, including when the worker cannot be
/// spawned or unwinds. Dropping an Arc alone does not stop a serving thread.
struct Http((super::HttpStop, SocketAddr));

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

/// Follow this host's ownership and, when supplied, its automatic listeners.
/// The default probe bounds each CLI attempt; waiting between probes also
/// checks shutdown so a late-starting Tailscale cannot delay daemon exit.
pub(super) fn watch(
    initial: Option<Vec<(super::HttpStop, SocketAddr)>>,
    status: TailnetStatus,
    surface: Arc<Surface>,
    cli_token: String,
    behind_serve: bool,
    serve: ServeOrigin,
    stop: Arc<AtomicBool>,
) -> Option<thread::JoinHandle<()>> {
    // Wrap before spawning: a spawn error must stop these listeners too.
    let mut listeners = initial.map(|initial| {
        Listeners::new(
            initial
                .into_iter()
                .map(|listener| (listener.1, Http(listener)))
                .collect(),
            super::tailnet_names(&status),
        )
    });
    let mut owner = owner::Owner::new(&status, surface.policy().owner().map(str::to_owned));
    let lease = OwnerLease(Arc::clone(&surface));
    let spawned = thread::Builder::new()
        .name("fsonos-tailnet".into())
        .spawn(move || {
            let _lease = lease;
            let probe = Probe::default();
            let mut unanswered = 0;
            while !stop.load(Ordering::Acquire) {
                let status = probe.detect();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                owner.probe(&status, &probe, |login| {
                    if surface.set_tailnet_owner(login.clone()) {
                        tracing::info!(owner = ?login, "Tailscale host ownership updated");
                    }
                });
                if let Some(listeners) = &mut listeners {
                    listeners.held.retain_mut(|(addr, listener)| {
                        if let Some(error) = super::supervision::listener_failure(
                            &mut listener.0.0.thread,
                            &format!("automatic tailnet HTTP API on {addr}"),
                        ) {
                            tracing::warn!("{error:#}; will retry");
                            false
                        } else {
                            true
                        }
                    });
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
                        tracing::debug!(
                            unanswered,
                            "Tailscale did not answer; HTTP listeners kept"
                        );
                    }
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
            tracing::warn!("not watching Tailscale ownership or automatic HTTP listeners: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests;
