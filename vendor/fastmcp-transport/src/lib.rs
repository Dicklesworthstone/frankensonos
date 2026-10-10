//! Transport layer for FastMCP.
//!
//! This crate provides transport implementations for MCP communication:
//! - **Stdio**: Standard input/output (primary transport)
//! - **SSE**: Server-Sent Events (HTTP-based streaming)
//! - **WebSocket**: Native `ws://` and `wss://` client connections, a
//!   one-reader source-preserving async split driver, bounded HTTP Upgrade
//!   admission, and caller-owned TCP listener building blocks
//! - **HTTP**: Request/response and streamable-HTTP building blocks
//! - **Memory**: In-process transport for tests and embedding
//!
//! MCP 2026-07-28 support is under implementation and remains unverified. The
//! public protocol constant is still `2024-11-05`; the transport inventory is
//! not aggregate conformance or release evidence.
//!
//! # Transport Design
//!
//! Transport APIs expose asupersync integration points:
//!
//! - **Cancellation context**: Operations receive a caller-provided `Cx`
//! - **Two-phase-send surface**: Selected transports expose reserve/commit APIs
//! - **Budget context**: Implementations can inspect the request's budget
//!
//! # Wire Format
//!
//! The stdio transport uses newline-delimited JSON (NDJSON) framing:
//! - Each message is a single line of JSON
//! - Messages are separated by `\n`
//! - UTF-8 encoding is required
//!
//! # Role in the System
//!
//! `fastmcp-transport` is the **I/O boundary** for FastMCP. It is deliberately
//! protocol-agnostic: transports move `JsonRpcMessage` values in and out while
//! the server/client layers handle semantics. This keeps transport
//! implementations small, testable, and reusable.
//!
//! If you need to add a new transport (for example, QUIC or a custom IPC),
//! this is the crate to extend.

#![forbid(unsafe_code)]

mod async_io;
mod codec;
pub mod event_store;
pub mod http;
/// AUTH-00 A lower ingress-authentication seam.
pub mod ingress;
pub mod memory;
pub mod sse;
mod stdio;
/// Native cancellation-aware WebSocket transport building blocks.
pub mod websocket;

pub use async_io::{AsyncLineReader, AsyncStdin, AsyncStdout};

pub use codec::{Codec, CodecError, InvalidMessageKind};
/// Public modern per-request HTTP admission and response-stream primitives.
///
/// These types admit one strict HTTP request and, where negotiated, bind one
/// finite request-scoped SSE response body. They do not start an HTTP listener
/// or qualify turnkey HTTP serving.
pub use http::{
    GuardedHttpFetchError, GuardedHttpFetchPolicy, GuardedHttpFetchResponse, GuardedHttpFetcher,
    GuardedHttpPeerProvenance, GuardedHttpRedirect, GuardedHttpResolver, GuardedHttpsUrl,
    guarded_admit_native_response,
};
pub use http::{
    GuardedHttpRequest, GuardedRootSet, MAX_GUARDED_REQUEST_BODY_BYTES,
    MAX_GUARDED_REQUEST_HEADER_VALUE_BYTES, MAX_GUARDED_ROOT_SET_CERTIFICATES,
    guarded_encode_post_request,
};
pub use http::{
    HttpError, HttpHandlerConfig, HttpMethod, HttpRequest, HttpRequestHandler,
    HttpResponse, HttpResponseRepresentation, HttpStatus, ModernHttpRequestAdmission,
    ModernHttpSseCollector, ModernHttpSseCollectorError, NativeGuardedResolver,
    StreamableHttpRequestCancellation, StreamableHttpRequestResponseStream,
    StreamableHttpResponseStream, StreamableHttpTransport,
};
pub use memory::{MemoryRecvHalf, MemorySendHalf};
pub use sse::{ModernSseDecoder, ModernSseEndOfStream, ModernSseLimits, ModernSseParseError};
pub use stdio::{AsyncStdioTransport, StdioRecvHalf, StdioSendHalf, StdioTransport};

use asupersync::Cx;
use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse};

/// Absolute source-retention ceiling for one client-ingress JSON-RPC document.
///
/// Every [`ClientTransportRecvHalf`] implementation, including third-party
/// implementations, must create a [`ReceivedTransportFrame`] through
/// [`ReceivedTransportFrame::admit`]. This fixed ceiling prevents a transport
/// from passing an arbitrarily large raw-result sidecar to client decoding.
pub const MAX_CLIENT_TRANSPORT_SOURCE_BYTES: usize = 10 * 1024 * 1024;

