//! Bounded sleep workers with rollback only before entering core execution.

use super::{Ran, Scheduler, Sleep};
use chrono::{DateTime, Utc};
use fsonos_core::sleep::{SleepOutcome, SleepRun};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const MAX_FADES: usize = 8;
type Work = Box<dyn FnOnce() + Send>;

/// The closure owns both resources, including if spawning fails and drops it.
/// Dropping after core execution is harmless: that ticket cannot be released.
struct Pending {
    sleep: Arc<Sleep>,
    ticket: SleepRun,
}

impl Pending {
    fn reserve(sleep: &Arc<Sleep>, ticket: SleepRun) -> Option<Self> {
        if sleep
            .fades_in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_FADES).then_some(count + 1)
            })
            .is_err()
        {
            sleep.timers.release_run(&ticket);
            return None;
        }
        Some(Self {
            sleep: Arc::clone(sleep),
            ticket,
        })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.sleep.timers.release_run(&self.ticket);
        self.sleep.fades_in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn start(scheduler: &Scheduler, now: DateTime<Utc>) -> Vec<Ran> {
    start_with(scheduler, now, |work| scheduler.spawn("fsonos-sleep", work))
}

fn start_with(
    scheduler: &Scheduler,
    now: DateTime<Utc>,
    mut spawn: impl FnMut(Work) -> bool,
) -> Vec<Ran> {
    if scheduler.dispatch.stopped() {
        return Vec::new();
    }
    let Some(sleep) = &scheduler.surface.sleep else {
        return Vec::new();
    };
    let free = MAX_FADES.saturating_sub(sleep.fades_in_flight.load(Ordering::Acquire));
    let mut ran = Vec::new();
    for ticket in sleep.timers.due_runs(now, free) {
        let zone = ticket.timer().room.clone();
        let Some(pending) = Pending::reserve(sleep, ticket) else {
            continue;
        };
        let surface = Arc::clone(&scheduler.surface);
        let dispatch = Arc::clone(&scheduler.dispatch);
        if spawn(Box::new(move || {
            // Keep this guard alive until the entire run, including restore,
            // settles. An unstarted closure releases its claim on every exit.
            let guard = pending;
            if dispatch.stopped() {
                return;
            }
            let households = surface.households().unwrap_or_default();
            if dispatch.stopped() {
                return;
            }
            let timer = guard.ticket.timer();
            let outcome = guard.sleep.timers.run_due(
                &*surface.transport,
                &households,
                &guard.sleep.fader,
                &guard.ticket,
            );
            match outcome {
                Ok(outcome) => tracing::info!(
                    "sleep timer in {}'s group: {}",
                    timer.room,
                    match outcome {
                        SleepOutcome::Paused => "faded out and paused",
                        SleepOutcome::Cancelled => "cancelled during its fade",
                        SleepOutcome::Interrupted => "ended by a volume change during its fade",
                        SleepOutcome::Gone => "already cancelled or replaced",
                    }
                ),
                Err(error) => tracing::warn!("sleep timer in {}'s group: {error}", timer.room),
            }
            guard.sleep.save();
        })) {
            ran.push(Ran::Fading { zone });
        }
    }
    ran
}

#[cfg(test)]
mod tests;
