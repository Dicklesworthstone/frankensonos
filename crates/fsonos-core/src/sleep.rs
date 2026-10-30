//! The sleep timer: "stop in 45 minutes", fading out at the end.
//!
//! [`SleepTimers::start`] arms a timer on a group (by its coordinator) and
//! sets the player's own sleep timer [`BACKSTOP`] later, which survives a
//! daemon restart but cannot fade. The daemon's tick asks
//! [`SleepTimers::due`] which timers have reached their last [`FADE`] and
//! runs [`SleepTimers::run`] for each, off the tick thread: fade the group
//! out, pause it, put every room's volume back (so the next play is not
//! silent), and clear the backstop. Cancelling or extending a timer that is
//! fading stops the fade and puts the volumes back; a volume change from
//! anyone else during the fade ends the timer where it is. A daemon that
//! restarts takes its saved timers up again with [`SleepTimers::adopt`].
//!
//! A timer follows the music: a move re-keys it to the coordinator the music
//! plays under now ([`SleepTimers::rekey`]), and a room whose group joined
//! another one since has only its own music stopped, by leaving that group.

use crate::fade::Fader;
use crate::snapshot::member_rooms;
use crate::{CoreError, HouseholdState};
use chrono::{DateTime, TimeDelta, Utc};
use fsonos_proto::Transport;
use fsonos_proto::control as soap;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

mod dispatch;
mod ownership;
pub use dispatch::SleepRun;
use ownership::{Claims, Operations, Run};

/// How long before the end the fade starts.
pub const FADE: Duration = Duration::from_mins(2);

/// How long after the daemon's timer the player's own one pauses, should
/// the daemon not be running then.
pub const BACKSTOP: Duration = Duration::from_mins(5);

/// After a move stops a fade, how long its run has to put the volumes back
/// before the timer fades again in the new group.
pub const REKEY_SETTLE: Duration = Duration::from_secs(2);

/// An armed sleep timer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SleepTimer {
    pub coordinator: PlayerId,
    /// The room it was set on, for messages.
    pub room: String,
    pub ends: DateTime<Utc>,
    /// The fade has started.
    #[serde(default)]
    pub fading: bool,
}

/// How a timer's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepOutcome {
    /// Faded out and paused; volumes put back.
    Paused,
    /// Cancelled or extended during the fade; volumes put back, playing on.
    Cancelled,
    /// Someone set a volume during the fade; the timer is over, nothing
    /// paused or put back.
    Interrupted,
    /// There was no timer to run (already cancelled).
    Gone,
}

#[derive(Debug)]
struct Armed {
    timer: SleepTimer,
    /// Changes whenever the timer is re-armed or dispatched, so a fade can tell.
    generation: u64,
    /// The worker has crossed into execution; it must never be requeued.
    running: bool,
    /// The worker's restoration ownership, including an explicit cancellation.
    fade_claims: Option<Arc<Claims>>,
    /// Earlier runs must finish restoration before this timer snapshots levels.
    predecessors: Vec<Arc<Claims>>,
    /// Fixed when due: dispatch and state reads consume the remaining fade time.
    fade_deadline: Option<Instant>,
    /// A volume change came after the timer was due but before its run took
    /// up its rooms: the run ends the timer instead of fading.
    interrupted: bool,
}

/// Every group's sleep timer. Share one per daemon.
#[derive(Debug)]
pub struct SleepTimers {
    armed: Mutex<HashMap<PlayerId, Armed>>,
    operations: Operations,
    next_generation: Mutex<u64>,
    fade: Duration,
}

impl Default for SleepTimers {
    fn default() -> Self {
        Self::with_fade(FADE)
    }
}

