//! The fade journal lives beside armed timers, so cancelling/replacing a timer
//! cannot erase the original levels before its old worker finishes cleanup.

use super::{FIRST, Sleep, State, Surface, Zone, expired, is_backstop, speaker_timer, write};
use fsonos_core::sleep::{SleepCheckpoint, SleepFade, SleepRecovery, SleepTimer};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn failure(error: impl std::fmt::Display) -> CoreError {
    CoreError::Store(format!("sleep fade recovery checkpoint: {error}"))
}

pub(in super::super) struct Checkpoint<'a> {
    sleep: &'a Sleep,
    id: Mutex<Option<String>>,
    owner: u64,
    intent_epochs: HashMap<PlayerId, u64>,
}

impl<'a> Checkpoint<'a> {
    pub(in super::super) fn new(sleep: &'a Sleep) -> Self {
        let state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
        Self {
            sleep,
            id: Mutex::new(None),
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            intent_epochs: state.intent_epochs.clone(),
        }
    }

    pub(super) fn existing(sleep: &'a Sleep, fade: &SleepFade) -> Option<Self> {
        let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped
            || state.fades.get(&fade.id) != Some(fade)
            || state.active_fades.contains_key(&fade.id)
            || state.resumed_fades.contains(&fade.id)
        {
            return None;
        }
        let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
        state.active_fades.insert(fade.id.clone(), owner);
        Some(Self {
            sleep,
            id: Mutex::new(Some(fade.id.clone())),
            owner,
            intent_epochs: state.intent_epochs.clone(),
        })
    }

    fn same_intent(&self, state: &State, fade: &SleepFade) -> bool {
        state
            .intent_epochs
            .iter()
            .filter(|(player, _)| fade.affects(player))
            .all(|(player, epoch)| self.intent_epochs.get(player) == Some(epoch))
    }
}

impl SleepCheckpoint for Checkpoint<'_> {
    fn save(&self, fade: &SleepFade) -> Result<(), CoreError> {
        let path = self
            .sleep
            .saved
            .as_deref()
            .ok_or_else(|| failure("no saved file"))?;
        let mut id = self.id.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = self
            .sleep
            .writing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.load(path).map_err(failure)?;
        if !self.same_intent(&state, fade)
            || id
                .as_ref()
                .is_some_and(|id| id != &fade.id || !state.fades.contains_key(id))
        {
            return Err(failure("a newer command superseded the fade"));
        }
        if state
            .active_fades
            .get(&fade.id)
            .is_some_and(|owner| *owner != self.owner)
            && !state.resumed_fades.contains(&fade.id)
        {
            return Err(failure("another worker owns the fade"));
        }
        if !fade.valid()
            || state.fades.values().any(|other| {
                other.id != fade.id && other.timer.coordinator == fade.timer.coordinator
            })
        {
            return Err(failure("ambiguous fade identity"));
        }
        *id = Some(fade.id.clone());
        state.fades.insert(fade.id.clone(), fade.clone());
        state.active_fades.insert(fade.id.clone(), self.owner);
        state.resumed_fades.remove(&fade.id);
        state.dirty = true;
        write(self.sleep, path, &mut state).map_err(failure)
    }

    fn complete(&self, fade: &SleepFade) -> Result<(), CoreError> {
        let Some(path) = self.sleep.saved.as_deref() else {
            return Ok(());
        };
        let mut state = self
            .sleep
            .writing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.active_fades.get(&fade.id) != Some(&self.owner) {
            return Ok(());
        }
        let old = state.fades.remove(&fade.id);
        state.active_fades.remove(&fade.id);
        state.resumed_fades.remove(&fade.id);
        if let Some(old) = old {
            state.dirty = true;
            if let Err(error) = write(self.sleep, path, &mut state) {
                state.active_fades.insert(old.id.clone(), self.owner);
                state.fades.insert(old.id.clone(), old);
                return Err(failure(error));
            }
        }
        Ok(())
    }

    fn current(&self, fade: &SleepFade) -> bool {
        let state = self
            .sleep
            .writing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.active_fades.get(&fade.id) == Some(&self.owner)
            && state.fades.contains_key(&fade.id)
            && self.same_intent(&state, fade)
    }
}

impl Drop for Checkpoint<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.get_mut().unwrap_or_else(PoisonError::into_inner) {
            let mut state = self
                .sleep
                .writing
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if state.active_fades.get(id) == Some(&self.owner) {
                state.active_fades.remove(id);
            }
        }
    }
}

/// An accepted manual volume/playback command wins over a late startup read.
/// Ordinary timers without an interrupted fade retain their existing policy.
pub(in super::super) fn supersede(sleep: &Sleep, coordinator: &PlayerId) -> Vec<PlayerId> {
    let Some(path) = sleep.saved.as_deref() else {
        return Vec::new();
    };
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    let epoch = state.intent_epochs.entry(coordinator.clone()).or_default();
    *epoch = epoch.wrapping_add(1);
    // The initial snapshot may still be reading, before there is a journal.
    // Fence that due timer on disk before the newer command reaches a speaker.
    let due = sleep.timers.get(coordinator).filter(|timer| timer.fading);
    if let Some(timer) = &due {
        state.retired_timers.insert(coordinator.clone(), timer.ends);
        state.pending.remove(coordinator);
        state.dirty = true;
    }
    if !state.loaded {
        // Volume notifications do not read files or cancel future timers. If
        // loading later reveals an interrupted fade, the newer intent wins.
        state.changed_fades.insert(coordinator.clone());
        return Vec::new();
    }
    let retired: Vec<_> = state
        .fades
        .values()
        .filter(|f| f.affects(coordinator))
        .map(|f| (f.id.clone(), f.timer.coordinator.clone(), f.timer.ends))
        .collect();
    if retired.is_empty() && due.is_none() {
        return Vec::new();
    }
    let affected: Vec<_> = retired
        .iter()
        .map(|(_, coordinator, _)| coordinator.clone())
        .collect();
    for (id, coordinator, ends) in retired {
        state.fades.remove(&id);
        state.active_fades.remove(&id);
        state.resumed_fades.remove(&id);
        state.pending.remove(&coordinator);
        // The interrupted worker has not necessarily retired its in-memory
        // timer yet; do not publish that timer again without its journal.
        state.retired_timers.insert(coordinator, ends);
        state.dirty = true;
    }
    if state.dirty
        && let Err(error) = write(sleep, path, &mut state)
    {
        tracing::warn!(%error, "sleep fade retirement will retry");
    }
    // The caller interrupts these claims after releasing writing: a final
    // fade step may itself be waiting to publish a checkpoint under this lock.
    affected
}

