//! Volume fades: smooth, cancellable changes instead of jumps.
//!
//! RampToVolume takes no duration (the player picks the speed; only
//! `SLEEP_TIMER_RAMP_TYPE` ramps linearly from the current level, at about
//! 1.25 steps a second), so a fade with a duration is a stepped SetVolume,
//! at most one step every [`DEFAULT_INTERVAL`]. Slow replies consume the
//! fade budget: obsolete steps are skipped instead of extending the fade.
//! A device-speed ramp uses
//! RampToVolume and, on a player that refuses it (UPnP 401/402), falls back
//! to stepping at the device's rate; the refusal is remembered per player.
//!
//! Every fade is one volume *intent* on its players. A newer intent on any
//! of them ([`Fader::supersede`], or a volume set through
//! [`Fader::set_volume`]) stops the fade before its next step, and waits out
//! a step (or a device-side ramp's start) already on its way, so that write
//! cannot land after the newer level; a device-side
//! ramp is stopped the only way a player allows, by sending SetVolume. The
//! daemon cancels a fade on Cx cancellation by superseding it. A stopping
//! daemon finishes its fades at once ([`Fader::finish_all`]).

use crate::snapshot::member_rooms;
use crate::{CoreError, HouseholdState};
use fsonos_proto::control::{self as soap, RampType};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

mod timing;
use timing::Timing;

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
        over.as_nanos().div_ceil(interval.as_nanos()).max(1)
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
    /// Per player, held while a fade checks its intent and writes a step,
    /// and by [`Self::supersede`]: a newer intent waits for a step in flight,
    /// so no older fade's write lands after the caller's own.
    steps: Mutex<HashMap<PlayerId, Arc<Mutex<()>>>>,
    no_device_ramp: Mutex<HashSet<PlayerId>>,
    interval: Duration,
    device_step: Duration,
    /// Set once the daemon stops: every fade takes its last step at once.
    finishing: AtomicBool,
    /// Fades in flight, each with its pause and restore.
    running: AtomicUsize,
}