fn delta(d: Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

fn secs(d: TimeDelta) -> u32 {
    u32::try_from(d.num_seconds().max(0)).unwrap_or(u32::MAX)
}

fn ip(households: &[HouseholdState], player: &PlayerId) -> Result<IpAddr, CoreError> {
    crate::control::locate(households, player).map(|p| p.ip)
}

impl SleepTimers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Timers that fade over `fade` (the last `fade` before they end).
    #[must_use]
    pub fn with_fade(fade: Duration) -> Self {
        Self {
            armed: Mutex::new(HashMap::new()),
            operations: Operations::default(),
            next_generation: Mutex::new(0),
            fade,
        }
    }

    fn generation(&self) -> u64 {
        let mut g = self
            .next_generation
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *g += 1;
        *g
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PlayerId, Armed>> {
        self.armed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop a fade in flight, transferring its still-owned intents to the
    /// old worker's restoration. A newer manual change is never reclaimed.
    fn stop_fade(armed: &Armed, fader: &Fader) {
        if let Some(claims) = &armed.fade_claims {
            claims.stop(fader, true);
        }
    }

    /// Point the player's own timer `BACKSTOP` past `ends`.
    fn backstop<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        ends: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), CoreError> {
        let after = secs(ends - now + delta(BACKSTOP));
        Ok(soap::configure_sleep_timer(
            t,
            ip(households, coordinator)?,
            Some(after),
        )?)
    }

    /// Pause the group `coordinator` leads `after` from `now` (replacing
    /// any timer it has).
    pub fn start<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        coordinator: &PlayerId,
        after: Duration,
        now: DateTime<Utc>,
    ) -> Result<SleepTimer, CoreError> {
        let operation = self.operations.get(coordinator);
        let mut active = operation.lock().unwrap_or_else(PoisonError::into_inner);
        let predecessors = active.remaining();
        let ends = now + delta(after);
        Self::backstop(t, households, coordinator, ends, now)?;
        let timer = SleepTimer {
            coordinator: coordinator.clone(),
            room: room_name(households, coordinator),
            ends,
            fading: false,
        };
        let generation = self.generation();
        let old = self.lock().insert(
            coordinator.clone(),
            Armed {
                timer: timer.clone(),
                generation,
                running: false,
                fade_claims: None,
                predecessors,
                fade_deadline: None,
                interrupted: false,
            },
        );
        if let Some(old) = old {
            Self::stop_fade(&old, fader);
        }
        Ok(timer)
    }

    /// Push `coordinator`'s timer `by` later (stopping its fade, if one
    /// has started). `None` if it has no timer.
    pub fn extend<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        coordinator: &PlayerId,
        by: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<SleepTimer>, CoreError> {
        let operation = self.operations.get(coordinator);
        let mut active = operation.lock().unwrap_or_else(PoisonError::into_inner);
        let predecessors = active.remaining();
        let generation = self.generation();
        let (timer, claims) = {
            let mut armed = self.lock();
            let Some(entry) = armed.get_mut(coordinator) else {
                return Ok(None);
            };
            let claims = entry.fade_claims.take();
            entry.predecessors = predecessors;
            entry.timer.ends = entry.timer.ends.max(now) + delta(by);
            entry.timer.fading = false;
            entry.generation = generation;
            entry.running = false;
            entry.interrupted = false;
            entry.fade_deadline = None;
            (entry.timer.clone(), claims)
        };
        if let Some(claims) = claims {
            claims.stop(fader, true);
        }
        Self::backstop(t, households, coordinator, timer.ends, now)?;
        Ok(Some(timer))
    }

    /// Cancel `coordinator`'s timer (stopping its fade, if one has started)
    /// and the player's own. Whether there was one.
    pub fn cancel<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        coordinator: &PlayerId,
    ) -> Result<bool, CoreError> {
        let operation = self.operations.get(coordinator);
        let _operation = operation.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(old) = self.lock().remove(coordinator) else {
            return Ok(false);
        };
        Self::stop_fade(&old, fader);
        soap::configure_sleep_timer(t, ip(households, coordinator)?, None)?;
        Ok(true)
    }

    /// The music `old` led plays under `new` now (a move): its timer follows
    /// it there, with the player's own timer, replacing any `new` had. A fade
    /// in flight stops (its run puts the volumes back) and starts again in
    /// the new group. Whether `old` had a timer.
    pub fn rekey<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        old: &PlayerId,
        new: &PlayerId,
        now: DateTime<Utc>,
    ) -> Result<bool, CoreError> {
        if old == new {
            return Ok(false);
        }
        // Moves lock both coordinators in the same order as other moves.
        let (first, second) = if old.0 < new.0 {
            (old, new)
        } else {
            (new, old)
        };
        let first = self.operations.get(first);
        let second = self.operations.get(second);
        let mut first_active = first.lock().unwrap_or_else(PoisonError::into_inner);
        let mut second_active = second.lock().unwrap_or_else(PoisonError::into_inner);
        // A fade in flight starts over in the new group, once the run the
        // move stopped has put the volumes back.
        let ends = match self.lock().get(old) {
            Some(a) if a.timer.fading => a.timer.ends.max(now + delta(self.fade + REKEY_SETTLE)),
            Some(a) => a.timer.ends,
            None => return Ok(false),
        };
        // The new group's own timer first: if it can't be set, the timer
        // stays where it was.
        Self::backstop(t, households, new, ends, now)?;
        let (old_active, new_active) = if old.0 < new.0 {
            (&mut first_active, &mut second_active)
        } else {
            (&mut second_active, &mut first_active)
        };
        new_active.inherit(old_active);
        let predecessors = new_active.remaining();
        let generation = self.generation();
        let (claims, replaced) = {
            let mut armed = self.lock();
            let Some(mut entry) = armed.remove(old) else {
                // Retired meanwhile: restore the target's own timer as it was.
                let had = armed.get(new).map(|a| a.timer.ends);
                drop(armed);
                own_timer(t, households, new, had, now);
                return Ok(false);
            };
            let claims = entry.fade_claims.take();
            entry.predecessors = predecessors;
            entry.timer.coordinator = new.clone();
            entry.timer.room = room_name(households, new);
            entry.timer.ends = ends;
            entry.timer.fading = false;
            entry.generation = generation;
            entry.running = false;
            entry.interrupted = false;
            entry.fade_deadline = None;
            (claims, armed.insert(new.clone(), entry))
        };
        if let Some(claims) = claims {
            claims.stop(fader, true);
        }
        if let Some(replaced) = replaced {
            Self::stop_fade(&replaced, fader);
        }
        // The room the music left must not pause later on its own timer.
        own_timer(t, households, old, None, now);
        Ok(true)
    }

    /// A volume change from outside once the timer is due: supersede the
    /// group's fade so its timer ends where it is ([`SleepOutcome::Interrupted`])
    /// — no re-arm (the generation is untouched, so [`Self::run`] does not put
    /// the volumes back), and no pause. Before the run has taken up its rooms
    /// the timer is marked, and the run ends it there. Does nothing for a
    /// timer that is not due. Returns whether the timer was due.
    pub fn interrupt(&self, fader: &Fader, coordinator: &PlayerId) -> bool {
        let claims = {
            let mut armed = self.lock();
            let Some(a) = armed
                .get_mut(coordinator)
                .filter(|a| a.timer.fading || a.fade_deadline.is_some())
            else {
                return false;
            };
            // Before the run has taken up its rooms there is no fade to stop.
            a.interrupted = true;
            a.fade_claims.clone()
        };
        if let Some(claims) = claims {
            claims.stop(fader, false);
        }
        true
    }

    /// Arm `timer` as it was saved, leaving the player's own timer (its
    /// backstop, still running) as it is: how a restarted daemon takes up
    /// its timers. A group that already has a timer keeps it. Whether it was
    /// armed.
    pub fn adopt(&self, timer: SleepTimer) -> bool {
        let operation = self.operations.get(&timer.coordinator);
        let mut active = operation.lock().unwrap_or_else(PoisonError::into_inner);
        let predecessors = active.remaining();
        let generation = self.generation();
        let mut armed = self.lock();
        if armed.contains_key(&timer.coordinator) {
            return false;
        }
        armed.insert(
            timer.coordinator.clone(),
            Armed {
                timer: SleepTimer {
                    fading: false,
                    ..timer
                },
                generation,
                running: false,
                fade_claims: None,
                predecessors,
                fade_deadline: None,
                interrupted: false,
            },
        );
        true
    }

    /// `coordinator`'s timer, if it has one.
    #[must_use]
    pub fn get(&self, coordinator: &PlayerId) -> Option<SleepTimer> {
        self.lock().get(coordinator).map(|a| a.timer.clone())
    }

    /// Every armed timer, soonest first.
    #[must_use]
    pub fn all(&self) -> Vec<SleepTimer> {
        let mut all: Vec<SleepTimer> = self.lock().values().map(|a| a.timer.clone()).collect();
        all.sort_by_key(|t| t.ends);
        all
    }

    /// The timers whose fade should start at `now`; each is returned once
    /// (it is marked as fading). Run [`Self::run`] for each. Short or late
    /// timers get only the time remaining, never a fresh full-length fade.
    pub fn due(&self, now: DateTime<Utc>) -> Vec<SleepTimer> {
        self.due_runs(now, usize::MAX)
            .into_iter()
            .map(|run| run.timer)
            .collect()
    }

    /// Fade the group `coordinator` leads out within its remaining time,
    /// pause it, and put its rooms' volumes back. An overdue timer has no
    /// intentional fade delay. Blocks for the fade and device I/O. When
    /// `coordinator`'s room has joined another group since, only that room
    /// stops: it fades, leaves the group (which plays on), and gets its
    /// volume back.
    pub fn run<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        coordinator: &PlayerId,
    ) -> Result<SleepOutcome, CoreError> {
        self.run_generation(t, households, fader, coordinator, None)
    }

    fn run_generation<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        coordinator: &PlayerId,
        expected: Option<u64>,
    ) -> Result<SleepOutcome, CoreError> {
        let operation = self.operations.get(coordinator);
        let mut starting = operation.lock().unwrap_or_else(PoisonError::into_inner);
        let household = households.iter().find(|h| h.player(coordinator).is_some());
        let rooms = household.map_or_else(Vec::new, |h| member_rooms(h, coordinator));
        let joined = household
            .and_then(|h| h.coordinator_of(coordinator))
            .is_some_and(|lead| lead != coordinator);
        let mut run = {
            let mut armed = self.lock();
            let Some(a) = armed.get_mut(coordinator).filter(|a| {
                a.timer.fading && !a.running && expected.is_none_or(|g| g == a.generation)
            }) else {
                return Ok(SleepOutcome::Gone);
            };
            // Preserve the original dispatch deadline if a predecessor was
            // registered after selection. Admission can be retried, without
            // waiting under an operation lock the old worker needs to finish.
            if !starting.remaining().is_empty() {
                a.timer.fading = false;
                return Ok(SleepOutcome::Gone);
            }
            if household.is_none() {
                armed.remove(coordinator);
                return Err(CoreError::UnknownPlayer(coordinator.0.clone()));
            }
            a.running = true;
            if a.interrupted {
                armed.remove(coordinator);
                drop(armed);
                if !joined {
                    soap::configure_sleep_timer(t, ip(households, coordinator)?, None)?;
                }
                return Ok(SleepOutcome::Interrupted);
            }
            let run = Run::new(
                coordinator,
                joined,
                a.generation,
                a.fade_deadline,
                fader.claim(&rooms),
            );
            a.fade_claims = Some(Arc::clone(&run.restoration));
            starting.remember(&run.restoration);
            run
        };
        drop(starting);
        let outcome = run.fade(t, households, fader);
        // Other groups and timer observations remain available during device
        // I/O. This coordinator cannot be re-armed during its final stop/clear.
        let _operation = operation.lock().unwrap_or_else(PoisonError::into_inner);
        run.finish(self, t, households, fader, outcome)
    }

    /// Retire only this generation, returning whether a manual interruption
    /// arrived before retirement. Failed workers never remove a replacement.
    fn forget(&self, coordinator: &PlayerId, generation: u64) -> bool {
        let mut armed = self.lock();
        if !armed
            .get(coordinator)
            .is_some_and(|a| a.generation == generation)
        {
            return false;
        }
        armed.remove(coordinator).is_some_and(|a| a.interrupted)
    }
}

