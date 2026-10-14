//! In-memory transport for testing MCP servers without subprocess spawning.
//!
//! This module provides a channel-based transport for direct client-server
//! communication within the same process. Essential for unit testing MCP
//! servers without network/IO overhead.
//!
//! # Overview
//!
//! The [`MemoryTransport`] uses asupersync channels to enable bidirectional
//! message passing between client and server. Create a pair using
//! [`create_memory_transport_pair`] which returns connected client and server
//! transports.
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_transport::memory::create_memory_transport_pair;
//! use fastmcp_transport::Transport;
//! use asupersync::Cx;
//!
//! // Create connected pair
//! let (client_transport, server_transport) = create_memory_transport_pair();
//!
//! // Use in separate threads/tasks
//! // Client sends, server receives (and vice versa)
//! let cx = Cx::for_testing();
//! let request = JsonRpcRequest::new("test", None, 1i64);
//! client_transport.send_request(&cx, &request)?;
//!
//! // Server receives the message
//! let msg = server_transport.recv(&cx)?;
//! ```
//!
//! # Testing Servers
//!
//! The primary use case is testing servers without subprocess spawning:
//!
//! ```ignore
//! use fastmcp_transport::memory::{create_memory_transport_pair, MemoryTransport};
//! use std::thread;
//!
//! let (mut client, mut server) = create_memory_transport_pair();
//!
//! // Spawn server handler in a thread
//! let server_handle = thread::spawn(move || {
//!     // Pass server transport to your server's run loop
//!     run_server_with_transport(server);
//! });
//!
//! // Use client to test
//! let cx = Cx::for_testing();
//! client.send_request(&cx, &init_request)?;
//! let response = client.recv(&cx)?;
//! assert!(matches!(response, JsonRpcMessage::Response(_)));
//! ```

/// Awaitable, source-preserving operations on bounded memory endpoints.
pub mod asynchronous;

use std::time::Duration;

use asupersync::{Cx, channel::mpsc};
use fastmcp_protocol::JsonRpcMessage;

use crate::{
    ClientTransportRecvHalf, Codec, ReceivedTransportFrame, Transport, TransportError,
    TransportRecvHalf, TransportSendHalf,
};

/// Default timeout for recv operations when polling for cancellation.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Smallest polling interval admitted by the synchronous transport adapter.
///
/// A zero interval turns an empty receive loop into a hot spin. The transport
/// keeps this polling interval for synchronous callers. Async operations use
/// channel wakers instead and do not sleep or consult this interval.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

fn normalize_poll_interval(interval: Duration) -> Duration {
    interval.max(MIN_POLL_INTERVAL)
}

fn send_memory_message(
    sender: &mut Option<mpsc::Sender<MemoryQueuedMessage>>,
    codec: &Codec,
    closed: &mut bool,
    cx: &Cx,
    message: &JsonRpcMessage,
) -> Result<(), TransportError> {
    if *closed || sender.is_none() {
        return Err(TransportError::Closed);
    }
    // Check before the nonblocking commit. Reporting cancellation after a
    // successful `try_send` would incorrectly claim the message was not sent.
    if cx.is_cancel_requested() {
        return Err(TransportError::Cancelled);
    }

    // Share bounded encoding with async sends, without copying the serialized
    // source into a second allocation before queueing it.
    let queued = asynchronous::encode_message(codec, message)?;

    match sender
        .as_ref()
        .ok_or(TransportError::Closed)?
        .try_send(queued)
    {
        Ok(()) => Ok(()),
        Err(mpsc::SendError::Disconnected(_)) => {
            // A disconnected peer is terminal. Drop our sender now so
            // subsequent calls deterministically report Closed without
            // revalidating or cloning another message.
            *closed = true;
            sender.take();
            Err(TransportError::Closed)
        }
        Err(mpsc::SendError::Cancelled(_)) => Err(TransportError::Cancelled),
        Err(mpsc::SendError::Full(_)) => Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory transport queue is full",
        ))),
    }
}

fn recv_memory_message(
    receiver: &mut mpsc::Receiver<MemoryQueuedMessage>,
    closed: &mut bool,
    poll_interval: Duration,
    cx: &Cx,
) -> Result<JsonRpcMessage, TransportError> {
    recv_memory_source(receiver, closed, poll_interval, cx)
        .and_then(ReceivedTransportFrame::admit)
        .map(ReceivedTransportFrame::into_message)
}

