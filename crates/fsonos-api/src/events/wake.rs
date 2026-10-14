//! One lazy deadline worker per event bus, not one sleeping thread per client.
//! Publishers and the worker take wakers out before invoking foreign code.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::Waker;
use std::time::Instant;

/// Concurrent HTTP event readers admitted by one bus.
pub const MAX_READERS: usize = 128;

#[derive(Debug, Default)]
pub(super) struct WakeHub {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    readers: usize,
    running: bool,
    waiting: HashMap<u64, Waiting>,
}

#[derive(Debug)]
struct Waiting {
    deadline: Instant,
    waker: Waker,
}

/// A capacity lease. Dropping a body immediately releases its slot and waker.
#[derive(Debug)]
pub(super) struct Reader {
    hub: Arc<WakeHub>,
    id: u64,
}

impl WakeHub {
    pub(super) fn reader(self: &Arc<Self>) -> io::Result<Reader> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.readers >= MAX_READERS {
            return Err(io::Error::other("too many concurrent event readers"));
        }
        let id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("event reader identity space exhausted"))?;
        if !state.running {
            let hub = Arc::clone(self);
            // The new worker waits for this lock: its first observation includes
            // the lease below. A spawn failure never consumes a reader slot.
            std::thread::Builder::new()
                .name("fsonos-event-deadlines".into())
                .spawn(move || hub.run())?;
            state.running = true;
        }
        state.next_id = id;
        state.readers += 1;
        Ok(Reader {
            hub: Arc::clone(self),
            id,
        })
    }

    /// Called after releasing the event ring's lock. Readers register while
    /// holding that lock, so a publish cannot be lost between read and park.
    pub(super) fn notify(&self) {
        let waiting = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut state.waiting)
        };
        self.changed.notify_one();
        for waiting in waiting.into_values() {
            waiting.waker.wake();
        }
    }

    fn run(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if state.readers == 0 {
                state.running = false;
                self.changed.notify_all();
                return;
            }
            let now = Instant::now();
            let due: Vec<u64> = state
                .waiting
                .iter()
                .filter(|(_, waiting)| waiting.deadline <= now)
                .map(|(id, _)| *id)
                .collect();
            if !due.is_empty() {
                let ready: Vec<Waiting> = due
                    .into_iter()
                    .filter_map(|id| state.waiting.remove(&id))
                    .collect();
                drop(state);
                for waiting in ready {
                    waiting.waker.wake();
                }
                state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            state = match state.waiting.values().map(|waiting| waiting.deadline).min() {
                Some(deadline) => {
                    self.changed
                        .wait_timeout(state, deadline.saturating_duration_since(now))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize, bool) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (state.readers, state.waiting.len(), state.running)
    }
}

impl Reader {
    /// The caller holds the event-ring lock until this registration completes.
    pub(super) fn park(&self, waker: Waker, deadline: Instant) -> Option<Waker> {
        let previous = self
            .hub
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .waiting
            .insert(self.id, Waiting { deadline, waker });
        self.hub.changed.notify_one();
        previous.map(|waiting| waiting.waker)
    }

    pub(super) fn clear(&self) {
        let previous = self
            .hub
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .waiting
            .remove(&self.id);
        drop(previous);
        self.hub.changed.notify_one();
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let previous = {
            let mut state = self
                .hub
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.readers -= 1;
            state.waiting.remove(&self.id)
        };
        drop(previous);
        self.hub.changed.notify_one();
    }
}

#[cfg(test)]
mod tests;
