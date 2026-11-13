//! One callback address per local route, with independent listener lifetimes.
//! The speakers never receive a callback on an unrelated interface. Retired
//! listeners stay open briefly because a clip may still be fetched through one.

use super::{EVENTLESS_RETRY, route_settled};
use crate::events::{Subscriber, Want};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::Subscription;
use fsonos_proto::net::{CallbackEvent, EventSink, Lan};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bound sockets and their inboxes, including old routes serving an admitted
/// clip. Sixteen simultaneous local interfaces is well beyond a normal house.
const MAX_LISTENERS: usize = 16;
/// Uploaded clips are bounded to five minutes. Retain their old URL through
/// that window plus the usual completion grace after a route changes.
const MEDIA_RETIRE: Duration = Duration::from_secs(360);
const RECEIVE_SLICE: Duration = Duration::from_millis(5);

struct Listening {
    sink: EventSink,
    retired_at: Option<Instant>,
}

#[derive(Default)]
struct Route {
    local: Option<IpAddr>,
    moving: Option<(IpAddr, Instant)>,
    resolved: bool,
}

/// The live loop alone mutates these routes. Workers receive an immutable
/// callback snapshot; their existing subscription epochs fence later changes.
#[derive(Default)]
pub(super) struct Listeners {
    listening: Vec<Listening>,
    routes: HashMap<IpAddr, Route>,
    failed: HashMap<IpAddr, (Instant, String)>,
    unresolved: HashMap<IpAddr, String>,
    next: Cell<usize>,
}

#[derive(Debug)]
pub(super) struct Closed {
    pub local: IpAddr,
    pub error: ProtoError,
}

