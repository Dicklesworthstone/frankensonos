//! Recover saved sleep timers without equating an unavailable speaker with
//! a cancelled timer. Only a verified, still-running backstop grants adoption.
//! All speaker I/O happens outside the persistence mutex, on one worker.

use super::{
    BACKSTOP, SavedTimers, Scheduler, Sleep, SleepTimer, Surface, Zone, delta, is_backstop,
    speaker_timer,
};
use chrono::{DateTime, Utc};
use fsonos_types::PlayerId;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::Path;
use std::sync::{Arc, PoisonError, TryLockError};
use std::time::{Duration, Instant};

mod file;

const FIRST: Duration = Duration::from_secs(5);
const MAX: Duration = Duration::from_secs(30);
const MAX_FILE: u64 = 1024 * 1024;

/// The original file is read once successfully. A failed read never grants
/// permission to replace it with an empty snapshot. Unresolved timers stay in
/// every subsequent snapshot alongside newly armed ones.
#[allow(clippy::struct_excessive_bools)] // independent conditions, each with its own transitions
pub(super) struct State {
    loaded: bool,
    pending: HashMap<PlayerId, SleepTimer>,
    // Explicit local changes can precede a successful first file read.
    superseded: HashSet<PlayerId>,
    dirty: bool,
    busy: bool,
    stopped: bool,
    next: Instant,
    delay: Duration,
}

impl Default for State {
    fn default() -> Self {
        Self {
            loaded: false,
            pending: HashMap::new(),
            superseded: HashSet::new(),
            dirty: false,
            busy: false,
            stopped: false,
            next: Instant::now(),
            delay: FIRST,
        }
    }
}

