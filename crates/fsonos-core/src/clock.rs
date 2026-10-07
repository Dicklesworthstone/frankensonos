//! Time as a dependency, so time-sensitive rules (quiet hours, history
//! windows) are testable with a fixed clock.

use chrono::{DateTime, FixedOffset, Local};
use std::sync::Mutex;

/// A source of the current local time. The offset matters: quiet hours are
/// wall-clock times in the house's time zone.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<FixedOffset>;
}

/// The host's real local time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<FixedOffset> {
        Local::now().fixed_offset()
    }
}

/// A settable clock for tests and the e2e harness.
#[derive(Debug)]
pub struct FakeClock(Mutex<DateTime<FixedOffset>>);

impl FakeClock {
    #[must_use]
    pub fn new(at: DateTime<FixedOffset>) -> Self {
        Self(Mutex::new(at))
    }

    /// Jump to `at`.
    pub fn set(&self, at: DateTime<FixedOffset>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = at;
    }

    /// Move forward (or back, for a negative delta) by `by`.
    pub fn advance(&self, by: chrono::TimeDelta) {
        let mut now = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *now += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<FixedOffset> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_is_settable_and_advances() {
        let t0 = DateTime::parse_from_rfc3339("2026-10-07T21:59:00-04:00").unwrap();
        let clock = FakeClock::new(t0);
        assert_eq!(clock.now(), t0);
        clock.advance(chrono::TimeDelta::minutes(2));
        assert_eq!(clock.now().to_rfc3339(), "2026-10-07T22:01:00-04:00");
        clock.set(t0);
        assert_eq!(clock.now(), t0);
    }

    #[test]
    fn system_clock_is_close_to_utc_now() {
        let skew = SystemClock.now().to_utc() - chrono::Utc::now();
        assert!(skew.num_seconds().abs() < 5, "{skew}");
    }
}
