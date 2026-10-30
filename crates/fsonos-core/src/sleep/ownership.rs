//! Sleep completion and explicit cancellation retain only their own volume intents.

use super::{
    CoreError, Fader, HouseholdState, PlayerId, SleepOutcome, SleepTimers, Transport, ip, soap,
};
use crate::fade::FadeOutcome;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// Retired runs remain here even across cancel followed immediately by start.
/// Completion is observed without waiting while an operation lock is held.
#[derive(Debug, Default)]
pub(super) struct Pending(Vec<Arc<Claims>>);

impl Pending {
    pub(super) fn remaining(&mut self) -> Vec<Arc<Claims>> {
        self.0.retain(|claims| !claims.finished());
        self.0.clone()
    }

    pub(super) fn remember(&mut self, claims: &Arc<Claims>) {
        if !claims.finished() && !self.0.iter().any(|old| Arc::ptr_eq(old, claims)) {
            self.0.push(Arc::clone(claims));
        }
    }

    pub(super) fn inherit(&mut self, other: &mut Self) {
        self.0.retain(|claims| !claims.finished());
        for claims in other.remaining() {
            self.remember(&claims);
        }
    }
}

/// Stable per-coordinator serialization for the device backstop and final stop.
/// The inventory-facing timer map is never locked across network operations.
#[derive(Debug, Default)]
pub(super) struct Operations(Mutex<HashMap<PlayerId, Arc<Mutex<Pending>>>>);

impl Operations {
    pub(super) fn get(&self, coordinator: &PlayerId) -> Arc<Mutex<Pending>> {
        let mut rooms = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(rooms.entry(coordinator.clone()).or_default())
    }
}

/// Shared by one run and the timer operation that may cancel it. Updating the
/// restoration claims under this lock closes the gap between stopping a fade
/// and its worker noticing cancellation. A manual interrupt never transfers
/// ownership to restoration.
#[derive(Debug)]
pub(super) struct Claims {
    intents: Mutex<Vec<(PlayerId, u64)>>,
    complete: AtomicBool,
}

impl Claims {
    fn new(claimed: Vec<(PlayerId, u64)>) -> Self {
        Self {
            intents: Mutex::new(claimed),
            complete: AtomicBool::new(false),
        }
    }

    pub(super) fn finished(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    pub(super) fn stop(&self, fader: &Fader, restore: bool) {
        let mut claims = self.intents.lock().unwrap_or_else(PoisonError::into_inner);
        for (room, intent) in claims.iter_mut() {
            if let Some(next) = fader.supersede_claimed(room, *intent)
                && restore
            {
                *intent = next;
            }
        }
    }

    fn restore<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        before: &[(PlayerId, u8)],
    ) -> Result<(), CoreError> {
        let claims = self.intents.lock().unwrap_or_else(PoisonError::into_inner);
        let owned: Vec<_> = before
            .iter()
            .filter_map(|(room, level)| {
                claims
                    .iter()
                    .find(|(player, _)| player == room)
                    .map(|(_, intent)| (room.clone(), *intent, *level))
            })
            .collect();
        fader.restore(t, households, &owned)
    }
}

/// Its Drop releases successor dispatch after restoration, including every
/// error return and unwinding worker. No successor snapshots an unfinished fade.
pub(super) struct Run {
    coordinator: PlayerId,
    joined: bool,
    generation: u64,
    deadline: Instant,
    claimed: Vec<(PlayerId, u64)>,
    pub(super) restoration: Arc<Claims>,
    before: Vec<(PlayerId, u8)>,
}

impl Run {
    pub(super) fn new(
        coordinator: &PlayerId,
        joined: bool,
        generation: u64,
        deadline: Option<Instant>,
        claimed: Vec<(PlayerId, u64)>,
    ) -> Self {
        Self {
            coordinator: coordinator.clone(),
            joined,
            generation,
            deadline: deadline.unwrap_or_else(Instant::now),
            restoration: Arc::new(Claims::new(claimed.clone())),
            claimed,
            before: Vec::new(),
        }
    }

    pub(super) fn fade<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
    ) -> Result<FadeOutcome, CoreError> {
        self.before = self
            .claimed
            .iter()
            .map(|(room, _)| Ok((room.clone(), soap::get_volume(t, ip(households, room)?)?)))
            .collect::<Result<_, CoreError>>()?;
        fader.fade_group_claimed_until(t, households, &self.claimed, 0, self.deadline)
    }

    fn clear<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
    ) -> Result<(), CoreError> {
        Ok(soap::configure_sleep_timer(
            t,
            ip(households, &self.coordinator)?,
            None,
        )?)
    }

    /// Caller holds this coordinator's operation lock, so no replacement can
    /// slip between the generation check and a device backstop clear.
    pub(super) fn finish<T: Transport + ?Sized>(
        &self,
        timers: &SleepTimers,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        outcome: Result<FadeOutcome, CoreError>,
    ) -> Result<SleepOutcome, CoreError> {
        let restore = || self.restoration.restore(t, households, fader, &self.before);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = restore();
                if timers.forget(&self.coordinator, self.generation) && !self.joined {
                    // An explicit manual interruption also retires the device
                    // fallback. It must not later stop the newer playback.
                    let _ = self.clear(t, households);
                }
                return Err(error);
            }
        };
        let current = timers.lock().get(&self.coordinator).map(|a| a.generation);
        if current != Some(self.generation) {
            restore()?;
            return Ok(SleepOutcome::Cancelled);
        }
        let stopped = if outcome == FadeOutcome::Reached {
            fader.with_claimed(&self.claimed, || {
                if self.joined {
                    crate::control::leave(t, households, &self.coordinator)
                } else {
                    crate::control::pause(t, households, &self.coordinator)
                }
            })
        } else {
            None
        };
        match stopped {
            Some(stopped) => {
                // A genuine failed stop keeps the fallback. An interruption
                // recorded before retiring this run takes precedence over it.
                let restored = restore();
                let interrupted = timers.forget(&self.coordinator, self.generation);
                let cleared = if stopped.is_ok() || (interrupted && !self.joined) {
                    self.clear(t, households)
                } else {
                    Ok(())
                };
                stopped
                    .and(cleared)
                    .and(restored)
                    .map(|()| SleepOutcome::Paused)
            }
            None => {
                timers.forget(&self.coordinator, self.generation);
                if !self.joined {
                    self.clear(t, households)?;
                }
                Ok(SleepOutcome::Interrupted)
            }
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.restoration.complete.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwinding_a_worker_releases_its_pending_successors() {
        let player = PlayerId("RINCON_TEST".into());
        let run = Run::new(&player, false, 1, None, vec![(player.clone(), 1)]);
        let mut pending = Pending::default();
        pending.remember(&run.restoration);
        assert_eq!(pending.remaining().len(), 1);
        assert!(
            std::panic::catch_unwind(move || {
                let _run = run;
                panic!("injected worker unwind");
            })
            .is_err()
        );
        assert!(pending.remaining().is_empty());
    }
}
