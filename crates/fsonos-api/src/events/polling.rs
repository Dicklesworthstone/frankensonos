//! One-shot JSON event reads share SSE's deadline worker and admission budget.

use asupersync::stream::Stream;
use std::pin::Pin;
use std::sync::{Arc, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use super::wake::Reader;
use super::{EventBus, MAX_WAIT, unavailable};
use crate::failure::Failure;

pub(super) struct PollStream {
    bus: Arc<EventBus>,
    lease: Option<Reader>,
    since: u64,
    deadline: Instant,
}

impl PollStream {
    pub(super) fn open(bus: Arc<EventBus>, since: u64, wait: Duration) -> Result<Self, Failure> {
        let lease = bus.wake.reader().map_err(|error| unavailable(&error))?;
        Ok(Self {
            bus,
            lease: Some(lease),
            since,
            deadline: Instant::now() + wait.min(MAX_WAIT),
        })
    }
}

impl Stream for PollStream {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        let this = self.get_mut();
        let Some(lease) = &this.lease else {
            return Poll::Ready(None);
        };
        let waker = cx.waker().clone();
        let ring = this.bus.ring.lock().unwrap_or_else(PoisonError::into_inner);
        if this.since == ring.last && Instant::now() < this.deadline {
            let previous = lease.park(waker, this.deadline);
            drop(ring);
            drop(previous);
            return Poll::Pending;
        }
        // The reset marker, retained events and final cursor describe exactly
        // one journal observation, even while publishers advance it.
        let snapshot = ring.snapshot(this.since);
        drop(ring);
        drop(this.lease.take());
        // These DTOs contain only finite JSON values, strings and integers.
        let bytes = serde_json::to_vec(&snapshot).expect("event snapshots are JSON values");
        Poll::Ready(Some(bytes))
    }
}

#[cfg(test)]
mod tests;