fn recv_memory_source(
    receiver: &mut mpsc::Receiver<MemoryQueuedMessage>,
    closed: &mut bool,
    poll_interval: Duration,
    cx: &Cx,
) -> Result<Box<[u8]>, TransportError> {
    if *closed {
        return Err(TransportError::Closed);
    }
    // Check for cancellation before receive.
    if cx.is_cancel_requested() {
        return Err(TransportError::Cancelled);
    }

    // Poll the bounded asupersync channel while retaining this synchronous
    // transport trait's cancellation responsiveness.
    loop {
        let recv_result = receiver.try_recv();
        match recv_result {
            Ok(frame) => return Ok(frame.source),
            Err(mpsc::RecvError::Empty) => {
                // Check for cancellation between polls.
                if cx.is_cancel_requested() {
                    return Err(TransportError::Cancelled);
                }
                std::thread::sleep(poll_interval);
            }
            Err(mpsc::RecvError::Disconnected) => {
                *closed = true;
                return Err(TransportError::Closed);
            }
            Err(mpsc::RecvError::Cancelled) => return Err(TransportError::Cancelled),
        }
    }
}

fn close_memory_receiver(
    receiver: &mut Option<mpsc::Receiver<MemoryQueuedMessage>>,
    closed: &mut bool,
) {
    *closed = true;
    let Some(mut receiver) = receiver.take() else {
        return;
    };

    receiver.close();
    // asupersync channel closure intentionally retains queued values for
    // receivers that want to finish draining. A transport close is a hard
    // terminal boundary, so release those potentially large messages now.
    while receiver.try_recv().is_ok() {}
}

/// One bounded memory queue entry with the source committed by its sender.
///
/// Memory endpoints do not accept raw wire input, so this source is the
/// canonical bounded encoding that the sender committed from its typed value.
/// The receiver derives its typed message from this one source document, so a
/// later serialization cannot fabricate a distinct final result source.
struct MemoryQueuedMessage {
    source: Box<[u8]>,
}

/// In-memory transport using channels for message passing.
///
/// This transport enables direct communication between a client and server
/// without any network or I/O overhead. Messages are passed through
/// bounded MPSC channels.
///
/// # Thread Safety
///
/// The transport is `Send` and can be passed to other threads, but it is
/// not `Sync`. Each endpoint (client/server) should be used from a single
/// thread at a time.
///
/// # Cancellation
///
/// Synchronous recv operations poll for cancellation between sleeps. The
/// [`Self::recv_async`] and [`Self::send_async`] methods instead await channel
/// readiness and cooperate with the executor without blocking its thread.
pub struct MemoryTransport {
    /// Channel for sending messages to the peer.
    sender: Option<mpsc::Sender<MemoryQueuedMessage>>,
    /// Channel for receiving messages from the peer.
    receiver: mpsc::Receiver<MemoryQueuedMessage>,
    /// Codec for typed validation and serialized-size admission.
    codec: Codec,
    /// Whether the transport has been closed.
    closed: bool,
    /// Poll interval for cancellation checks during recv.
    poll_interval: Duration,
}

impl std::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTransport")
            .field("closed", &self.closed)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl MemoryTransport {
    /// Creates a new memory transport from channel endpoints.
    ///
    /// This is an internal constructor. Use [`create_memory_transport_pair`]
    /// to create a connected pair of transports.
    fn new(
        sender: mpsc::Sender<MemoryQueuedMessage>,
        receiver: mpsc::Receiver<MemoryQueuedMessage>,
    ) -> Self {
        Self {
            sender: Some(sender),
            receiver,
            codec: Codec::new(),
            closed: false,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Sets the poll interval for cancellation checks during recv.
    ///
    /// Lower values provide faster cancellation response but use more CPU.
    /// Default is 50ms. Values below 1ms are clamped to 1ms so an idle
    /// receiver cannot busy-spin.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = normalize_poll_interval(interval);
        self
    }

    /// Returns whether this transport has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Separates bounded memory ingress from independently owned egress.
    ///
    /// Both halves preserve this endpoint's channel capacity, typed-message
    /// admission, and cancellation behavior. The same pair of half types is
    /// used for either the client or server endpoint because memory transport
    /// directions are symmetric.
    #[must_use]
    pub fn into_split(self) -> (MemoryRecvHalf, MemorySendHalf) {
        let Self {
            sender,
            receiver,
            codec,
            closed,
            poll_interval,
        } = self;

        (
            MemoryRecvHalf {
                receiver: Some(receiver),
                closed,
                poll_interval,
            },
            MemorySendHalf {
                sender,
                codec,
                closed,
            },
        )
    }
}

impl Transport for MemoryTransport {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        send_memory_message(&mut self.sender, &self.codec, &mut self.closed, cx, message)
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        recv_memory_message(&mut self.receiver, &mut self.closed, self.poll_interval, cx)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        self.sender.take();
        self.receiver.close();
        // asupersync channel closure intentionally retains queued values for
        // receivers that want to finish draining. A transport close is a hard
        // terminal boundary, so release those potentially large messages now.
        while self.receiver.try_recv().is_ok() {}
        Ok(())
    }
}

