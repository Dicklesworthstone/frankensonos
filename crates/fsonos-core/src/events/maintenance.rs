//! Scheduled lease I/O, separate from the live subscription table.
//! A worker never owns the model's sequence cursor. Only the caller adopts a
//! result, then authorizes cleanup; the occupied slot survives slow cleanup.

use super::{Active, Report, Service, Subscriber, Subscriptions, Want};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::{Notify, Subscription};
use fsonos_types::PlayerId;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_WORKERS: usize = 4;
const EARLY_COUNT: usize = 128;
const EARLY_BYTES: usize = 2 * 1024 * 1024;
const EARLY_AGE: Duration = Duration::from_secs(30);

type Provider = Arc<dyn Subscriber + Send + Sync>;
type Lease = (Want, String);

#[derive(Clone)]
enum Baseline {
    Active(Active),
    Pending { due: Instant, epoch: Arc<()> },
}

#[derive(Clone)]
struct Plan {
    want: Want,
    baseline: Baseline,
    due: Instant,
}

impl Plan {
    fn current(&self, subs: &Subscriptions) -> bool {
        match &self.baseline {
            Baseline::Active(before) => subs.active.iter().any(|now| {
                Arc::ptr_eq(&now.epoch, &before.epoch)
                    && now.want == self.want
                    && now.sid == before.sid
                    && now.renew_at == before.renew_at
                    && now.refresh_pending == before.refresh_pending
                    // A queued initial event makes an unanswered-stream
                    // replacement unnecessary, but not an ordinary renewal.
                    && (before.refresh_pending
                        || before.last_seq.is_some()
                        || before.initial_deadline > Instant::now()
                        || now.last_seq.is_none())
            }),
            Baseline::Pending { due, epoch } => {
                Arc::ptr_eq(epoch, &subs.pending_epoch)
                    && subs
                        .pending
                        .iter()
                        .any(|(want, at)| want == &self.want && at == due)
            }
        }
    }

    fn old_sid(&self) -> Option<&str> {
        match &self.baseline {
            Baseline::Active(active) => Some(&active.sid),
            Baseline::Pending { .. } => None,
        }
    }

    fn read(&self, subscriber: &dyn Subscriber, callback: &str) -> Completion {
        let started = Instant::now();
        let mut scratch = Subscriptions::default();
        match &self.baseline {
            Baseline::Active(before) => scratch.active.push(before.clone()),
            Baseline::Pending { due, .. } => scratch.pending.push((self.want.clone(), *due)),
        }
        // The existing engine remains the source of renewal, replacement,
        // unanswered-initial and retry behavior. Its table is worker-local.
        let report = scratch.renew_due(subscriber, |_| callback.to_string(), started);
        Completion {
            next: scratch.active.pop(),
            retry: scratch.pending.pop(),
            report,
            started,
            completed: Instant::now(),
        }
    }

    fn failed(&self, detail: &'static str) -> Completion {
        let now = Instant::now();
        let (next, retry) = match &self.baseline {
            Baseline::Active(before) => {
                let mut next = before.clone();
                next.refresh_pending = true;
                next.renew_at = now + super::RETRY;
                (Some(next), None)
            }
            Baseline::Pending { .. } => (None, Some((self.want.clone(), now + super::RETRY))),
        };
        Completion {
            next,
            retry,
            report: Report {
                failed: vec![(self.want.player.clone(), self.want.service, detail.into())],
                ..Report::default()
            },
            started: now,
            completed: now,
        }
    }
}

struct Completion {
    next: Option<Active>,
    retry: Option<(Want, Instant)>,
    report: Report,
    started: Instant,
    completed: Instant,
}

impl Completion {
    fn orphan(&self, plan: &Plan) -> Option<Lease> {
        self.next.as_ref().and_then(|next| {
            (Some(next.sid.as_str()) != plan.old_sid())
                .then(|| (plan.want.clone(), next.sid.clone()))
        })
    }
}

struct Job {
    plan: Plan,
    cancelled: Arc<AtomicBool>,
    output: Mutex<Receiver<Completion>>,
    cleanup: SyncSender<Option<Lease>>,
    finalized: bool,
    thread: JoinHandle<()>,
}

