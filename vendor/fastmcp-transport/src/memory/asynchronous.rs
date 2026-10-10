//! Awaitable operations for the existing bounded memory transport.
//!
//! These methods use the channel's wakers rather than sleeping or retrying a
//! nonblocking send. They share queues and admission limits with the synchronous
//! API, so an endpoint may use either API, including after `into_split`.
//!
//! A pending send owns at most one bounded encoded frame. Capacity and runtime
//! send-obligation admission are reserved before publication. Dropping a pending
//! operation releases its waiter without publishing or consuming a message.
//! Successful publication is never subsequently reported as cancellation.

use asupersync::{Cx, channel::mpsc};
use fastmcp_protocol::JsonRpcMessage;

use super::{MemoryQueuedMessage, MemoryRecvHalf, MemorySendHalf, MemoryTransport};
use crate::{Codec, MAX_CLIENT_TRANSPORT_SOURCE_BYTES, ReceivedTransportFrame, TransportError};

/// Encode once, retaining exactly the source that will be committed to the queue.
pub(super) fn encode_message(
    codec: &Codec,
    message: &JsonRpcMessage,
) -> Result<MemoryQueuedMessage, TransportError> {
    let mut source = match message {
        JsonRpcMessage::Request(request) => codec.encode_request(request)?,
        JsonRpcMessage::Response(response) => codec.encode_response(response)?,
    };
    assert_eq!(
        source.pop(),
        Some(b'\n'),
        "codec encodings always retain their NDJSON delimiter"
    );
    if source.len() > MAX_CLIENT_TRANSPORT_SOURCE_BYTES {
        return Err(TransportError::Codec(crate::CodecError::MessageTooLarge(
            source.len(),
        )));
    }
    Ok(MemoryQueuedMessage {
        source: source.into_boxed_slice(),
    })
}

fn send_error<T>(error: mpsc::SendError<T>) -> TransportError {
    match error {
        mpsc::SendError::Disconnected(_) => TransportError::Closed,
        mpsc::SendError::Cancelled(_) => TransportError::Cancelled,
        mpsc::SendError::Full(_) => TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory transport queue is full",
        )),
    }
}

fn reservation_error(error: mpsc::CheckedSendError<()>) -> TransportError {
    match error {
        mpsc::CheckedSendError::Channel(error) => send_error(error),
        // Refusing an obligation must not silently fall back to an untracked
        // send. Do not expose runtime internals through a transport error.
        _ => TransportError::Io(std::io::Error::other(
            "memory transport send obligation admission refused",
        )),
    }
}

async fn send_message(
    sender: &mut Option<mpsc::Sender<MemoryQueuedMessage>>,
    codec: &Codec,
    closed: &mut bool,
    cx: &Cx,
    message: &JsonRpcMessage,
) -> Result<(), TransportError> {
    if *closed || sender.is_none() {
        return Err(TransportError::Closed);
    }
    if cx.is_cancel_requested() {
        return Err(TransportError::Cancelled);
    }
    // Validate before waiting: a full queue must not hide an invalid or
    // oversized message behind an indefinitely pending reservation.
    let queued = encode_message(codec, message)?;
    let result = match sender
        .as_ref()
        .ok_or(TransportError::Closed)?
        .reserve_checked(cx)
        .await
    {
        Ok(permit) => permit.try_send(queued).map_err(send_error),
        Err(error) => Err(reservation_error(error)),
    };
    if matches!(result, Err(TransportError::Closed)) {
        *closed = true;
        sender.take();
    }
    result
}

async fn recv_source(
    receiver: &mut mpsc::Receiver<MemoryQueuedMessage>,
    closed: &mut bool,
    cx: &Cx,
) -> Result<Box<[u8]>, TransportError> {
    if *closed {
        return Err(TransportError::Closed);
    }
    if cx.is_cancel_requested() {
        return Err(TransportError::Cancelled);
    }
    match receiver.recv(cx).await {
        Ok(frame) => Ok(frame.source),
        Err(mpsc::RecvError::Disconnected) => {
            *closed = true;
            Err(TransportError::Closed)
        }
        Err(mpsc::RecvError::Cancelled) => Err(TransportError::Cancelled),
        Err(mpsc::RecvError::Empty) => Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory transport queue is empty",
        ))),
    }
}