/// Independently owned bounded memory ingress.
///
/// This receive half is symmetric for client and server endpoints. Dropping or
/// closing it releases the underlying receiver, so the peer's matching send
/// half observes a terminal channel on its next send attempt.
pub struct MemoryRecvHalf {
    receiver: Option<mpsc::Receiver<MemoryQueuedMessage>>,
    closed: bool,
    poll_interval: Duration,
}

impl std::fmt::Debug for MemoryRecvHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryRecvHalf")
            .field("closed", &self.closed)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl MemoryRecvHalf {
    /// Returns whether this receive half has entered a terminal state.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed || self.receiver.is_none()
    }
}

impl TransportRecvHalf for MemoryRecvHalf {
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }

        let receiver = self.receiver.as_mut().ok_or(TransportError::Closed)?;
        recv_memory_message(receiver, &mut self.closed, self.poll_interval, cx)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        close_memory_receiver(&mut self.receiver, &mut self.closed);
        Ok(())
    }
}

impl ClientTransportRecvHalf for MemoryRecvHalf {
    fn recv_with_source(&mut self, cx: &Cx) -> Result<ReceivedTransportFrame, TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }

        let receiver = self.receiver.as_mut().ok_or(TransportError::Closed)?;
        let source = recv_memory_source(receiver, &mut self.closed, self.poll_interval, cx)?;
        ReceivedTransportFrame::admit(source)
    }
}

/// Independently owned bounded memory egress.
///
/// This send half is symmetric for client and server endpoints. It checks the
/// caller context before the nonblocking channel commit, retaining the exact
/// cancellation contract of [`MemoryTransport::send`].
pub struct MemorySendHalf {
    sender: Option<mpsc::Sender<MemoryQueuedMessage>>,
    codec: Codec,
    closed: bool,
}

impl std::fmt::Debug for MemorySendHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySendHalf")
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl MemorySendHalf {
    /// Returns whether this send half has entered a terminal state.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed || self.sender.is_none()
    }
}

impl TransportSendHalf for MemorySendHalf {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        send_memory_message(&mut self.sender, &self.codec, &mut self.closed, cx, message)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        self.sender.take();
        Ok(())
    }
}

/// Creates a connected pair of memory transports.
///
/// Returns `(client, server)` transports where:
/// - Messages sent on `client` are received on `server`
/// - Messages sent on `server` are received on `client`
///
/// # Channel Capacity
///
/// Uses bounded channels with a default capacity of 64 messages.
/// This prevents unbounded memory growth if one side is slower.
///
/// # Example
///
/// ```
/// use fastmcp_transport::memory::create_memory_transport_pair;
/// use fastmcp_transport::Transport;
/// use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest};
/// use asupersync::Cx;
///
/// let (mut client, mut server) = create_memory_transport_pair();
/// let cx = Cx::for_testing();
///
/// // Client sends a request
/// let request = JsonRpcRequest::new("test/method", None, 1i64);
/// client.send_request(&cx, &request).unwrap();
///
/// // Server receives it
/// let msg = server.recv(&cx).unwrap();
/// match &msg {
///     JsonRpcMessage::Request(req) => assert_eq!(req.method, "test/method"),
///     _ => assert!(matches!(msg, JsonRpcMessage::Request(_)), "Expected request"),
/// }
/// ```
#[must_use]
pub fn create_memory_transport_pair() -> (MemoryTransport, MemoryTransport) {
    create_memory_transport_pair_with_capacity(64)
}

