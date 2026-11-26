//! An interrupted sleep fade may resume only for the announcement that stopped it.

use super::{
    Armed, CoreError, DateTime, Fader, HouseholdState, PlayerId, SleepTimer, SleepTimers,
    Transport, Utc, backstop, delta, ip, room_name,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// A single announcement's right to resume an interrupted sleep fade.
/// Cancellation, replacement, extension, movement and manual level changes
/// invalidate it, even after the interrupted worker has retired its timer.
/// Dropping it leaves the interrupted timer ended; it never starts playback.
#[derive(Debug)]
pub struct SleepResume {
    coordinator: PlayerId,
    current: Arc<AtomicBool>,
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    fn timer() -> SleepTimer {
        SleepTimer {
            coordinator: PlayerId("RINCON_ANNOUNCE_TEST".into()),
            room: "Sleep room".into(),
            ends: DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
                .unwrap()
                .to_utc(),
            fading: false,
        }
    }

    #[test]
    fn a_future_sleep_is_neither_a_checkpoint_candidate_nor_interrupted() {
        let timers = SleepTimers::new();
        let timer = timer();
        assert!(timers.adopt(timer.clone()));
        assert!(
            timers
                .announcement_interruption(&timer.coordinator)
                .is_none()
        );
        assert!(
            timers
                .interrupt_for_announcement_matching(&Fader::default(), &timer.coordinator, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(timers.get(&timer.coordinator), Some(timer));
    }

    #[test]
    fn a_durable_sleep_identity_authorizes_only_one_matching_interruption() {
        let timers = SleepTimers::new();
        let timer = timer();
        assert!(timers.adopt(timer.clone()));
        assert_eq!(timers.due(timer.ends).len(), 1);
        let expected = timers
            .announcement_interruption(&timer.coordinator)
            .unwrap();
        let fader = Fader::default();
        let receipt = timers
            .interrupt_for_announcement_matching(&fader, &timer.coordinator, Some(&expected))
            .unwrap()
            .unwrap();
        assert_eq!(receipt.coordinator(), &timer.coordinator);
        assert!(
            timers
                .announcement_interruption(&timer.coordinator)
                .is_none()
        );
        assert!(
            timers
                .interrupt_for_announcement_matching(&fader, &timer.coordinator, Some(&expected))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_unrecorded_or_replaced_due_sleep_keeps_its_newer_intent() {
        let timers = SleepTimers::new();
        let original = timer();
        let mut newer = original.clone();
        newer.ends += chrono::TimeDelta::seconds(1);
        assert!(timers.adopt(newer.clone()));
        assert_eq!(timers.due(newer.ends).len(), 1);
        let fader = Fader::default();
        for expected in [None, Some(&original)] {
            assert!(
                timers
                    .interrupt_for_announcement_matching(&fader, &newer.coordinator, expected)
                    .is_err()
            );
            assert_eq!(
                timers
                    .announcement_interruption(&newer.coordinator)
                    .unwrap()
                    .ends,
                newer.ends
            );
        }
        assert!(
            timers
                .interrupt_for_announcement_matching(&fader, &newer.coordinator, Some(&newer))
                .unwrap()
                .is_some()
        );
    }
}

impl SleepResume {
    #[must_use]
    pub fn coordinator(&self) -> &PlayerId {
        &self.coordinator
    }
}

impl Drop for SleepResume {
    fn drop(&mut self) {
        self.current.store(false, Ordering::Release);
    }
}

/// Kept separately from Armed: the old worker may finish while the clip plays.
/// Timer mutations and receipt creation take the timer-map lock before this
/// short local lock. No receipt lock is held during speaker I/O.
#[derive(Debug, Default)]
pub(super) struct Resumes(Mutex<HashMap<PlayerId, Arc<AtomicBool>>>);

impl Resumes {
    fn issue(&self, coordinator: &PlayerId) -> SleepResume {
        let current = Arc::new(AtomicBool::new(true));
        let old = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(coordinator.clone(), Arc::clone(&current));
        if let Some(old) = old {
            old.store(false, Ordering::Release);
        }
        SleepResume {
            coordinator: coordinator.clone(),
            current,
        }
    }

    pub(super) fn supersede(&self, coordinator: &PlayerId) {
        if let Some(old) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(coordinator)
        {
            old.store(false, Ordering::Release);
        }
    }

    fn owns(&self, receipt: &SleepResume) -> bool {
        receipt.current.load(Ordering::Acquire)
            && self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&receipt.coordinator)
                .is_some_and(|current| Arc::ptr_eq(current, &receipt.current))
    }

    fn finish(&self, receipt: &SleepResume) {
        let mut current = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if current
            .get(&receipt.coordinator)
            .is_some_and(|flag| Arc::ptr_eq(flag, &receipt.current))
        {
            current.remove(&receipt.coordinator);
        }
        receipt.current.store(false, Ordering::Release);
    }
}

impl SleepTimers {
    /// Observe the exact due fade an announcement may suspend, before its
    /// durable checkpoint. A future timer is independent listener intent.
    #[must_use]
    pub fn announcement_interruption(&self, coordinator: &PlayerId) -> Option<SleepTimer> {
        self.lock()
            .get(coordinator)
            .filter(|entry| {
                (entry.timer.fading || entry.fade_deadline.is_some() || entry.recovery.is_some())
                    && !entry.interrupted
            })
            .map(|entry| entry.timer.clone())
    }

    /// A newly accepted playback or grouping command supersedes an
    /// announcement's continuation, without changing an ordinary sleep timer.
    /// Call before speaker I/O so an ambiguous command failure cannot revive
    /// the older continuation.
    pub fn supersede_announcement(&self, coordinator: &PlayerId) {
        let _armed = self.lock();
        self.resumes.supersede(coordinator);
    }

    /// End a due fade for a temporary announcement, retaining one conditional
    /// right to restart it after the original music has been restored. The
    /// first room in a group claims it; later rooms must not renew that right.
    pub fn interrupt_for_announcement(
        &self,
        fader: &Fader,
        coordinator: &PlayerId,
    ) -> Option<SleepResume> {
        let (receipt, claims) = {
            let mut armed = self.lock();
            let entry = armed.get_mut(coordinator).filter(|entry| {
                (entry.timer.fading || entry.fade_deadline.is_some() || entry.recovery.is_some())
                    && !entry.interrupted
            })?;
            entry.interrupted = true;
            let receipt = self.resumes.issue(coordinator);
            (receipt, entry.fade_claims.clone())
        };
        if let Some(claims) = claims {
            claims.stop(fader, false);
        }
        Some(receipt)
    }

    /// Suspend only the exact fade recorded before an announcement's first
    /// durable checkpoint. A newly due/replaced timer cannot be interrupted
    /// without being represented in that proof. The comparison and admission
    /// share the timer-map lock, including with timer replacement.
    pub fn interrupt_for_announcement_matching(
        &self,
        fader: &Fader,
        coordinator: &PlayerId,
        expected: Option<&SleepTimer>,
    ) -> Result<Option<SleepResume>, CoreError> {
        let (receipt, claims) = {
            let mut armed = self.lock();
            let Some(entry) = armed.get_mut(coordinator).filter(|entry| {
                (entry.timer.fading || entry.fade_deadline.is_some() || entry.recovery.is_some())
                    && !entry.interrupted
            }) else {
                return Ok(None);
            };
            if expected.is_none_or(|expected| {
                expected.coordinator != entry.timer.coordinator || expected.ends != entry.timer.ends
            }) {
                return Err(CoreError::Store(
                    "a newer sleep fade appeared after the announcement checkpoint; nothing was interrupted".into(),
                ));
            }
            entry.interrupted = true;
            (self.resumes.issue(coordinator), entry.fade_claims.clone())
        };
        if let Some(claims) = claims {
            claims.stop(fader, false);
        }
        Ok(Some(receipt))
    }

    /// Rearm an announcement's interrupted fade only while its receipt is
    /// current. One coordinator's operation lock serializes device backstops
    /// with replacement/cancel/move. A manual interruption can invalidate the
    /// receipt during I/O and wins before the fresh timer is published.
    pub fn resume_after_announcement<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        receipt: &SleepResume,
        now: DateTime<Utc>,
    ) -> Result<Option<SleepTimer>, CoreError> {
        let coordinator = receipt.coordinator();
        let operation = self.operations.get(coordinator);
        let mut active = operation.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.resumes.owns(receipt) {
            return Ok(None);
        }
        let ends = now + delta(self.fade);
        let timer = SleepTimer {
            coordinator: coordinator.clone(),
            room: room_name(households, coordinator),
            ends,
            fading: false,
        };
        if let Err(error) = Self::backstop(t, households, coordinator, ends, now) {
            self.resumes.finish(receipt);
            if let Ok(host) = ip(households, coordinator) {
                let _ = backstop::clear(t, host, coordinator);
            }
            return Err(error);
        }
        let old = {
            let mut armed = self.lock();
            if receipt
                .current
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                drop(armed);
                backstop::clear(t, ip(households, coordinator)?, coordinator)?;
                return Ok(None);
            }
            self.resumes.finish(receipt);
            armed.insert(
                coordinator.clone(),
                Armed {
                    timer: timer.clone(),
                    recovery: None,
                    generation: self.generation(),
                    running: false,
                    fade_claims: None,
                    predecessors: active.remaining(),
                    // This replacement is due immediately. A manual level change
                    // must interrupt it even before the dispatcher allocates work.
                    fade_deadline: Some(
                        Instant::now()
                            .checked_add(self.fade)
                            .unwrap_or_else(Instant::now),
                    ),
                    interrupted: false,
                },
            )
        };
        if let Some(old) = old {
            Self::stop_fade(&old, fader);
        }
        Ok(Some(timer))
    }
}