/// An actual bounded-channel slot reserved for one memory-transport message.
///
/// Obtain this through [`MemoryTransport::reserve_send_async`] or
/// [`MemorySendHalf::reserve_send_async`] before constructing an expensive
/// response. Reservation awaits channel capacity and checks runtime obligation
/// admission before returning, without retaining an encoded message.
///
/// [`Self::send`] validates and commits synchronously, with no subsequent
/// cancellation checkpoint. Success means the queue owns the message, not that
/// the peer has processed it. Encoding or peer closure can still fail. Dropping
/// or aborting a permit releases its capacity without publishing anything.
#[must_use = "send or abort the reserved message; dropping releases capacity"]
pub struct MemorySendPermit<'a> {
    permit: mpsc::SendPermit<'a, MemoryQueuedMessage>,
    codec: &'a Codec,
    closed: &'a mut bool,
}

impl std::fmt::Debug for MemorySendPermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySendPermit").finish_non_exhaustive()
    }
}

impl MemorySendPermit<'_> {
    /// Validates, encodes and publishes exactly one message in the reserved slot.
    ///
    /// There is no cancellation check after reservation. Even when the context
    /// is cancelled in the meantime, a successful commit returns success. An
    /// encoding error releases the slot and leaves the endpoint usable; a peer
    /// disconnect releases the slot and latches the endpoint closed.
    pub fn send(self, message: &JsonRpcMessage) -> Result<(), TransportError> {
        let queued = encode_message(self.codec, message)?;
        let result = self.permit.try_send(queued).map_err(send_error);
        if matches!(result, Err(TransportError::Closed)) {
            *self.closed = true;
        }
        result
    }

    /// Releases the reserved slot and runtime obligation without sending.
    pub fn abort(self) {
        self.permit.abort();
    }
}

async fn reserve_send<'a>(
    sender: &'a Option<mpsc::Sender<MemoryQueuedMessage>>,
    codec: &'a Codec,
    closed: &'a mut bool,
    cx: &'a Cx,
) -> Result<MemorySendPermit<'a>, TransportError> {
    if *closed || sender.is_none() {
        return Err(TransportError::Closed);
    }
    if cx.is_cancel_requested() {
        return Err(TransportError::Cancelled);
    }
    match sender
        .as_ref()
        .ok_or(TransportError::Closed)?
        .reserve_checked(cx)
        .await
    {
        Ok(permit) => Ok(MemorySendPermit {
            permit,
            codec,
            closed,
        }),
        Err(error) => {
            let error = reservation_error(error);
            if matches!(error, TransportError::Closed) {
                *closed = true;
            }
            Err(error)
        }
    }
}

impl MemoryTransport {
    /// Reserves outbound capacity before constructing a message.
    ///
    /// This is a real channel reservation, not merely a cancellation preflight.
    /// Waiting yields to the executor. Dropping the waiting future or an unused
    /// permit publishes nothing and releases its waiter or slot. Use split
    /// halves when ingress must continue while an outbound permit is held.
    pub async fn reserve_send_async<'a>(
        &'a mut self,
        cx: &'a Cx,
    ) -> Result<MemorySendPermit<'a>, TransportError> {
        reserve_send(&self.sender, &self.codec, &mut self.closed, cx).await
    }

    /// Sends one bounded message, asynchronously waiting for channel capacity.
    ///
    /// Unlike the synchronous `Transport::send`, a full queue yields to the
    /// executor instead of returning `WouldBlock`. Cancellation or dropping
    /// this future before reservation publishes nothing. Once publication
    /// succeeds, this returns success without another cancellation check.
    /// A cancelled operation does not close the endpoint.
    pub async fn send_async(
        &mut self,
        cx: &Cx,
        message: &JsonRpcMessage,
    ) -> Result<(), TransportError> {
        send_message(&mut self.sender, &self.codec, &mut self.closed, cx, message).await
    }

    /// Receives one message without blocking the executor thread.
    ///
    /// Dropping a pending receive consumes no message. The channel checks the
    /// caller's context before dequeueing; no post-dequeue cancellation check
    /// discards an already received frame. Closing still uses `Transport::close`,
    /// which performs no blocking I/O for a memory endpoint.
    pub async fn recv_async(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.recv_with_source_async(cx)
            .await
            .map(ReceivedTransportFrame::into_message)
    }

    /// Receives the typed message together with its exact committed source.
    ///
    /// The same fixed source ceiling and strict decoder apply to synchronous
    /// and asynchronous ingress. No reserialization reconstructs the source.
    pub async fn recv_with_source_async(
        &mut self,
        cx: &Cx,
    ) -> Result<ReceivedTransportFrame, TransportError> {
        let source = recv_source(&mut self.receiver, &mut self.closed, cx).await?;
        ReceivedTransportFrame::admit(source)
    }
}