/// Creates a connected pair of memory transports with specified channel capacity.
///
/// # Arguments
///
/// * `capacity` - Maximum number of messages that can be buffered in each direction.
///
/// # Panics
///
/// Panics if `capacity` is zero. Memory transports are always bounded.
///
/// # Example
///
/// ```
/// use fastmcp_transport::memory::create_memory_transport_pair_with_capacity;
///
/// // Small buffer for testing backpressure
/// let (client, server) = create_memory_transport_pair_with_capacity(4);
/// ```
#[must_use]
pub fn create_memory_transport_pair_with_capacity(
    capacity: usize,
) -> (MemoryTransport, MemoryTransport) {
    let (client_to_server_tx, client_to_server_rx) = mpsc::channel(capacity);
    let (server_to_client_tx, server_to_client_rx) = mpsc::channel(capacity);

    let client = MemoryTransport::new(client_to_server_tx, server_to_client_rx);
    let server = MemoryTransport::new(server_to_client_tx, client_to_server_rx);

    (client, server)
}

/// Builder for creating memory transport pairs with custom configuration.
///
/// # Example
///
/// ```
/// use fastmcp_transport::memory::MemoryTransportBuilder;
/// use std::time::Duration;
///
/// let (client, server) = MemoryTransportBuilder::new()
///     .poll_interval(Duration::from_millis(10))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct MemoryTransportBuilder {
    poll_interval: Duration,
    max_message_size: usize,
}

