//! Volume fades: smooth, cancellable changes instead of jumps.
//!
//! RampToVolume takes no duration (the player picks the speed; only
//! `SLEEP_TIMER_RAMP_TYPE` ramps linearly from the current level, at about
//! 1.25 steps a second), so a fade with a duration is a stepped SetVolume,
//! one step every [`DEFAULT_INTERVAL`]. A device-speed ramp uses
//! RampToVolume and, on a player that refuses it (UPnP 401/402), falls back
//! to stepping at the device's rate; the refusal is remembered per player.
//!
//! Every fade is one volume *intent* on its players. A newer intent on any
//! of them ([`Fader::supersede`], or a volume set through
//! [`Fader::set_volume`]) stops the fade before its next step; a device-side
//! ramp is stopped the only way a player allows, by sending SetVolume. The
//! daemon cancels a fade on Cx cancellation by superseding it.

use crate::snapshot::member_rooms;
use crate::{CoreError, HouseholdState};
use fsonos_proto::control::{self as soap, RampType};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

/// Time between the steps of a fade.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(250);
/// How long a player's own SLEEP_TIMER ramp spends per volume step.
pub const DEVICE_STEP: Duration = Duration::from_millis(800);

/// How a fade ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeOutcome {
    /// Every player reached its target.
    Reached,
    /// A newer volume intent took over; `at` is the last level this fade set
    /// on the first player.
    Superseded { at: u8 },
}

/// How a device-speed ramp was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampOutcome {
    /// The player ramps itself; it reported this many seconds.
    Device { secs: u32 },
    /// The player refused RampToVolume, so the ramp was stepped.
    Stepped(FadeOutcome),
}

/// The levels a fade from `from` to `to` sets, one per step of `interval`
/// over `over`; the last is always `to`. A zero `over` is one step.
#[must_use]
pub fn plan_steps(from: u8, to: u8, over: Duration, interval: Duration) -> Vec<u8> {
    let ticks = if interval.is_zero() {
        1
    } else {
        over.as_millis().div_ceil(interval.as_millis()).max(1)
    };
    let n = i64::try_from(ticks).unwrap_or(i64::MAX);
    let (from, to) = (i64::from(from), i64::from(to));
    (1..=n)
        .map(|k| u8::try_from(from + (to - from) * k / n).unwrap_or(0))
        .collect()
}

/// Runs fades and tracks the current volume intent per player. Share one per
/// daemon (it is `Sync`).
#[derive(Debug)]
pub struct Fader {
    intents: Mutex<HashMap<PlayerId, u64>>,
    no_device_ramp: Mutex<HashSet<PlayerId>>,
    interval: Duration,
    device_step: Duration,
}

impl Default for Fader {
    fn default() -> Self {
        Self::with_timing(DEFAULT_INTERVAL, DEVICE_STEP)
    }
}

impl Fader {
    /// A fader stepping every `interval`, and stepping device-speed ramps
    /// (on players without RampToVolume) every `device_step`.
    #[must_use]
    pub fn with_timing(interval: Duration, device_step: Duration) -> Self {
        Self {
            intents: Mutex::new(HashMap::new()),
            no_device_ramp: Mutex::new(HashSet::new()),
            interval,
            device_step,
        }
    }

    /// Start a new volume intent on `player`: any fade in flight on it stops
    /// before its next step. Returns the intent's number.
    pub fn supersede(&self, player: &PlayerId) -> u64 {
        let mut intents = self
            .intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = intents.entry(player.clone()).or_insert(0);
        *n += 1;
        *n
    }

    fn current(&self, player: &PlayerId) -> u64 {
        self.intents
            .lock()
            .map_or(0, |m| m.get(player).copied().unwrap_or(0))
    }