impl MemoryRecvHalf {
    /// Receives without sleeping, independently of the matching send half.
    ///
    /// Cancellation leaves queued messages for a subsequent operation with a
    /// live context. Dropping a pending future releases its receive waiter.
    pub async fn recv_async(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.recv_with_source_async(cx)
            .await
            .map(ReceivedTransportFrame::into_message)
    }

    /// Receives a source-preserving frame without blocking the executor.
    pub async fn recv_with_source_async(
        &mut self,
        cx: &Cx,
    ) -> Result<ReceivedTransportFrame, TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }
        let receiver = self.receiver.as_mut().ok_or(TransportError::Closed)?;
        let source = recv_source(receiver, &mut self.closed, cx).await?;
        ReceivedTransportFrame::admit(source)
    }
}

impl MemorySendHalf {
    /// Reserves one outbound slot while the receive half remains independent.
    ///
    /// This uses the same checked admission and cancellation contract as
    /// [`MemoryTransport::reserve_send_async`]. Commit or abort the returned
    /// permit; dropping it is also an abort and never sends a message.
    pub async fn reserve_send_async<'a>(
        &'a mut self,
        cx: &'a Cx,
    ) -> Result<MemorySendPermit<'a>, TransportError> {
        reserve_send(&self.sender, &self.codec, &mut self.closed, cx).await
    }

    /// Waits for capacity and commits a message independently of ingress.
    ///
    /// This has the same bounds and cancellation contract as
    /// [`MemoryTransport::send_async`]. The receiver may continue handling
    /// peer messages while this send is waiting for backpressure to clear.
    pub async fn send_async(
        &mut self,
        cx: &Cx,
        message: &JsonRpcMessage,
    ) -> Result<(), TransportError> {
        send_message(&mut self.sender, &self.codec, &mut self.closed, cx, message).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use fastmcp_protocol::{JsonRpcRequest, JsonRpcResponse, RequestId};

    use super::*;
    use crate::memory::{MemoryTransportBuilder, create_memory_transport_pair_with_capacity};
    use crate::{Transport, TransportRecvHalf, TransportSendHalf};

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(waker))
    }

    fn ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match poll(future.as_mut(), Waker::noop()) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("operation unexpectedly waited"),
        }
    }

    fn request(id: i64) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest::new("test/message", None, id))
    }

    fn request_id(message: JsonRpcMessage) -> RequestId {
        match message {
            JsonRpcMessage::Request(request) => request.id.expect("request has an id"),
            JsonRpcMessage::Response(_) => panic!("expected a request"),
        }
    }

    #[test]
    fn async_receive_yields_and_is_woken_by_send() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        let mut receiving = pin!(server.recv_async(&cx));
        assert!(poll(receiving.as_mut(), &waker).is_pending());

        client.send(&cx, &request(1)).unwrap();
        assert!(counter.0.load(Ordering::SeqCst) > 0);
        let Poll::Ready(Ok(message)) = poll(receiving.as_mut(), &waker) else {
            panic!("send must wake and complete the receive");
        };
        assert_eq!(request_id(message), RequestId::Number(1));
    }

    #[test]
    fn async_send_waits_for_capacity_and_preserves_fifo() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        client.send(&cx, &request(1)).unwrap();
        let second = request(2);
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        let mut sending = pin!(client.send_async(&cx, &second));
        assert!(poll(sending.as_mut(), &waker).is_pending());

        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(1));
        assert!(counter.0.load(Ordering::SeqCst) > 0);
        assert!(matches!(poll(sending.as_mut(), &waker), Poll::Ready(Ok(()))));
        assert_eq!(
            request_id(ready(server.recv_async(&cx)).unwrap()),
            RequestId::Number(2)
        );
    }

    #[test]
    fn dropped_pending_send_publishes_nothing_and_releases_waiter() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        client.send(&cx, &request(1)).unwrap();
        let second = request(2);
        {
            let mut sending = pin!(client.send_async(&cx, &second));
            assert!(poll(sending.as_mut(), Waker::noop()).is_pending());
        }
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(1));
        ready(client.send_async(&cx, &request(3))).unwrap();
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(3));
        assert_eq!(server.receiver.len(), 0);
    }

    #[test]
    fn cancelled_pending_send_keeps_endpoint_reusable() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let live = Cx::for_testing();
        let cancelled = Cx::for_testing();
        client.send(&live, &request(1)).unwrap();
        let second = request(2);
        {
            let mut sending = pin!(client.send_async(&cancelled, &second));
            assert!(poll(sending.as_mut(), Waker::noop()).is_pending());
            cancelled.set_cancel_requested(true);
            assert!(matches!(
                poll(sending.as_mut(), Waker::noop()),
                Poll::Ready(Err(TransportError::Cancelled))
            ));
        }
        assert!(!client.is_closed());
        assert_eq!(request_id(server.recv(&live).unwrap()), RequestId::Number(1));
        ready(client.send_async(&live, &request(3))).unwrap();
        assert_eq!(request_id(server.recv(&live).unwrap()), RequestId::Number(3));
    }

    #[test]
    fn cancelled_receive_does_not_consume_a_late_message() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let live = Cx::for_testing();
        let cancelled = Cx::for_testing();
        {
            let mut receiving = pin!(server.recv_async(&cancelled));
            assert!(poll(receiving.as_mut(), Waker::noop()).is_pending());
            client.send(&live, &request(7)).unwrap();
            cancelled.set_cancel_requested(true);
            assert!(matches!(
                poll(receiving.as_mut(), Waker::noop()),
                Poll::Ready(Err(TransportError::Cancelled))
            ));
        }
        assert!(!server.is_closed());
        assert_eq!(
            request_id(ready(server.recv_async(&live)).unwrap()),
            RequestId::Number(7)
        );
    }

    #[test]
    fn dropped_pending_receive_leaves_message_for_next_operation() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        {
            let mut receiving = pin!(server.recv_with_source_async(&cx));
            assert!(poll(receiving.as_mut(), Waker::noop()).is_pending());
        }
        ready(client.send_async(&cx, &request(9))).unwrap();
        assert_eq!(
            request_id(ready(server.recv_async(&cx)).unwrap()),
            RequestId::Number(9)
        );
    }

    #[test]
    fn closing_receive_half_wakes_blocked_async_sender() {
        let (client, server) = create_memory_transport_pair_with_capacity(1);
        let (_client_recv, mut client_send) = client.into_split();
        let (mut server_recv, _server_send) = server.into_split();
        let cx = Cx::for_testing();
        client_send.send(&cx, &request(1)).unwrap();
        let second = request(2);
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        {
            let mut sending = pin!(client_send.send_async(&cx, &second));
            assert!(poll(sending.as_mut(), &waker).is_pending());
            server_recv.close(&cx).unwrap();
            assert!(counter.0.load(Ordering::SeqCst) > 0);
            assert!(matches!(
                poll(sending.as_mut(), &waker),
                Poll::Ready(Err(TransportError::Closed))
            ));
        }
        assert!(client_send.is_closed());
        assert!(client_send.sender.is_none());
    }

    #[test]
    fn dropping_sender_wakes_pending_async_receive() {
        let (client, server) = create_memory_transport_pair_with_capacity(1);
        let (_client_recv, client_send) = client.into_split();
        let (mut server_recv, _server_send) = server.into_split();
        let cx = Cx::for_testing();
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        {
            let mut receiving = pin!(server_recv.recv_async(&cx));
            assert!(poll(receiving.as_mut(), &waker).is_pending());
            drop(client_send);
            assert!(counter.0.load(Ordering::SeqCst) > 0);
            assert!(matches!(
                poll(receiving.as_mut(), &waker),
                Poll::Ready(Err(TransportError::Closed))
            ));
        }
        assert!(server_recv.is_closed());
    }

    #[test]
    fn async_receive_drains_committed_messages_before_peer_eof() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        ready(client.send_async(&cx, &request(11))).unwrap();
        drop(client);
        assert_eq!(
            request_id(ready(server.recv_async(&cx)).unwrap()),
            RequestId::Number(11)
        );
        assert!(matches!(
            ready(server.recv_async(&cx)),
            Err(TransportError::Closed)
        ));
        assert!(server.is_closed());
    }

    #[test]
    fn async_operations_on_closed_endpoints_precede_cancellation() {
        let (client, server) = create_memory_transport_pair_with_capacity(1);
        let (mut recv, mut send) = client.into_split();
        let cx = Cx::for_testing();
        recv.close(&cx).unwrap();
        send.close(&cx).unwrap();
        cx.set_cancel_requested(true);
        assert!(matches!(
            ready(recv.recv_async(&cx)),
            Err(TransportError::Closed)
        ));
        assert!(matches!(
            ready(send.send_async(&cx, &request(1))),
            Err(TransportError::Closed)
        ));
        drop(server);
    }

    #[test]
    fn async_send_preserves_limits_and_does_not_poison_endpoint() {
        let message = request(13);
        let size = match &message {
            JsonRpcMessage::Request(request) => serde_json::to_vec(request).unwrap().len(),
            JsonRpcMessage::Response(_) => unreachable!(),
        };
        let (mut exact, mut peer) = MemoryTransportBuilder::new().max_message_size(size).build();
        let cx = Cx::for_testing();
        ready(exact.send_async(&cx, &message)).unwrap();
        assert_eq!(request_id(peer.recv(&cx).unwrap()), RequestId::Number(13));

        let (mut small, peer) = MemoryTransportBuilder::new()
            .max_message_size(size - 1)
            .build();
        assert!(matches!(
            ready(small.send_async(&cx, &message)),
            Err(TransportError::Codec(crate::CodecError::MessageTooLarge(_)))
        ));
        assert!(!small.is_closed());
        assert_eq!(peer.receiver.len(), 0);
    }

    #[test]
    fn invalid_message_is_rejected_even_when_channel_is_full() {
        let (mut client, server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        client.send(&cx, &request(1)).unwrap();
        let invalid = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(RequestId::Number(2)),
        });
        assert!(matches!(
            ready(client.send_async(&cx, &invalid)),
            Err(TransportError::Codec(_))
        ));
        assert!(!client.is_closed());
        assert_eq!(server.receiver.len(), 1);
    }

    #[test]
    fn async_split_halves_retain_exact_committed_source() {
        let (client, server) = create_memory_transport_pair_with_capacity(1);
        let (mut client_recv, _client_send) = client.into_split();
        let (_server_recv, mut server_send) = server.into_split();
        let cx = Cx::for_testing();
        let response = JsonRpcResponse::success(
            RequestId::Number(21),
            serde_json::from_str(r#"{"zeta":1.20e+4,"alpha":{"second":2,"first":1}}"#).unwrap(),
        );
        let expected = Codec::new().encode_response(&response).unwrap();
        let expected = expected.strip_suffix(b"\n").unwrap();
        ready(server_send.send_async(&cx, &JsonRpcMessage::Response(response))).unwrap();
        let frame = ready(client_recv.recv_with_source_async(&cx)).unwrap();
        assert_eq!(frame.source(), expected);
        assert!(matches!(frame.message(), JsonRpcMessage::Response(_)));
    }

    #[test]
    fn async_futures_are_send() {
        fn assert_send<T: Send>(_: T) {}
        let (mut client, server) = create_memory_transport_pair_with_capacity(1);
        let (mut recv, mut send) = server.into_split();
        let cx = Cx::for_testing();
        let message = request(1);
        assert_send(client.send_async(&cx, &message));
        assert_send(client.recv_async(&cx));
        assert_send(client.recv_with_source_async(&cx));
        assert_send(send.send_async(&cx, &message));
        assert_send(recv.recv_async(&cx));
        assert_send(recv.recv_with_source_async(&cx));
    }

    #[test]
    fn reservation_owns_capacity_without_publishing_a_message() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let competing = client.sender.as_ref().unwrap().clone();
        let cx = Cx::for_testing();
        let permit = ready(client.reserve_send_async(&cx)).unwrap();
        assert_eq!(server.receiver.len(), 0);
        let other = encode_message(&Codec::new(), &request(2)).unwrap();
        assert!(matches!(
            competing.try_send(other),
            Err(mpsc::SendError::Full(_))
        ));
        permit.send(&request(1)).unwrap();
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(1));
    }

    #[test]
    fn dropping_or_aborting_permits_releases_capacity_without_sending() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        let permit = ready(client.reserve_send_async(&cx)).unwrap();
        drop(permit);
        assert_eq!(server.receiver.len(), 0);
        ready(client.reserve_send_async(&cx)).unwrap().abort();
        assert_eq!(server.receiver.len(), 0);
        ready(client.reserve_send_async(&cx))
            .unwrap()
            .send(&request(3))
            .unwrap();
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(3));
    }

    #[test]
    fn reserved_send_commits_even_if_context_is_cancelled_after_reservation() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        let permit = ready(client.reserve_send_async(&cx)).unwrap();
        cx.set_cancel_requested(true);
        permit.send(&request(4)).unwrap();
        let live = Cx::for_testing();
        assert_eq!(request_id(server.recv(&live).unwrap()), RequestId::Number(4));
        assert!(!client.is_closed());
    }

    #[test]
    fn cancelled_reservation_never_claims_capacity() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        assert!(matches!(
            ready(client.reserve_send_async(&cx)),
            Err(TransportError::Cancelled)
        ));
        assert!(!client.is_closed());
        let live = Cx::for_testing();
        ready(client.reserve_send_async(&live))
            .unwrap()
            .send(&request(5))
            .unwrap();
        assert_eq!(request_id(server.recv(&live).unwrap()), RequestId::Number(5));
    }

    #[test]
    fn pending_reservation_cancellation_and_drop_remove_their_waiters() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let live = Cx::for_testing();
        let cancelled = Cx::for_testing();
        client.send(&live, &request(1)).unwrap();
        {
            let mut reservation = pin!(client.reserve_send_async(&cancelled));
            assert!(poll(reservation.as_mut(), Waker::noop()).is_pending());
            cancelled.set_cancel_requested(true);
            assert!(matches!(
                poll(reservation.as_mut(), Waker::noop()),
                Poll::Ready(Err(TransportError::Cancelled))
            ));
        }
        {
            let mut reservation = pin!(client.reserve_send_async(&live));
            assert!(poll(reservation.as_mut(), Waker::noop()).is_pending());
        }
        assert_eq!(request_id(server.recv(&live).unwrap()), RequestId::Number(1));
        ready(client.reserve_send_async(&live))
            .unwrap()
            .send(&request(6))
            .unwrap();
        assert_eq!(request_id(server.recv(&live).unwrap()), RequestId::Number(6));
    }

    #[test]
    fn split_reservation_waits_for_capacity_without_blocking_ingress() {
        let (client, mut server) = create_memory_transport_pair_with_capacity(1);
        let (mut recv, mut send) = client.into_split();
        let cx = Cx::for_testing();
        send.send(&cx, &request(1)).unwrap();
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        let mut reservation = pin!(send.reserve_send_async(&cx));
        assert!(poll(reservation.as_mut(), &waker).is_pending());

        ready(server.send_async(&cx, &request(7))).unwrap();
        assert_eq!(
            request_id(ready(recv.recv_async(&cx)).unwrap()),
            RequestId::Number(7)
        );
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(1));
        assert!(counter.0.load(Ordering::SeqCst) > 0);
        let Poll::Ready(Ok(permit)) = poll(reservation.as_mut(), &waker) else {
            panic!("draining the peer must wake and admit the reservation");
        };
        permit.send(&request(8)).unwrap();
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(8));
    }

    #[test]
    fn invalid_reserved_message_releases_slot_and_preserves_endpoint() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        let invalid = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(RequestId::Number(1)),
        });
        let permit = ready(client.reserve_send_async(&cx)).unwrap();
        assert!(matches!(
            permit.send(&invalid),
            Err(TransportError::Codec(_))
        ));
        assert!(!client.is_closed());
        assert_eq!(server.receiver.len(), 0);
        ready(client.send_async(&cx, &request(9))).unwrap();
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(9));
    }

    #[test]
    fn oversized_reserved_message_releases_slot_and_keeps_the_source_limit() {
        let message = request(1);
        let size = match &message {
            JsonRpcMessage::Request(request) => serde_json::to_vec(request).unwrap().len(),
            JsonRpcMessage::Response(_) => unreachable!(),
        };
        let (mut client, mut server) = MemoryTransportBuilder::new()
            .max_message_size(size)
            .build();
        let cx = Cx::for_testing();
        let large = JsonRpcMessage::Request(JsonRpcRequest::new("x".repeat(size), None, 1_i64));
        let permit = ready(client.reserve_send_async(&cx)).unwrap();
        assert!(matches!(
            permit.send(&large),
            Err(TransportError::Codec(crate::CodecError::MessageTooLarge(_)))
        ));
        assert!(!client.is_closed());
        assert_eq!(server.receiver.len(), 0);
        ready(client.reserve_send_async(&cx))
            .unwrap()
            .send(&message)
            .unwrap();
        assert_eq!(request_id(server.recv(&cx).unwrap()), RequestId::Number(1));
    }

    #[test]
    fn peer_closure_after_reservation_refuses_commit_and_latches_closed() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        let permit = ready(client.reserve_send_async(&cx)).unwrap();
        server.close(&cx).unwrap();
        assert!(matches!(
            permit.send(&request(10)),
            Err(TransportError::Closed)
        ));
        assert!(client.is_closed());
        cx.set_cancel_requested(true);
        assert!(matches!(
            ready(client.reserve_send_async(&cx)),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn peer_closure_wakes_pending_reservation_and_latches_closed() {
        let (client, server) = create_memory_transport_pair_with_capacity(1);
        let (_recv, mut send) = client.into_split();
        let (mut peer_recv, _peer_send) = server.into_split();
        let cx = Cx::for_testing();
        send.send(&cx, &request(1)).unwrap();
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        {
            let mut reservation = pin!(send.reserve_send_async(&cx));
            assert!(poll(reservation.as_mut(), &waker).is_pending());
            peer_recv.close(&cx).unwrap();
            assert!(counter.0.load(Ordering::SeqCst) > 0);
            assert!(matches!(
                poll(reservation.as_mut(), &waker),
                Poll::Ready(Err(TransportError::Closed))
            ));
        }
        assert!(send.is_closed());
    }

    #[test]
    fn reservation_on_explicitly_closed_endpoint_precedes_cancellation() {
        let (mut client, server) = create_memory_transport_pair_with_capacity(1);
        let cx = Cx::for_testing();
        client.close(&cx).unwrap();
        cx.set_cancel_requested(true);
        assert!(matches!(
            ready(client.reserve_send_async(&cx)),
            Err(TransportError::Closed)
        ));
        let (_recv, mut send) = server.into_split();
        send.close(&cx).unwrap();
        assert!(matches!(
            ready(send.reserve_send_async(&cx)),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn send_permits_and_reservation_futures_are_send() {
        fn assert_type_send<T: Send>() {}
        fn assert_future_send<T: Future + Send>(_: T) {}
        assert_type_send::<MemorySendPermit<'static>>();
        let (mut client, server) = create_memory_transport_pair_with_capacity(1);
        let (_recv, mut send) = server.into_split();
        let cx = Cx::for_testing();
        assert_future_send(client.reserve_send_async(&cx));
        assert_future_send(send.reserve_send_async(&cx));
    }
}