impl State {
    fn load(&mut self, path: &Path) -> io::Result<()> {
        if self.loaded {
            return Ok(());
        }
        let timers = match std::fs::File::open(path) {
            Ok(file) => {
                // Bound corrupted or accidentally replaced files before JSON
                // parsing; don't partially adopt an ambiguous snapshot.
                let mut text = String::new();
                file.take(MAX_FILE + 1).read_to_string(&mut text)?;
                if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_FILE {
                    return Err(io::Error::other("saved sleep timers exceed 1 MiB"));
                }
                serde_json::from_str::<SavedTimers>(&text)
                    .map_err(io::Error::other)?
                    .timers
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && path
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."))
                        .is_dir() =>
            {
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        let mut pending = HashMap::new();
        for timer in timers {
            if timer.coordinator.0.trim().is_empty()
                || timer.room.trim().is_empty()
                || pending.insert(timer.coordinator.clone(), timer).is_some()
            {
                return Err(io::Error::other(
                    "saved sleep timers have ambiguous identities",
                ));
            }
        }
        pending.retain(|id, _| !self.superseded.contains(id));
        self.pending = pending;
        self.superseded.clear();
        self.loaded = true;
        Ok(())
    }

    fn snapshot(&self, active: Vec<SleepTimer>) -> SavedTimers {
        let mut by_id = self.pending.clone();
        for timer in active {
            by_id.insert(timer.coordinator.clone(), timer);
        }
        let mut timers: Vec<_> = by_id.into_values().collect();
        timers.sort_by(|a, b| a.coordinator.0.cmp(&b.coordinator.0));
        SavedTimers { timers }
    }

    fn needs_work(&self) -> bool {
        !self.loaded || !self.pending.is_empty() || self.dirty
    }
}

/// Called before a timer-changing command may reach a speaker. Retire its
/// startup candidate before I/O so a late verification cannot resurrect it.
/// Keep the old deadline for an extension after verifying its backstop.
pub(super) fn supersede(sleep: &Sleep, id: &PlayerId) -> Option<SleepTimer> {
    let path = sleep.saved.as_deref()?;
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(error) = state.load(path) {
        tracing::warn!(%error, "saved sleep timers not yet readable; preserving the file");
        state.superseded.insert(id.clone());
    }
    state.dirty = true;
    state.pending.remove(id)
}

/// Startup candidates only, never a second copy of an already armed timer.
pub(super) fn saved(sleep: &Sleep) -> Vec<SleepTimer> {
    let Some(path) = sleep.saved.as_deref() else {
        return Vec::new();
    };
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(error) = state.load(path) {
        tracing::warn!(%error, "saved sleep timers not yet readable; recovery will retry");
        return Vec::new();
    }
    let mut timers: Vec<_> = state.pending.values().cloned().collect();
    timers.sort_by(|a, b| a.coordinator.0.cmp(&b.coordinator.0));
    timers
}

/// Save requests are serialized with candidate retirement/adoption, not with
/// speaker requests. A failed publication remains dirty for the next worker.
pub(super) fn save(sleep: &Sleep, changed: bool) {
    let Some(path) = sleep.saved.as_deref() else {
        return;
    };
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    state.dirty |= changed;
    if let Err(error) = state.load(path) {
        tracing::warn!(%error, "sleep snapshot preserved: its previous revision is unreadable");
        return;
    }
    if !state.dirty {
        return;
    }
    let written = serde_json::to_vec_pretty(&state.snapshot(sleep.timers.all()))
        .map_err(io::Error::other)
        .and_then(|bytes| {
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE {
                return Err(io::Error::other("sleep snapshot exceeds 1 MiB"));
            }
            file::replace(path, &bytes)
        });
    match written {
        Ok(()) => state.dirty = false,
        Err(error) => tracing::warn!(%error, "sleep timers not saved; publication will retry"),
    }
}

/// Admission is retained by the closure even if thread creation fails or a
/// worker panics. No overlapping recovery passes and no tick-thread I/O.
pub(super) struct Permit(Arc<Sleep>);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self
            .0
            .writing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.busy = false;
        state.next = Instant::now() + state.delay;
        state.delay = state.delay.saturating_mul(2).min(MAX);
    }
}

pub(super) fn reserve(sleep: &Arc<Sleep>, force: bool) -> Option<Permit> {
    sleep.saved.as_ref()?;
    let mut state = match sleep.writing.try_lock() {
        Ok(state) => state,
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    if state.stopped || state.busy || !state.needs_work() || (!force && Instant::now() < state.next)
    {
        return None;
    }
    state.busy = true;
    Some(Permit(Arc::clone(sleep)))
}

/// Close adoption under the same mutex used by `finish`. A read that passed
/// its last cancellation check still cannot arm a timer after this boundary.
/// Pending evidence remains durable for a new daemon, not a later call here.
pub(super) fn stop(sleep: &Sleep) {
    sleep
        .writing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .stopped = true;
}

/// Retry verification and dirty persistence away from the scheduling thread.
pub(super) fn start(scheduler: &Scheduler) {
    let Some(sleep) = &scheduler.surface.sleep else {
        return;
    };
    let Some(permit) = reserve(sleep, false) else {
        return;
    };
    let surface = Arc::clone(&scheduler.surface);
    let dispatch = Arc::clone(&scheduler.dispatch);
    scheduler.spawn("fsonos-sleep-recovery", move || {
        let _permit = permit;
        if !dispatch.stopped()
            && let Some(sleep) = &surface.sleep
        {
            adopt(&surface, sleep, || !dispatch.stopped());
        }
    });
}

fn expired(timer: &SleepTimer, now: DateTime<Utc>) -> bool {
    timer
        .ends
        .checked_add_signed(delta(BACKSTOP))
        .is_none_or(|end| end <= now)
}

/// Commit read evidence only while it still refers to this startup candidate.
/// Removing a candidate before a concurrent command's I/O fences late reads.
fn finish(sleep: &Sleep, timer: &SleepTimer, adopt: bool, room: &str) -> bool {
    let mut state = sleep.writing.lock().unwrap_or_else(PoisonError::into_inner);
    if (adopt && state.stopped) || state.pending.get(&timer.coordinator) != Some(timer) {
        return false;
    }
    let adopted = adopt
        && sleep.timers.adopt(SleepTimer {
            room: room.into(),
            ..timer.clone()
        });
    state.pending.remove(&timer.coordinator);
    state.dirty = true;
    state.delay = FIRST;
    adopted
}

pub(super) fn adopt(surface: &Surface, sleep: &Sleep, allowed: impl Fn() -> bool) -> usize {
    if !allowed() {
        return 0;
    }
    let candidates = sleep.saved_timers();
    // Expired candidates can be retired without any reachable household.
    let now = surface.clock.now().to_utc();
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|timer| {
            if expired(timer, now) {
                finish(sleep, timer, false, &timer.room);
                false
            } else {
                true
            }
        })
        .collect();
    if candidates.is_empty() {
        save(sleep, false);
        return 0;
    }
    let households = match surface.households() {
        Ok(households) => households,
        Err(failure) => {
            tracing::warn!("sleep recovery awaits discovery: {}", failure.detail);
            save(sleep, false);
            return 0;
        }
    };
    let mut adopted = 0;
    for timer in candidates {
        if !allowed() {
            break;
        }
        // Never infer a replacement coordinator by room name or adopt a
        // former coordinator into the unrelated group it has since joined.
        if !households.iter().any(|household| {
            household
                .groups
                .iter()
                .any(|group| group.coordinator == timer.coordinator)
        }) {
            continue;
        }
        let Some(zone) = Zone::of(&households, &timer.coordinator) else {
            continue;
        };
        let observed = speaker_timer(&*surface.transport, &zone);
        if !allowed() {
            break;
        }
        match observed {
            Ok(Some(left)) => {
                let now = surface.clock.now().to_utc();
                let matches =
                    !expired(&timer, now) && !left.is_zero() && is_backstop(&timer, left, now);
                if finish(sleep, &timer, matches, &zone.room) {
                    adopted += 1;
                }
            }
            Ok(None) => {
                finish(sleep, &timer, false, &zone.room);
            }
            Err(failure) => {
                surface.notice(&failure);
                tracing::debug!(
                    "sleep timer in {} awaits a readable backstop: {}",
                    timer.room,
                    failure.detail
                );
            }
        }
    }
    save(sleep, false);
    adopted
}

#[cfg(test)]
mod tests;