impl Listeners {
    /// Fixtures which already subscribed their players must retain those exact
    /// callback routes instead of treating their next initial event as a move.
    #[cfg(test)]
    pub(in crate::live) fn for_hosts(
        sink: EventSink,
        hosts: impl IntoIterator<Item = IpAddr>,
    ) -> Self {
        let local = sink.local_addr().ip();
        Self {
            routes: hosts
                .into_iter()
                .map(|host| {
                    (
                        host,
                        Route {
                            local: Some(local),
                            moving: None,
                            resolved: true,
                        },
                    )
                })
                .collect(),
            listening: vec![Listening {
                sink,
                retired_at: None,
            }],
            ..Self::default()
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.listening.is_empty()
    }

    /// Compatibility address for diagnostics and single-interface callers.
    /// Per-player work must use `base_for`, never this arbitrary primary.
    pub(super) fn local_addr(&self) -> SocketAddr {
        self.listening
            .iter()
            .find(|entry| entry.retired_at.is_none())
            .unwrap_or(&self.listening[0])
            .sink
            .local_addr()
    }

    pub(super) fn callback_url(&self, tag: &str) -> String {
        format!(
            "http://{}/{}",
            self.local_addr(),
            tag.trim_start_matches('/')
        )
    }

    pub(super) fn base_for(&self, host: IpAddr) -> Option<String> {
        let route = self.routes.get(&host)?;
        if !route.resolved {
            return None;
        }
        let local = route.local?;
        self.listening
            .iter()
            .find(|entry| entry.sink.local_addr().ip() == local)
            .map(|entry| {
                entry
                    .sink
                    .callback_url("")
                    .trim_end_matches('/')
                    .to_string()
            })
    }

    pub(super) fn bases(&self) -> HashMap<IpAddr, String> {
        self.routes
            .keys()
            .filter_map(|host| self.base_for(*host).map(|base| (*host, base)))
            .collect()
    }

    pub(super) fn diagnostic(&self) -> Option<String> {
        let mut errors: Vec<_> = self
            .failed
            .values()
            .map(|(_, error)| error.as_str())
            .chain(self.unresolved.values().map(String::as_str))
            .collect();
        errors.sort_unstable();
        errors.dedup();
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    /// Resolve every route without network I/O, and bind each distinct local
    /// address once. A failed interface does not close or rebind a healthy one.
    /// `force` retries failed binds after explicit refresh/new inventory; normal
    /// route checks keep their cooldown. Existing working routes settle first.
    pub(super) fn reconcile(
        &mut self,
        hosts: &[IpAddr],
        now: Instant,
        force: bool,
        local: impl FnMut(IpAddr) -> Result<IpAddr, String>,
        open: impl FnMut(IpAddr) -> Result<EventSink, String>,
    ) -> Vec<IpAddr> {
        let before = self.bases();
        self.observe_routes(hosts, now, local);
        self.maintain_listeners(now, force, open);
        let after = self.bases();
        let mut changed: Vec<_> = before
            .keys()
            .chain(after.keys())
            .copied()
            .filter(|host| before.get(host) != after.get(host))
            .collect();
        changed.sort_unstable();
        changed.dedup();
        changed
    }

    fn observe_routes(
        &mut self,
        hosts: &[IpAddr],
        now: Instant,
        mut local: impl FnMut(IpAddr) -> Result<IpAddr, String>,
    ) {
        self.routes.retain(|host, _| hosts.contains(host));
        self.unresolved.clear();
        for &host in hosts {
            let observed = match local(host) {
                Ok(local) => local,
                Err(error) => {
                    self.unresolved.insert(host, error);
                    let route = self.routes.entry(host).or_default();
                    route.resolved = false;
                    route.moving = None;
                    continue;
                }
            };
            let route = self.routes.entry(host).or_default();
            let working = route.resolved
                && route.local.is_some_and(|address| {
                    self.listening
                        .iter()
                        .any(|entry| entry.sink.local_addr().ip() == address)
                });
            route.resolved = true;
            match route.local {
                Some(current) if working && current != observed => {
                    if route_settled(current, observed, &mut route.moving, now) {
                        route.local = Some(observed);
                    }
                }
                _ => {
                    route.local = Some(observed);
                    route.moving = None;
                }
            }
        }
    }

    fn maintain_listeners(
        &mut self,
        now: Instant,
        force: bool,
        mut open: impl FnMut(IpAddr) -> Result<EventSink, String>,
    ) {
        let needed: HashSet<_> = self
            .routes
            .values()
            .filter(|route| route.resolved)
            .filter_map(|route| route.local)
            .collect();
        for entry in &mut self.listening {
            if needed.contains(&entry.sink.local_addr().ip()) {
                entry.retired_at = None;
            } else {
                entry.retired_at.get_or_insert(now);
            }
        }
        self.listening.retain(|entry| {
            entry
                .retired_at
                .is_none_or(|retired| now.saturating_duration_since(retired) < MEDIA_RETIRE)
        });
        self.failed.retain(|address, _| needed.contains(address));
        let mut needed: Vec<_> = needed.into_iter().collect();
        needed.sort_unstable();
        for address in needed {
            if self
                .listening
                .iter()
                .any(|entry| entry.sink.local_addr().ip() == address)
            {
                self.failed.remove(&address);
                continue;
            }
            if !force
                && self
                    .failed
                    .get(&address)
                    .is_some_and(|(after, _)| now < *after)
            {
                continue;
            }
            let opened = if self.listening.len() < MAX_LISTENERS {
                open(address)
            } else {
                Err(format!(
                    "cannot listen for events on {address}: callback listener limit reached"
                ))
            };
            match opened {
                Ok(sink) => {
                    self.listening.push(Listening {
                        sink,
                        retired_at: None,
                    });
                    self.failed.remove(&address);
                }
                Err(error) => {
                    self.failed.insert(address, (now + EVENTLESS_RETRY, error));
                }
            }
        }
    }

    /// Only this listener failed. Keep every other socket and callback URL.
    pub(super) fn closed(&mut self, closed: &Closed, now: Instant) -> Vec<IpAddr> {
        let affected = self
            .routes
            .iter()
            .filter_map(|(host, route)| (route.local == Some(closed.local)).then_some(*host))
            .collect();
        self.listening
            .retain(|entry| entry.sink.local_addr().ip() != closed.local);
        self.failed.insert(
            closed.local,
            (now + EVENTLESS_RETRY, closed.error.to_string()),
        );
        affected
    }

    /// A hot interface cannot starve the others. Waiting is shared across all
    /// listeners, not repeated per socket; one-interface behavior still blocks
    /// on its receiver directly. Closed listeners identify only their own route.
    pub(super) fn recv_event(&self, wait: Duration) -> Result<Option<CallbackEvent>, Closed> {
        if self.listening.len() == 1 {
            let sink = &self.listening[0].sink;
            return sink.recv_event(wait).map_err(|error| Closed {
                local: sink.local_addr().ip(),
                error,
            });
        }
        let until = Instant::now() + wait;
        loop {
            let count = self.listening.len();
            for offset in 0..count {
                let index = (self.next.get() + offset) % count;
                let sink = &self.listening[index].sink;
                let received = sink.recv_event(Duration::ZERO).map_err(|error| Closed {
                    local: sink.local_addr().ip(),
                    error,
                })?;
                if received.is_some() {
                    self.next.set((index + 1) % count);
                    return Ok(received);
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(RECEIVE_SLICE.min(left));
        }
    }
}

/// The subscription engine retains its existing service callback API. This
/// edge adapter resolves its actual receiver using the addressed player.
pub(super) struct Routed {
    pub lan: Arc<Lan>,
    pub bases: HashMap<IpAddr, String>,
}

impl Subscriber for Routed {
    fn subscribe(&self, want: &Want, _: &str) -> Result<Subscription, ProtoError> {
        let base = self
            .bases
            .get(&want.host)
            .ok_or_else(|| ProtoError::Network {
                target: format!("callback route toward {}", want.host),
                detail: "no listener is available on this player's local route".into(),
            })?;
        self.lan.subscribe(
            want.host,
            want.service.event_path(),
            &format!("{base}/{}", want.service.tag()),
            crate::events::TIMEOUT_SECS,
        )
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        Subscriber::renew(&*self.lan, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        Subscriber::unsubscribe(&*self.lan, want, sid)
    }
}

#[cfg(test)]
mod tests;