pub(super) fn pending(sleep: &Sleep) -> Vec<SleepFade> {
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    // A dispatch can expire without ever entering the core worker.
    let abandoned: Vec<_> = state
        .resumed_fades
        .iter()
        .filter(|id| {
            state.fades.get(*id).is_none_or(|fade| {
                sleep
                    .timers
                    .get(&fade.timer.coordinator)
                    .is_none_or(|timer| timer.ends != fade.timer.ends)
            })
        })
        .cloned()
        .collect();
    for id in abandoned {
        state.resumed_fades.remove(&id);
    }
    state
        .fades
        .values()
        .filter(|fade| {
            !state.active_fades.contains_key(&fade.id) && !state.resumed_fades.contains(&fade.id)
        })
        .cloned()
        .collect()
}

pub(super) fn blocks_adoption(sleep: &Sleep, coordinator: &PlayerId) -> bool {
    sleep
        .writing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .fades
        .values()
        .any(|fade| fade.timer.coordinator == *coordinator)
}

/// A replacement must not snapshot a predecessor's still-attenuated levels.
pub(in super::super) fn blocks_dispatch(sleep: &Sleep, timer: &SleepTimer) -> bool {
    let state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    state.fades.values().any(|fade| {
        fade.timer.coordinator == timer.coordinator
            && !(fade.timer.ends == timer.ends && state.resumed_fades.contains(&fade.id))
    })
}

fn matching_timer(sleep: &Sleep, fade: &SleepFade) -> Option<SleepTimer> {
    sleep
        .writing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pending
        .get(&fade.timer.coordinator)
        .filter(|timer| timer.ends == fade.timer.ends)
        .cloned()
}

fn retire_timer(sleep: &Sleep, fade: &SleepFade) {
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    if state
        .pending
        .get(&fade.timer.coordinator)
        .is_some_and(|timer| timer.ends == fade.timer.ends)
    {
        state.pending.remove(&fade.timer.coordinator);
        state.dirty = true;
    }
}

pub(super) fn recover(
    surface: &Surface,
    sleep: &Sleep,
    households: &[HouseholdState],
    fades: Vec<SleepFade>,
    allowed: &impl Fn() -> bool,
) -> usize {
    let mut adopted = 0;
    for mut fade in fades {
        if !allowed() {
            break;
        }
        let Some(checkpoint) = Checkpoint::existing(sleep, &fade) else {
            continue;
        };
        let Some(zone) = Zone::of(households, &fade.timer.coordinator) else {
            continue;
        };
        let timer = matching_timer(sleep, &fade);
        let now = surface.clock.now().to_utc();
        let matches = if let Some(timer) = &timer {
            if expired(timer, now) {
                false
            } else {
                match speaker_timer(&*surface.transport, &zone) {
                    Ok(Some(left)) => !left.is_zero() && is_backstop(timer, left, now),
                    Ok(None) => false,
                    Err(failure) => {
                        surface.notice(&failure);
                        continue;
                    }
                }
            }
        } else {
            false
        };
        let decision = fade.recovery(&*surface.transport, households, now, matches);
        if !allowed() || !checkpoint.current(&fade) {
            continue;
        }
        let result = match decision {
            Ok(SleepRecovery::Resume) => {
                let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
                if !state.stopped
                    && state.fades.contains_key(&fade.id)
                    && timer
                        .as_ref()
                        .is_some_and(|timer| state.pending.get(&timer.coordinator) == Some(timer))
                    && sleep.timers.adopt_resuming(
                        SleepTimer {
                            room: zone.room,
                            ..fade.timer.clone()
                        },
                        fade.clone(),
                    )
                {
                    state.pending.remove(&fade.timer.coordinator);
                    state.resumed_fades.insert(fade.id.clone());
                    state.dirty = true;
                    state.delay = FIRST;
                    adopted += 1;
                }
                Ok(())
            }
            Ok(SleepRecovery::Restore) => {
                retire_timer(sleep, &fade);
                // Publish retirement before a fallback clear or restoration.
                checkpoint
                    .save(&fade)
                    .and_then(|()| {
                        sleep.timers.clear_recovered_backstop(
                            &*surface.transport,
                            households,
                            &fade,
                            &checkpoint,
                            now,
                        )
                    })
                    .and_then(|()| {
                        fade.restore(&*surface.transport, households, &sleep.fader, &checkpoint)
                    })
            }
            Ok(SleepRecovery::Changed) => {
                retire_timer(sleep, &fade);
                checkpoint.complete(&fade)
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!(room = %fade.timer.room, %error, "sleep fade recovery will retry");
        }
    }
    adopted
}