/// Transport trait for context-aware message passing.
///
/// Each operation receives an asupersync capability context (`Cx`). Concrete
/// implementations document where they check cancellation or budget state.
///
/// # Cancel-Safety
///
/// Implementations should:
/// - Call `cx.checkpoint()` before blocking operations
/// - Use two-phase patterns (reserve/commit) where applicable
/// - Respect budget constraints from the context
///
/// # Example
///
/// ```ignore
/// impl Transport for MyTransport {
///     fn send(&mut self, cx: &Cx, msg: &JsonRpcMessage) -> Result<(), TransportError> {
///         cx.checkpoint()?;  // Check for cancellation
///         let bytes = self.codec.encode(msg)?;
///         self.write_all(&bytes)?;
///         Ok(())
///     }
/// }
/// ```
pub trait Transport {
    /// Send a JSON-RPC message through this transport.
    ///
    /// # Cancel-Safety
    ///
    /// Implementations must document their cancellation checkpoints. A
    /// transport backed by a generic blocking `Write` may check before the
    /// write but cannot necessarily interrupt that write once it starts.
    ///
    /// # Errors
    ///
    /// Returns an error if the transport is closed, an I/O error occurs,
    /// or the request has been cancelled.
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError>;

    /// Receive the next JSON-RPC message from this transport.
    ///
    /// # Cancel-Safety
    ///
    /// Implementations must document where they observe cancellation. A
    /// transport backed by a generic blocking `Read` may only check at frame
    /// boundaries; callers must not assume that every implementation can
    /// interrupt an in-progress blocking read.
    ///
    /// # Errors
    ///
    /// Returns an error if the transport is closed, an I/O error occurs,
    /// or the request has been cancelled.
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError>;

    /// Send a request through this transport.
    ///
    /// Convenience method that wraps a request in a message.
    fn send_request(&mut self, cx: &Cx, request: &JsonRpcRequest) -> Result<(), TransportError> {
        self.send(cx, &JsonRpcMessage::Request(request.clone()))
    }

    /// Send a response through this transport.
    ///
    /// Convenience method that wraps a response in a message.
    fn send_response(&mut self, cx: &Cx, response: &JsonRpcResponse) -> Result<(), TransportError> {
        self.send(cx, &JsonRpcMessage::Response(response.clone()))
    }

    /// Close the transport gracefully.
    ///
    /// This flushes any pending data and releases resources.
    fn close(&mut self, cx: &Cx) -> Result<(), TransportError>;
}

/// Independently owned receive half of a full-duplex MCP transport.
///
/// Unlike [`Transport`], this half never owns the response writer. A server
/// may therefore block in [`Self::recv`] while a request-owned child commits a
/// response through the matching [`TransportSendHalf`].
pub trait TransportRecvHalf {
    /// Receives the next JSON-RPC message.
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError>;

    /// Closes the receive half.
    fn close(&mut self, cx: &Cx) -> Result<(), TransportError>;
}

/// One decoded inbound message paired with its exact transport source.
///
/// The source excludes transport framing such as a terminating NDJSON newline,
/// but otherwise retains the complete JSON document exactly as admitted. Client
/// result decoders use it to preserve JSON-number lexemes and member order that
/// the typed [`JsonRpcMessage`] representation cannot reconstruct.
#[derive(Clone, Debug)]
pub struct ReceivedTransportFrame {
    message: JsonRpcMessage,
    source: Box<[u8]>,
}

impl ReceivedTransportFrame {
    /// Strictly admits one bounded source document and derives its typed message.
    ///
    /// This is intentionally the only constructor: callers cannot pair a
    /// typed message with a different raw source whose member order or number
    /// lexemes would later be treated as peer input.
    pub fn admit(source: impl Into<Box<[u8]>>) -> Result<Self, TransportError> {
        let source = source.into();
        if source.len() > MAX_CLIENT_TRANSPORT_SOURCE_BYTES {
            return Err(TransportError::Codec(CodecError::MessageTooLarge(
                source.len(),
            )));
        }

        let mut codec = Codec::new();
        codec.set_max_message_size(MAX_CLIENT_TRANSPORT_SOURCE_BYTES);
        let message = codec.decode_complete_message(&source)?;
        Ok(Self { message, source })
    }

    /// Returns the typed JSON-RPC message.
    #[must_use]
    pub const fn message(&self) -> &JsonRpcMessage {
        &self.message
    }

    /// Returns the exact admitted JSON document without transport framing.
    #[must_use]
    pub fn source(&self) -> &[u8] {
        &self.source
    }

