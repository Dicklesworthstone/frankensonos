//! Elapsed-time fade steps; never replay steps made obsolete by device I/O.

use std::time::{Duration, Instant};

/// Wake often enough to notice cancellation or daemon shutdown even when the
/// configured step interval is long. These wakeups perform no speaker reads.
const WAKE_NANOS: u128 = 50_000_000;

pub(super) struct Timing {
    started: Instant,
    over: Duration,
    interval: Duration,
}

pub(super) struct Frame {
    tick: u128,
    ticks: u128,
    wait: Duration,
    pub complete: bool,
}

impl Timing {
    pub fn new(over: Duration, interval: Duration) -> Self {
        Self {
            started: Instant::now(),
            over,
            interval,
        }
    }

    pub fn until(deadline: Instant, interval: Duration) -> Self {
        let started = Instant::now();
        Self {
            started,
            over: deadline.saturating_duration_since(started),
            interval,
        }
    }

    pub fn frame(&self, now: Instant, finishing: bool) -> Frame {
        let span = self.over.as_nanos();
        let interval = self.interval.as_nanos();
        let elapsed = now.saturating_duration_since(self.started).as_nanos();
        if finishing || span == 0 || interval == 0 || elapsed >= span {
            return Frame {
                tick: 1,
                ticks: 1,
                wait: Duration::ZERO,
                complete: true,
            };
        }
        let tick = elapsed / interval;
        let next = ((tick + 1) * interval).min(span);
        Frame {
            tick,
            ticks: span.div_ceil(interval),
            wait: Duration::from_nanos(
                u64::try_from((next - elapsed).min(WAKE_NANOS)).unwrap_or(0),
            ),
            complete: false,
        }
    }

    pub fn wait(&self) {
        let wait = self.frame(Instant::now(), false).wait;
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

impl Frame {
    pub fn level(&self, from: u8, to: u8) -> u8 {
        let delta = u8::try_from(u128::from(from.abs_diff(to)) * self.tick / self.ticks)
            .unwrap_or(from.abs_diff(to));
        if to >= from {
            from + delta
        } else {
            from - delta
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(over_ms: u64, interval_ms: u64) -> Timing {
        Timing::new(
            Duration::from_millis(over_ms),
            Duration::from_millis(interval_ms),
        )
    }

    #[test]
    fn elapsed_time_selects_the_step_without_replaying_missed_steps() {
        let timing = timing(1000, 250);
        for (ms, level) in [
            (0, 20),
            (249, 20),
            (250, 35),
            (750, 65),
            (1000, 80),
            (2000, 80),
        ] {
            let frame = timing.frame(timing.started + Duration::from_millis(ms), false);
            assert_eq!(frame.level(20, 80), level);
            assert_eq!(frame.complete, ms >= 1000);
            assert!(frame.wait <= Duration::from_millis(50));
        }
    }

    #[test]
    fn falling_fades_and_partial_intervals_end_at_the_deadline() {
        let timing = timing(900, 250);
        for (ms, level) in [(0, 30), (250, 23), (500, 15), (750, 8), (899, 8), (900, 0)] {
            let frame = timing.frame(timing.started + Duration::from_millis(ms), false);
            assert_eq!(frame.level(30, 0), level);
            assert_eq!(frame.complete, ms == 900);
        }
    }

    #[test]
    fn shutdown_zero_budgets_and_expired_deadlines_finish_without_waiting() {
        for timing in [
            timing(0, 250),
            timing(1000, 0),
            Timing::until(Instant::now(), Duration::from_secs(1)),
        ] {
            let frame = timing.frame(Instant::now(), false);
            assert!(frame.complete);
            assert_eq!(frame.level(30, 0), 0);
            assert_eq!(frame.wait, Duration::ZERO);
        }
        let timing = timing(60_000, 5000);
        let frame = timing.frame(timing.started, true);
        assert!(frame.complete);
        assert_eq!(frame.level(20, 80), 80);
        assert_eq!(frame.wait, Duration::ZERO);
    }

    #[test]
    fn tiny_intervals_and_huge_durations_need_no_step_vector() {
        let timing = Timing::new(Duration::MAX, Duration::from_nanos(1));
        assert_eq!(timing.frame(timing.started, false).level(0, 100), 0);
        let timing = Timing::new(Duration::from_nanos(3), Duration::from_nanos(1));
        assert_eq!(
            timing
                .frame(timing.started + Duration::from_nanos(2), false)
                .level(0, 90),
            60
        );
        assert_eq!(
            timing
                .frame(timing.started + Duration::from_nanos(3), false)
                .level(0, 90),
            90
        );
    }
}
