//! Schedule admission: allocate a worker before consuming an occurrence.
//!
//! Capacity and spawn failures leave the persistent schedule due. Immediately
//! before execution, check its definition and enabled state under the store
//! lock and claim it once. Never replay an action after that boundary: even
//! an error can follow a partial or unacknowledged speaker mutation.

use super::{Ran, Schedule, Scheduler, Surface, Tick, core_schedule, fire, span, summary};
use crate::failure::{ErrorCode, Failure};
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

const MAX_RUNS: usize = 8;
type Work = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub(super) struct Dispatch {
    active: Mutex<HashSet<i64>>,
    stopping: AtomicBool,
}

impl Dispatch {
    pub(super) fn stopped(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub(super) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    fn reserve(self: &Arc<Self>, id: i64) -> Option<Permit> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopped() || active.len() >= MAX_RUNS || !active.insert(id) {
            return None;
        }
        Some(Permit {
            dispatch: Arc::clone(self),
            id,
        })
    }
}

/// Own the reservation even if spawning fails or the worker unwinds.
struct Permit {
    dispatch: Arc<Dispatch>,
    id: i64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.dispatch
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Run,
    Skip,
    Hold,
}

pub(super) fn run_schedules<Tz: TimeZone>(
    scheduler: &Scheduler,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Vec<Ran> {
    let schedules = match scheduler.surface.load_schedules() {
        Ok(schedules) => schedules,
        Err(failure) => {
            if failure.code != ErrorCode::NotImplemented {
                tracing::warn!("schedules not read: {}", failure.detail);
            }
            return Vec::new();
        }
    };
    let mut ran = Vec::new();
    for (id, tick) in core_schedule::due(&schedules, now, tz, scheduler.grace) {
        let Some(schedule) = schedules.iter().find(|schedule| schedule.id == id).cloned() else {
            continue;
        };
        match tick {
            Tick::Fire { at } => {
                if launch(scheduler, schedule, at, now, |name, work| {
                    thread::Builder::new().name(name).spawn(work)
                }) {
                    ran.push(Ran::Fired { id, at });
                }
            }
            Tick::Skip { at } => {
                match claim_current(&scheduler.surface, &schedule, at, || Admission::Skip) {
                    Ok(Admission::Skip) => {
                        tracing::warn!(
                            "schedule #{id} ({}) skipped its run at {at}: more than {} late",
                            summary(&schedule.action),
                            span(scheduler.grace)
                        );
                        ran.push(Ran::Skipped { id, at });
                    }
                    Ok(Admission::Run | Admission::Hold) => {}
                    Err(failure) => {
                        tracing::warn!("schedule #{id} not recorded: {}", failure.detail);
                    }
                }
            }
        }
    }
    ran
}

/// Definition check, deadline check and durable claim share the store lock.
/// A paused/removed/resumed/edited schedule cannot run from a stale snapshot.
fn claim_current(
    surface: &Surface,
    expected: &Schedule,
    at: DateTime<Utc>,
    admit: impl FnOnce() -> Admission,
) -> Result<Admission, Failure> {
    Ok(surface
        .with_store(|store| {
            let stored = store.schedules()?;
            let current = stored
                .iter()
                .find(|stored| stored.id == expected.id)
                .and_then(|stored| Schedule::from_stored(stored).ok());
            if !current.as_ref().is_some_and(|current| {
                current == expected
                    && current.enabled
                    && current.last_fired.is_none_or(|last| last < at)
            }) {
                return Ok(Admission::Hold);
            }
            let admission = admit();
            if admission == Admission::Hold
                || !store.mark_schedule_fired(expected.id, at.timestamp())?
            {
                return Ok(Admission::Hold);
            }
            Ok(admission)
        })?
        .unwrap_or(Admission::Hold))
}

fn launch(
    scheduler: &Scheduler,
    schedule: Schedule,
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    spawn: impl FnOnce(String, Work) -> io::Result<JoinHandle<()>>,
) -> bool {
    let Some(permit) = scheduler.dispatch.reserve(schedule.id) else {
        return false;
    };
    let surface = Arc::clone(&scheduler.surface);
    let grace = scheduler.grace;
    let admitted = Instant::now();
    let id = schedule.id;
    let work = Box::new(move || {
        let reservation = permit;
        let result = claim_current(&surface, &schedule, at, || {
            if reservation.dispatch.stopped() {
                return Admission::Hold;
            }
            let late = (now - at)
                .to_std()
                .unwrap_or_default()
                .saturating_add(admitted.elapsed());
            if late > grace {
                Admission::Skip
            } else {
                Admission::Run
            }
        });
        match result {
            Ok(Admission::Run) => {}
            Ok(Admission::Skip) => {
                tracing::warn!("schedule #{id} skipped: worker admission passed the grace period");
                return;
            }
            Ok(Admission::Hold) => return,
            Err(failure) => {
                tracing::warn!("schedule #{id} not claimed, so not run: {}", failure.detail);
                return;
            }
        }
        match fire(&surface, &schedule) {
            Ok(done) => tracing::info!("schedule #{id} ({}): {done}", summary(&schedule.action)),
            Err(failure) => tracing::warn!(
                "schedule #{id} ({}): {}",
                summary(&schedule.action),
                failure.cli_text()
            ),
        }
    });
    match spawn(format!("fsonos-schedule-{id}"), work) {
        Ok(handle) => {
            let mut runs = scheduler
                .runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            runs.retain(|run| !run.is_finished());
            runs.push(handle);
            true
        }
        Err(error) => {
            tracing::warn!("schedule #{id} worker not started; occurrence remains due: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests;