/// Point `player`'s own timer `BACKSTOP` past `ends`, or clear it (`None`),
/// best effort: the daemon's timer is what pauses the group.
fn own_timer<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    ends: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) {
    let _ = match ends {
        Some(ends) => SleepTimers::backstop(t, households, player, ends, now),
        None => ip(households, player)
            .and_then(|host| soap::configure_sleep_timer(t, host, None).map_err(CoreError::from)),
    };
}

/// The name of the room `player` is in, for messages.
fn room_name(households: &[HouseholdState], player: &PlayerId) -> String {
    households
        .iter()
        .flat_map(|h| &h.rooms)
        .find(|r| r.players.contains(player))
        .map_or_else(|| player.0.clone(), |r| r.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_come_due_once_at_the_start_of_their_fade() {
        let timers = SleepTimers::with_fade(Duration::from_mins(2));
        let t0 = DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
            .unwrap()
            .to_utc();
        let insert = |id: &str, mins: i64| {
            timers.lock().insert(
                PlayerId(id.into()),
                Armed {
                    timer: SleepTimer {
                        coordinator: PlayerId(id.into()),
                        room: id.into(),
                        ends: t0 + TimeDelta::minutes(mins),
                        fading: false,
                    },
                    generation: 0,
                    running: false,
                    fade_claims: None,
                    predecessors: Vec::new(),
                    fade_deadline: None,
                    interrupted: false,
                },
            );
        };
        insert("B", 45);
        insert("A", 10);
        assert_eq!(
            timers
                .all()
                .iter()
                .map(|t| t.room.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert!(timers.due(t0 + TimeDelta::minutes(7)).is_empty());
        let due = timers.due(t0 + TimeDelta::minutes(8));
        assert_eq!(due.len(), 1);
        assert!(due[0].fading && due[0].room == "A");
        assert!(
            timers.due(t0 + TimeDelta::minutes(9)).is_empty(),
            "only once"
        );
        assert_eq!(timers.due(t0 + TimeDelta::hours(2)).len(), 1, "B, late");
    }

    #[test]
    fn an_adopted_timer_is_armed_afresh_and_never_replaces_one() {
        let timers = SleepTimers::with_fade(Duration::from_mins(2));
        let ends = DateTime::parse_from_rfc3339("2026-10-07T22:45:00Z")
            .unwrap()
            .to_utc();
        let saved = SleepTimer {
            coordinator: PlayerId("RINCON_A".into()),
            room: "Den".into(),
            ends,
            fading: true,
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<SleepTimer>(&json).unwrap(), saved);
        assert!(timers.adopt(saved.clone()));
        let armed = timers.get(&saved.coordinator).unwrap();
        assert!(!armed.fading, "its fade starts again when it comes due");
        assert_eq!(armed.ends, ends);
        let later = SleepTimer {
            ends: ends + TimeDelta::hours(1),
            ..saved.clone()
        };
        assert!(!timers.adopt(later), "the timer already armed stands");
        assert_eq!(timers.get(&saved.coordinator).unwrap().ends, ends);
    }
}

#[cfg(test)]
#[path = "sleep/deadline_tests.rs"]
mod deadline_tests;