struct Early {
    notify: Notify,
    at: Instant,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Workers {
    jobs: Vec<Job>,
    early: VecDeque<Early>,
    early_bytes: usize,
}

impl std::fmt::Debug for Workers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseWorkers")
            .field("jobs", &self.jobs.len())
            .field("early_events", &self.early.len())
            .field("early_bytes", &self.early_bytes)
            .finish()
    }
}

impl Workers {
    pub(super) fn cancel_all(&self) {
        for job in &self.jobs {
            job.cancelled.store(true, Ordering::Release);
        }
    }

    fn busy(&self, player: &PlayerId) -> bool {
        self.jobs.iter().any(|job| job.plan.want.player == *player)
    }

    fn invalidate(&self, subs: &Subscriptions) {
        for job in &self.jobs {
            if !job.finalized && !job.plan.current(subs) {
                job.cancelled.store(true, Ordering::Release);
            }
        }
    }

    fn reap(&mut self, subs: &mut Subscriptions, stopping: bool) -> Vec<(Report, Instant)> {
        self.invalidate(subs);
        if stopping {
            self.cancel_all();
        }
        let mut reports = Vec::new();
        let count = self.jobs.len();
        for _ in 0..count {
            let mut job = self.jobs.remove(0);
            // Observe completion before taking output, so final output cannot
            // be lost by a worker finishing between try_recv and removal.
            let finished = job.thread.is_finished();
            if !job.finalized {
                let result = match job
                    .output
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .try_recv()
                {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => {
                        Some(job.plan.failed("lease worker stopped"))
                    }
                    Err(TryRecvError::Empty) => None,
                };
                if let Some(result) = result {
                    let valid = !stopping && !job.cancelled.load(Ordering::Acquire);
                    let (report, cleanup) = adopt(subs, &job.plan, result, valid);
                    if let Some(report) = report {
                        reports.push(report);
                    }
                    let _ = job.cleanup.try_send(cleanup);
                    job.finalized = true;
                }
            }
            if finished && job.finalized {
                if job.thread.join().is_err() {
                    tracing::warn!("lease worker panicked; retrying without blocking event intake");
                }
            } else {
                self.jobs.push(job);
            }
        }
        reports
    }

    fn start(
        &mut self,
        subs: &mut Subscriptions,
        provider: &Provider,
        callback: &impl Fn(Service) -> String,
        stop: &Arc<AtomicBool>,
        now: Instant,
    ) {
        let mut plans: Vec<_> = subs
            .active
            .iter()
            .filter(|a| a.next_due() <= now)
            .map(|a| Plan {
                want: a.want.clone(),
                baseline: Baseline::Active(a.clone()),
                due: a.next_due(),
            })
            .chain(
                subs.pending
                    .iter()
                    .filter(|(_, at)| *at <= now)
                    .map(|(want, at)| Plan {
                        want: want.clone(),
                        baseline: Baseline::Pending {
                            due: *at,
                            epoch: Arc::clone(&subs.pending_epoch),
                        },
                        due: *at,
                    }),
            )
            .collect();
        plans.sort_by(|a, b| {
            a.due
                .cmp(&b.due)
                .then_with(|| a.want.player.0.cmp(&b.want.player.0))
        });
        for plan in plans {
            if self.jobs.len() >= MAX_WORKERS || stop.load(Ordering::Acquire) {
                break;
            }
            if self.busy(&plan.want.player) {
                continue;
            }
            self.spawn(subs, plan, provider, callback, stop);
        }
    }

