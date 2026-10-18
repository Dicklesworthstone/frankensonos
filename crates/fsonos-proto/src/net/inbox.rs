//! Bounded callback storage with an ordered, coalesced loss notification.
//! Once a notification is lost, retain the accepted prefix and reject the
//! rest of that burst until the consumer receives its resynchronization marker.

use crate::gena::Notify;
use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Maximum number of notifications waiting for a callback consumer.
pub const EVENT_BACKLOG: usize = 1024;
/// Maximum accounted storage of queued envelopes, strings and property vectors.
/// HTTP request parsing and the queue's bounded allocation overhead are separate.
pub const EVENT_BACKLOG_BYTES: usize = 8 * 1024 * 1024;

/// One callback delivery. Loss is delivered after the retained prefix, even
/// when the dropped notification was the last one the players ever sent.
#[derive(Debug, PartialEq, Eq)]
pub enum CallbackEvent {
    Notify(Notify),
    /// One or more notifications were not retained. Request current state;
    /// do not infer playback actions from the incomplete event history.
    ResyncRequired,
}

struct Entry {
    notify: Notify,
    bytes: usize,
}

struct State {
    queue: VecDeque<Entry>,
    bytes: usize,
    lagged: bool,
    senders: usize,
    receiving: bool,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    count_limit: usize,
    byte_limit: usize,
}

pub(super) struct Sender(Arc<Shared>);
pub(super) struct Receiver(Arc<Shared>);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SendError {
    Full,
    Closed,
}

pub(super) fn channel(count_limit: usize, byte_limit: usize) -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            bytes: 0,
            lagged: false,
            senders: 1,
            receiving: true,
        }),
        ready: Condvar::new(),
        count_limit,
        byte_limit,
    });
    (Sender(Arc::clone(&shared)), Receiver(shared))
}

/// Account capacities, not just string lengths: a short value may still own
/// a large allocation. Count the property-vector backing storage as well.
fn storage(notify: &Notify) -> usize {
    let fixed = size_of::<Entry>()
        .saturating_add(notify.sid.capacity())
        .saturating_add(notify.path.capacity())
        .saturating_add(
            notify
                .properties
                .capacity()
                .saturating_mul(size_of::<(String, String)>()),
        );
    notify.properties.iter().fold(fixed, |bytes, (key, value)| {
        bytes
            .saturating_add(key.capacity())
            .saturating_add(value.capacity())
    })
}

impl Sender {
    /// Never wait for capacity or for the consumer to acknowledge a loss.
    pub(super) fn send(&self, notify: Notify) -> Result<(), SendError> {
        let bytes = storage(&notify);
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.receiving {
            return Err(SendError::Closed);
        }
        if state.lagged
            || state.queue.len() >= self.0.count_limit
            || bytes > self.0.byte_limit.saturating_sub(state.bytes)
        {
            state.lagged = true;
            self.0.ready.notify_one();
            return Err(SendError::Full);
        }
        state.bytes += bytes;
        state.queue.push_back(Entry { notify, bytes });
        self.0.ready.notify_one();
        Ok(())
    }
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .senders += 1;
        Self(Arc::clone(&self.0))
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.senders -= 1;
        if state.senders == 0 {
            self.0.ready.notify_all();
        }
    }
}

impl Receiver {
    pub(super) fn recv_timeout(&self, wait: Duration) -> Result<CallbackEvent, RecvTimeoutError> {
        let started = Instant::now();
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(entry) = state.queue.pop_front() {
                state.bytes -= entry.bytes;
                return Ok(CallbackEvent::Notify(entry.notify));
            }
            if std::mem::take(&mut state.lagged) {
                return Ok(CallbackEvent::ResyncRequired);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let remaining = wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            // Recheck the predicate after every wake, retaining the original
            // deadline. Oversize-only bursts and last-sender drop also wake us.
            state = self
                .0
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.receiving = false;
        state.queue.clear();
        state.bytes = 0;
    }
}

#[cfg(test)]
mod tests;