    /// Splits the typed message from its owned admitted source.
    #[must_use]
    pub fn into_parts(self) -> (JsonRpcMessage, Box<[u8]>) {
        (self.message, self.source)
    }

    /// Returns the typed message and discards its retained source.
    #[must_use]
    pub fn into_message(self) -> JsonRpcMessage {
        self.message
    }
}

/// Client ingress that preserves the source needed by negotiated result decoding.
///
/// This extends the ordinary typed receive half without selecting a protocol
/// policy or performing any negotiation. A caller must construct it only after
/// its connection factory has chosen one immutable peer era.
pub trait ClientTransportRecvHalf: TransportRecvHalf + Send {
    /// Receives one typed message with its exact bounded admitted JSON source.
    fn recv_with_source(&mut self, cx: &Cx) -> Result<ReceivedTransportFrame, TransportError>;
}

/// Independently owned send half of a full-duplex MCP transport.
pub trait TransportSendHalf: Send {
    /// Sends one JSON-RPC message without acquiring the receive half.
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError>;

    /// Closes the send half.
    fn close(&mut self, cx: &Cx) -> Result<(), TransportError>;
}

/// Transport error types.
#[derive(Debug)]
pub enum TransportError {
    /// I/O error during read or write.
    Io(std::io::Error),
    /// Transport was closed (EOF or explicit close).
    Closed,
    /// Codec error (JSON parsing or encoding).
    Codec(CodecError),
    /// Connection timeout.
    Timeout,
    /// A caller-supplied receive deadline elapsed before completion was
    /// returned to that caller.
    ///
    /// This is distinct from [`Self::Timeout`], which can reflect exhaustion
    /// of the capability context carried by the connection. Keeping the two
    /// outcomes separate lets request owners preserve the result that was
    /// selected at the I/O boundary without re-reading mutable context state.
    /// Strict receive helpers may also report this after consuming a complete
    /// frame that finished at or beyond the supplied deadline; their API docs
    /// state whether that condition latches the transport closed.
    ReceiveDeadlineExceeded,
    /// A connection-control frame exceeds the transport's atomic-write bound.
    ///
    /// This is a local capacity failure, not a protocol codec failure: the
    /// message can be valid JSON-RPC and still be too large for the direct
    /// stdio adapter's single nonblocking pipe write. No bytes are committed
    /// when this error is returned.
    ControlFrameTooLarge {
        /// Encoded control-frame size, including transport framing.
        size: usize,
        /// Maximum frame size this transport can commit atomically.
        max: usize,
    },
    /// Request was cancelled.
    Cancelled,
}

impl TransportError {
    /// Returns true if this is a cancellation error.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, TransportError::Cancelled)
    }

    /// Returns true if this is an EOF/closed condition.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(self, TransportError::Closed)
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "I/O error: {e}"),
            TransportError::Closed => write!(f, "Transport closed"),
            TransportError::Codec(e) => write!(f, "Codec error: {e}"),
            TransportError::Timeout => write!(f, "Connection timeout"),
            TransportError::ReceiveDeadlineExceeded => {
                write!(f, "Receive deadline exceeded")
            }
            TransportError::ControlFrameTooLarge { size, max } => {
                write!(
                    f,
                    "Control frame requires {size} bytes but atomic capacity is {max} bytes"
                )
            }
            TransportError::Cancelled => write!(f, "Request cancelled"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Io(e) => Some(e),
            TransportError::Codec(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(err: std::io::Error) -> Self {
        TransportError::Io(err)
    }
}

impl From<CodecError> for TransportError {
    fn from(err: CodecError) -> Self {
        TransportError::Codec(err)
    }
}

// =============================================================================
// Two-Phase Send Protocol
// =============================================================================

/// A permit returned after a send-cancellation preflight.
///
/// `reserve_send` checks cancellation and gives the permit an exclusive borrow
/// of the transport's writer and codec. Consuming the permit encodes, writes,
/// and flushes synchronously without another cancellation check.
///
/// # Delivery behavior
///
/// A permit is not a transactional delivery guarantee. Encoding, writing, or
/// flushing can still fail, and an I/O error can occur after a partial write.
/// Any commit-phase I/O error latches the originating transport terminal;
/// encoding failures happen before I/O and leave it reusable.
///
/// ```ignore
/// let permit = transport.reserve_send(cx)?; // Cancellation preflight
/// permit.send(message)?;                     // Codec/I/O errors remain possible
/// ```
///
/// # Example
///
/// ```ignore
/// use fastmcp_transport::{AsyncStdioTransport, TwoPhaseTransport};
/// use asupersync::Cx;
///
/// let mut transport = AsyncStdioTransport::new();
/// let cx = Cx::for_testing();
///
/// // Check cancellation and borrow the send path
/// let permit = transport.reserve_send(&cx)?;
///
/// // No further cancellation check; codec/I/O errors are still returned
/// permit.send(&JsonRpcMessage::Request(request))?;
/// ```
pub struct SendPermit<'a, W: std::io::Write> {
    writer: &'a mut W,
    codec: &'a Codec,
    terminal: &'a mut bool,
}

