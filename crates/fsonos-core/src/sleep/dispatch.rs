//! Generation-bound sleep work. Failed worker admission can release a ticket;
//! once execution begins, even an error is not permission to replay it.

use super::{
    CoreError, DateTime, Fader, HouseholdState, Instant, SleepOutcome, SleepTimer, SleepTimers,
    Transport, Utc, delta,
};

/// One dispatch of one timer, issued by [`SleepTimers::due_runs`]. Use it only
/// with the issuing `SleepTimers`. A worker either calls `run_due` or returns
/// unstarted work with `release_run`; retaining a stale ticket grants no work.
#[derive(Debug, Clone)]
pub struct SleepRun {
    pub(super) timer: SleepTimer,
    generation: u64,
}

impl SleepRun {
    /// The timer this worker was allocated for, never a replacement timer.
    #[must_use]
    pub fn timer(&self) -> &SleepTimer {
        &self.timer
    }
}

impl SleepTimers {
    /// Reserve at most `limit` due fades, earliest deadline first. Timers left
    /// out by capacity stay armed, not falsely marked as running. The original
    /// fade deadline survives a failed admission and every subsequent dispatch.
    pub fn due_runs(&self, now: DateTime<Utc>, limit: usize) -> Vec<SleepRun> {
        if limit == 0 {
            return Vec::new();
        }
        let monotonic_now = Instant::now();
        let mut armed = self.lock();
        let mut ready: Vec<_> = armed
            .values_mut()
            .filter(|a| {
                !a.timer.fading
                    && now
                        >= a.timer
                            .ends
                            .checked_sub_signed(delta(self.fade))
                            .unwrap_or(DateTime::<Utc>::MIN_UTC)
            })
            .collect();
        ready.sort_by(|a, b| {
            a.timer
                .ends
                .cmp(&b.timer.ends)
                .then_with(|| a.timer.coordinator.0.cmp(&b.timer.coordinator.0))
        });
        ready
            .into_iter()
            .take(limit)
            .map(|a| {
                a.timer.fading = true;
                a.running = false;
                // Every dispatch gets a fresh identity, including redispatch of
                // an unchanged timer. A late release cannot undo the next worker.
                a.generation = self.generation();
                let remaining = (a.timer.ends - now).to_std().unwrap_or_default();
                let deadline = monotonic_now
                    .checked_add(remaining.min(self.fade))
                    .unwrap_or(monotonic_now);
                a.fade_deadline = Some(a.fade_deadline.map_or(deadline, |old| old.min(deadline)));
                // Preserve interruption recorded after an earlier dispatch that
                // could not start. A fresh start/extend/rekey clears it instead.
                SleepRun {
                    timer: a.timer.clone(),
                    generation: a.generation,
                }
            })
            .collect()
    }

    /// Return only this unstarted dispatch to the due set. No speaker command
    /// or file I/O occurs here. Work that entered `run_due` cannot be released,
    /// nor can an old ticket alter a cancellation, replacement or newer claim.
    pub fn release_run(&self, run: &SleepRun) -> bool {
        let mut armed = self.lock();
        let Some(a) = armed
            .get_mut(&run.timer.coordinator)
            .filter(|a| a.generation == run.generation && a.timer.fading && !a.running)
        else {
            return false;
        };
        a.timer.fading = false;
        true
    }

    /// Execute precisely this dispatch, not whatever timer the coordinator
    /// may have acquired since. Duplicate workers return `Gone` without I/O.
    pub fn run_due<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        run: &SleepRun,
    ) -> Result<SleepOutcome, CoreError> {
        self.run_generation(
            t,
            households,
            fader,
            &run.timer.coordinator,
            Some(run.generation),
        )
    }
}

#[cfg(test)]
mod tests;