    /// Set `player`'s volume now, superseding any fade on it.
    pub fn set_volume<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        player: &PlayerId,
        level: u8,
    ) -> Result<(), CoreError> {
        self.supersede(player);
        Ok(soap::set_volume(t, ip(households, player)?, level)?)
    }

    /// Fade `player` to `to` over `over`.
    pub fn fade<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        player: &PlayerId,
        to: u8,
        over: Duration,
    ) -> Result<FadeOutcome, CoreError> {
        self.fade_group(t, households, std::slice::from_ref(player), to, over)
    }

    /// Fade every player in `players` to `to` together, each from its own
    /// current level, over `over`. Stops as soon as any of them gets a newer
    /// intent.
    pub fn fade_group<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        players: &[PlayerId],
        to: u8,
        over: Duration,
    ) -> Result<FadeOutcome, CoreError> {
        self.stepped(t, households, players, to, over, self.interval)
    }

    fn stepped<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        players: &[PlayerId],
        to: u8,
        over: Duration,
        interval: Duration,
    ) -> Result<FadeOutcome, CoreError> {
        let mut lanes = Vec::with_capacity(players.len());
        for p in players {
            let host = ip(households, p)?;
            let from = soap::get_volume(t, host)?;
            lanes.push((
                p,
                host,
                self.supersede(p),
                from,
                plan_steps(from, to, over, interval),
            ));
        }
        let ticks = lanes.first().map_or(0, |l| l.4.len());
        let mut first_level = lanes.first().map_or(to, |l| l.3);
        for tick in 0..ticks {
            if tick > 0 {
                std::thread::sleep(interval);
            }
            for (i, (player, host, intent, from, steps)) in lanes.iter().enumerate() {
                if self.current(player) != *intent {
                    return Ok(FadeOutcome::Superseded { at: first_level });
                }
                let level = steps[tick];
                let before = if tick == 0 { *from } else { steps[tick - 1] };
                if level != before || tick == 0 {
                    soap::set_volume(t, *host, level)?;
                }
                if i == 0 {
                    first_level = level;
                }
            }
        }
        Ok(FadeOutcome::Reached)
    }

    /// Let `player` ramp itself to `to` at its own speed (RampToVolume,
    /// SLEEP_TIMER); on a player that refuses it, step there at the same
    /// rate instead, and remember the refusal.
    pub fn ramp_at_device_speed<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        player: &PlayerId,
        to: u8,
    ) -> Result<RampOutcome, CoreError> {
        let host = ip(households, player)?;
        let refused = self.no_device_ramp.lock().is_ok_and(|s| s.contains(player));
        if !refused {
            self.supersede(player);
            match soap::ramp_to_volume(t, host, RampType::SleepTimer, to) {
                Ok(secs) => return Ok(RampOutcome::Device { secs }),
                Err(ProtoError::SoapFault {
                    code: 401 | 402, ..
                }) => {
                    if let Ok(mut s) = self.no_device_ramp.lock() {
                        s.insert(player.clone());
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        let from = soap::get_volume(t, host)?;
        let over = self.device_step * u32::from(from.abs_diff(to));
        let stepped = self.stepped(
            t,
            households,
            std::slice::from_ref(player),
            to,
            over,
            self.device_step,
        )?;
        Ok(RampOutcome::Stepped(stepped))
    }

    /// Fade the group `coordinator` leads down to silence over `over`, pause
    /// it, then put every member room's volume back where it was, so the next
    /// play is not silent. Nothing is paused if the fade is superseded.
    pub fn fade_out_and_pause<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        over: Duration,
    ) -> Result<FadeOutcome, CoreError> {
        let household = households
            .iter()
            .find(|h| h.player(coordinator).is_some())
            .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
        let members = member_rooms(household, coordinator);
        let before = members
            .iter()
            .map(|m| Ok((m.clone(), soap::get_volume(t, ip(households, m)?)?)))
            .collect::<Result<Vec<_>, CoreError>>()?;
        let outcome = self.fade_group(t, households, &members, 0, over)?;
        if outcome != FadeOutcome::Reached {
            return Ok(outcome);
        }
        soap::pause(t, ip(households, coordinator)?)?;
        for (player, level) in before {
            soap::set_volume(t, ip(households, &player)?, level)?;
        }
        Ok(FadeOutcome::Reached)
    }
}

fn ip(households: &[HouseholdState], player: &PlayerId) -> Result<IpAddr, CoreError> {
    households
        .iter()
        .find_map(|h| h.player(player))
        .map(|p| p.ip)
        .ok_or_else(|| CoreError::UnknownPlayer(player.0.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn steps_are_even_and_end_on_the_target() {
        assert_eq!(plan_steps(20, 80, MS(1000), MS(250)), [35, 50, 65, 80]);
        assert_eq!(plan_steps(30, 0, MS(1000), MS(250)), [23, 15, 8, 0]);
        assert_eq!(
            plan_steps(30, 0, MS(900), MS(250)),
            [23, 15, 8, 0],
            "a partial tick rounds up"
        );
        assert_eq!(plan_steps(10, 90, MS(0), MS(250)), [90]);
        assert_eq!(plan_steps(40, 40, MS(500), MS(250)), [40, 40]);
        assert_eq!(plan_steps(0, 100, MS(5000), MS(250)).len(), 20);
        assert_eq!(*plan_steps(0, 100, MS(5000), MS(250)).last().unwrap(), 100);
    }

    #[test]
    fn a_newer_intent_supersedes() {
        let f = Fader::default();
        let p = PlayerId("RINCON_A".into());
        let first = f.supersede(&p);
        assert_eq!(f.current(&p), first);
        let second = f.supersede(&p);
        assert!(second > first);
        assert_eq!(f.current(&PlayerId("RINCON_B".into())), 0);
    }
}