impl<'a, W: std::io::Write> SendPermit<'a, W> {
    /// Creates a new send permit.
    ///
    /// This is an internal constructor. Use `TwoPhaseTransport::reserve_send()`
    /// to obtain a permit.
    fn new(writer: &'a mut W, codec: &'a Codec, terminal: &'a mut bool) -> Self {
        Self {
            writer,
            codec,
            terminal,
        }
    }

    fn commit(self, bytes: &[u8]) -> Result<(), TransportError> {
        if let Err(error) = self.writer.write_all(bytes) {
            *self.terminal = true;
            return Err(TransportError::Io(error));
        }
        if let Err(error) = self.writer.flush() {
            *self.terminal = true;
            return Err(TransportError::Io(error));
        }
        Ok(())
    }

    /// Consumes the permit and writes the message.
    ///
    /// This method is synchronous and performs no additional cancellation
    /// check after reservation. The permit is consumed whether the operation
    /// succeeds or fails.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding, writing, or flushing fails. An I/O error
    /// does not imply that zero bytes were written and latches the transport
    /// terminal.
    pub fn send(self, message: &JsonRpcMessage) -> Result<(), TransportError> {
        let bytes = match message {
            JsonRpcMessage::Request(req) => self.codec.encode_request(req)?,
            JsonRpcMessage::Response(resp) => self.codec.encode_response(resp)?,
        };

        self.commit(&bytes)
    }

    /// Commits the send by writing a request.
    ///
    /// Convenience method for sending a request directly.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding, writing, or flushing fails. An I/O error
    /// can occur after a partial write and latches the transport terminal.
    pub fn send_request(self, request: &JsonRpcRequest) -> Result<(), TransportError> {
        let bytes = self.codec.encode_request(request)?;
        self.commit(&bytes)
    }

    /// Commits the send by writing a response.
    ///
    /// Convenience method for sending a response directly.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding, writing, or flushing fails. An I/O error
    /// can occur after a partial write and latches the transport terminal.
    pub fn send_response(self, response: &JsonRpcResponse) -> Result<(), TransportError> {
        let bytes = self.codec.encode_response(response)?;
        self.commit(&bytes)
    }
}

/// Extension trait for two-phase send operations.
///
/// This trait splits cancellation preflight from synchronous send work:
///
/// - **Reserve phase**: Check cancellation and borrow the writer/codec
/// - **Send phase**: Encode, write, and flush without another cancellation check
///
/// The API does not make the underlying I/O transactional. Send methods can
/// return codec or I/O errors, including after a partial write.
///
/// ```ignore
/// fn send_after_preflight(cx: &Cx, msg: &JsonRpcMessage) -> Result<(), TransportError> {
///     let permit = transport.reserve_send(cx)?;
///     permit.send(msg)
/// }
/// ```
pub trait TwoPhaseTransport: Transport {
    /// The writer type for permits.
    type Writer: std::io::Write;

    /// Reserve a send slot.
    ///
    /// This is the cancellation preflight for sends. If it succeeds, the
    /// subsequent permit operation performs no further cancellation check;
    /// encoding and I/O can still fail.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::Cancelled` if the request has been cancelled.
    fn reserve_send(&mut self, cx: &Cx) -> Result<SendPermit<'_, Self::Writer>, TransportError>;
}

