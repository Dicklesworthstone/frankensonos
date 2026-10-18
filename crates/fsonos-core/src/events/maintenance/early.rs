//! Bounded early callbacks are a prefix, not proof of a complete event stream.
//! Losing a tail (including by expiry) requests a new full-state subscription.

use super::{Active, EARLY_AGE, EARLY_BYTES, EARLY_COUNT, Early, Job, Subscriptions, Workers};
use fsonos_proto::gena::Notify;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

fn service_matches(active: &Active, notify: &Notify) -> bool {
    notify.path.strip_prefix('/') == Some(active.want.service.tag())
}

fn candidate(job: &Job, notify: &Notify) -> bool {
    !job.finalized
        && !job.cancelled.load(Ordering::Acquire)
        && notify.path.strip_prefix('/') == Some(job.plan.want.service.tag())
}

/// Local queue loss is not evidence that the speaker is unhealthy. Keep the
/// observed prefix, but request full state even if no later event exposes a
/// sequence gap. A fixed cooldown bounds repeated overflow recovery; an
/// already-earlier lease deadline or an existing failed retry is preserved.
pub(super) fn repair(active: &mut Active, now: Instant) {
    if !active.refresh_pending {
        active.epoch = Arc::new(());
        active.refresh_pending = true;
        active.renew_at = active.renew_at.min(now + super::super::RETRY);
    }
}

impl Workers {
    fn lose_early(&mut self, notify: &Notify) {
        // Before SUBSCRIBE returns we cannot attribute an unknown SID to one
        // player. Mark only outstanding jobs for that callback service. Four
        // booleans bound this evidence; rejected SID strings are never stored.
        for job in &mut self.jobs {
            if candidate(job, notify) {
                job.early_loss = true;
            }
        }
    }

    pub(super) fn prune_early(&mut self, active: &mut [Active], now: Instant) {
        let count = self.early.len();
        for _ in 0..count {
            let Some(early) = self.early.pop_front() else {
                break;
            };
            let expired = now.saturating_duration_since(early.at) >= EARLY_AGE;
            let keep = if let Some(current) = active
                .iter_mut()
                .find(|a| a.sid == early.notify.sid)
            {
                let forward = service_matches(current, &early.notify)
                    && super::super::sequence_gap(current.last_seq, early.notify.seq).is_some();
                if forward && expired {
                    repair(current, now);
                }
                forward && !expired
            } else if self.jobs.iter().any(|job| candidate(job, &early.notify)) {
                if expired {
                    self.lose_early(&early.notify);
                }
                !expired
            } else {
                false
            };
            if keep {
                self.early.push_back(early);
            } else {
                self.early_bytes -= early.bytes;
            }
        }
    }
}

impl Subscriptions {
    /// Preserve a bounded prefix while a matching service's SID is unknown.
    /// Rejected tails leave bounded loss evidence on the outstanding jobs.
    pub(crate) fn hold_early(&mut self, notify: &Notify) -> bool {
        self.maintenance.prune_early(&mut self.active, Instant::now());
        self.invalidate_renewals();
        if self.active.iter().any(|a| a.sid == notify.sid)
            || !self.maintenance.jobs.iter().any(|job| candidate(job, notify))
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
            workers.lose_early(notify);
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
        self.maintenance.prune_early(&mut self.active, Instant::now());
        self.invalidate_renewals();
        let index = self
            .maintenance
            .early
            .iter()
            .position(|early| self.active.iter().any(|a| a.sid == early.notify.sid))?;
        let early = self.maintenance.early.remove(index)?;
        self.maintenance.early_bytes -= early.bytes;
        Some(early.notify)
    }
}

#[cfg(test)]
mod tests;
