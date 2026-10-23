//! One background inventory survey, independent of playback read capacity.
//! It owns hints and a result, never the live model or GENA subscriptions.

use super::{Engine, Model, bootstrap, progress};
use crate::CoreError;
use crate::events::{Service, wanted};
use crate::inventory::Survey;
use crate::reconcile::InventoryPlan;
use fsonos_proto::{Discovered, ProtoError, Transport};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Pending {
    plan: InventoryPlan,
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<Result<Survey, CoreError>>,
    /// When it began reading: what it finds is the house as of then.
    started: Instant,
}

#[derive(Default)]
pub(super) struct Surveys {
    pending: Option<Pending>,
    again: bool,
    listener_after: Option<Instant>,
    listener_error: Option<String>,
}

impl Surveys {
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// An explicit refresh during a read becomes one follow-up, not an
    /// overlapping survey and not a request lost when success resets backoff.
    pub(super) fn refresh_soon(&mut self) {
        self.again |= self.busy();
        self.listener_after = None;
    }

    pub(super) fn start(&mut self, engine: &Engine, model: &mut Model) {
        if self.busy() || engine.stop.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        self.maintain_listener(engine, model);
        bootstrap::observed(&mut model.remembered, &model.rec.households);
        let seeds = bootstrap::seeds(&engine.config.seeds, &model.remembered);
        let plan = model.rec.inventory_plan(&seeds);
        let input = plan.clone();
        let transport = Arc::clone(&engine.transport);
        let stop = Arc::clone(&engine.stop);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(&cancelled);
        let spawned = std::thread::Builder::new()
            .name("fsonos-inventory-read".into())
            .spawn(move || {
                input.read(&Cancellable {
                    transport: &*transport,
                    stop: &stop,
                    cancelled: &cancellation,
                })
            });
        match spawned {
            Ok(thread) => {
                self.again = false;
                self.pending = Some(Pending {
                    plan,
                    cancelled,
                    thread,
                    started,
                });
            }
            Err(error) => {
                let error = CoreError::Proto(ProtoError::Network {
                    target: "inventory worker".into(),
                    detail: error.to_string(),
                });
                if let Err(error) = model
                    .rec
                    .finish_inventory(&plan, Err(error), Instant::now())
                {
                    failed(model, self.listener_error.clone(), &error);
                }
                engine.publish(model, Vec::new());
            }
        }
    }