#[cfg(test)]
mod tests {
    use super::{
        Codec, CodecError, HttpHandlerConfig, HttpMethod, HttpRequest, HttpRequestHandler,
        HttpResponseRepresentation, MAX_CLIENT_TRANSPORT_SOURCE_BYTES, ReceivedTransportFrame,
        SendPermit, StreamableHttpTransport, Transport, TransportError, TwoPhaseTransport,
    };
    use asupersync::Cx;
    use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, RequestId};
    use std::error::Error;

    #[derive(Default)]
    struct RecordingTransport {
        sent: Vec<JsonRpcMessage>,
        closed: bool,
    }

    impl Transport for RecordingTransport {
        fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
            Err(TransportError::Closed)
        }

        fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
            self.closed = true;
            Ok(())
        }
    }

    struct TwoPhaseFixture {
        writer: Vec<u8>,
        codec: Codec,
        terminal: bool,
    }

    impl Default for TwoPhaseFixture {
        fn default() -> Self {
            Self {
                writer: Vec::new(),
                codec: Codec::new(),
                terminal: false,
            }
        }
    }

    impl Transport for TwoPhaseFixture {
        fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
            if self.terminal {
                return Err(TransportError::Closed);
            }
            let permit = self.reserve_send(cx)?;
            permit.send(message)
        }

        fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
            Err(TransportError::Closed)
        }

        fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
            self.terminal = true;
            Ok(())
        }
    }

    impl TwoPhaseTransport for TwoPhaseFixture {
        type Writer = Vec<u8>;

        fn reserve_send(
            &mut self,
            cx: &Cx,
        ) -> Result<SendPermit<'_, Self::Writer>, TransportError> {
            if cx.is_cancel_requested() {
                return Err(TransportError::Cancelled);
            }
            if self.terminal {
                return Err(TransportError::Closed);
            }

            Ok(SendPermit::new(
                &mut self.writer,
                &self.codec,
                &mut self.terminal,
            ))
        }
    }

    fn test_error(message: &str) -> Box<dyn Error> {
        std::io::Error::other(message).into()
    }

    fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
        if condition {
            Ok(())
        } else {
            Err(test_error(message))
        }
    }

    #[test]
    fn transport_error_predicates_match_variants() -> Result<(), Box<dyn Error>> {
        require(
            TransportError::Cancelled.is_cancelled(),
            "cancelled flag mismatch",
        )?;
        require(
            !TransportError::Timeout.is_cancelled(),
            "timeout should not be cancelled",
        )?;
        require(
            !TransportError::ReceiveDeadlineExceeded.is_cancelled(),
            "receive deadline should not be cancelled",
        )?;
        require(TransportError::Closed.is_closed(), "closed flag mismatch")?;
        require(
            !TransportError::Timeout.is_closed(),
            "timeout should not be closed",
        )?;
        require(
            !TransportError::ReceiveDeadlineExceeded.is_closed(),
            "receive deadline should not be closed",
        )?;
        Ok(())
    }

    #[test]
    fn received_transport_frame_derives_message_from_its_exact_bounded_source() {
        let source = br#"{"jsonrpc":"2.0","id":73,"result":{"zeta":1.20e+4,"alpha":{"second":2,"first":1}}}"#;
        let frame = ReceivedTransportFrame::admit(source.to_vec().into_boxed_slice())
            .expect("one bounded response source is strictly admitted");

        assert_eq!(frame.source(), source);
        let JsonRpcMessage::Response(response) = frame.message() else {
            panic!("the admitted response source must derive a response message");
        };
        assert_eq!(response.id, Some(RequestId::Number(73)));
    }

    #[test]
    fn received_transport_frame_rejects_source_above_the_fixed_hard_ceiling() {
        let source = vec![b' '; MAX_CLIENT_TRANSPORT_SOURCE_BYTES + 1].into_boxed_slice();
        let error = ReceivedTransportFrame::admit(source)
            .expect_err("third-party ingress cannot retain a source beyond the hard ceiling");

        assert!(matches!(
            error,
            TransportError::Codec(CodecError::MessageTooLarge(size))
                if size == MAX_CLIENT_TRANSPORT_SOURCE_BYTES + 1
        ));
    }

    #[test]
    fn transport_error_display_and_source_are_exposed() -> Result<(), Box<dyn Error>> {
        let io_error = std::io::Error::other("write failed");
        let io_transport_error = TransportError::Io(io_error);
        require(
            io_transport_error.to_string() == "I/O error: write failed",
            "io display mismatch",
        )?;
        require(
            io_transport_error.source().is_some(),
            "io source should exist",
        )?;

        let json_error = match serde_json::from_str::<serde_json::Value>("not json") {
            Err(err) => err,
            Ok(_) => return Err(test_error("invalid json unexpectedly parsed")),
        };
        let codec_error = CodecError::from(json_error);
        let codec_transport_error = TransportError::Codec(codec_error);
        require(
            codec_transport_error
                .to_string()
                .starts_with("Codec error: JSON error:"),
            "codec display mismatch",
        )?;
        require(
            codec_transport_error.source().is_some(),
            "codec source should exist",
        )?;

        require(
            TransportError::Timeout.source().is_none(),
            "timeout should not have source",
        )?;
        require(
            TransportError::ReceiveDeadlineExceeded.source().is_none(),
            "receive deadline should not have source",
        )?;
        require(
            TransportError::ControlFrameTooLarge {
                size: 513,
                max: 512,
            }
            .source()
            .is_none(),
            "control-frame capacity should not have source",
        )?;
        require(
            TransportError::Closed.source().is_none(),
            "closed should not have source",
        )?;
        require(
            TransportError::Cancelled.source().is_none(),
            "cancelled should not have source",
        )?;
        Ok(())
    }

    #[test]
    fn transport_error_from_conversions_wrap_underlying_types() -> Result<(), Box<dyn Error>> {
        let io_transport_error = TransportError::from(std::io::Error::other("socket closed"));
        require(
            matches!(io_transport_error, TransportError::Io(_)),
            "io conversion mismatch",
        )?;

        let json_error = match serde_json::from_str::<serde_json::Value>("bad json") {
            Err(err) => err,
            Ok(_) => return Err(test_error("invalid json unexpectedly parsed")),
        };
        let codec_transport_error = TransportError::from(CodecError::from(json_error));
        require(
            matches!(codec_transport_error, TransportError::Codec(_)),
            "codec conversion mismatch",
        )?;
        Ok(())
    }

    #[test]
    fn send_request_wraps_request_message() -> Result<(), Box<dyn Error>> {
        let mut transport = RecordingTransport::default();
        let cx = Cx::for_testing();
        let request = JsonRpcRequest::new("tools/list", None, 7i64);

        transport.send_request(&cx, &request)?;

        require(transport.sent.len() == 1, "expected one sent message")?;
        match &transport.sent[0] {
            JsonRpcMessage::Request(req) => {
                require(req.method == "tools/list", "request method mismatch")?;
                require(
                    req.id == Some(RequestId::Number(7)),
                    "request id mismatch for wrapped message",
                )?;
            }
            JsonRpcMessage::Response(_) => {
                return Err(test_error("expected request message"));
            }
        }
        Ok(())
    }

    #[test]
    fn send_response_wraps_response_message() -> Result<(), Box<dyn Error>> {
        let mut transport = RecordingTransport::default();
        let cx = Cx::for_testing();
        let response = JsonRpcResponse::success(
            RequestId::Number(9),
            serde_json::json!({"server": "fastmcp"}),
        );

        transport.send_response(&cx, &response)?;

        require(transport.sent.len() == 1, "expected one sent message")?;
        match &transport.sent[0] {
            JsonRpcMessage::Response(resp) => {
                require(
                    resp.id == Some(RequestId::Number(9)),
                    "response id mismatch for wrapped message",
                )?;
            }
            JsonRpcMessage::Request(_) => {
                return Err(test_error("expected response message"));
            }
        }
        Ok(())
    }

    #[test]
    fn send_permit_writes_request_bytes() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();
        let request = JsonRpcRequest::new("resources/list", None, 11i64);

        let permit = fixture.reserve_send(&cx)?;
        permit.send_request(&request)?;

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 1, "expected one decoded message")?;
        match &messages[0] {
            JsonRpcMessage::Request(req) => {
                require(
                    req.method == "resources/list",
                    "decoded request method mismatch",
                )?;
                require(
                    req.id == Some(RequestId::Number(11)),
                    "decoded request id mismatch",
                )?;
            }
            JsonRpcMessage::Response(_) => {
                return Err(test_error("expected request message"));
            }
        }
        Ok(())
    }

    #[test]
    fn send_permit_writes_response_bytes() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();
        let response =
            JsonRpcResponse::success(RequestId::Number(22), serde_json::json!({"status": "ok"}));

        let permit = fixture.reserve_send(&cx)?;
        permit.send_response(&response)?;

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 1, "expected one decoded message")?;
        match &messages[0] {
            JsonRpcMessage::Response(resp) => {
                require(
                    resp.id == Some(RequestId::Number(22)),
                    "decoded response id mismatch",
                )?;
            }
            JsonRpcMessage::Request(_) => {
                return Err(test_error("expected response message"));
            }
        }
        Ok(())
    }

    #[test]
    fn reserve_send_returns_cancelled_when_context_is_cancelled() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let mut fixture = TwoPhaseFixture::default();

        let result = fixture.reserve_send(&cx);

        match result {
            Err(TransportError::Cancelled) => Ok(()),
            _ => Err(test_error("reserve_send should return cancelled")),
        }
    }

    #[test]
    fn recording_transport_close() {
        let mut transport = RecordingTransport::default();
        assert!(!transport.closed);
        let cx = Cx::for_testing();
        transport.close(&cx).unwrap();
        assert!(transport.closed);
    }

    #[test]
    fn recording_transport_recv_returns_closed() {
        let mut transport = RecordingTransport::default();
        let cx = Cx::for_testing();
        let result = transport.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    #[test]
    fn transport_error_display_all_variants() {
        assert_eq!(TransportError::Closed.to_string(), "Transport closed");
        assert_eq!(TransportError::Timeout.to_string(), "Connection timeout");
        assert_eq!(
            TransportError::ReceiveDeadlineExceeded.to_string(),
            "Receive deadline exceeded"
        );
        assert_eq!(
            TransportError::ControlFrameTooLarge {
                size: 513,
                max: 512
            }
            .to_string(),
            "Control frame requires 513 bytes but atomic capacity is 512 bytes"
        );
        assert_eq!(TransportError::Cancelled.to_string(), "Request cancelled");
    }

    #[test]
    fn transport_error_is_cancelled_false_for_other_variants() {
        assert!(!TransportError::Closed.is_cancelled());
        assert!(!TransportError::ReceiveDeadlineExceeded.is_cancelled());
        assert!(
            !TransportError::ControlFrameTooLarge {
                size: 513,
                max: 512
            }
            .is_cancelled()
        );
        assert!(!TransportError::Io(std::io::Error::other("err")).is_cancelled());
        assert!(!TransportError::Codec(CodecError::MessageTooLarge(1)).is_cancelled());
    }

    #[test]
    fn transport_error_is_closed_false_for_other_variants() {
        assert!(!TransportError::Cancelled.is_closed());
        assert!(!TransportError::Timeout.is_closed());
        assert!(!TransportError::ReceiveDeadlineExceeded.is_closed());
        assert!(
            !TransportError::ControlFrameTooLarge {
                size: 513,
                max: 512
            }
            .is_closed()
        );
        assert!(!TransportError::Io(std::io::Error::other("err")).is_closed());
        assert!(!TransportError::Codec(CodecError::MessageTooLarge(1)).is_closed());
    }

    fn root_modern_sse_request() -> HttpRequest {
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "weather",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            })),
            1_001_i64,
        );
        HttpRequest::new(HttpMethod::Post, "/mcp")
            .with_header("content-type", "application/json")
            .with_header("accept", "text/event-stream")
            .with_header("MCP-Protocol-Version", "2026-07-28")
            .with_header("Mcp-Method", "tools/call")
            .with_header("Mcp-Name", "weather")
            .with_body(serde_json::to_vec(&request).expect("serialize modern request"))
    }

    #[test]
    fn modern_http_root_api_admits_and_finishes_one_sse_response() {
        let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: "/mcp".to_owned(),
            ..HttpHandlerConfig::default()
        });
        let admission = handler
            .admit_modern_request(&root_modern_sse_request())
            .expect("the root public HTTP API admits a matching modern request");
        assert_eq!(
            admission.response_representation(),
            HttpResponseRepresentation::Sse
        );

        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("one response slot is valid");
        let responses = transport
            .response_stream()
            .expect("the root public response stream is available");
        let body = admission
            .bind_sse_response_body(&responses)
            .expect("the admitted request binds one root public SSE body");
        let cancellation = body.cancellation();
        let cx = Cx::for_testing();

        transport
            .send_response_for_request(
                &cx,
                &cancellation,
                JsonRpcResponse::success(
                    RequestId::Number(1_001),
                    serde_json::json!({"forecast": "clear"}),
                ),
            )
            .expect("the request-bound terminal response commits once");
        assert_eq!(
            body.recv_response(&cx)
                .expect("the SSE body receives its terminal response")
                .id,
            Some(RequestId::Number(1_001))
        );
        assert!(body.is_finished());
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn modern_http_root_api_rejects_sse_binding_when_only_json_is_selected() {
        let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: "/mcp".to_owned(),
            ..HttpHandlerConfig::default()
        });
        let mut request = root_modern_sse_request();
        // Planted forbidden dimension: only response negotiation changes.
        request
            .headers
            .insert("accept".to_owned(), "application/json".to_owned());
        let admission = handler
            .admit_modern_request(&request)
            .expect("JSON remains an admissible response representation");
        assert_eq!(
            admission.response_representation(),
            HttpResponseRepresentation::Json
        );

        let mut transport = StreamableHttpTransport::new();
        let responses = transport
            .response_stream()
            .expect("the root public response stream is available");
        let bodies_before = responses
            .live_request_bodies()
            .expect("the root public body registry is observable");

        assert!(matches!(
            admission.bind_sse_response_body(&responses),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert_eq!(
            responses
                .live_request_bodies()
                .expect("rejected SSE binding leaves no response body"),
            bodies_before
        );
        assert!(
            handler.admit_modern_request(&request).is_ok(),
            "the planted negative changes only the response representation"
        );
    }

    #[cfg(not(feature = "legacy-2024-11-05"))]
    #[test]
    fn stripped_graph_retains_modern_http_and_sse_surfaces() {
        let _: Option<super::http::HttpRequestHandler> = None;
        let _: Option<super::http::ModernHttpRequestAdmission> = None;
        let _: Option<super::http::StreamableHttpTransport> = None;
        let _: Option<super::sse::ModernSseDecoder> = None;
        let _: Option<super::memory::MemoryTransport> = None;
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn legacy_feature_exposes_only_the_opt_in_adapter_surface() {
        let _: Option<super::http::LegacySseHttpPostSink> = None;
        let _: Option<super::http::DualEraHttpEndpoint> = None;
        let _: Option<super::sse::LegacySseMessagePost> = None;
        let _: Option<super::sse::LegacySseClientTransport<std::io::Empty, ()>> = None;
    }

    #[test]
    fn send_permit_sends_request_as_message() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();
        let request = JsonRpcRequest::new("tools/call", None, 1i64);

        let permit = fixture.reserve_send(&cx)?;
        permit.send(&JsonRpcMessage::Request(request))?;

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 1, "expected one decoded message")?;
        match &messages[0] {
            JsonRpcMessage::Request(req) => {
                require(req.method == "tools/call", "method mismatch")?;
            }
            _ => return Err(test_error("expected request")),
        }
        Ok(())
    }

    #[test]
    fn send_permit_sends_response_as_message() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();
        let response =
            JsonRpcResponse::success(RequestId::Number(5), serde_json::json!({"ok": true}));

        let permit = fixture.reserve_send(&cx)?;
        permit.send(&JsonRpcMessage::Response(response))?;

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 1, "expected one decoded message")?;
        assert!(matches!(&messages[0], JsonRpcMessage::Response(_)));
        Ok(())
    }

    #[test]
    fn send_multiple_messages_via_transport() {
        let mut transport = RecordingTransport::default();
        let cx = Cx::for_testing();

        for i in 0..5 {
            let request = JsonRpcRequest::new(format!("method/{i}"), None, i as i64);
            transport.send_request(&cx, &request).unwrap();
        }

        assert_eq!(transport.sent.len(), 5);
        for (i, msg) in transport.sent.iter().enumerate() {
            if let JsonRpcMessage::Request(req) = msg {
                assert_eq!(req.method, format!("method/{i}"));
            } else {
                panic!("expected request at index {i}");
            }
        }
    }

    #[test]
    fn two_phase_fixture_send_via_transport_trait() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();
        let request = JsonRpcRequest::new("test/method", None, 42i64);

        // Use the Transport::send method which delegates to reserve_send
        fixture.send(&cx, &JsonRpcMessage::Request(request))?;

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 1, "expected one decoded message")?;
        Ok(())
    }

    #[test]
    fn two_phase_fixture_close_succeeds() {
        let mut fixture = TwoPhaseFixture::default();
        let cx = Cx::for_testing();
        assert!(fixture.close(&cx).is_ok());
    }

    #[test]
    fn two_phase_multiple_sends() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();

        // Send multiple messages through two-phase
        for i in 0..3 {
            let permit = fixture.reserve_send(&cx)?;
            let request = JsonRpcRequest::new(format!("method/{i}"), None, i as i64);
            permit.send_request(&request)?;
        }

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 3, "expected three decoded messages")?;
        Ok(())
    }

    #[test]
    fn send_permit_notification_without_id() -> Result<(), Box<dyn Error>> {
        let cx = Cx::for_testing();
        let mut fixture = TwoPhaseFixture::default();
        let notification = JsonRpcRequest::notification("notifications/progress", None);

        let permit = fixture.reserve_send(&cx)?;
        permit.send_request(&notification)?;

        let mut decode_codec = Codec::new();
        let messages = decode_codec.decode(&fixture.writer)?;
        require(messages.len() == 1, "expected one decoded message")?;
        if let JsonRpcMessage::Request(req) = &messages[0] {
            require(req.id.is_none(), "notification should have no id")?;
        }
        Ok(())
    }
}
