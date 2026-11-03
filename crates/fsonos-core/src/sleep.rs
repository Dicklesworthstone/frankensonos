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

use crate::fade::{FadeOutcome, Fader};
use crate::snapshot::member_rooms;
use crate::{CoreError, HouseholdState};
use chrono::{DateTime, TimeDelta, Utc};
use fsonos_proto::Transport;
use fsonos_proto::control as soap;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

mod dispatch;
pub use dispatch::SleepRun;

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
    /// The rooms a running fade is turning down.
    fading_rooms: Vec<PlayerId>,
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

    /// Stop a fade in flight on `armed`, if any.
    fn stop_fade(armed: &Armed, fader: &Fader) {
        for room in &armed.fading_rooms {
            fader.supersede(room);
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
        let ends = now + delta(after);
        Self::backstop(t, households, coordinator, ends, now)?;
        let timer = SleepTimer {
            coordinator: coordinator.clone(),
            room: room_name(households, coordinator),
            ends,
            fading: false,
        };
        let generation = self.generation();
        if let Some(old) = self.lock().insert(
            coordinator.clone(),
            Armed {
                timer: timer.clone(),
                generation,
                running: false,
                fading_rooms: Vec::new(),
                fade_deadline: None,
                interrupted: false,
            },
        ) {
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
        let generation = self.generation();
        let timer = {
            let mut armed = self.lock();
            let Some(entry) = armed.get_mut(coordinator) else {
                return Ok(None);
            };
            Self::stop_fade(entry, fader);
            entry.timer.ends = entry.timer.ends.max(now) + delta(by);
            entry.timer.fading = false;
            entry.generation = generation;
            entry.running = false;
            entry.interrupted = false;
            entry.fading_rooms.clear();
            entry.fade_deadline = None;
            entry.timer.clone()
        };
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
        let generation = self.generation();
        {
            let mut armed = self.lock();
            let Some(mut entry) = armed.remove(old) else {
                // Cancelled meanwhile: the new group's own timer as it was.
                let had = armed.get(new).map(|a| a.timer.ends);
                drop(armed);
                own_timer(t, households, new, had, now);
                return Ok(false);
            };
            Self::stop_fade(&entry, fader);
            entry.timer.coordinator = new.clone();
            entry.timer.room = room_name(households, new);
            entry.timer.ends = ends;
            entry.timer.fading = false;
            entry.generation = generation;
            entry.running = false;
            entry.interrupted = false;
            entry.fading_rooms.clear();
            entry.fade_deadline = None;
            if let Some(replaced) = armed.insert(new.clone(), entry) {
                Self::stop_fade(&replaced, fader);
            }
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
        let mut armed = self.lock();
        match armed.get_mut(coordinator) {
            Some(a) if a.timer.fading || a.fade_deadline.is_some() => {
                // Before the run has taken up its rooms there is no fade to
                // stop yet: the run ends the timer when it gets there.
                a.interrupted |= a.fading_rooms.is_empty();
                Self::stop_fade(a, fader);
                true
            }
            _ => false,
        }
    }

    /// Arm `timer` as it was saved, leaving the player's own timer (its
    /// backstop, still running) as it is: how a restarted daemon takes up
    /// its timers. A group that already has a timer keeps it. Whether it was
    /// armed.
    pub fn adopt(&self, timer: SleepTimer) -> bool {
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
                fading_rooms: Vec::new(),
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
        let household = households.iter().find(|h| h.player(coordinator).is_some());
        let rooms = household.map_or_else(Vec::new, |h| member_rooms(h, coordinator));
        let joined = household
            .and_then(|h| h.coordinator_of(coordinator))
            .is_some_and(|lead| lead != coordinator);
        let (generation, deadline, claimed) = {
            let mut armed = self.lock();
            let Some(a) = armed.get_mut(coordinator).filter(|a| {
                a.timer.fading && !a.running && expected.is_none_or(|g| g == a.generation)
            }) else {
                return Ok(SleepOutcome::Gone);
            };
            // Validate the ticket before even retiring an absent player: an
            // obsolete worker must not consume a newly armed timer here.
            if household.is_none() {
                armed.remove(coordinator);
                return Err(CoreError::UnknownPlayer(coordinator.0.clone()));
            }
            a.running = true;
            if a.interrupted {
                // Someone set a volume before this run got going: the timer
                // ends where it is, as it would during the fade.
                armed.remove(coordinator);
                drop(armed);
                if !joined {
                    soap::configure_sleep_timer(t, ip(households, coordinator)?, None)?;
                }
                return Ok(SleepOutcome::Interrupted);
            }
            a.fading_rooms.clone_from(&rooms);
            // The fade's intents, taken under the lock a cancel or an extend
            // stops it under: one in between stops it too.
            (a.generation, a.fade_deadline, fader.claim(&rooms))
        };
        let before = match rooms
            .iter()
            .map(|r| Ok((r.clone(), soap::get_volume(t, ip(households, r)?)?)))
            .collect::<Result<Vec<_>, CoreError>>()
        {
            Ok(before) => before,
            Err(e) => {
                self.forget(coordinator, generation);
                return Err(e);
            }
        };
        // Whatever fails, the volumes go back (every room, the first failure
        // reported): a group is never left silent for its next play. The
        // player's own timer, set past the end, pauses it if the daemon can't.
        let restore = || put_back(t, households, &before);
        let deadline = deadline.unwrap_or_else(Instant::now);
        let outcome = match fader.fade_group_claimed_until(t, households, &claimed, 0, deadline) {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = restore();
                self.forget(coordinator, generation);
                return Err(e);
            }
        };
        let current = {
            let mut armed = self.lock();
            let current = armed.get(coordinator).map(|a| a.generation);
            if current == Some(generation) {
                armed.remove(coordinator);
            }
            current
        };
        if current != Some(generation) {
            restore()?;
            return Ok(SleepOutcome::Cancelled);
        }
        let host = ip(households, coordinator)?;
        match outcome {
            FadeOutcome::Reached => {
                // Paused first, the levels back after; the player's own timer
                // is cleared only once the pause has held.
                let stopped = if joined {
                    crate::control::leave(t, households, coordinator)
                } else {
                    crate::control::pause_at(t, host)
                };
                if let Err(e) = stopped {
                    let _ = restore();
                    return Err(e);
                }
                let restored = restore();
                soap::configure_sleep_timer(t, host, None)?;
                restored?;
                Ok(SleepOutcome::Paused)
            }
            FadeOutcome::Superseded { .. } => {
                if !joined {
                    soap::configure_sleep_timer(t, host, None)?;
                }
                Ok(SleepOutcome::Interrupted)
            }
        }
    }

    /// Drop `coordinator`'s timer if it is still the one a run took up as
    /// `generation`: a run that failed must not leave it fading for good.
    fn forget(&self, coordinator: &PlayerId, generation: u64) {
        let mut armed = self.lock();
        if armed
            .get(coordinator)
            .is_some_and(|a| a.generation == generation)
        {
            armed.remove(coordinator);
        }
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

/// Put each room back to its level in `before`: every room, even after one
/// fails, and the first failure reported.
pub(crate) fn put_back<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    before: &[(PlayerId, u8)],
) -> Result<(), CoreError> {
    let mut first = Ok(());
    for (room, level) in before {
        let done = ip(households, room)
            .and_then(|host| soap::set_volume(t, host, *level).map_err(CoreError::from));
        if first.is_ok() {
            first = done;
        }
    }
    first
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
                    fading_rooms: Vec::new(),
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