impl Default for MemoryTransportBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryTransportBuilder {
    /// Creates a new builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_message_size: Codec::new().max_message_size(),
        }
    }

    /// Sets the poll interval for cancellation checks during recv.
    /// Values below 1ms are clamped to 1ms.
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = normalize_poll_interval(interval);
        self
    }

    /// Sets the maximum serialized bytes admitted for one typed message.
    #[must_use]
    pub fn max_message_size(mut self, max_message_size: usize) -> Self {
        self.max_message_size = max_message_size;
        self
    }

    /// Builds the transport pair with the configured settings.
    #[must_use]
    pub fn build(self) -> (MemoryTransport, MemoryTransport) {
        let (mut client, mut server) = create_memory_transport_pair();
        client.poll_interval = self.poll_interval;
        server.poll_interval = self.poll_interval;
        client.codec.set_max_message_size(self.max_message_size);
        server.codec.set_max_message_size(self.max_message_size);
        (client, server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_CLIENT_TRANSPORT_SOURCE_BYTES;
    use fastmcp_protocol::{JsonRpcRequest, JsonRpcResponse, RequestId};
    use std::thread;

    #[test]
    fn zero_poll_interval_is_clamped_away_from_busy_spin() {
        let (client, server) = MemoryTransportBuilder::new()
            .poll_interval(Duration::ZERO)
            .build();
        assert_eq!(client.poll_interval, MIN_POLL_INTERVAL);
        assert_eq!(server.poll_interval, MIN_POLL_INTERVAL);

        let (client, _) = create_memory_transport_pair();
        let client = client.with_poll_interval(Duration::ZERO);
        assert_eq!(client.poll_interval, MIN_POLL_INTERVAL);
    }

    #[test]
    fn memory_send_and_builder_enforce_exact_serialized_byte_boundary() {
        let cx = Cx::for_testing();
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({"payload": "bounded"})),
            1_i64,
        );
        let frame_size = serde_json::to_vec(&request).unwrap().len();

        let (mut exact, mut exact_peer) = MemoryTransportBuilder::new()
            .max_message_size(frame_size)
            .build();
        assert_eq!(exact.codec.max_message_size(), frame_size);
        assert_eq!(exact_peer.codec.max_message_size(), frame_size);
        exact.send_request(&cx, &request).unwrap();
        assert!(matches!(
            exact_peer.recv(&cx).unwrap(),
            JsonRpcMessage::Request(_)
        ));

        let (mut one_past, one_past_peer) = MemoryTransportBuilder::new()
            .max_message_size(frame_size - 1)
            .build();
        let error = one_past
            .send_request(&cx, &request)
            .expect_err("a frame one byte past the configured limit must be rejected");
        assert!(matches!(
            error,
            TransportError::Codec(crate::CodecError::MessageTooLarge(size))
                if size >= frame_size
        ));
        assert_eq!(one_past_peer.receiver.len(), 0);
    }

    #[test]
    fn memory_send_enforces_fixed_client_ingress_source_boundary_before_queueing() {
        let cx = Cx::for_testing();
        let template = JsonRpcRequest::new(
            "ingress/ceiling",
            Some(serde_json::json!({"payload": ""})),
            41_i64,
        );
        let template_size = serde_json::to_vec(&template)
            .expect("template request serializes")
            .len();
        let exact_payload_size = MAX_CLIENT_TRANSPORT_SOURCE_BYTES - template_size;
        let exact = JsonRpcRequest::new(
            "ingress/ceiling",
            Some(serde_json::json!({"payload": "x".repeat(exact_payload_size)})),
            41_i64,
        );
        assert_eq!(
            serde_json::to_vec(&exact)
                .expect("exact-limit request serializes")
                .len(),
            MAX_CLIENT_TRANSPORT_SOURCE_BYTES
        );

        let (mut client, mut server) = MemoryTransportBuilder::new()
            .max_message_size(MAX_CLIENT_TRANSPORT_SOURCE_BYTES + 1)
            .build();
        client
            .send_request(&cx, &exact)
            .expect("the fixed ingress limit admits exactly N source bytes");
        let JsonRpcMessage::Request(received) = server
            .recv(&cx)
            .expect("the peer receives the exact-limit queued request")
        else {
            panic!("the exact-limit source must derive a request");
        };
        assert_eq!(received.id, Some(RequestId::Number(41)));
        drop(received);
        drop(exact);

        let one_past = JsonRpcRequest::new(
            "ingress/ceiling",
            Some(serde_json::json!({"payload": "x".repeat(exact_payload_size + 1)})),
            41_i64,
        );
        assert_eq!(
            serde_json::to_vec(&one_past)
                .expect("N-plus-one request serializes")
                .len(),
            MAX_CLIENT_TRANSPORT_SOURCE_BYTES + 1
        );
        let error = client
            .send_request(&cx, &one_past)
            .expect_err("N-plus-one source bytes must reject before queueing");
        assert!(matches!(
            error,
            TransportError::Codec(crate::CodecError::MessageTooLarge(size))
                if size == MAX_CLIENT_TRANSPORT_SOURCE_BYTES + 1
        ));
        assert_eq!(server.receiver.len(), 0);
        assert!(!client.is_closed());

        client
            .send_request(&cx, &JsonRpcRequest::new("after-limit", None, 42_i64))
            .expect("the rejected oversized source leaves the sender usable");
        let JsonRpcMessage::Request(received) = server
            .recv(&cx)
            .expect("the peer remains usable after the rejected source")
        else {
            panic!("post-rejection peer traffic must retain request direction");
        };
        assert_eq!(received.method, "after-limit");
        assert_eq!(received.id, Some(RequestId::Number(42)));
    }

    #[test]
    fn memory_send_rejects_invalid_typed_message_before_queueing() {
        let (mut client, server) = MemoryTransportBuilder::new().max_message_size(1024).build();
        let cx = Cx::for_testing();
        let invalid = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(RequestId::Number(1)),
        };

        assert!(matches!(
            client.send(&cx, &JsonRpcMessage::Response(invalid)),
            Err(TransportError::Codec(crate::CodecError::Json(_)))
        ));
        assert_eq!(server.receiver.len(), 0);
    }

    #[test]
    fn test_basic_send_receive() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Client sends request
        let request = JsonRpcRequest::new("test/method", None, 1i64);
        client.send_request(&cx, &request).unwrap();

        // Server receives it
        let msg = server.recv(&cx).unwrap();
        assert!(
            matches!(msg, JsonRpcMessage::Request(_)),
            "Expected request"
        );
        let JsonRpcMessage::Request(req) = msg else {
            return;
        };
        assert_eq!(req.method, "test/method");
        assert_eq!(req.id, Some(RequestId::Number(1)));
    }

    #[test]
    fn test_bidirectional_communication() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Client sends request
        let request = JsonRpcRequest::new("ping", None, 1i64);
        client.send_request(&cx, &request).unwrap();

        // Server receives and responds
        let _msg = server.recv(&cx).unwrap();
        let response =
            JsonRpcResponse::success(RequestId::Number(1), serde_json::json!({"pong": true}));
        server.send_response(&cx, &response).unwrap();

        // Client receives response
        let msg = client.recv(&cx).unwrap();
        assert!(
            matches!(msg, JsonRpcMessage::Response(_)),
            "Expected response"
        );
        let JsonRpcMessage::Response(resp) = msg else {
            return;
        };
        assert!(resp.result.is_some());
    }

    #[test]
    fn split_halves_round_trip_with_bounded_cancel_correct_channels() {
        let (client, server) = create_memory_transport_pair_with_capacity(1);
        let (mut client_recv, mut client_send) = client.into_split();
        let (mut server_recv, mut server_send) = server.into_split();

        let cancelled = Cx::for_testing();
        cancelled.set_cancel_requested(true);
        let cancelled_request =
            JsonRpcMessage::Request(JsonRpcRequest::new("cancelled/request", None, 1_i64));
        assert!(matches!(
            client_send.send(&cancelled, &cancelled_request),
            Err(TransportError::Cancelled)
        ));

        let cx = Cx::for_testing();
        let request = JsonRpcMessage::Request(JsonRpcRequest::new("ping", None, 2_i64));
        client_send.send(&cx, &request).unwrap();
        let overflow = JsonRpcMessage::Request(JsonRpcRequest::new("overflow", None, 3_i64));
        assert!(matches!(
            client_send.send(&cx, &overflow),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        let JsonRpcMessage::Request(received) = server_recv.recv(&cx).unwrap() else {
            panic!("expected request");
        };
        assert_eq!(received.method, "ping");

        let cancelled = Cx::for_testing();
        cancelled.set_cancel_requested(true);
        assert!(matches!(
            server_recv.recv(&cancelled),
            Err(TransportError::Cancelled)
        ));

        let response = JsonRpcMessage::Response(JsonRpcResponse::success(
            RequestId::Number(2),
            serde_json::json!({"pong": true}),
        ));
        server_send.send(&cx, &response).unwrap();
        let JsonRpcMessage::Response(received) = client_recv.recv(&cx).unwrap() else {
            panic!("expected response");
        };
        assert_eq!(received.id, Some(RequestId::Number(2)));
    }

    #[test]
    fn split_halves_preserve_committed_frame_source() {
        let (client, server) = create_memory_transport_pair();
        let (mut client_recv, _client_send) = client.into_split();
        let (_server_recv, mut server_send) = server.into_split();
        let cx = Cx::for_testing();
        let response = JsonRpcResponse::success(
            RequestId::Number(7),
            serde_json::json!({
                "resultType": "complete",
                "opaque": {"origin": "memory"}
            }),
        );
        let expected = Codec::new()
            .encode_response(&response)
            .expect("response fits the default memory codec limit");
        let expected = expected
            .strip_suffix(b"\n")
            .expect("codec source includes an NDJSON delimiter");

        server_send
            .send(&cx, &JsonRpcMessage::Response(response.clone()))
            .expect("memory sender commits one bounded source document");
        let received = client_recv
            .recv_with_source(&cx)
            .expect("memory receive retains the committed source");

        assert_eq!(received.source(), expected);
        let JsonRpcMessage::Response(actual_response) = received.message() else {
            panic!("the committed response source must retain its response variant");
        };
        assert_eq!(
            actual_response, &response,
            "typed message is derived from the one committed source document"
        );
    }

    #[test]
    fn split_halves_drop_notifies_the_matching_peer() {
        let (client, server) = create_memory_transport_pair();
        let (_client_recv, client_send) = client.into_split();
        let (mut server_recv, _server_send) = server.into_split();
        let cx = Cx::for_testing();

        drop(client_send);
        assert!(matches!(server_recv.recv(&cx), Err(TransportError::Closed)));
        assert!(server_recv.is_closed());

        let (client, server) = create_memory_transport_pair();
        let (_client_recv, mut client_send) = client.into_split();
        let (server_recv, _server_send) = server.into_split();

        drop(server_recv);
        let request = JsonRpcMessage::Request(JsonRpcRequest::new("peer/dropped", None, 4_i64));
        assert!(matches!(
            client_send.send(&cx, &request),
            Err(TransportError::Closed)
        ));
        assert!(client_send.is_closed());
    }

    #[test]
    fn test_multiple_messages() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Send multiple messages
        for i in 1..=5 {
            let request = JsonRpcRequest::new(format!("method_{i}"), None, i as i64);
            client.send_request(&cx, &request).unwrap();
        }

        // Receive all messages
        for i in 1..=5 {
            let msg = server.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Request(_)),
                "Expected request"
            );
            let JsonRpcMessage::Request(req) = msg else {
                return;
            };
            assert_eq!(req.method, format!("method_{i}"));
        }
    }

    #[test]
    fn test_cancellation_on_recv() {
        let (client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Don't send anything, so recv will block

        // Set up cancellation
        cx.set_cancel_requested(true);

        // Recv should return cancelled immediately
        let result = server.recv(&cx);
        assert!(matches!(result, Err(TransportError::Cancelled)));

        // Keep client alive to prevent disconnection error
        drop(client);
    }

    #[test]
    fn test_cancellation_on_send() {
        let (mut client, _server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        cx.set_cancel_requested(true);

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = client.send_request(&cx, &request);
        assert!(matches!(result, Err(TransportError::Cancelled)));
    }

    #[test]
    fn test_close_signals_disconnection() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Close client
        client.close(&cx).unwrap();
        drop(client);

        // Server should get closed error on recv
        let result = server.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    #[test]
    fn test_send_after_close_fails() {
        let (mut client, _server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        client.close(&cx).unwrap();

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = client.send_request(&cx, &request);
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    #[test]
    fn test_recv_after_close_fails() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Send a message before closing
        let request = JsonRpcRequest::new("test", None, 1i64);
        client.send_request(&cx, &request).unwrap();

        // Close server
        server.close(&cx).unwrap();

        // Recv should fail
        let result = server.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    #[test]
    fn test_cross_thread_communication() {
        let (mut client, mut server) = create_memory_transport_pair();

        let server_handle = thread::spawn(move || {
            let cx = Cx::for_testing();

            // Receive request
            let msg = server.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Request(_)),
                "Expected request"
            );
            let JsonRpcMessage::Request(req) = msg else {
                return;
            };
            let request_id = req.id.clone().unwrap();

            // Send response
            let response = JsonRpcResponse::success(request_id, serde_json::json!({"ok": true}));
            server.send_response(&cx, &response).unwrap();
        });

        let client_handle = thread::spawn(move || {
            let cx = Cx::for_testing();

            // Send request
            let request = JsonRpcRequest::new("cross_thread_test", None, 42i64);
            client.send_request(&cx, &request).unwrap();

            // Receive response
            let msg = client.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Response(_)),
                "Expected response"
            );
            let JsonRpcMessage::Response(resp) = msg else {
                return;
            };
            assert!(resp.result.is_some());
        });

        server_handle.join().unwrap();
        client_handle.join().unwrap();
    }

    #[test]
    fn test_builder_custom_poll_interval() {
        use std::time::Duration;

        let (client, server) = MemoryTransportBuilder::new()
            .poll_interval(Duration::from_millis(5))
            .build();

        assert_eq!(client.poll_interval, Duration::from_millis(5));
        assert_eq!(server.poll_interval, Duration::from_millis(5));
    }

    #[test]
    fn test_is_closed() {
        let (mut client, server) = create_memory_transport_pair();

        assert!(!client.is_closed());
        assert!(!server.is_closed());

        let cx = Cx::for_testing();
        client.close(&cx).unwrap();

        assert!(client.is_closed());
        // Server doesn't know yet until recv fails
        assert!(!server.is_closed());
    }

    #[test]
    fn test_with_poll_interval() {
        use std::time::Duration;

        let (client, _server) = create_memory_transport_pair();
        let client = client.with_poll_interval(Duration::from_millis(100));

        assert_eq!(client.poll_interval, Duration::from_millis(100));
    }

    #[test]
    fn test_debug_format() {
        let (client, _server) = create_memory_transport_pair();
        let debug = format!("{client:?}");
        assert!(debug.contains("MemoryTransport"));
        assert!(debug.contains("closed: false"));
    }

    #[test]
    fn test_debug_format_closed() {
        let (mut client, _server) = create_memory_transport_pair();
        let cx = Cx::for_testing();
        client.close(&cx).unwrap();
        let debug = format!("{client:?}");
        assert!(debug.contains("closed: true"));
    }

    #[test]
    fn test_send_response_and_receive() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        let response =
            JsonRpcResponse::success(RequestId::Number(99), serde_json::json!({"val": 42}));
        server.send_response(&cx, &response).unwrap();

        let msg = client.recv(&cx).unwrap();
        let JsonRpcMessage::Response(resp) = msg else {
            panic!("expected response");
        };
        assert_eq!(resp.id, Some(RequestId::Number(99)));
    }

    #[test]
    fn test_send_to_dropped_peer_fails() {
        let (mut client, server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Drop server, so the receiver is gone
        drop(server);

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = client.send_request(&cx, &request);
        assert!(matches!(result, Err(TransportError::Closed)));
        assert!(client.is_closed());
        assert!(client.sender.is_none());

        cx.set_cancel_requested(true);
        assert!(matches!(
            client.send_request(&cx, &request),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn test_recv_from_dropped_peer_returns_closed() {
        let (client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Drop client sender
        drop(client);

        let result = server.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
        assert!(server.is_closed());
    }

    #[test]
    fn test_create_pair_with_capacity() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(2);
        let cx = Cx::for_testing();

        client
            .send_request(&cx, &JsonRpcRequest::new("first", None, 1i64))
            .unwrap();
        client
            .send_request(&cx, &JsonRpcRequest::new("second", None, 2i64))
            .unwrap();

        let full = client
            .send_request(&cx, &JsonRpcRequest::new("rejected", None, 3i64))
            .expect_err("the third message must exceed capacity two");
        assert!(matches!(
            full,
            TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        let JsonRpcMessage::Request(first) = server.recv(&cx).unwrap() else {
            panic!("expected first request");
        };
        assert_eq!(first.method, "first");

        client
            .send_request(&cx, &JsonRpcRequest::new("third", None, 3i64))
            .unwrap();
        let JsonRpcMessage::Request(second) = server.recv(&cx).unwrap() else {
            panic!("expected second request");
        };
        let JsonRpcMessage::Request(third) = server.recv(&cx).unwrap() else {
            panic!("expected third request");
        };
        assert_eq!(second.method, "second");
        assert_eq!(third.method, "third");
    }

    #[test]
    #[should_panic(expected = "channel capacity must be non-zero")]
    fn test_create_pair_rejects_zero_capacity() {
        let _ = create_memory_transport_pair_with_capacity(0);
    }

    #[test]
    fn test_builder_default() {
        let builder = MemoryTransportBuilder::default();
        let (client, server) = builder.build();
        assert_eq!(client.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(server.poll_interval, DEFAULT_POLL_INTERVAL);
    }

    #[test]
    fn test_close_is_idempotent() {
        let (mut client, _server) = create_memory_transport_pair();
        let cx = Cx::for_testing();
        client.close(&cx).unwrap();
        assert!(client.is_closed());
        // Close again - should not panic
        client.close(&cx).unwrap();
        assert!(client.is_closed());
    }

    #[test]
    fn close_discards_queued_messages_and_closed_precedes_cancellation() {
        let (mut client, mut server) = create_memory_transport_pair_with_capacity(2);
        let cx = Cx::for_testing();
        client
            .send_request(&cx, &JsonRpcRequest::new("first", None, 1_i64))
            .unwrap();
        client
            .send_request(&cx, &JsonRpcRequest::new("second", None, 2_i64))
            .unwrap();
        assert_eq!(server.receiver.len(), 2);

        server.close(&cx).unwrap();

        assert_eq!(server.receiver.len(), 0);
        cx.set_cancel_requested(true);
        assert!(matches!(server.recv(&cx), Err(TransportError::Closed)));
        assert!(matches!(
            server.send_request(&cx, &JsonRpcRequest::new("closed", None, 3_i64)),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn test_message_ordering() {
        let (mut client, mut server) = create_memory_transport_pair();
        let cx = Cx::for_testing();

        // Send 10 messages
        for i in 0..10 {
            let request = JsonRpcRequest::new(format!("msg_{i}"), None, i as i64);
            client.send_request(&cx, &request).unwrap();
        }

        // Verify they arrive in order
        for i in 0..10 {
            let msg = server.recv(&cx).unwrap();
            let JsonRpcMessage::Request(req) = msg else {
                panic!("expected request");
            };
            assert_eq!(req.method, format!("msg_{i}"));
        }
    }

    #[test]
    fn test_cancellation_during_poll() {
        let (_client, mut server) = MemoryTransportBuilder::new()
            .poll_interval(Duration::from_millis(5))
            .build();

        let cx = Cx::for_testing();

        // Cancel after a short delay from another thread
        let cx_clone = cx.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            cx_clone.set_cancel_requested(true);
        });

        // recv should eventually return Cancelled
        let result = server.recv(&cx);
        assert!(matches!(result, Err(TransportError::Cancelled)));

        handle.join().unwrap();
    }
}
