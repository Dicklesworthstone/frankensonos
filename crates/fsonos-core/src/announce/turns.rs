//! FIFO household turns with cancellable waiting tickets. This module owns
//! no speaker state; abandoning a queued ticket cannot dispatch any work.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

pub(super) const CANCEL_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
struct Tickets {
    next: u64,
    serving: u64,
    cancelled: BTreeSet<u64>,
}

impl Tickets {
    fn advance(&mut self) {
        self.serving += 1;
        while self.cancelled.remove(&self.serving) {
            self.serving += 1;
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Turns {
    state: Mutex<HashMap<String, Tickets>>,
    changed: Condvar,
}

pub(super) struct Turn<'a> {
    queue: &'a Turns,
    key: String,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut turns = self
            .queue
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = turns.get_mut(&self.key) {
            entry.advance();
            if entry.serving == entry.next {
                turns.remove(&self.key);
            }
        }
        self.queue.changed.notify_all();
    }
}

impl Turns {
    pub fn pending(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|entry| entry.next - entry.serving - entry.cancelled.len() as u64)
            .sum()
    }

    pub fn take(&self, key: String, cancel: Option<&AtomicBool>) -> Option<Turn<'_>> {
        let cancelled = || cancel.is_some_and(|flag| flag.load(Ordering::Acquire));
        let mut turns = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if cancelled() {
            return None;
        }
        let entry = turns.entry(key.clone()).or_default();
        let ticket = entry.next;
        entry.next += 1;
        loop {
            let entry = turns.get_mut(&key).expect("ticket was assigned");
            if cancelled() {
                if entry.serving == ticket {
                    entry.advance();
                } else {
                    entry.cancelled.insert(ticket);
                }
                if entry.serving == entry.next {
                    turns.remove(&key);
                }
                self.changed.notify_all();
                return None;
            }
            if entry.serving == ticket {
                return Some(Turn { queue: self, key });
            }
            turns = if cancel.is_some() {
                self.changed
                    .wait_timeout(turns, CANCEL_POLL)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            } else {
                self.changed
                    .wait(turns)
                    .unwrap_or_else(PoisonError::into_inner)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    fn wait_pending(turns: &Turns, expected: u64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while turns.pending() != expected {
            assert!(Instant::now() < deadline, "waiting for {expected} turns");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_cancelled_request_never_takes_a_turn() {
        let turns = Turns::default();
        let cancelled = AtomicBool::new(true);
        assert!(turns.take("house".into(), Some(&cancelled)).is_none());
        assert_eq!(turns.pending(), 0);
        assert!(turns.state.lock().unwrap().is_empty());
    }

    #[test]
    fn a_waiting_cancel_returns_before_the_active_turn_and_preserves_fifo() {
        let turns = Turns::default();
        let active = turns.take("house".into(), None).unwrap();
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (send, receive) = mpsc::channel();
            let queue = &turns;
            let cancel = &cancelled;
            let waiting = scope.spawn(move || {
                let turn = queue.take("house".into(), Some(cancel));
                send.send(turn.is_none()).unwrap();
            });
            wait_pending(&turns, 2);
            cancelled.store(true, Ordering::Release);
            assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
            waiting.join().unwrap();
            assert_eq!(turns.pending(), 1, "active turn is still held");
            let (send, receive) = mpsc::channel();
            let next = scope.spawn(move || {
                let _turn = queue.take("house".into(), None).unwrap();
                send.send(()).unwrap();
            });
            // The later ticket must remain queued until the active turn ends.
            assert!(receive.recv_timeout(Duration::from_millis(20)).is_err());
            drop(active);
            receive.recv_timeout(Duration::from_secs(2)).unwrap();
            next.join().unwrap();
        });
        assert_eq!(turns.pending(), 0);
        assert!(turns.state.lock().unwrap().is_empty());
    }

    #[test]
    fn successive_cancelled_tickets_do_not_block_the_next_household_turn() {
        let turns = Turns::default();
        let active = turns.take("house".into(), None).unwrap();
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| turns.take("house".into(), Some(&cancelled)).is_none());
            wait_pending(&turns, 2);
            let b = scope.spawn(|| turns.take("house".into(), Some(&cancelled)).is_none());
            wait_pending(&turns, 3);
            cancelled.store(true, Ordering::Release);
            assert!(a.join().unwrap());
            assert!(b.join().unwrap());
            assert_eq!(turns.pending(), 1);
            drop(active);
        });
        let next = turns.take("house".into(), None).unwrap();
        assert_eq!(turns.pending(), 1);
        drop(next);
        assert_eq!(turns.pending(), 0);
    }

    #[test]
    fn another_household_does_not_wait_for_a_busy_one() {
        let turns = Turns::default();
        let first = turns.take("s1".into(), None).unwrap();
        let second = turns.take("s2".into(), None).unwrap();
        assert_eq!(turns.pending(), 2);
        drop(second);
        assert_eq!(turns.pending(), 1);
        drop(first);
        assert_eq!(turns.pending(), 0);
    }
}
