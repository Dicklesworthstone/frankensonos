//! Pull frames from the shared journal only when the HTTP transport is ready.
//! There is no per-client event queue or pump thread to fall behind.

use asupersync::stream::Stream;
use std::pin::Pin;
use std::sync::{Arc, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use super::wake::Reader;
use super::{EventBus, HEARTBEAT, HEARTBEAT_FRAME, frame, reset};
use crate::failure::{ErrorCode, Failure};

struct Subscription {
    lease: Reader,
    bus: Arc<EventBus>,
    after: u64,
    heartbeat_at: Instant,
}

/// The body of `GET /events`; see [`super::stream`] and [`super::try_stream`].
pub struct EventStream {
    subscription: Option<Subscription>,
    unavailable: Option<Vec<u8>>,
}

impl EventStream {
    pub(super) fn open(bus: Arc<EventBus>, after: u64) -> Result<Self, Failure> {
        let lease = bus.wake.reader().map_err(|error| {
            Failure::new(
                ErrorCode::NotReady,
                format!("event delivery unavailable: {error}"),
            )
            .with_hint("Close an unused event connection, then reconnect.")
        })?;
        Ok(Self {
            subscription: Some(Subscription {
                lease,
                bus,
                after,
                heartbeat_at: Instant::now() + HEARTBEAT,
            }),
            unavailable: None,
        })
    }

    /// Keep the infallible raw-stream API, but do not hide admission failure.
    /// HTTP uses the fallible constructor and returns 503 before streaming.
    pub(super) fn unavailable(failure: &Failure) -> Self {
        Self {
            subscription: None,
            unavailable: Some(frame(
                None,
                "events.unavailable",
                &serde_json::json!({ "detail": failure.detail }).to_string(),
            )),
        }
    }
}

impl Stream for EventStream {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        let this = self.get_mut();
        if let Some(unavailable) = this.unavailable.take() {
            return Poll::Ready(Some(unavailable));
        }
        let Some(subscription) = &mut this.subscription else {
            return Poll::Ready(None);
        };
        let waker = cx.waker().clone();
        let ring = subscription
            .bus
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let oldest = ring.oldest();
        let output = if ring.needs_reset(subscription.after) {
            subscription.after = oldest.saturating_sub(1);
            Some(reset(oldest))
        } else if let Some(event) = ring
            .events
            .iter()
            .find(|event| event.id > subscription.after)
        {
            subscription.after = event.id;
            Some(event.frame())
        } else if Instant::now() >= subscription.heartbeat_at {
            Some(HEARTBEAT_FRAME.to_vec())
        } else {
            None
        };
        if let Some(output) = output {
            subscription.heartbeat_at = Instant::now() + HEARTBEAT;
            drop(ring);
            subscription.lease.clear();
            return Poll::Ready(Some(output));
        }
        // Keep the journal locked through registration. A publisher must
        // either be visible above or wake this registration after unlocking.
        let previous = subscription.lease.park(waker, subscription.heartbeat_at);
        drop(ring);
        drop(previous);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests;
