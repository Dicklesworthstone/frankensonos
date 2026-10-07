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
//! anyone else during the fade ends the timer where it is.

use crate::fade::{FadeOutcome, Fader};
use crate::snapshot::member_rooms;
use crate::{CoreError, HouseholdState};
use chrono::{DateTime, TimeDelta, Utc};
use fsonos_proto::Transport;
use fsonos_proto::control as soap;
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// How long before the end the fade starts.
pub const FADE: Duration = Duration::from_mins(2);

/// How long after the daemon's timer the player's own one pauses, should
/// the daemon not be running then.
pub const BACKSTOP: Duration = Duration::from_mins(5);

/// An armed sleep timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SleepTimer {
    pub coordinator: PlayerId,
    /// The room it was set on, for messages.
    pub room: String,
    pub ends: DateTime<Utc>,
    /// The fade has started.
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
    /// Changes whenever the timer is re-armed, so a fade can tell.
    generation: u64,
    /// The rooms a running fade is turning down.
    fading_rooms: Vec<PlayerId>,
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
        let room = households
            .iter()
            .flat_map(|h| &h.rooms)
            .find(|r| r.players.contains(coordinator))
            .map_or_else(|| coordinator.0.clone(), |r| r.name.clone());
        let timer = SleepTimer {
            coordinator: coordinator.clone(),
            room,
            ends,
            fading: false,
        };
        let generation = self.generation();
        if let Some(old) = self.lock().insert(
            coordinator.clone(),
            Armed {
                timer: timer.clone(),
                generation,
                fading_rooms: Vec::new(),
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
            entry.fading_rooms.clear();
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
    /// (it is marked as fading). Run [`Self::run`] for each.
    pub fn due(&self, now: DateTime<Utc>) -> Vec<SleepTimer> {
        let mut due: Vec<SleepTimer> = self
            .lock()
            .values_mut()
            .filter(|a| !a.timer.fading && now >= a.timer.ends - delta(self.fade))
            .map(|a| {
                a.timer.fading = true;
                a.timer.clone()
            })
            .collect();
        due.sort_by_key(|t| t.ends);
        due
    }

    /// Fade the group `coordinator` leads out over the fade time, pause it,
    /// and put its rooms' volumes back. Blocks for the fade.
    pub fn run<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        coordinator: &PlayerId,
    ) -> Result<SleepOutcome, CoreError> {
        let household = households
            .iter()
            .find(|h| h.player(coordinator).is_some())
            .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
        let rooms = member_rooms(household, coordinator);
        let generation = {
            let mut armed = self.lock();
            match armed.get_mut(coordinator) {
                Some(a) if a.timer.fading => {
                    a.fading_rooms.clone_from(&rooms);
                    a.generation
                }
                _ => return Ok(SleepOutcome::Gone),
            }
        };
        let before = rooms
            .iter()
            .map(|r| Ok((r.clone(), soap::get_volume(t, ip(households, r)?)?)))
            .collect::<Result<Vec<_>, CoreError>>()?;
        let outcome = fader.fade_group(t, households, &rooms, 0, self.fade)?;
        let current = {
            let mut armed = self.lock();
            let current = armed.get(coordinator).map(|a| a.generation);
            if current == Some(generation) {
                armed.remove(coordinator);
            }
            current
        };
        let put_back = || -> Result<(), CoreError> {
            for (room, level) in &before {
                soap::set_volume(t, ip(households, room)?, *level)?;
            }
            Ok(())
        };
        if current != Some(generation) {
            put_back()?;
            return Ok(SleepOutcome::Cancelled);
        }
        let host = ip(households, coordinator)?;
        soap::configure_sleep_timer(t, host, None)?;
        match outcome {
            FadeOutcome::Reached => {
                soap::pause(t, host)?;
                put_back()?;
                Ok(SleepOutcome::Paused)
            }
            FadeOutcome::Superseded { .. } => Ok(SleepOutcome::Interrupted),
        }
    }
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
                    fading_rooms: Vec::new(),
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
}