    pub(super) fn complete(&mut self, engine: &Engine, model: &mut Model) {
        // Local callback repair must not wait for a stuck discovery call.
        // After sleep, a retained listener may also need all its leases back.
        self.maintain_listener(engine, model);
        let Some(pending) = &self.pending else {
            return;
        };
        if engine.stop.load(Ordering::Acquire) || !pending.plan.current(&model.rec) {
            pending.cancelled.store(true, Ordering::Release);
        }
        if !pending.thread.is_finished() {
            return;
        }
        // The normal pump is bounded. Never let a completed survey overtake
        // another already-buffered notification, including a newer topology.
        if !engine.stop.load(Ordering::Acquire)
            && let Some(events) = engine.next_event(model, Duration::ZERO)
        {
            progress::invalidate(model);
            engine.publish(model, events);
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        let result = pending
            .thread
            .join()
            .unwrap_or(Err(CoreError::Proto(ProtoError::NotWired(
                "inventory worker panicked",
            ))));
        if engine.stop.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if pending.cancelled.load(Ordering::Acquire) {
            model.rec.obsolete_inventory(now);
        } else {
            match model.rec.finish_inventory(&pending.plan, result, now) {
                Ok(Some(report)) => {
                    progress::invalidate_rebooted(model, &report.rebooted);
                    for player in &report.missing {
                        model.playback.remove(player);
                    }
                    bootstrap::observed(&mut model.remembered, &model.rec.households);
                    // Fresh discovery may supply a newly usable local route.
                    self.listener_after = None;
                    self.maintain_listener(engine, model);
                    sync_known(engine, model);
                    if let Some(surveyed) = &engine.config.surveyed {
                        surveyed(&model.rec.households);
                    }
                    progress::request_survey(model);
                    let completed = Instant::now();
                    // Stamped with its start, as the inline survey is: a
                    // command that asked for a fresh look while this one
                    // read must wait for the follow-up (api live::resurvey).
                    model.surveyed_at = Some(pending.started);
                    model.last_error = self.listener_error.clone();
                    model.rec.schedule.succeeded(completed);
                    if model.sink.is_none() {
                        model
                            .rec
                            .schedule
                            .retry_within(completed, super::EVENTLESS_RETRY);
                    }
                    engine.publish(model, Vec::new());
                }
                Ok(None) => {}
                Err(error) => {
                    failed(model, self.listener_error.clone(), &error);
                    engine.publish(model, Vec::new());
                }
            }
        }
        if std::mem::take(&mut self.again) {
            model.rec.schedule.due_now(Instant::now());
        }
    }

    /// Reopen a failed listener, or replace the leases explicitly released
    /// after sleep, independently of inventory I/O. Failed local binds keep
    /// a fixed cooldown; only an explicit refresh or new inventory retries
    /// sooner. Existing subscriptions retain the normal renewal engine.
    fn maintain_listener(&mut self, engine: &Engine, model: &mut Model) {
        if engine.stop.load(Ordering::Acquire)
            || (model.surveyed_at.is_none()
                && model.rec.households.is_empty()
                && model.remembered.is_empty()
                && engine.config.seeds.is_empty())
        {
            return;
        }
        if model.sink.is_some() && !model.rec.subscriptions.is_empty() {
            self.listener_after = None;
            return;
        }
        let now = Instant::now();
        if self.listener_after.is_some_and(|due| now < due) {
            return;
        }
        let opening = model.sink.is_none();
        if let Some(error) = open_known_listener(engine, model) {
            if self.listener_error.as_ref() != Some(&error) || model.last_error.is_none() {
                model.last_error = Some(error.clone());
            }
            self.listener_error = Some(error);
            self.listener_after = Some(Instant::now() + super::EVENTLESS_RETRY);
            engine.publish(model, Vec::new());
            return;
        }
        self.clear_listener_error(model);
        let before = model.rec.subscriptions.len();
        sync_known(engine, model);
        self.listener_after = Some(Instant::now() + super::EVENTLESS_RETRY);
        if opening || model.rec.subscriptions.len() > before {
            // Initial topology notifications can invalidate the old survey;
            // do not lose the recovery request to its obsolete-read cooldown.
            self.again |= self.busy();
            model.rec.schedule.due_now(Instant::now());
        }
        progress::invalidate(model);
        engine.publish(model, Vec::new());
    }

    fn clear_listener_error(&mut self, model: &mut Model) {
        if let Some(previous) = self.listener_error.take()
            && let Some(current) = model.last_error.take()
        {
            // Clearing a repaired listener must not hide a separate inventory
            // failure which was recorded while callbacks were unavailable.
            model.last_error = if current == previous {
                None
            } else {
                Some(
                    current
                        .strip_prefix(&format!("{previous}; "))
                        .unwrap_or(&current)
                        .to_string(),
                )
            };
        }
    }
}

fn sync_known(engine: &Engine, model: &mut Model) {
    if engine.stop.load(Ordering::Acquire) {
        return;
    }
    if let Some(sink) = &model.sink {
        let callback = |service: Service| sink.callback_url(service.tag());
        let report = model.rec.subscriptions.sync(
            &engine.via(),
            &wanted(&model.rec.households),
            callback,
            Instant::now(),
        );
        model.rec.health.record(&report, Instant::now());
    }
}

impl Drop for Surveys {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.cancelled.store(true, Ordering::Release);
        }
        // Never join unfinished I/O. One retained slot prevents replacement
        // workers accumulating; dropping the controller cancels further calls.
    }
}

fn open_known_listener(engine: &Engine, model: &mut Model) -> Option<String> {
    if model.sink.is_some() || engine.stop.load(Ordering::Acquire) {
        return None;
    }
    // Unlike bootstrap, a running model must not perform multicast discovery
    // just to choose a callback address. A completed survey supplies new hints.
    if bootstrap::listener_candidates(&model.rec.households, &engine.config).is_empty() {
        return Some("no known player address for the event listener".into());
    }
    match engine.open_sink(&model.rec.households) {
        Ok(sink) => {
            model.sink = Some(sink);
            None
        }
        Err(error) => Some(error),
    }
}

fn failed(model: &mut Model, listener_error: Option<String>, error: &CoreError) {
    model.last_error = Some(match listener_error.filter(|_| model.sink.is_none()) {
        Some(listener) => format!("{listener}; survey failed: {error}"),
        None => error.to_string(),
    });
}

/// Cancellation between every network operation, including remembered-address
/// fallback. A call already in progress retains the transport's own timeout.
struct Cancellable<'a> {
    transport: &'a (dyn Transport + Send + Sync),
    stop: &'a AtomicBool,
    cancelled: &'a AtomicBool,
}

impl Cancellable<'_> {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire)
    }

    fn read<T>(&self, call: impl FnOnce() -> Result<T, ProtoError>) -> Result<T, ProtoError> {
        if self.stopping() {
            return Err(ProtoError::NotWired("inventory read cancelled"));
        }
        let result = call()?;
        if self.stopping() {
            return Err(ProtoError::NotWired("inventory read cancelled"));
        }
        Ok(result)
    }
}

impl Transport for Cancellable<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.read(|| self.transport.soap_post(host, path, action, body))
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.read(|| self.transport.http_get(url))
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        if !self.stopping() {
            let found = self.transport.discover(mx_secs, wait);
            if !self.stopping() {
                return found;
            }
        }
        Discovered {
            ssdp: Err(ProtoError::NotWired("inventory discovery cancelled")),
            mdns: Err(ProtoError::NotWired("inventory discovery cancelled")),
        }
    }
}

#[cfg(test)]
mod tests;