    fn spawn(
        &mut self,
        subs: &mut Subscriptions,
        plan: Plan,
        provider: &Provider,
        callback: &impl Fn(Service) -> String,
        stop: &Arc<AtomicBool>,
    ) {
        let callback = callback(plan.want.service);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(&cancelled);
        let stopping = Arc::clone(stop);
        let subscriber = Arc::clone(provider);
        let input = plan.clone();
        let (send, output) = sync_channel(1);
        let (cleanup, receive) = sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("fsonos-lease".into())
            .spawn(move || {
                let via = Cancellable {
                    subscriber: &*subscriber,
                    stop: &stopping,
                    cancelled: &cancellation,
                };
                let result = input.read(&via, &callback);
                let orphan = result.orphan(&input);
                let cleanup = if send.send(result).is_err() {
                    orphan
                } else {
                    loop {
                        match receive.recv_timeout(Duration::from_millis(100)) {
                            Ok(cleanup) => break cleanup,
                            Err(RecvTimeoutError::Disconnected) => break orphan,
                            Err(RecvTimeoutError::Timeout) if via.stopping() => break orphan,
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    }
                };
                if let Some((want, sid)) = cleanup {
                    // Best effort after adoption, or cleanup of a new SID
                    // never adopted. Never hold up event delivery for this.
                    let _ = subscriber.unsubscribe(&want, &sid);
                }
            });
        match spawned {
            Ok(thread) => self.jobs.push(Job {
                plan,
                cancelled,
                output: Mutex::new(output),
                cleanup,
                finalized: false,
                thread,
            }),
            Err(error) => {
                tracing::warn!(%error, "cannot start lease maintenance worker");
                // A local spawn failure is not evidence of speaker health.
                let _ = adopt(subs, &plan, plan.failed("cannot start lease worker"), true);
            }
        }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.cancel_all();
        // Dropping the cleanup senders tells workers to retire unadopted SIDs.
        // No join of an unfinished network call on the model thread.
    }
}

/// Apply only to the original lease/request, preserving a live SID's latest
/// sequence and initial-state evidence. Return cleanup for the worker to send.
fn adopt(
    subs: &mut Subscriptions,
    plan: &Plan,
    mut result: Completion,
    valid: bool,
) -> (Option<(Report, Instant)>, Option<Lease>) {
    let orphan = result.orphan(plan);
    if !valid || !plan.current(subs) {
        let cleanup = orphan.filter(|(_, sid)| !subs.active.iter().any(|a| a.sid == *sid));
        return (None, cleanup);
    }
    let old = match &plan.baseline {
        Baseline::Active(before) => subs
            .active
            .iter()
            .position(|a| Arc::ptr_eq(&a.epoch, &before.epoch)),
        Baseline::Pending { .. } => None,
    };
    if let Some(next) = &mut result.next {
        let fresh = match &plan.baseline {
            Baseline::Active(before) => !Arc::ptr_eq(&before.epoch, &next.epoch),
            Baseline::Pending { .. } => true,
        };
        next.renew_at = result.completed + next.renew_at.saturating_duration_since(result.started);
        if fresh {
            next.initial_deadline = result.completed
                + next
                    .initial_deadline
                    .saturating_duration_since(result.started);
        }
        if let Some(index) = old {
            let current = &subs.active[index];
            if next.sid == current.sid && (result.report.renewed > 0 || !fresh) {
                next.last_seq = current.last_seq;
                next.initial_deadline = current.initial_deadline;
                next.initial_misses = current.initial_misses;
            }
        }
    }
    let cleanup = match (plan.old_sid(), result.next.as_ref()) {
        (Some(old_sid), Some(next)) if old_sid != next.sid => {
            Some((plan.want.clone(), old_sid.to_string()))
        }
        _ => None,
    };
    if let Some(index) = old {
        if let Some(next) = result.next {
            subs.active[index] = next;
        }
    } else {
        subs.pending.retain(|(want, _)| want != &plan.want);
        if let Some(next) = result.next {
            subs.active.push(next);
        }
        if let Some((want, at)) = result.retry {
            subs.pending.push((
                want,
                result.completed + at.saturating_duration_since(result.started),
            ));
        }
    }
    (Some((result.report, result.started)), cleanup)
}

struct Cancellable<'a> {
    subscriber: &'a dyn Subscriber,
    stop: &'a AtomicBool,
    cancelled: &'a AtomicBool,
}

impl Cancellable<'_> {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire)
    }
}