/// One fade in flight, for [`Fader::finish_all`] to wait for.
struct Running<'a>(&'a AtomicUsize);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
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
            steps: Mutex::new(HashMap::new()),
            no_device_ramp: Mutex::new(HashSet::new()),
            interval,
            device_step,
            finishing: AtomicBool::new(false),
            running: AtomicUsize::new(0),
        }
    }

    /// Finish every fade in flight at once, and any started from now on: each
    /// sets its last step now, so a fade-out pauses its group and puts the
    /// volumes back, and a ramp reaches its level. For a stopping daemon,
    /// which would otherwise strand its fades part-way. Whether they all
    /// ended within `timeout`.
    pub fn finish_all(&self, timeout: Duration) -> bool {
        self.finishing.store(true, Ordering::Release);
        let deadline = Instant::now() + timeout;
        while self.running.load(Ordering::Acquire) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn running(&self) -> Running<'_> {
        self.running.fetch_add(1, Ordering::AcqRel);
        Running(&self.running)
    }

    /// `player`'s step lock (see `steps`).
    fn step(&self, player: &PlayerId) -> Arc<Mutex<()>> {
        let mut steps = self.steps.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(steps.entry(player.clone()).or_default())
    }

    /// Start a new volume intent on `player`: any fade in flight on it stops
    /// before its next step, and a step already on its way has landed when
    /// this returns. Returns the intent's number.
    pub fn supersede(&self, player: &PlayerId) -> u64 {
        let step = self.step(player);
        let _landed = step.lock().unwrap_or_else(PoisonError::into_inner);
        self.next_intent(player)
    }

    /// Retire an intent only if it still belongs to the caller. A cancelled
    /// sleep fade can retain the returned claim for restoration without taking
    /// ownership back from a newer manual level or another fade.
    pub(crate) fn supersede_claimed(&self, player: &PlayerId, intent: u64) -> Option<u64> {
        let step = self.step(player);
        let _landed = step.lock().unwrap_or_else(PoisonError::into_inner);
        (self.current(player) == intent).then(|| self.next_intent(player))
    }

    /// Run a final group action only while every room still has its original
    /// intent. Sorted locks serialize it with manual changes and overlapping
    /// group fades, without holding the intent-map lock during device I/O.
    pub(crate) fn with_claimed<R>(
        &self,
        claimed: &[(PlayerId, u64)],
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let mut steps: Vec<_> = claimed.iter().map(|(p, _)| (p, self.step(p))).collect();
        steps.sort_unstable_by(|(a, _), (b, _)| a.0.cmp(&b.0));
        steps.dedup_by(|(a, _), (b, _)| a == b);
        let _writing: Vec<_> = steps
            .iter()
            .map(|(_, step)| step.lock().unwrap_or_else(PoisonError::into_inner))
            .collect();
        claimed
            .iter()
            .all(|(p, intent)| self.current(p) == *intent)
            .then(action)
    }

    /// `player`'s next intent number; the caller holds its step lock.
    fn next_intent(&self, player: &PlayerId) -> u64 {
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
        let claimed = self.claim(players);
        self.stepped(
            t,
            households,
            &claimed,
            to,
            Timing::new(over, self.interval),
        )
    }

    /// Start a volume intent on each of `players` now, for a fade that
    /// begins a little later ([`Self::fade_group_claimed_until`]): a newer intent
    /// in between (a cancel, say) stops it before its first step.
    pub fn claim(&self, players: &[PlayerId]) -> Vec<(PlayerId, u64)> {
        players
            .iter()
            .map(|p| (p.clone(), self.supersede(p)))
            .collect()
    }

    /// Fade under previously claimed intents, finishing at a fixed monotonic
    /// deadline. State reads and slow speaker replies consume this budget too.
    pub fn fade_group_claimed_until<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        claimed: &[(PlayerId, u64)],
        to: u8,
        deadline: Instant,
    ) -> Result<FadeOutcome, CoreError> {
        self.stepped(
            t,
            households,
            claimed,
            to,
            Timing::until(deadline, self.interval),
        )
    }

    /// Fade each player to its own level, all in step over `over`, each from
    /// its own current level. A player that gets a newer intent drops out
    /// and keeps it while the others carry on; those players are returned.
    pub fn fade_each<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        targets: &[(PlayerId, u8)],
        over: Duration,
    ) -> Result<Vec<PlayerId>, CoreError> {
        let _running = self.running();
        let timing = Timing::new(over, self.interval);
        // Claim before any state read: a newer intent during a slow read wins.
        let claimed: Vec<_> = targets
            .iter()
            .map(|(player, _)| self.supersede(player))
            .collect();
        let mut lanes = Vec::with_capacity(targets.len());
        for ((player, to), intent) in targets.iter().zip(claimed) {
            let host = ip(households, player)?;
            let from = soap::get_volume(t, host)?;
            lanes.push((player, host, intent, from, *to, from));
        }
        let mut left: Vec<PlayerId> = Vec::new();
        while left.len() < lanes.len() {
            let mut complete = true;
            for (player, host, intent, from, to, last) in &mut lanes {
                if left.contains(player) {
                    continue;
                }
                let step = self.step(player);
                let _writing = step.lock().unwrap_or_else(PoisonError::into_inner);
                if self.current(player) != *intent {
                    left.push((*player).clone());
                    continue;
                }
                let frame = timing.frame(Instant::now(), self.finishing.load(Ordering::Acquire));
                let level = frame.level(*from, *to);
                if level != *last || frame.complete {
                    soap::set_volume(t, *host, level)?;
                    *last = level;
                }
                complete &= frame.complete;
            }
            if complete {
                break;
            }
            timing.wait();
        }
        Ok(left)
    }

    fn stepped<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        claimed: &[(PlayerId, u64)],
        to: u8,
        timing: Timing,
    ) -> Result<FadeOutcome, CoreError> {
        let _running = self.running();
        let mut lanes = Vec::with_capacity(claimed.len());
        for (p, intent) in claimed {
            let host = ip(households, p)?;
            let from = soap::get_volume(t, host)?;
            lanes.push((p, host, *intent, from, from));
        }
        let mut first_level = lanes.first().map_or(to, |l| l.3);
        loop {
            let mut complete = true;
            for (i, (player, host, intent, from, last)) in lanes.iter_mut().enumerate() {
                let step = self.step(player);
                let _writing = step.lock().unwrap_or_else(PoisonError::into_inner);
                if self.current(player) != *intent {
                    return Ok(FadeOutcome::Superseded { at: first_level });
                }
                let frame = timing.frame(Instant::now(), self.finishing.load(Ordering::Acquire));
                let level = frame.level(*from, to);
                if level != *last || frame.complete {
                    soap::set_volume(t, *host, level)?;
                    *last = level;
                }
                if i == 0 {
                    first_level = level;
                }
                complete &= frame.complete;
            }
            if complete {
                break;
            }
            timing.wait();
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
            // The ramp starts under the step lock, as a step is written: a
            // newer level waits for the start to land, then stops the ramp.
            let step = self.step(player);
            let _starting = step.lock().unwrap_or_else(PoisonError::into_inner);
            self.next_intent(player);
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
        let claimed = self.claim(std::slice::from_ref(player));
        let stepped = self.stepped(
            t,
            households,
            &claimed,
            to,
            Timing::new(over, self.device_step),
        )?;
        Ok(RampOutcome::Stepped(stepped))
    }

    /// Fade the group `coordinator` leads down to silence over `over`, pause
    /// it, then put every member room's volume back where it was, so the next
    /// play is not silent. A newer intent before the pause prevents it; a
    /// newer level during recovery or restoration keeps that room's volume.
    pub fn fade_out_and_pause<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        over: Duration,
    ) -> Result<FadeOutcome, CoreError> {
        let _running = self.running();
        let household = households
            .iter()
            .find(|h| h.player(coordinator).is_some())
            .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
        let members = member_rooms(household, coordinator);
        let timing = Timing::new(over, self.interval);
        let claimed = self.claim(&members);
        let before = claimed
            .iter()
            .map(|(player, intent)| {
                Ok((
                    player.clone(),
                    *intent,
                    soap::get_volume(t, ip(households, player)?)?,
                ))
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        // Restore rooms this fade still owns, including after a failed step
        // or pause. A newer volume must never be overwritten by old recovery.
        let put_back = || self.restore(t, households, &before);
        let outcome = match self.stepped(t, households, &claimed, 0, timing) {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = put_back();
                return Err(e);
            }
        };
        if outcome != FadeOutcome::Reached {
            return Ok(outcome);
        }
        // A newer intent on an earlier member can arrive while the last
        // member finishes its final frame. Recheck every claim before pausing.
        let Some(paused) = self.with_claimed(&claimed, || {
            crate::control::pause(t, households, coordinator)
        }) else {
            return Ok(FadeOutcome::Superseded { at: 0 });
        };
        let restored = put_back();
        paused?;
        restored?;
        Ok(FadeOutcome::Reached)
    }

    /// Recover every room still owned by this fade, reporting the first
    /// failure after trying the rest. Supersession and each restore share the
    /// step lock: a newer level either comes afterwards or prevents the write.
    pub(crate) fn restore<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        before: &[(PlayerId, u64, u8)],
    ) -> Result<(), CoreError> {
        let mut first = Ok(());
        for (room, intent, level) in before {
            let step = self.step(room);
            let _writing = step.lock().unwrap_or_else(PoisonError::into_inner);
            if self.current(room) != *intent {
                continue;
            }
            let done = ip(households, room)
                .and_then(|host| soap::set_volume(t, host, *level).map_err(CoreError::from));
            if first.is_ok() {
                first = done;
            }
        }
        first
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

    /// A transport that holds the first `action` it is sent until released.
    struct Held {
        inner: fsonos_sim::SimLan,
        action: &'static str,
        host: Option<IpAddr>,
        level: Option<u8>,
        holding: AtomicBool,
        entered: AtomicBool,
        release: AtomicBool,
    }

    impl Held {
        fn new(inner: fsonos_sim::SimLan, action: &'static str) -> Self {
            Self {
                inner,
                action,
                host: None,
                level: None,
                holding: AtomicBool::new(true),
                entered: AtomicBool::new(false),
                release: AtomicBool::new(false),
            }
        }
    }

    impl Transport for Held {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            let held = action
                .trim_matches('"')
                .ends_with(&format!("#{}", self.action))
                && self.host.is_none_or(|target| target == host)
                && self.level.is_none_or(|level| {
                    body.contains(&format!("<DesiredVolume>{level}</DesiredVolume>"))
                });
            if held && self.holding.swap(false, Ordering::AcqRel) {
                self.entered.store(true, Ordering::Release);
                while !self.release.load(Ordering::Acquire) {
                    std::thread::sleep(MS(1));
                }
            }
            self.inner.soap_post(host, path, action, body)
        }
    }

    /// A simulated Kitchen at volume 40: the sim, its address, its id and
    /// its household.
    fn kitchen_at_40() -> (fsonos_sim::SimHandle, IpAddr, PlayerId, Vec<HouseholdState>) {
        use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
        let sim = SimHousehold::builder()
            .s2([SimPlayerSpec::new("Kitchen", SimModel::One)])
            .spawn()
            .unwrap();
        let host = sim.player("Kitchen").unwrap().ip;
        let player = PlayerId(sim.player("Kitchen").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(
            &fsonos_proto::topology::get_zone_group_state(&sim.lan(), host).unwrap(),
        );
        soap::set_volume(&sim.lan(), host, 40).unwrap();
        (sim, host, player, vec![house])
    }

    /// Someone sets 50 while `held`'s write is on its way; it is released
    /// 200 ms later. Returns what `write` returned.
    fn set_50_meanwhile<R: Send>(
        fader: &Fader,
        held: &Held,
        houses: &[HouseholdState],
        player: &PlayerId,
        write: impl FnOnce() -> R + Send,
    ) -> R {
        std::thread::scope(|s| {
            let writing = s.spawn(write);
            while !held.entered.load(Ordering::Acquire) {
                std::thread::sleep(MS(1));
            }
            let set = s.spawn(|| fader.set_volume(&held.inner, houses, player, 50));
            std::thread::sleep(MS(200));
            held.release.store(true, Ordering::Release);
            set.join().unwrap().unwrap();
            writing.join().unwrap()
        })
    }

    #[test]
    fn a_newer_level_lands_after_a_fade_step_already_on_its_way() {
        let (sim, host, player, houses) = kitchen_at_40();
        let held = Held::new(sim.lan(), "SetVolume");
        let fader = Fader::with_timing(MS(50), MS(50));
        let outcome = set_50_meanwhile(&fader, &held, &houses, &player, || {
            fader.fade(&held, &houses, &player, 0, MS(1000))
        });
        assert!(
            matches!(outcome, Ok(FadeOutcome::Superseded { .. })),
            "{outcome:?}"
        );
        assert_eq!(
            soap::get_volume(&sim.lan(), host).unwrap(),
            50,
            "the newer level stands"
        );
        sim.shutdown();
    }

    #[test]
    fn a_newer_level_lands_after_a_device_ramp_start_on_its_way() {
        let (sim, host, player, houses) = kitchen_at_40();
        let held = Held::new(sim.lan(), "RampToVolume");
        let fader = Fader::with_timing(MS(50), MS(50));
        // The sim ramps at once, so a start landing after the newer level
        // would leave the ramp's 10.
        let outcome = set_50_meanwhile(&fader, &held, &houses, &player, || {
            fader.ramp_at_device_speed(&held, &houses, &player, 10)
        });
        assert!(
            matches!(outcome, Ok(RampOutcome::Device { .. })),
            "{outcome:?}"
        );
        assert_eq!(
            soap::get_volume(&sim.lan(), host).unwrap(),
            50,
            "the newer level stands"
        );
        sim.shutdown();
    }

    #[test]
    fn a_level_set_during_a_fade_out_pause_survives_success_and_failure() {
        for fail in [false, true] {
            let (sim, host, player, houses) = kitchen_at_40();
            crate::control::play_uri(
                &sim.lan(),
                &houses,
                &player,
                "x-rincon-mp3radio://stream.example.invalid/fade.mp3",
                "",
            )
            .unwrap();
            if fail {
                sim.upnp_fault("Kitchen", "Pause", 500).unwrap();
            }
            let held = Held::new(sim.lan(), "Pause");
            let fader = Fader::default();
            let outcome = set_50_meanwhile(&fader, &held, &houses, &player, || {
                fader.fade_out_and_pause(&held, &houses, &player, Duration::ZERO)
            });
            if fail {
                assert!(matches!(
                    outcome,
                    Err(CoreError::Proto(ProtoError::SoapFault { code: 500, .. }))
                ));
            } else {
                assert_eq!(outcome.unwrap(), FadeOutcome::Reached);
            }
            assert_eq!(
                soap::get_volume(&sim.lan(), host).unwrap(),
                50,
                "pause success or failure must not restore over a newer level"
            );
        }
    }

    #[test]
    fn a_newer_level_waits_out_a_fade_out_restore_already_on_its_way() {
        let (sim, host, player, houses) = kitchen_at_40();
        let mut held = Held::new(sim.lan(), "SetVolume");
        held.level = Some(40);
        let fader = Fader::default();
        let outcome = set_50_meanwhile(&fader, &held, &houses, &player, || {
            fader.fade_out_and_pause(&held, &houses, &player, Duration::ZERO)
        });
        assert_eq!(outcome.unwrap(), FadeOutcome::Reached);
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 50);
    }

    #[test]
    fn an_override_during_the_last_group_step_prevents_the_pause() {
        use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
        let sim = SimHousehold::builder()
            .s2([
                SimPlayerSpec::new("Kitchen", SimModel::One),
                SimPlayerSpec::new("Office", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let host = sim.player("Kitchen").unwrap().ip;
        let player = PlayerId(sim.player("Kitchen").unwrap().uuid.clone());
        let office = PlayerId(sim.player("Office").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&fsonos_proto::topology::get_zone_group_state(&lan, host).unwrap());
        crate::control::join(&lan, &[house.clone()], &office, &player).unwrap();
        house.apply_topology(&fsonos_proto::topology::get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        let members = member_rooms(&houses[0], &player);
        assert_eq!(members.len(), 2);
        let overridden = &members[0];
        let held_host = ip(&houses, &members[1]).unwrap();
        crate::control::play_uri(
            &lan,
            &houses,
            &player,
            "x-rincon-mp3radio://stream.example.invalid/fade.mp3",
            "",
        )
        .unwrap();
        let mut held = Held::new(lan.clone(), "SetVolume");
        held.host = Some(held_host);
        held.level = Some(0);
        let fader = Fader::default();
        let outcome = set_50_meanwhile(&fader, &held, &houses, overridden, || {
            fader.fade_out_and_pause(&held, &houses, &player, Duration::ZERO)
        });
        assert_eq!(outcome.unwrap(), FadeOutcome::Superseded { at: 0 });
        assert_eq!(
            soap::get_volume(&lan, ip(&houses, overridden).unwrap()).unwrap(),
            50
        );
        assert_eq!(
            soap::get_transport_info(&lan, host).unwrap().state,
            fsonos_types::TransportState::Playing
        );
        assert!(sim.soap_log().iter().all(|entry| entry.action != "Pause"));
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