impl Subscriber for Cancellable<'_> {
    fn subscribe(&self, want: &Want, callback: &str) -> Result<Subscription, ProtoError> {
        if self.stopping() {
            return Err(ProtoError::NotWired("lease request cancelled"));
        }
        self.subscriber.subscribe(want, callback)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        if self.stopping() {
            return Err(ProtoError::NotWired("lease renewal cancelled"));
        }
        self.subscriber.renew(want, sid)
    }

    fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
        // The model authorizes retirement only after it adopts the new SID.
        // The worker performs that one cleanup call after returning the result.
        Ok(())
    }
}

impl Subscriptions {
    /// A malformed event or a sequence gap requests a fresh full-state
    /// stream, but must not make the live event handler do network I/O.
    /// Coalesce repeats without resetting a failed request's backoff.
    pub(super) fn defer_refresh(&mut self, sid: &str, now: Instant) -> bool {
        if !self.deferred_refresh {
            return false;
        }
        if let Some(active) = self.active.iter_mut().find(|a| a.sid == sid)
            && !active.refresh_pending
        {
            active.epoch = Arc::new(());
            active.refresh_pending = true;
            active.renew_at = now;
        }
        self.invalidate_renewals();
        true
    }

    pub(crate) fn renew_background(
        &mut self,
        provider: &Provider,
        callback: impl Fn(Service) -> String,
        stop: &Arc<AtomicBool>,
        now: Instant,
    ) -> Vec<(Report, Instant)> {
        self.deferred_refresh = true;
        let mut workers = std::mem::take(&mut self.maintenance);
        let reports = workers.reap(self, stop.load(Ordering::Acquire));
        workers.start(self, provider, &callback, stop, now);
        self.maintenance = workers;
        reports
    }

    pub(crate) fn poll_renewals(&mut self, stopping: bool) -> Vec<(Report, Instant)> {
        self.deferred_refresh = true;
        let mut workers = std::mem::take(&mut self.maintenance);
        let reports = workers.reap(self, stopping);
        self.maintenance = workers;
        reports
    }

    pub(crate) fn invalidate_renewals(&self) {
        self.maintenance.invalidate(self);
    }

    /// A new subscription can send its initial event before SUBSCRIBE returns.
    /// Retain a bounded prefix until the corresponding SID is adopted. Known
    /// duplicate/replayed SIDs never enter this queue.
    pub(crate) fn hold_early(&mut self, notify: &Notify) -> bool {
        if self.active.iter().any(|a| a.sid == notify.sid)
            || !self.maintenance.jobs.iter().any(|job| !job.finalized)
        {
            return false;
        }
        let bytes = notify.properties.iter().fold(
            notify.sid.len() + notify.path.len(),
            |size, (key, value)| size.saturating_add(key.len()).saturating_add(value.len()),
        );
        let workers = &mut self.maintenance;
        if workers.early.len() >= EARLY_COUNT
            || bytes > EARLY_BYTES.saturating_sub(workers.early_bytes)
        {
            return false;
        }
        workers.early.push_back(Early {
            notify: notify.clone(),
            at: Instant::now(),
            bytes,
        });
        workers.early_bytes += bytes;
        true
    }

    pub(crate) fn take_early(&mut self) -> Option<Notify> {
        let now = Instant::now();
        let waiting = self.maintenance.jobs.iter().any(|job| !job.finalized);
        let active = &self.active;
        let workers = &mut self.maintenance;
        let mut removed = 0;
        workers.early.retain(|early| {
            let known = active.iter().find(|a| a.sid == early.notify.sid);
            let retain = now.saturating_duration_since(early.at) < EARLY_AGE
                && known.map_or(waiting, |a| {
                    super::sequence_gap(a.last_seq, early.notify.seq).is_some()
                });
            if !retain {
                removed += early.bytes;
            }
            retain
        });
        workers.early_bytes -= removed;
        let index = workers
            .early
            .iter()
            .position(|early| active.iter().any(|a| a.sid == early.notify.sid))?;
        let early = workers.early.remove(index)?;
        workers.early_bytes -= early.bytes;
        Some(early.notify)
    }
}

#[cfg(test)]
mod tests;
