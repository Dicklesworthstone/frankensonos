//! Server-Sent Events (SSE) transport for MCP.
//!
//! SSE provides a unidirectional event stream from server to client, with
//! client-to-server communication handled via HTTP POST requests.
//!
//! # MCP SSE Protocol
//!
//! The MCP SSE transport works as follows:
//!
//! 1. **Client connects** to the server's SSE endpoint
//! 2. **Server sends `endpoint` event** containing the POST URL for client messages
//! 3. **Client sends requests** via HTTP POST to the endpoint URL
//! 4. **Server sends responses** as `message` events on the SSE stream
//!
//! # Wire Format
//!
//! SSE events follow the standard format:
//! ```text
//! event: <event-type>
//! data: <JSON payload>
//!
//! ```
//!
//! Event types:
//! - `endpoint`: Contains the URL for client POST requests
//! - `message`: Contains a JSON-RPC message (request or response)
//!
//! # Size limits
//!
//! SSE wire lines are limited to 64 KiB. Because a message data line also
//! contains the `data: ` prefix and newline delimiter, JSON-RPC message data
//! is limited to 65,529 bytes. Both [`SseWriter`] and [`SseReader`] enforce
//! that same effective message ceiling before codec admission.
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_transport::sse::{SseEvent, SseEventType, SseWriter};
//! use fastmcp_protocol::JsonRpcResponse;
//!
//! // Create an SSE writer for the response stream
//! let mut writer = SseWriter::new(response_body);
//!
//! // Send the endpoint event first
//! writer.write_endpoint("http://localhost:8080/mcp/messages")?;
//!
//! // Send JSON-RPC responses as message events
//! let response = JsonRpcResponse { /* ... */ };
//! writer.write_response(&response)?;
//! ```
//!
//! # Cancellation checks
//!
//! Readers and writers checkpoint the caller's cancellation, deadline and
//! execution budget before I/O and between partial operations. A budget refusal
//! before progress is retryable; abandoning a partially consumed or written
//! frame makes that stream terminal to prevent frame splicing. Caller-provided
//! synchronous `Read`, `Write`, flush, iterator and POST implementations are
//! not made interruptible once a call has entered them.
//!
//! # Integration Note
//!
//! This module provides SSE event handling but does NOT include an HTTP server.
//! You'll need to integrate with an HTTP server framework that works with
//! asupersync (or use the provided adapters if available).

#[cfg(feature = "legacy-2024-11-05")]
use std::io::{BufReader, Read, Write};

#[cfg(feature = "legacy-2024-11-05")]
use asupersync::Cx;
#[cfg(feature = "legacy-2024-11-05")]
use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse};

#[cfg(feature = "legacy-2024-11-05")]
use crate::{Codec, Transport, TransportRecvHalf, TransportSendHalf};
use crate::{CodecError, TransportError};

/// Observes the caller's budget, mapping an exhausted one to a typed refusal.
///
/// Mirrors `stdio_checkpoint` and `http_checkpoint` exactly, and is deliberately
/// STRONGER than this module's older `cx.is_cancel_requested()` guard: that bit
/// detects explicit cancellation only, so a caller whose DEADLINE has already
/// expired passes it and proceeds into blocking I/O. `Cx::checkpoint` observes
/// deadline exhaustion, cancellation, and poll/cost quota exhaustion, and
/// distinguishes `Timeout` from `Cancelled` for the caller.
///
/// All legacy SSE I/O entry points share this classification. It does not make
/// a synchronous adapter interruptible while that adapter is executing.
// Keep the helper and its Cx import behind the same legacy feature boundary.
#[cfg(feature = "legacy-2024-11-05")]
fn sse_checkpoint(cx: &Cx) -> Result<(), TransportError> {
    cx.checkpoint().map_err(|error| {
        use asupersync::{CancelKind, error::ErrorKind};

        match cx.cancel_reason().map(|reason| reason.kind) {
            Some(CancelKind::Deadline | CancelKind::Timeout) => TransportError::Timeout,
            Some(_) => TransportError::Cancelled,
            None => match error.kind() {
                ErrorKind::DeadlineExceeded | ErrorKind::CancelTimeout => TransportError::Timeout,
                ErrorKind::Cancelled
                | ErrorKind::PollQuotaExhausted
                | ErrorKind::CostQuotaExhausted => TransportError::Cancelled,
                _ => TransportError::Cancelled,
            },
        }
    })
}

/// Commits a bounded frame without allowing an expired caller to enter another
/// partial write or flush. This cannot interrupt a synchronous I/O call that
/// has already entered the kernel or caller-supplied adapter.
///
/// Cancellation before the first accepted byte is retryable. After any byte
/// escapes, a refusal is terminal: appending a new frame could otherwise splice
/// it into the unfinished one. A successful flush is the commit point; no
/// checkpoint may retroactively turn that success into a retryable failure.
#[cfg(feature = "legacy-2024-11-05")]
fn sse_commit_frame<W: Write>(
    cx: &Cx,
    writer: &mut W,
    closed: &mut bool,
    mut bytes: &[u8],
) -> Result<(), TransportError> {
    let mut wrote_any = false;
    while !bytes.is_empty() {
        if let Err(error) = sse_checkpoint(cx) {
            *closed |= wrote_any;
            return Err(error);
        }
        match writer.write(bytes) {
            Ok(0) => {
                *closed = true;
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "SSE frame write made no progress",
                )));
            }
            Ok(written) => {
                // Defend this slicing boundary even against an invalid custom
                // Write implementation; do not panic with a half-written frame.
                let Some(remaining) = bytes.get(written..) else {
                    *closed = true;
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "SSE writer reported an invalid byte count",
                    )));
                };
                wrote_any = true;
                bytes = remaining;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                *closed = true;
                return Err(TransportError::Io(error));
            }
        }
    }
    loop {
        if let Err(error) = sse_checkpoint(cx) {
            *closed |= wrote_any;
            return Err(error);
        }
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                *closed = true;
                return Err(TransportError::Io(error));
            }
        }
    }
}

/// Maximum wire-line size for SSE events.
const MAX_SSE_LINE_SIZE: usize = 64 * 1024;

/// Bytes added around one serialized data line: `data: ` plus LF.
const SSE_DATA_LINE_WIRE_OVERHEAD: usize = b"data: \n".len();

/// Effective JSON-RPC message ceiling for this SSE implementation.
const MAX_SSE_MESSAGE_SIZE: usize = MAX_SSE_LINE_SIZE - SSE_DATA_LINE_WIRE_OVERHEAD;

/// Explicit bounds for a modern request-scoped SSE response body.
///
/// Modern Streamable HTTP uses the WHATWG event-stream framing rules.  The
/// collector receives chunks from the HTTP body, so both the current line and
/// the assembled event need their own bounded accounting rather than relying
/// on a caller to retain an arbitrary body prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModernSseLimits {
    line_bytes: usize,
    event_bytes: usize,
    keepalive_lines: usize,
}

impl ModernSseLimits {
    /// Creates nonzero bounds for one modern HTTP SSE body.
    #[must_use]
    pub const fn new(
        max_line_bytes: usize,
        max_event_bytes: usize,
        max_keepalive_lines: usize,
    ) -> Option<Self> {
        if max_line_bytes == 0 || max_event_bytes == 0 || max_keepalive_lines == 0 {
            return None;
        }
        Some(Self {
            line_bytes: max_line_bytes,
            event_bytes: max_event_bytes,
            keepalive_lines: max_keepalive_lines,
        })
    }

    /// Returns the maximum raw or decoded bytes retained for one line.
    #[must_use]
    pub const fn max_line_bytes(self) -> usize {
        self.line_bytes
    }

    /// Returns the maximum raw or decoded bytes retained for one event.
    #[must_use]
    pub const fn max_event_bytes(self) -> usize {
        self.event_bytes
    }

    /// Returns the maximum non-dispatching lines accepted between events.
    #[must_use]
    pub const fn max_keepalive_lines(self) -> usize {
        self.keepalive_lines
    }
}

/// The bounded parser's report when the HTTP response body reaches EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModernSseEndOfStream {
    /// Whether an unterminated event with at least one `data` field was discarded.
    pub discarded_pending_event: bool,
    /// Whether an unterminated final line was discarded.
    pub discarded_partial_line: bool,
}

/// Refusals from the bounded modern HTTP SSE framing parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModernSseParseError {
    /// A raw or replacement-decoded line exceeds its configured bound.
    LineTooLong {
        /// The configured line bound.
        limit_bytes: usize,
    },
    /// An assembled event exceeds its configured bound.
    EventTooLarge {
        /// The configured event bound.
        limit_bytes: usize,
    },
    /// Too many inert/comment-only lines arrived before a dispatch.
    KeepaliveFlood {
        /// The configured line-count bound.
        limit_lines: usize,
    },
    /// Input was supplied after an earlier refusal released parser state.
    Poisoned,
}

/// The outcome of incrementally dispatching a decoded modern SSE event.
///
/// This remains crate-private because public callers use [`ModernSseDecoder::push`].
/// The HTTP collector uses it to process each completed event immediately
/// instead of retaining every event from an arbitrarily large body chunk.
#[derive(Debug)]
pub(crate) enum ModernSsePushError<E> {
    /// Bounded SSE framing refused the input.
    Parse(ModernSseParseError),
    /// The immediate event consumer refused a completed payload.
    Consumer(E),
}

impl std::fmt::Display for ModernSseParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineTooLong { limit_bytes } => {
                write!(formatter, "modern SSE line exceeds {limit_bytes} bytes")
            }
            Self::EventTooLarge { limit_bytes } => {
                write!(formatter, "modern SSE event exceeds {limit_bytes} bytes")
            }
            Self::KeepaliveFlood { limit_lines } => {
                write!(
                    formatter,
                    "modern SSE stream exceeds {limit_lines} inert lines"
                )
            }
            Self::Poisoned => formatter.write_str("modern SSE parser already refused input"),
        }
    }
}

impl std::error::Error for ModernSseParseError {}

/// Incremental, chunk-invariant WHATWG event-stream decoder for modern HTTP.
///
/// Each successful [`Self::push`] returns only completed `data` payloads in
/// wire order.  `event`, `id`, `retry`, and unknown fields remain framing
/// inputs only: modern request-scoped bodies must not acquire reconnect or
/// cross-request routing state from them.
#[derive(Debug)]
pub struct ModernSseDecoder {
    limits: ModernSseLimits,
    raw_line: Vec<u8>,
    pending_cr: bool,
    bom_window_open: bool,
    data: String,
    event_raw_bytes: usize,
    keepalive_lines: usize,
    poisoned: bool,
}

impl ModernSseDecoder {
    /// Creates an empty bounded decoder.
    #[must_use]
    pub fn new(limits: ModernSseLimits) -> Self {
        Self {
            limits,
            raw_line: Vec::new(),
            pending_cr: false,
            bom_window_open: true,
            data: String::new(),
            event_raw_bytes: 0,
            keepalive_lines: 0,
            poisoned: false,
        }
    }

    /// Feeds one HTTP body chunk and returns every completed SSE data payload.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, ModernSseParseError> {
        let mut dispatched = Vec::new();
        match self.push_with(chunk, |payload| {
            dispatched.push(payload);
            Ok::<(), std::convert::Infallible>(())
        }) {
            Ok(()) => Ok(dispatched),
            Err(ModernSsePushError::Parse(error)) => Err(error),
            Err(ModernSsePushError::Consumer(never)) => match never {},
        }
    }

    /// Feeds one HTTP body chunk and immediately dispatches each completed
    /// event in wire order.
    ///
    /// Unlike [`Self::push`], this does not retain all completed events from
    /// the supplied chunk.  A consumer refusal leaves final lifecycle policy
    /// to the owner of the stream.
    pub(crate) fn push_with<E>(
        &mut self,
        chunk: &[u8],
        mut consume: impl FnMut(String) -> Result<(), E>,
    ) -> Result<(), ModernSsePushError<E>> {
        if self.poisoned {
            return Err(ModernSsePushError::Parse(ModernSseParseError::Poisoned));
        }
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    if let Err(error) = self.complete_line(&mut consume) {
                        return Err(self.poison_push_error(error));
                    }
                    self.pending_cr = true;
                }
                b'\n' => {
                    if let Err(error) = self.complete_line(&mut consume) {
                        return Err(self.poison_push_error(error));
                    }
                }
                byte => {
                    if self.raw_line.len() >= self.limits.line_bytes {
                        return Err(ModernSsePushError::Parse(self.poison(
                            ModernSseParseError::LineTooLong {
                                limit_bytes: self.limits.line_bytes,
                            },
                        )));
                    }
                    self.raw_line.push(byte);
                }
            }
        }
        Ok(())
    }

    /// Finishes the body without synthesizing a final blank line.
    pub fn finish(self) -> Result<ModernSseEndOfStream, ModernSseParseError> {
        if self.poisoned {
            return Err(ModernSseParseError::Poisoned);
        }
        Ok(ModernSseEndOfStream {
            discarded_pending_event: !self.data.is_empty(),
            discarded_partial_line: !self.raw_line.is_empty(),
        })
    }

    fn poison(&mut self, error: ModernSseParseError) -> ModernSseParseError {
        self.raw_line = Vec::new();
        self.data = String::new();
        self.event_raw_bytes = 0;
        self.keepalive_lines = 0;
        self.poisoned = true;
        error
    }

    fn poison_push_error<E>(&mut self, error: ModernSsePushError<E>) -> ModernSsePushError<E> {
        match error {
            ModernSsePushError::Parse(error) => ModernSsePushError::Parse(self.poison(error)),
            ModernSsePushError::Consumer(error) => ModernSsePushError::Consumer(error),
        }
    }

    fn complete_line<E>(
        &mut self,
        consume: &mut impl FnMut(String) -> Result<(), E>,
    ) -> Result<(), ModernSsePushError<E>> {
        let mut raw = std::mem::take(&mut self.raw_line);
        if self.bom_window_open {
            self.bom_window_open = false;
            if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
                raw.drain(..3);
            }
        }
        let raw_len = raw.len();
        let line = String::from_utf8_lossy(&raw);
        if line.len() > self.limits.line_bytes {
            return Err(ModernSsePushError::Parse(self.poison(
                ModernSseParseError::LineTooLong {
                    limit_bytes: self.limits.line_bytes,
                },
            )));
        }
        self.process_line(&line, raw_len, consume)
    }

    fn process_line<E>(
        &mut self,
        line: &str,
        raw_len: usize,
        consume: &mut impl FnMut(String) -> Result<(), E>,
    ) -> Result<(), ModernSsePushError<E>> {
        if line.is_empty() {
            if self.data.is_empty() {
                return self.count_inert_line().map_err(ModernSsePushError::Parse);
            }
            let mut payload = std::mem::take(&mut self.data);
            debug_assert!(payload.ends_with('\n'));
            payload.pop();
            self.event_raw_bytes = 0;
            self.keepalive_lines = 0;
            return consume(payload).map_err(ModernSsePushError::Consumer);
        }
        if line.starts_with(':') {
            return self.count_inert_line().map_err(ModernSsePushError::Parse);
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        if field != "data" {
            return self.count_inert_line().map_err(ModernSsePushError::Parse);
        }
        let decoded_after = self
            .data
            .len()
            .saturating_add(value.len())
            .saturating_add(1);
        let raw_after = self.event_raw_bytes.saturating_add(raw_len);
        if decoded_after > self.limits.event_bytes || raw_after > self.limits.event_bytes {
            return Err(ModernSsePushError::Parse(self.poison(
                ModernSseParseError::EventTooLarge {
                    limit_bytes: self.limits.event_bytes,
                },
            )));
        }
        self.data.push_str(value);
        self.data.push('\n');
        self.event_raw_bytes = raw_after;
        self.keepalive_lines = 0;
        Ok(())
    }

    fn count_inert_line(&mut self) -> Result<(), ModernSseParseError> {
        self.keepalive_lines = self.keepalive_lines.saturating_add(1);
        if self.keepalive_lines > self.limits.keepalive_lines {
            return Err(ModernSseParseError::KeepaliveFlood {
                limit_lines: self.limits.keepalive_lines,
            });
        }
        Ok(())
    }
}

// =============================================================================
// SSE Event Types
// =============================================================================

/// SSE event types used by MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseEventType {
    /// The `endpoint` event sent by the server to indicate the POST URL.
    Endpoint,
    /// The `message` event containing a JSON-RPC message.
    Message,
}

impl SseEventType {
    /// Returns the event type string for SSE format.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            SseEventType::Endpoint => "endpoint",
            SseEventType::Message => "message",
        }
    }

    /// Parse an event type from a string.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "endpoint" => Some(SseEventType::Endpoint),
            "message" => Some(SseEventType::Message),
            _ => None,
        }
    }
}

/// A parsed SSE event.
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// The event type.
    pub event_type: SseEventType,
    /// The event data (JSON string for messages, URL for endpoint).
    pub data: String,
    /// Optional event ID for reconnection.
    pub id: Option<String>,
    /// Optional retry interval in milliseconds.
    pub retry: Option<u64>,
}

impl SseEvent {
    /// Creates a new endpoint event with the given POST URL.
    #[must_use]
    pub fn endpoint(url: impl Into<String>) -> Self {
        Self {
            event_type: SseEventType::Endpoint,
            data: url.into(),
            id: None,
            retry: None,
        }
    }

    /// Creates a new message event with the given JSON data.
    #[must_use]
    pub fn message(data: impl Into<String>) -> Self {
        Self {
            event_type: SseEventType::Message,
            data: data.into(),
            id: None,
            retry: None,
        }
    }

    /// Sets the event ID.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Sets the retry interval.
    #[must_use]
    pub fn with_retry(mut self, retry_ms: u64) -> Self {
        self.retry = Some(retry_ms);
        self
    }

    /// Serializes the event to bounded SSE wire format.
    ///
    /// # Returns
    ///
    /// The SSE-formatted event as bytes, including the trailing blank line.
    ///
    /// # Errors
    ///
    /// Returns an error when event data or fields exceed the transport bounds,
    /// or when an ID/endpoint value contains characters that could inject SSE
    /// fields or event boundaries.
    pub fn to_bytes(&self) -> Result<Vec<u8>, TransportError> {
        self.validate()?;
        let mut output = Vec::with_capacity(self.data.len() + 64);

        // Event type
        output.extend_from_slice(b"event: ");
        output.extend_from_slice(self.event_type.as_str().as_bytes());
        output.push(b'\n');

        // Optional ID
        if let Some(ref id) = self.id {
            output.extend_from_slice(b"id: ");
            output.extend_from_slice(id.as_bytes());
            output.push(b'\n');
        }

        // Optional retry
        if let Some(retry) = self.retry {
            output.extend_from_slice(b"retry: ");
            output.extend_from_slice(retry.to_string().as_bytes());
            output.push(b'\n');
        }

        // Data (handle multi-line data by prefixing each line with "data: ").
        // `str::lines()` discards the trailing empty segment, but each such
        // segment is semantically significant in SSE: `"line\n"` must become
        // `data: line` followed by `data: ` so a reader reconstructs the
        // terminal newline. Normalize accepted CRLF input while retaining
        // every logical data field, including a final empty one.
        for line in self
            .data
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
        {
            output.extend_from_slice(b"data: ");
            output.extend_from_slice(line.as_bytes());
            output.push(b'\n');
        }

        // Blank line to terminate the event
        output.push(b'\n');

        Ok(output)
    }

    fn validate(&self) -> Result<(), TransportError> {
        if self.data.len() > MAX_SSE_MESSAGE_SIZE {
            return Err(TransportError::Codec(CodecError::MessageTooLarge(
                self.data.len(),
            )));
        }
        if has_invalid_sse_data_characters(&self.data) {
            return Err(invalid_sse_field("event data"));
        }
        for line in self
            .data
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
        {
            let wire_size = line.len().saturating_add(SSE_DATA_LINE_WIRE_OVERHEAD);
            if wire_size > MAX_SSE_LINE_SIZE {
                return Err(TransportError::Codec(CodecError::MessageTooLarge(
                    line.len(),
                )));
            }
        }
        if self.event_type == SseEventType::Endpoint
            && self
                .data
                .bytes()
                .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
        {
            return Err(invalid_sse_field("endpoint data"));
        }
        if let Some(id) = &self.id {
            if id.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | b'\0')) {
                return Err(invalid_sse_field("event ID"));
            }
            let wire_size = id.len().saturating_add(b"id: \n".len());
            if wire_size > MAX_SSE_LINE_SIZE {
                return Err(TransportError::Codec(CodecError::MessageTooLarge(id.len())));
            }
        }
        Ok(())
    }
}

fn has_invalid_sse_data_characters(data: &str) -> bool {
    let bytes = data.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| match byte {
        b'\0' => true,
        b'\r' => bytes.get(index + 1) != Some(&b'\n'),
        _ => false,
    })
}

fn invalid_sse_field(field: &str) -> TransportError {
    TransportError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("invalid {field} for SSE wire format"),
    ))
}

// =============================================================================
// SSE Writer
// =============================================================================

/// Writer for SSE event streams.
///
/// This writes properly formatted SSE events to any `Write` implementation.
/// It handles JSON-RPC message serialization and event formatting.
///
/// # Example
///
/// ```ignore
/// let mut writer = SseWriter::new(tcp_stream);
///
/// // Send endpoint event
/// writer.write_endpoint("http://localhost:8080/messages")?;
///
/// // Send a response
/// writer.write_message(&JsonRpcMessage::Response(response))?;
/// ```
#[cfg(feature = "legacy-2024-11-05")]
pub struct SseWriter<W> {
    writer: W,
    codec: Codec,
    event_counter: u64,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write> SseWriter<W> {
    /// Creates a new SSE writer.
    #[must_use]
    pub fn new(writer: W) -> Self {
        let mut codec = Codec::new();
        codec.set_max_message_size(MAX_SSE_MESSAGE_SIZE);
        Self {
            writer,
            codec,
            event_counter: 0,
            closed: false,
        }
    }

    fn commit(&mut self, cx: &Cx, bytes: &[u8]) -> Result<(), TransportError> {
        sse_commit_frame(cx, &mut self.writer, &mut self.closed, bytes)
    }

    /// Writes an SSE event.
    ///
    /// # Cancel-Safety
    ///
    /// Checks the caller's budget before encoding and between partial writes.
    /// A refusal after any bytes are written makes the writer terminal.
    pub fn write_event(&mut self, cx: &Cx, event: &SseEvent) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        let bytes = event.to_bytes()?;
        self.commit(cx, &bytes)
    }

    /// Writes the endpoint event with the POST URL.
    ///
    /// This should be the first event sent when a client connects.
    pub fn write_endpoint(&mut self, cx: &Cx, url: &str) -> Result<(), TransportError> {
        let event = SseEvent::endpoint(url);
        self.write_event(cx, &event)
    }

    /// Writes a JSON-RPC message as an SSE message event.
    pub fn write_message(
        &mut self,
        cx: &Cx,
        message: &JsonRpcMessage,
    ) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        let mut encoded = match message {
            JsonRpcMessage::Request(request) => self.codec.encode_request(request)?,
            JsonRpcMessage::Response(response) => self.codec.encode_response(response)?,
        };
        if encoded.pop() != Some(b'\n') {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "JSON-RPC codec omitted its NDJSON delimiter",
            )));
        }
        let json = String::from_utf8(encoded).map_err(|error| {
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;

        let next_event_id = self
            .event_counter
            .checked_add(1)
            .ok_or_else(|| invalid_sse_field("event ID counter"))?;
        let event = SseEvent::message(json).with_id(next_event_id.to_string());
        self.write_event(cx, &event)?;
        self.event_counter = next_event_id;
        Ok(())
    }

    /// Writes a JSON-RPC response as an SSE message event.
    pub fn write_response(
        &mut self,
        cx: &Cx,
        response: &JsonRpcResponse,
    ) -> Result<(), TransportError> {
        self.write_message(cx, &JsonRpcMessage::Response(response.clone()))
    }

    /// Writes a JSON-RPC request as an SSE message event.
    ///
    /// Note: In typical MCP SSE usage, the server sends responses and the
    /// client sends requests via POST. This method is provided for flexibility.
    pub fn write_request(
        &mut self,
        cx: &Cx,
        request: &JsonRpcRequest,
    ) -> Result<(), TransportError> {
        self.write_message(cx, &JsonRpcMessage::Request(request.clone()))
    }

    /// Sends a comment (for keep-alive).
    ///
    /// SSE comments start with `:` and are ignored by the client.
    /// They're useful for keeping connections alive.
    pub fn write_comment(&mut self, cx: &Cx, comment: &str) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        if comment
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
        {
            return Err(invalid_sse_field("comment"));
        }
        let wire_size = comment.len().saturating_add(b": \n".len());
        if wire_size > MAX_SSE_LINE_SIZE {
            return Err(TransportError::Codec(CodecError::MessageTooLarge(
                comment.len(),
            )));
        }

        // Comments are lines starting with ':'. Build the bounded line before
        // committing so any I/O error has one terminal-latch path.
        let mut bytes = Vec::with_capacity(wire_size);
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(comment.as_bytes());
        bytes.push(b'\n');
        self.commit(cx, &bytes)
    }

    /// Sends a keep-alive comment.
    pub fn keep_alive(&mut self, cx: &Cx) -> Result<(), TransportError> {
        self.write_comment(cx, "keep-alive")
    }

    /// Closes the writer terminally, flushing any buffered output once.
    pub fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        // Observe the caller's budget BEFORE the terminal commit, so a cancelled
        // or expired caller is not made to wait on the flush below. Refusing here
        // leaves the writer non-terminal and retryable; placing the check after
        // the already-closed branch keeps an idempotent close Ok, because
        // terminal state performs no I/O and has no budget to spend.
        //
        // `sse_checkpoint` rather than this module's older
        // `cx.is_cancel_requested()` guard: that bit would let an expired
        // DEADLINE through into the blocking flush.
        sse_checkpoint(cx)?;
        self.closed = true;
        self.writer.flush().map_err(TransportError::Io)
    }

    /// Returns a reference to the underlying writer.
    pub fn inner(&self) -> &W {
        &self.writer
    }

    /// Returns a mutable reference to the underlying writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consumes the writer and returns the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

// =============================================================================
// SSE Reader
// =============================================================================

/// Reader for SSE event streams.
///
/// Parses SSE events from any `Read` implementation.
///
/// # Example
///
/// ```ignore
/// let mut reader = SseReader::new(tcp_stream);
///
/// loop {
///     match reader.read_event(&cx)? {
///         Some(event) => handle_event(event),
///         None => break, // EOF
///     }
/// }
/// ```
#[cfg(feature = "legacy-2024-11-05")]
pub struct SseReader<R> {
    reader: BufReader<R>,
    line_buffer: Vec<u8>,
    codec: Codec,
    /// Maximum line size to prevent memory exhaustion.
    max_line_size: usize,
    /// Whether EOF or a terminal framing/I/O error has ended this stream.
    terminal: bool,
    /// A CR terminates a line immediately; one following LF belongs to the
    /// same delimiter and is discarded at the start of the next read.
    discard_lf_after_cr: bool,
    /// Last valid SSE event ID, persisted across event blocks.
    last_event_id: Option<String>,
    /// Last valid reconnection delay, persisted across event blocks.
    reconnection_time: Option<u64>,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<R: Read> SseReader<R> {
    /// Creates a new SSE reader.
    #[must_use]
    pub fn new(reader: R) -> Self {
        let mut codec = Codec::new();
        codec.set_max_message_size(MAX_SSE_MESSAGE_SIZE);
        Self {
            reader: BufReader::new(reader),
            line_buffer: Vec::with_capacity(4096),
            codec,
            max_line_size: MAX_SSE_LINE_SIZE,
            terminal: false,
            discard_lf_after_cr: false,
            last_event_id: None,
            reconnection_time: None,
        }
    }

    /// Reads a line with size limit to prevent memory exhaustion.
    ///
    /// Returns the number of bytes read, or an error if the line exceeds
    /// the maximum size.
    ///
    /// # Note
    ///
    /// `consumed_input` belongs to the whole event, not just this line. It lets
    /// the caller distinguish a retryable no-progress budget refusal from a
    /// partially consumed event whose local parser state cannot be resumed.
    fn read_line_bounded(
        &mut self,
        cx: &Cx,
        consumed_input: &mut bool,
    ) -> Result<usize, TransportError> {
        use std::io::BufRead;

        let mut total_read = 0;
        loop {
            // A long line must not bypass the budget merely because no line
            // delimiter has arrived yet. Interrupted reads also return here.
            sse_checkpoint(cx)?;
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(TransportError::Io(error)),
            };
            if available.is_empty() {
                // EOF
                return Ok(total_read);
            }

            if self.discard_lf_after_cr {
                self.discard_lf_after_cr = false;
                if available[0] == b'\n' {
                    self.reader.consume(1);
                    *consumed_input = true;
                    continue;
                }
            }

            // Event streams recognize CR, LF, and CRLF. A CR ends this line;
            // a following LF is skipped by the next invocation, including
            // when the two delimiter bytes arrive in separate reads.
            let delimiter = available
                .iter()
                .position(|byte| matches!(*byte, b'\r' | b'\n'));
            let content_bytes = delimiter.unwrap_or(available.len());
            let bytes_to_consume = content_bytes + usize::from(delimiter.is_some());

            // Check if this would exceed our limit
            if self.line_buffer.len().saturating_add(bytes_to_consume) > self.max_line_size {
                self.line_buffer.clear();
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "SSE line exceeds maximum size of {} bytes",
                        self.max_line_size
                    ),
                )));
            }

            // Preserve raw bytes until the complete line is available. A
            // valid UTF-8 scalar may be split across two underlying reads.
            self.line_buffer
                .extend_from_slice(&available[..content_bytes]);
            total_read += bytes_to_consume;

            let ended_with_cr = delimiter.is_some_and(|position| available[position] == b'\r');

            self.reader.consume(bytes_to_consume);
            *consumed_input = true;

            if delimiter.is_some() {
                self.discard_lf_after_cr = ended_with_cr;
                return Ok(total_read);
            }
        }
    }

    /// Reads the next SSE event.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(event))`: An event was successfully read
    /// - `Ok(None)`: EOF reached
    /// - `Err(_)`: An error occurred
    ///
    /// # Cancel-Safety
    ///
    /// Checks the caller's full budget between reads, even within one line.
    /// A no-progress budget refusal leaves the reader reusable. Refusal after
    /// input is consumed makes the reader terminal rather than losing part of
    /// an event and interpreting its suffix as a new event.
    pub fn read_event(&mut self, cx: &Cx) -> Result<Option<SseEvent>, TransportError> {
        if self.terminal {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        let mut event_type: Option<SseEventType> = None;
        let mut unknown_event = false;
        let mut data_lines: Vec<String> = Vec::new();
        let mut total_data_size: usize = 0;
        let mut consumed_input = false;
        loop {
            self.line_buffer.clear();
            let bytes_read = match self.read_line_bounded(cx, &mut consumed_input) {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    if consumed_input
                        || !matches!(&error, TransportError::Cancelled | TransportError::Timeout)
                    {
                        self.terminal = true;
                    }
                    return Err(error);
                }
            };

            if bytes_read == 0 {
                // EOF
                self.terminal = true;
                return Ok(None);
            }

            // Check the budget again before exposing or processing a line.
            if let Err(error) = sse_checkpoint(cx) {
                // At least one line of the current event was consumed. The
                // parser state is local to this call, so resuming would splice
                // the remainder into a different event.
                self.terminal = true;
                return Err(error);
            }

            let line = std::str::from_utf8(&self.line_buffer).map_err(|error| {
                self.terminal = true;
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid UTF-8 in SSE line: {error}"),
                ))
            })?;

            // Empty line = end of event
            if line.is_empty() {
                // SSE dispatches only when at least one data field was seen.
                // An empty `data` field still counts and dispatches an event
                // with an empty payload; event/id/retry-only blocks do not.
                if !data_lines.is_empty() && !unknown_event {
                    let data = data_lines.join("\n");
                    return Ok(Some(SseEvent {
                        event_type: event_type.unwrap_or(SseEventType::Message),
                        data,
                        id: self.last_event_id.clone(),
                        retry: self.reconnection_time,
                    }));
                }

                // Per-event buffers never leak through an empty or ignored
                // block. ID and retry state deliberately persist separately.
                event_type = None;
                unknown_event = false;
                data_lines.clear();
                total_data_size = 0;
                continue;
            }

            // Comment line (starts with ':')
            if line.starts_with(':') {
                continue;
            }

            // A field without a colon has the empty string as its value.
            let (field, raw_value) = line.split_once(':').unwrap_or((line, ""));
            // If present, exactly one leading ASCII space is removed.
            let value = raw_value.strip_prefix(' ').unwrap_or(raw_value);

            match field {
                "event" => {
                    if value.is_empty() {
                        event_type = None;
                        unknown_event = false;
                    } else if let Some(parsed) = SseEventType::from_str(value) {
                        event_type = Some(parsed);
                        unknown_event = false;
                    } else {
                        event_type = None;
                        unknown_event = true;
                    }
                }
                "data" => {
                    // Check accumulated data size to prevent memory exhaustion.
                    let separator_size = usize::from(!data_lines.is_empty());
                    total_data_size = total_data_size
                        .saturating_add(separator_size)
                        .saturating_add(value.len());
                    if total_data_size > MAX_SSE_MESSAGE_SIZE {
                        self.terminal = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "SSE event data exceeds maximum size of {} bytes",
                                MAX_SSE_MESSAGE_SIZE
                            ),
                        )));
                    }
                    data_lines.push(value.to_string());
                }
                "id" => {
                    // U+0000 makes this field a no-op; it does not clear the
                    // last valid ID and is not a terminal stream error.
                    if !value.contains('\0') {
                        self.last_event_id = Some(value.to_string());
                    }
                }
                "retry" => {
                    if !value.is_empty()
                        && value.bytes().all(|byte| byte.is_ascii_digit())
                        && let Ok(retry) = value.parse()
                    {
                        self.reconnection_time = Some(retry);
                    }
                }
                _ => {
                    // Unknown field, ignore per SSE spec.
                }
            }
        }
    }

    /// Returns the persistent last event ID established by valid `id` fields.
    #[must_use]
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    /// Returns the persistent reconnection delay established by valid `retry` fields.
    #[must_use]
    pub fn retry_interval(&self) -> Option<u64> {
        self.reconnection_time
    }

    /// Reads the next message event and parses it as a JSON-RPC message.
    ///
    /// Skips non-message events (like endpoint events).
    ///
    /// # Returns
    ///
    /// - `Ok(Some(message))`: A message was successfully read
    /// - `Ok(None)`: EOF reached
    /// - `Err(_)`: An error occurred
    pub fn read_message(&mut self, cx: &Cx) -> Result<Option<JsonRpcMessage>, TransportError> {
        loop {
            match self.read_event(cx)? {
                Some(event) => {
                    if event.event_type == SseEventType::Message {
                        let message = self.codec.decode_complete_message(event.data.as_bytes())?;
                        return Ok(Some(message));
                    }
                    // Skip non-message events
                    continue;
                }
                None => return Ok(None),
            }
        }
    }

    /// Reads the endpoint event and returns the POST URL.
    ///
    /// This should be called once when the SSE connection is established.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(url))`: The endpoint URL
    /// - `Ok(None)`: EOF reached before endpoint event
    /// - `Err(_)`: An error occurred
    pub fn read_endpoint(&mut self, cx: &Cx) -> Result<Option<String>, TransportError> {
        loop {
            match self.read_event(cx)? {
                Some(event) => {
                    if event.event_type == SseEventType::Endpoint {
                        return Ok(Some(event.data));
                    }
                    // Skip non-endpoint events (shouldn't happen at start)
                    continue;
                }
                None => return Ok(None),
            }
        }
    }

    /// Returns a reference to the underlying reader.
    pub fn inner(&self) -> &BufReader<R> {
        &self.reader
    }
}

// =============================================================================
// Exact MCP 2024-11-05 SSE transport
// =============================================================================

/// One client-to-server HTTP POST selected by an exact-2024 SSE endpoint event.
///
/// The endpoint is opaque to this transport: the caller's HTTP adapter owns
/// connection establishment and any origin, credential, redirect, or TLS
/// policy. The adapter receives the exact advertised URI and already-framed
/// JSON-RPC bytes, so it cannot silently substitute a derived modern route.
#[cfg(feature = "legacy-2024-11-05")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacySseMessagePost {
    endpoint: String,
    body: Vec<u8>,
}

#[cfg(feature = "legacy-2024-11-05")]
impl LegacySseMessagePost {
    fn new(endpoint: String, body: Vec<u8>) -> Self {
        Self { endpoint, body }
    }

    /// Returns the exact URI advertised by the first SSE endpoint event.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the newline-delimited JSON-RPC body for the advertised POST.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Caller-owned HTTP POST adapter for exact MCP 2024-11-05 SSE clients.
///
/// A legacy SSE connection has two directions: server messages remain on the
/// SSE reader, while every client JSON-RPC message is delivered to this sink
/// using the one URI advertised by the first endpoint event.
#[cfg(feature = "legacy-2024-11-05")]
pub trait LegacySsePostSink {
    /// Delivers one JSON-RPC message to the advertised endpoint.
    fn post(&mut self, cx: &Cx, post: LegacySseMessagePost) -> Result<(), TransportError>;
}

/// Exact MCP 2024-11-05 client-side SSE adapter.
///
/// This adapter is deliberately separate from [`SseClientTransport`]. It
/// requires the first valid event to be `endpoint`, latches the advertised
/// POST URI once, and rejects any send before that handshake. Modern
/// streamable-HTTP routing never constructs this type.
#[cfg(feature = "legacy-2024-11-05")]
pub struct LegacySseClientTransport<R, P> {
    reader: SseReader<R>,
    post_sink: P,
    codec: Codec,
    advertised_endpoint: Option<String>,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<R: Read, P: LegacySsePostSink> LegacySseClientTransport<R, P> {
    /// Creates a legacy SSE adapter over an already-opened SSE GET body.
    #[must_use]
    pub fn new(reader: R, post_sink: P) -> Self {
        Self {
            reader: SseReader::new(reader),
            post_sink,
            codec: Codec::new(),
            advertised_endpoint: None,
            closed: false,
        }
    }

    /// Consumes and latches the required first `endpoint` event.
    ///
    /// A `message` event, end of stream, or an empty endpoint is a terminal
    /// legacy-adapter denial. The reader intentionally does not search ahead
    /// for a later endpoint because that would allow endpoint replacement.
    pub fn establish(&mut self, cx: &Cx) -> Result<&str, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        if self.advertised_endpoint.is_some() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "legacy SSE endpoint has already been established",
            )));
        }

        let event = match self.reader.read_event(cx) {
            Ok(Some(event)) => event,
            Ok(None) => {
                self.closed = true;
                return Err(TransportError::Closed);
            }
            Err(error @ (TransportError::Cancelled | TransportError::Timeout)) => {
                self.closed = self.reader.terminal;
                return Err(error);
            }
            Err(error) => {
                self.closed = true;
                return Err(error);
            }
        };
        if event.event_type != SseEventType::Endpoint || event.data.is_empty() {
            self.closed = true;
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "legacy SSE stream must begin with one nonempty endpoint event",
            )));
        }
        self.advertised_endpoint = Some(event.data);
        if let Some(endpoint) = self.advertised_endpoint.as_deref() {
            Ok(endpoint)
        } else {
            self.closed = true;
            Err(TransportError::Closed)
        }
    }

    /// Returns the endpoint fixed by [`Self::establish`].
    #[must_use]
    pub fn advertised_endpoint(&self) -> Option<&str> {
        self.advertised_endpoint.as_deref()
    }

    fn encode_client_message(&self, message: &JsonRpcMessage) -> Result<Vec<u8>, TransportError> {
        match message {
            JsonRpcMessage::Request(request) => self
                .codec
                .encode_request(request)
                .map_err(TransportError::Codec),
            JsonRpcMessage::Response(response) => self
                .codec
                .encode_response(response)
                .map_err(TransportError::Codec),
        }
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl<R: Read, P: LegacySsePostSink> Transport for LegacySseClientTransport<R, P> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;
        let endpoint = self.advertised_endpoint.clone().ok_or_else(|| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "legacy SSE endpoint event must be established before POSTing messages",
            ))
        })?;
        let post = LegacySseMessagePost::new(endpoint, self.encode_client_message(message)?);
        // Encoding may itself spend the remaining budget. Do not invoke the
        // caller-owned HTTP adapter after that budget is exhausted.
        sse_checkpoint(cx)?;
        match self.post_sink.post(cx, post) {
            Ok(()) => Ok(()),
            Err(TransportError::Cancelled) => Err(TransportError::Cancelled),
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        match self.reader.read_message(cx) {
            Ok(Some(message)) => Ok(message),
            Ok(None) => {
                self.closed = true;
                Err(TransportError::Closed)
            }
            Err(error @ TransportError::Codec(_)) => Err(error),
            Err(error @ (TransportError::Cancelled | TransportError::Timeout)) => {
                if self.reader.terminal {
                    self.closed = true;
                }
                Err(error)
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        Ok(())
    }
}

/// Exact MCP 2024-11-05 server-side SSE adapter.
///
/// The HTTP GET handler calls [`Self::open`] immediately after establishing
/// its SSE response body, guaranteeing that the endpoint event precedes every
/// server-to-client JSON-RPC message. Existing [`SseServerTransport`] callers
/// retain their current lazy endpoint behavior.
#[cfg(feature = "legacy-2024-11-05")]
pub struct LegacySseServerTransport<W, R> {
    inner: SseServerTransport<W, R>,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write, R: Iterator<Item = JsonRpcRequest>> LegacySseServerTransport<W, R> {
    /// Creates the exact-2024 server adapter with one advertised POST URI.
    #[must_use]
    pub fn new(writer: W, request_source: R, endpoint_url: impl Into<String>) -> Self {
        Self {
            inner: SseServerTransport::new(writer, request_source, endpoint_url),
        }
    }

    /// Opens the SSE body by sending its mandatory first endpoint event.
    pub fn open(&mut self, cx: &Cx) -> Result<(), TransportError> {
        if self.inner.closed || self.inner.writer.closed {
            self.inner.closed = true;
            return Err(TransportError::Closed);
        }
        self.inner.ensure_endpoint_sent(cx)
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write, R: Iterator<Item = JsonRpcRequest>> Transport for LegacySseServerTransport<W, R> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        self.open(cx)?;
        self.inner.send(cx, message)
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.open(cx)?;
        self.inner.recv(cx)
    }

    fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        self.inner.close(cx)
    }
}

// =============================================================================
// SSE Transport (Server-Side)
// =============================================================================

/// Server-side SSE transport.
///
/// This transport is designed for the server side of an MCP SSE connection:
/// - Receives requests from an HTTP POST handler (via `inject_request`)
/// - Sends responses via the SSE event stream
///
/// # Architecture
///
/// The SSE transport is split because of the protocol's nature:
/// - The SSE stream (this transport's writer) is one-way to the client
/// - Client requests come in via separate HTTP POST requests
///
/// A typical integration looks like:
///
/// ```ignore
/// // HTTP handler for SSE connection
/// fn sse_handler(req: Request) -> Response {
///     let (tx, rx) = channel();
///     let transport = SseServerTransport::new(response_writer, rx);
///
///     // Run the MCP server with this transport
///     server.run(transport);
/// }
///
/// // HTTP handler for POST requests
/// fn post_handler(req: Request) {
///     let message: JsonRpcRequest = parse_body(&req);
///     tx.send(message).unwrap();
/// }
/// ```
///
/// # Note
///
/// This is a basic implementation. For production use, you'll need to integrate
/// with an HTTP server and handle the POST endpoint separately.
#[cfg(feature = "legacy-2024-11-05")]
pub struct SseServerTransport<W, R> {
    writer: SseWriter<W>,
    request_codec: Codec,
    /// Channel or queue for receiving requests from POST handler.
    request_source: R,
    endpoint_sent: bool,
    endpoint_url: String,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write, R: Iterator<Item = JsonRpcRequest>> SseServerTransport<W, R> {
    /// Creates a new SSE server transport.
    ///
    /// # Arguments
    ///
    /// * `writer` - The SSE stream writer (response body)
    /// * `request_source` - Source of requests from POST handler
    /// * `endpoint_url` - The URL to advertise for client POST requests
    #[must_use]
    pub fn new(writer: W, request_source: R, endpoint_url: impl Into<String>) -> Self {
        Self {
            writer: SseWriter::new(writer),
            request_codec: Codec::new(),
            request_source,
            endpoint_sent: false,
            endpoint_url: endpoint_url.into(),
            closed: false,
        }
    }

    /// Sends the endpoint event if not already sent.
    fn ensure_endpoint_sent(&mut self, cx: &Cx) -> Result<(), TransportError> {
        if !self.endpoint_sent {
            self.writer.write_endpoint(cx, &self.endpoint_url)?;
            self.endpoint_sent = true;
        }
        Ok(())
    }

    /// Separates typed POST ingress from SSE response egress.
    ///
    /// The two returned halves own disjoint I/O resources, so a server can
    /// continue receiving requests while a request-owned child commits its
    /// response to the event stream.
    #[must_use]
    pub fn into_split(self) -> (SseServerRecvHalf<R>, SseServerSendHalf<W>) {
        (
            SseServerRecvHalf {
                request_codec: self.request_codec,
                request_source: self.request_source,
                closed: self.closed,
            },
            SseServerSendHalf {
                writer: self.writer,
                endpoint_sent: self.endpoint_sent,
                endpoint_url: self.endpoint_url,
                closed: self.closed,
            },
        )
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write, R: Iterator<Item = JsonRpcRequest>> Transport for SseServerTransport<W, R> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed || self.writer.closed {
            self.closed = true;
            return Err(TransportError::Closed);
        }
        self.ensure_endpoint_sent(cx)?;
        self.writer.write_message(cx, message)
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        if self.closed || self.writer.closed {
            self.closed = true;
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        // Get next request from the request source (POST handler)
        match self.request_source.next() {
            Some(request) => {
                // Iterator-backed typed ingress otherwise bypasses all wire
                // validation and per-message size admission.
                self.request_codec.encode_request(&request)?;
                Ok(JsonRpcMessage::Request(request))
            }
            None => {
                self.closed = true;
                Err(TransportError::Closed)
            }
        }
    }

    fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        // SSE connections don't have a close frame; flush once and let the
        // connection drop. SseWriter makes this idempotent and terminal.
        let result = self.writer.close(cx);
        self.closed |= self.writer.closed;
        result
    }
}

/// Independently owned typed POST ingress for an SSE server transport.
#[cfg(feature = "legacy-2024-11-05")]
pub struct SseServerRecvHalf<R> {
    request_codec: Codec,
    request_source: R,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<R: Iterator<Item = JsonRpcRequest>> TransportRecvHalf for SseServerRecvHalf<R> {
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        match self.request_source.next() {
            Some(request) => {
                self.request_codec.encode_request(&request)?;
                Ok(JsonRpcMessage::Request(request))
            }
            None => {
                self.closed = true;
                Err(TransportError::Closed)
            }
        }
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        Ok(())
    }
}

/// Independently owned event-stream egress for an SSE server transport.
#[cfg(feature = "legacy-2024-11-05")]
pub struct SseServerSendHalf<W> {
    writer: SseWriter<W>,
    endpoint_sent: bool,
    endpoint_url: String,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write> SseServerSendHalf<W> {
    fn ensure_endpoint_sent(&mut self, cx: &Cx) -> Result<(), TransportError> {
        if !self.endpoint_sent {
            self.writer.write_endpoint(cx, &self.endpoint_url)?;
            self.endpoint_sent = true;
        }
        Ok(())
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl<W: Write + Send> TransportSendHalf for SseServerSendHalf<W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed || self.writer.closed {
            self.closed = true;
            return Err(TransportError::Closed);
        }
        self.ensure_endpoint_sent(cx)?;
        self.writer.write_message(cx, message)
    }

    fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        let result = self.writer.close(cx);
        self.closed |= self.writer.closed;
        result
    }
}

// =============================================================================
// SSE Client Transport
// =============================================================================

/// Client-side SSE transport.
///
/// This transport is designed for the client side of an MCP SSE connection:
/// - Receives responses via SSE event stream
/// - Sends requests via HTTP POST (using provided sender)
///
/// # Architecture
///
/// ```ignore
/// // Connect to SSE endpoint
/// let sse_stream = http_client.get(sse_url).send()?;
/// let (tx, rx) = channel();
///
/// // Create transport
/// let transport = SseClientTransport::new(sse_stream, tx);
///
/// // Read endpoint URL
/// let post_url = transport.read_endpoint(&cx)?;
///
/// // Use transport for MCP client
/// client.run(transport);
/// ```
#[cfg(feature = "legacy-2024-11-05")]
pub struct SseClientTransport<R, W> {
    reader: SseReader<R>,
    /// Sender for POST requests (injected into HTTP client)
    request_sink: W,
    codec: Codec,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl<R: Read, W: Write> SseClientTransport<R, W> {
    /// Creates a new SSE client transport.
    ///
    /// # Arguments
    ///
    /// * `reader` - The SSE event stream reader
    /// * `request_sink` - Sink for outgoing requests (typically an HTTP POST body)
    #[must_use]
    pub fn new(reader: R, request_sink: W) -> Self {
        Self {
            reader: SseReader::new(reader),
            request_sink,
            codec: Codec::new(),
            closed: false,
        }
    }

    fn commit_request(&mut self, cx: &Cx, bytes: &[u8]) -> Result<(), TransportError> {
        sse_commit_frame(cx, &mut self.request_sink, &mut self.closed, bytes)
    }

    /// Reads the endpoint URL from the SSE stream.
    ///
    /// This should be called once when the connection is established.
    pub fn read_endpoint(&mut self, cx: &Cx) -> Result<Option<String>, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        match self.reader.read_endpoint(cx) {
            Ok(endpoint) => {
                if endpoint.is_none() {
                    self.closed = true;
                }
                Ok(endpoint)
            }
            Err(error @ (TransportError::Cancelled | TransportError::Timeout)) => {
                if self.reader.terminal {
                    self.closed = true;
                }
                Err(error)
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl<R: Read, W: Write> Transport for SseClientTransport<R, W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        sse_checkpoint(cx)?;

        // Send via POST (write to request sink)
        let bytes = match message {
            JsonRpcMessage::Request(req) => self.codec.encode_request(req)?,
            JsonRpcMessage::Response(resp) => self.codec.encode_response(resp)?,
        };

        self.commit_request(cx, &bytes)
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        match self.reader.read_message(cx) {
            Ok(Some(message)) => Ok(message),
            Ok(None) => {
                self.closed = true;
                Err(TransportError::Closed)
            }
            // A complete message event with invalid JSON-RPC has already
            // consumed a safe event boundary and does not corrupt the stream.
            Err(error @ TransportError::Codec(_)) => Err(error),
            Err(error @ (TransportError::Cancelled | TransportError::Timeout)) => {
                if self.reader.terminal {
                    self.closed = true;
                }
                Err(error)
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }

    fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        // Budget observed after the terminal branch and before the write-side
        // commit, per the conversion template: an already-closed transport does
        // no I/O and must stay Ok under cancellation, while a live one refuses
        // before the blocking flush and stays retryable.
        sse_checkpoint(cx)?;
        self.closed = true;
        self.request_sink.flush().map_err(TransportError::Io)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(all(test, feature = "legacy-2024-11-05"))]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedWriteBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedWriteBuffer {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("shared SSE output lock").clone()
        }
    }

    impl Write for SharedWriteBuffer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("shared SSE output lock poisoned"))?
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ByteAtATime {
        inner: Cursor<Vec<u8>>,
    }

    impl Read for ByteAtATime {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let limit = buffer.len().min(1);
            self.inner.read(&mut buffer[..limit])
        }
    }

    #[derive(Default)]
    struct PartialWriteThenFail {
        bytes: Vec<u8>,
        wrote_partial_prefix: bool,
    }

    impl Write for PartialWriteThenFail {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.wrote_partial_prefix {
                return Err(std::io::Error::other("write failed after partial SSE data"));
            }
            let written = buffer.len().min(8);
            self.bytes.extend_from_slice(&buffer[..written]);
            self.wrote_partial_prefix = true;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_sse_event_endpoint() {
        let event = SseEvent::endpoint("http://localhost:8080/messages");
        let bytes = event.to_bytes().unwrap();
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.contains("event: endpoint\n"));
        assert!(output.contains("data: http://localhost:8080/messages\n"));
        assert!(output.ends_with("\n\n")); // Blank line terminator
    }

    #[test]
    fn sse_server_split_halves_preserve_endpoint_and_response() {
        let output = SharedWriteBuffer::default();
        let request = JsonRpcRequest::new("tools/list", None, 41_i64);
        let (mut recv_half, mut send_half) =
            SseServerTransport::new(output.clone(), [request.clone()].into_iter(), "/mcp")
                .into_split();
        let cx = Cx::for_testing();

        let received = recv_half.recv(&cx).expect("split ingress request");
        let JsonRpcMessage::Request(received) = received else {
            panic!("split ingress must preserve the request variant");
        };
        assert_eq!(received.method, request.method);
        assert_eq!(received.id, request.id);
        send_half
            .send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    41.into(),
                    serde_json::json!({"tools": []}),
                )),
            )
            .expect("split egress response");

        let bytes = String::from_utf8(output.bytes()).expect("SSE output is UTF-8");
        assert!(bytes.starts_with("event: endpoint\ndata: /mcp\n\n"));
        assert!(bytes.contains("event: message\n"));
        assert!(bytes.contains("\"id\":41"));
    }

    #[test]
    fn sse_server_split_cancelled_ingress_leaves_egress_unchanged() {
        let output = SharedWriteBuffer::default();
        let request = JsonRpcRequest::new("tools/list", None, 42_i64);
        let (mut recv_half, _send_half) =
            SseServerTransport::new(output.clone(), [request].into_iter(), "/mcp").into_split();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        assert!(matches!(
            recv_half.recv(&cx),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(output.bytes().len(), 0);
    }

    #[test]
    fn test_sse_event_message() {
        let event = SseEvent::message(r#"{"jsonrpc":"2.0","id":1}"#).with_id("42");
        let bytes = event.to_bytes().unwrap();
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.contains("event: message\n"));
        assert!(output.contains("id: 42\n"));
        assert!(output.contains(r#"data: {"jsonrpc":"2.0","id":1}"#));
    }

    #[test]
    fn test_sse_event_with_retry() {
        let event = SseEvent::message("test").with_retry(5000);
        let bytes = event.to_bytes().unwrap();
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.contains("retry: 5000\n"));
    }

    #[test]
    fn test_sse_event_multiline_data() {
        let event = SseEvent::message("line1\nline2\nline3");
        let bytes = event.to_bytes().unwrap();
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.contains("data: line1\n"));
        assert!(output.contains("data: line2\n"));
        assert!(output.contains("data: line3\n"));
    }

    #[test]
    fn public_sse_event_round_trip_preserves_a_terminal_data_newline() {
        let event = SseEvent::message("line\n");
        let bytes = event.to_bytes().expect("terminal data field serializes");

        assert_eq!(
            std::str::from_utf8(&bytes).expect("SSE bytes remain UTF-8"),
            "event: message\ndata: line\ndata: \n\n"
        );
        let mut reader = SseReader::new(Cursor::new(bytes));
        let decoded = reader
            .read_event(&Cx::for_testing())
            .expect("public SSE reader accepts its public writer output")
            .expect("one data event is dispatched");
        assert_eq!(decoded.data, "line\n");
    }

    #[test]
    fn public_sse_event_without_terminal_data_newline_does_not_invent_one() {
        // Planted negative: only the final empty logical data field is absent.
        let event = SseEvent::message("line");
        let bytes = event.to_bytes().expect("single data field serializes");

        assert_eq!(
            std::str::from_utf8(&bytes).expect("SSE bytes remain UTF-8"),
            "event: message\ndata: line\n\n"
        );
        let mut reader = SseReader::new(Cursor::new(bytes));
        let decoded = reader
            .read_event(&Cx::for_testing())
            .expect("public SSE reader accepts its public writer output")
            .expect("one data event is dispatched");
        assert_eq!(decoded.data, "line");
    }

    #[test]
    fn test_sse_reader_simple_event() {
        let input = b"event: message\ndata: hello\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let event = sse_reader.read_event(&cx).unwrap().unwrap();

        assert_eq!(event.event_type, SseEventType::Message);
        assert_eq!(event.data, "hello");
    }

    #[test]
    fn event_without_data_is_not_dispatched_and_does_not_leak_its_type() {
        let input = b"event: endpoint\n\ndata: payload\n\n";
        let mut reader = SseReader::new(Cursor::new(input.to_vec()));

        let event = reader.read_event(&Cx::for_testing()).unwrap().unwrap();

        assert_eq!(event.event_type, SseEventType::Message);
        assert_eq!(event.data, "payload");
    }

    #[test]
    fn colonless_data_field_dispatches_an_empty_message_event() {
        let input = b"data\n\n";
        let mut reader = SseReader::new(Cursor::new(input.to_vec()));

        let event = reader.read_event(&Cx::for_testing()).unwrap().unwrap();

        assert_eq!(event.event_type, SseEventType::Message);
        assert_eq!(event.data.len(), 0);
    }

    #[test]
    fn reader_accepts_cr_crlf_and_lf_line_endings_even_when_split_across_reads() {
        let input = b"id: 7\rretry: 1500\r\ndata: first\rdata: second\n\n";
        let source = ByteAtATime {
            inner: Cursor::new(input.to_vec()),
        };
        let mut reader = SseReader::new(source);

        let event = reader.read_event(&Cx::for_testing()).unwrap().unwrap();

        assert_eq!(event.event_type, SseEventType::Message);
        assert_eq!(event.data, "first\nsecond");
        assert_eq!(event.id.as_deref(), Some("7"));
        assert_eq!(event.retry, Some(1500));
        assert_eq!(reader.last_event_id(), Some("7"));
        assert_eq!(reader.retry_interval(), Some(1500));
    }

    #[test]
    fn id_and_retry_state_persist_while_nul_and_invalid_retry_fields_are_ignored() {
        let input = b"id: stable\nretry: 2500\n\ndata: first\n\n\
id: ignored\0value\nretry: -1\ndata: second\n\n\
id\ndata: third\n\n";
        let mut reader = SseReader::new(Cursor::new(input.to_vec()));
        let cx = Cx::for_testing();

        let first = reader.read_event(&cx).unwrap().unwrap();
        assert_eq!(first.data, "first");
        assert_eq!(first.id.as_deref(), Some("stable"));
        assert_eq!(first.retry, Some(2500));

        let second = reader.read_event(&cx).unwrap().unwrap();
        assert_eq!(second.data, "second");
        assert_eq!(second.id.as_deref(), Some("stable"));
        assert_eq!(second.retry, Some(2500));

        let third = reader.read_event(&cx).unwrap().unwrap();
        assert_eq!(third.data, "third");
        assert_eq!(third.id.as_deref(), Some(""));
        assert_eq!(third.retry, Some(2500));
        assert_eq!(reader.last_event_id(), Some(""));
        assert_eq!(reader.retry_interval(), Some(2500));
    }

    #[test]
    fn test_sse_reader_accepts_utf8_split_across_underlying_reads() {
        let input = "event: message\ndata: méthod\n\n";
        let reader = ByteAtATime {
            inner: Cursor::new(input.as_bytes().to_vec()),
        };
        let mut sse_reader = SseReader::new(reader);

        let event = sse_reader.read_event(&Cx::for_testing()).unwrap().unwrap();

        assert_eq!(event.event_type, SseEventType::Message);
        assert_eq!(event.data, "méthod");
    }

    #[test]
    fn test_sse_reader_with_id() {
        let input = b"event: message\nid: 42\ndata: test\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let event = sse_reader.read_event(&cx).unwrap().unwrap();

        assert_eq!(event.id, Some("42".to_string()));
    }

    #[test]
    fn test_sse_reader_multiline_data() {
        let input = b"event: message\ndata: line1\ndata: line2\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let event = sse_reader.read_event(&cx).unwrap().unwrap();

        assert_eq!(event.data, "line1\nline2");
    }

    #[test]
    fn test_sse_reader_skips_comments() {
        let input = b": this is a comment\nevent: message\ndata: hello\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let event = sse_reader.read_event(&cx).unwrap().unwrap();

        assert_eq!(event.data, "hello");
    }

    #[test]
    fn test_sse_reader_skips_unknown_events() {
        let input = b"event: ping\ndata: keep-alive\n\n\
event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":1}\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let message = sse_reader.read_message(&cx).unwrap().unwrap();

        assert!(
            matches!(message, JsonRpcMessage::Request(_)),
            "Expected request"
        );
        if let JsonRpcMessage::Request(req) = message {
            assert_eq!(req.method, "ping");
        }
    }

    #[test]
    fn test_sse_reader_rejects_escaped_duplicate_object_member() {
        let input = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"first\",\"m\\u0065thod\":\"second\",\"id\":1}\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let error = sse_reader.read_message(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Codec(CodecError::InvalidMessage {
                kind: crate::InvalidMessageKind::Request,
                ..
            })
        ));
        assert!(error.to_string().contains("duplicate JSON object member"));
    }

    #[test]
    fn test_sse_reader_eof() {
        let input = b"";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let result = sse_reader.read_event(&cx).unwrap();

        assert!(result.is_none());
        assert!(matches!(
            sse_reader.read_event(&cx),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn sse_reader_latches_documented_terminal_line_errors() {
        let input = b"event: message\ndata: too-long\n\n";
        let mut reader = SseReader::new(Cursor::new(input.to_vec()));
        reader.max_line_size = 8;
        let cx = Cx::for_testing();

        assert!(matches!(reader.read_event(&cx), Err(TransportError::Io(_))));
        assert!(reader.terminal);
        assert!(matches!(
            reader.read_event(&cx),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn complete_invalid_message_event_does_not_poison_sse_reader() {
        let input = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":1,\"id\":1}\n\n\
event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"valid\",\"id\":2}\n\n";
        let mut reader = SseReader::new(Cursor::new(input.to_vec()));
        let cx = Cx::for_testing();

        assert!(matches!(
            reader.read_message(&cx),
            Err(TransportError::Codec(_))
        ));
        assert!(!reader.terminal);

        let JsonRpcMessage::Request(request) = reader.read_message(&cx).unwrap().unwrap() else {
            panic!("expected request");
        };
        assert_eq!(request.method, "valid");
    }

    #[test]
    fn test_sse_reader_endpoint_event() {
        let input = b"event: endpoint\ndata: http://localhost/post\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        let url = sse_reader.read_endpoint(&cx).unwrap().unwrap();

        assert_eq!(url, "http://localhost/post");
    }

    #[test]
    fn test_sse_writer_endpoint() {
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);

        let cx = Cx::for_testing();
        writer
            .write_endpoint(&cx, "http://localhost:8080/messages")
            .unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains("event: endpoint\n"));
        assert!(output.contains("data: http://localhost:8080/messages\n"));
    }

    #[test]
    fn test_sse_writer_keep_alive() {
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);

        let cx = Cx::for_testing();
        writer.keep_alive(&cx).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(": keep-alive\n"));
    }

    #[test]
    fn sse_reader_and_writer_share_effective_message_ceiling() {
        let reader = SseReader::new(Cursor::new(Vec::<u8>::new()));
        let writer = SseWriter::new(Vec::<u8>::new());

        assert_eq!(reader.codec.max_message_size(), MAX_SSE_MESSAGE_SIZE);
        assert_eq!(writer.codec.max_message_size(), MAX_SSE_MESSAGE_SIZE);
        assert_eq!(
            MAX_SSE_MESSAGE_SIZE + SSE_DATA_LINE_WIRE_OVERHEAD,
            MAX_SSE_LINE_SIZE
        );
    }

    #[test]
    fn sse_writer_rejects_oversized_event_before_writing() {
        let event = SseEvent::message("x".repeat(MAX_SSE_MESSAGE_SIZE + 1));
        let mut writer = SseWriter::new(Vec::new());

        let error = writer.write_event(&Cx::for_testing(), &event).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Codec(CodecError::MessageTooLarge(size))
                if size == MAX_SSE_MESSAGE_SIZE + 1
        ));
        assert_eq!(writer.inner().len(), 0);
    }

    #[test]
    fn sse_writer_bounds_and_validates_typed_messages_before_writing() {
        let cx = Cx::for_testing();
        let mut writer = SseWriter::new(Vec::new());
        let oversized = JsonRpcMessage::Response(JsonRpcResponse::success(
            fastmcp_protocol::RequestId::Number(1),
            serde_json::json!({"payload": "x".repeat(MAX_SSE_MESSAGE_SIZE)}),
        ));
        assert!(matches!(
            writer.write_message(&cx, &oversized),
            Err(TransportError::Codec(CodecError::MessageTooLarge(_)))
        ));
        assert_eq!(writer.inner().len(), 0);
        assert_eq!(writer.event_counter, 0);

        let invalid = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        });
        assert!(matches!(
            writer.write_message(&cx, &invalid),
            Err(TransportError::Codec(CodecError::Json(_)))
        ));
        assert_eq!(writer.inner().len(), 0);
        assert_eq!(writer.event_counter, 0);
    }

    #[test]
    fn partial_sse_write_latches_writer_closed_but_validation_errors_do_not() {
        let cx = Cx::for_testing();
        let mut reusable = SseWriter::new(Vec::new());
        assert!(SseEvent::message("bad\rfield").to_bytes().is_err());
        assert!(
            reusable
                .write_event(&cx, &SseEvent::message("bad\rfield"))
                .is_err()
        );
        assert!(!reusable.closed);
        reusable
            .write_event(&cx, &SseEvent::message("valid"))
            .unwrap();

        let mut writer = SseWriter::new(PartialWriteThenFail::default());
        let error = writer
            .write_event(&cx, &SseEvent::message("will be partial"))
            .unwrap_err();

        assert!(matches!(error, TransportError::Io(_)));
        assert!(writer.closed);
        assert_ne!(writer.inner().bytes.len(), 0);
        assert!(matches!(
            writer.write_event(&cx, &SseEvent::message("retry")),
            Err(TransportError::Closed)
        ));
        assert!(writer.close(&cx).is_ok());
        assert!(writer.close(&cx).is_ok());
    }

    #[test]
    fn sse_event_rejects_nul_and_bare_carriage_return_but_accepts_multiline_data() {
        for data in ["safe\0data", "safe\revent: endpoint"] {
            let error = SseEvent::message(data).to_bytes().unwrap_err();
            assert!(matches!(
                error,
                TransportError::Io(ref source)
                    if source.kind() == std::io::ErrorKind::InvalidInput
            ));
        }

        for data in ["first\nsecond", "first\r\nsecond"] {
            let output = String::from_utf8(SseEvent::message(data).to_bytes().unwrap()).unwrap();
            assert!(output.contains("data: first\ndata: second\n"));
            assert!(!output.contains('\r'));
        }
    }

    #[test]
    fn sse_writer_rejects_event_id_injection_before_writing() {
        for id in ["safe\nid: injected", "safe\revent: endpoint", "safe\0id"] {
            let event = SseEvent::message("data").with_id(id);
            assert!(event.to_bytes().is_err());
            let mut writer = SseWriter::new(Vec::new());

            let error = writer.write_event(&Cx::for_testing(), &event).unwrap_err();

            assert!(matches!(
                error,
                TransportError::Io(ref source)
                    if source.kind() == std::io::ErrorKind::InvalidInput
            ));
            assert_eq!(writer.inner().len(), 0);
        }
    }

    #[test]
    fn sse_writer_rejects_comment_and_endpoint_injection_before_writing() {
        for comment in [
            "safe\ndata: injected",
            "safe\revent: message",
            "safe\0comment",
        ] {
            let mut writer = SseWriter::new(Vec::new());
            let error = writer
                .write_comment(&Cx::for_testing(), comment)
                .unwrap_err();
            assert!(matches!(
                error,
                TransportError::Io(ref source)
                    if source.kind() == std::io::ErrorKind::InvalidInput
            ));
            assert_eq!(writer.inner().len(), 0);
        }

        let mut writer = SseWriter::new(Vec::new());
        let error = writer
            .write_endpoint(&Cx::for_testing(), "https://example.invalid\nretry: 0")
            .unwrap_err();
        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert_eq!(writer.inner().len(), 0);
    }

    #[test]
    fn test_sse_roundtrip() {
        // Write an event
        let write_buffer = Vec::new();
        let mut writer = SseWriter::new(write_buffer);

        let cx = Cx::for_testing();
        let message = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({"status": "ok"})),
            error: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        });

        writer.write_message(&cx, &message).unwrap();
        let written = writer.into_inner();

        // Read it back
        let mut reader = SseReader::new(Cursor::new(written));
        let read_message = reader.read_message(&cx).unwrap().unwrap();

        assert!(
            matches!(read_message, JsonRpcMessage::Response(_)),
            "Expected response"
        );
        if let JsonRpcMessage::Response(resp) = read_message {
            assert_eq!(resp.result, Some(serde_json::json!({"status": "ok"})));
        }
    }

    #[test]
    fn test_sse_reader_cancellation() {
        let input = b"event: message\ndata: hello\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);

        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result = sse_reader.read_event(&cx);
        assert!(matches!(result, Err(TransportError::Cancelled)));
    }

    #[test]
    fn test_sse_writer_cancellation() {
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);

        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result = writer.write_endpoint(&cx, "http://test");
        assert!(matches!(result, Err(TransportError::Cancelled)));
    }

    // =========================================================================
    // E2E SSE Streaming Tests (bd-2kv / bd-1pua)
    // =========================================================================

    #[test]
    fn e2e_sse_connection_establishment() {
        // Test the full connection establishment flow
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);
        let cx = Cx::for_testing();

        // Server sends endpoint event first
        writer
            .write_endpoint(&cx, "http://localhost:8080/mcp/messages")
            .unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();

        // Verify proper SSE format
        assert!(output.starts_with("event: endpoint\n"));
        assert!(output.contains("data: http://localhost:8080/mcp/messages\n"));
        assert!(output.contains("\n\n")); // Event terminator
    }

    #[test]
    fn e2e_sse_event_stream_sequence() {
        // Test multiple events in sequence (simulating a session)
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);
        let cx = Cx::for_testing();

        // 1. Send endpoint
        writer.write_endpoint(&cx, "http://localhost/post").unwrap();

        // 2. Send a few responses
        for i in 1..=3 {
            let response = JsonRpcResponse {
                jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
                result: Some(serde_json::json!({"count": i})),
                error: None,
                id: Some(fastmcp_protocol::RequestId::Number(i)),
            };
            writer.write_response(&cx, &response).unwrap();
        }

        // 3. Send keep-alive
        writer.keep_alive(&cx).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();

        // Verify all events are present
        assert!(output.contains("event: endpoint\n"));
        assert!(output.contains("event: message\n"));
        assert!(output.contains("id: 1\n")); // First message has id 1
        assert!(output.contains("id: 2\n"));
        assert!(output.contains("id: 3\n"));
        assert!(output.contains(": keep-alive\n"));
    }

    #[test]
    fn e2e_sse_resumability_with_last_event_id() {
        // Test reading events and tracking Last-Event-ID for resumption
        let input = b"\
event: message\n\
id: 100\n\
data: {\"jsonrpc\":\"2.0\",\"result\":{\"n\":1},\"id\":1}\n\
\n\
event: message\n\
id: 101\n\
data: {\"jsonrpc\":\"2.0\",\"result\":{\"n\":2},\"id\":2}\n\
\n\
event: message\n\
id: 102\n\
data: {\"jsonrpc\":\"2.0\",\"result\":{\"n\":3},\"id\":3}\n\
\n";

        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);
        let cx = Cx::for_testing();

        // Read all events and track IDs
        let mut event_ids = Vec::new();
        while let Some(event) = sse_reader.read_event(&cx).unwrap() {
            if let Some(id) = event.id {
                event_ids.push(id);
            }
        }

        assert_eq!(event_ids, vec!["100", "101", "102"]);

        // Last event ID for resumption would be "102"
        let last_event_id = event_ids.last().unwrap();
        assert_eq!(last_event_id, "102");
    }

    #[test]
    fn e2e_sse_graceful_disconnect_on_eof() {
        // Test that EOF is handled gracefully
        let input = b"\
event: message\n\
data: {\"jsonrpc\":\"2.0\",\"method\":\"test\"}\n\
\n";

        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);
        let cx = Cx::for_testing();

        // First read should succeed
        let event = sse_reader.read_event(&cx).unwrap();
        assert!(event.is_some());

        // Second read should return None (EOF), not error
        let event = sse_reader.read_event(&cx).unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn e2e_sse_server_transport_flow() {
        // Test the server-side transport with injected requests
        let requests = vec![
            JsonRpcRequest::new("initialize", None, 1i64),
            JsonRpcRequest::new("tools/list", None, 2i64),
        ];

        let buffer = Vec::new();
        let mut transport =
            SseServerTransport::new(buffer, requests.into_iter(), "http://localhost/post");
        let cx = Cx::for_testing();

        // Receive requests from POST handler
        let msg1 = transport.recv(&cx).unwrap();
        assert!(matches!(msg1, JsonRpcMessage::Request(_)));
        if let JsonRpcMessage::Request(req) = msg1 {
            assert_eq!(req.method, "initialize");
        }

        // Send a response (triggers endpoint event first)
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({"capabilities": {}})),
            error: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        };
        transport
            .send(&cx, &JsonRpcMessage::Response(response))
            .unwrap();

        // Receive second request
        let msg2 = transport.recv(&cx).unwrap();
        if let JsonRpcMessage::Request(req) = msg2 {
            assert_eq!(req.method, "tools/list");
        }

        // EOF after requests are exhausted
        let result = transport.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
        assert!(matches!(transport.recv(&cx), Err(TransportError::Closed)));
    }

    #[test]
    fn e2e_sse_client_transport_flow() {
        // Test the client-side transport
        let sse_input = b"\
event: endpoint\n\
data: http://localhost/post\n\
\n\
event: message\n\
data: {\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[]},\"id\":1}\n\
\n";

        let reader = Cursor::new(sse_input.to_vec());
        let mut request_buffer = Vec::new();

        {
            let mut transport = SseClientTransport::new(reader, &mut request_buffer);
            let cx = Cx::for_testing();

            // Read endpoint URL
            let endpoint = transport.read_endpoint(&cx).unwrap().unwrap();
            assert_eq!(endpoint, "http://localhost/post");

            // Send a request (goes to request_buffer)
            let request = JsonRpcRequest::new("tools/list", None, 1i64);
            transport
                .send(&cx, &JsonRpcMessage::Request(request))
                .unwrap();

            // Receive response from SSE stream
            let msg = transport.recv(&cx).unwrap();
            assert!(matches!(msg, JsonRpcMessage::Response(_)));
        }

        // Verify request was sent correctly (NDJSON format)
        let sent = String::from_utf8(request_buffer).unwrap();
        assert!(sent.contains("\"method\":\"tools/list\""));
    }

    #[test]
    fn e2e_sse_event_with_retry() {
        // Test retry field for reconnection hints
        let input = b"\
event: message\n\
id: 1\n\
retry: 5000\n\
data: test\n\
\n";

        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);
        let cx = Cx::for_testing();

        let event = sse_reader.read_event(&cx).unwrap().unwrap();
        assert_eq!(event.retry, Some(5000));
    }

    #[test]
    fn e2e_sse_multiple_data_lines() {
        // Test handling of multi-line JSON (split across data lines)
        // This can happen with pretty-printed JSON
        let input = b"\
event: message\n\
data: {\n\
data:   \"jsonrpc\": \"2.0\",\n\
data:   \"result\": {\"key\": \"value\"},\n\
data:   \"id\": 1\n\
data: }\n\
\n";

        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);
        let cx = Cx::for_testing();

        let event = sse_reader.read_event(&cx).unwrap().unwrap();

        // Data lines are joined with newlines
        assert!(event.data.contains("\"jsonrpc\""));
        assert!(event.data.contains("\"result\""));

        // Parse the JSON (should work when lines are joined)
        let parsed: serde_json::Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(parsed.get("id"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn e2e_sse_unicode_in_events() {
        // Test Unicode handling in SSE events
        let input = "event: message\ndata: {\"text\":\"Hello 世界 👋\"}\n\n";

        let reader = Cursor::new(input.as_bytes().to_vec());
        let mut sse_reader = SseReader::new(reader);
        let cx = Cx::for_testing();

        let event = sse_reader.read_event(&cx).unwrap().unwrap();
        assert!(event.data.contains("世界"));
        assert!(event.data.contains("👋"));
    }

    // =========================================================================
    // Additional coverage tests (bd-137i)
    // =========================================================================

    #[test]
    fn sse_event_type_as_str_round_trip() {
        for ty in [SseEventType::Endpoint, SseEventType::Message] {
            let s = ty.as_str();
            let parsed = SseEventType::from_str(s).unwrap();
            assert_eq!(parsed, ty);
        }
    }

    #[test]
    fn sse_event_type_from_str_unknown_returns_none() {
        assert!(SseEventType::from_str("ping").is_none());
        assert!(SseEventType::from_str("").is_none());
        assert!(SseEventType::from_str("MESSAGE").is_none());
    }

    #[test]
    fn sse_event_empty_data_serialization() {
        // Empty data should still produce a "data: " line
        let event = SseEvent::message("");
        let bytes = event.to_bytes().unwrap();
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.contains("data: \n"));
        assert!(output.contains("event: message\n"));
        assert!(output.ends_with("\n\n"));
    }

    #[test]
    fn sse_writer_event_counter_auto_increments() {
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);
        let cx = Cx::for_testing();

        // Write three messages — IDs should be 1, 2, 3
        for _ in 0..3 {
            let msg = JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
                result: Some(serde_json::json!(null)),
                error: None,
                id: Some(fastmcp_protocol::RequestId::Number(1)),
            });
            writer.write_message(&cx, &msg).unwrap();
        }

        let output = String::from_utf8(writer.into_inner()).unwrap();
        // Split into individual events and verify sequential ids
        let events: Vec<&str> = output.split("\n\n").filter(|s| !s.is_empty()).collect();
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("id: 1\n"));
        assert!(events[1].contains("id: 2\n"));
        assert!(events[2].contains("id: 3\n"));
    }

    #[test]
    fn sse_writer_event_counter_exhaustion_fails_before_writing() {
        let mut writer = SseWriter::new(Vec::new());
        writer.event_counter = u64::MAX;
        let message = JsonRpcMessage::Response(JsonRpcResponse::success(
            fastmcp_protocol::RequestId::Number(1),
            serde_json::Value::Null,
        ));

        let error = writer
            .write_message(&Cx::for_testing(), &message)
            .unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert_eq!(writer.inner().len(), 0);
        assert_eq!(writer.event_counter, u64::MAX);
    }

    #[test]
    fn sse_writer_inner_and_inner_mut_accessors() {
        let buffer: Vec<u8> = Vec::new();
        let mut writer = SseWriter::new(buffer);

        // inner() returns a reference
        assert_eq!(writer.inner().len(), 0);

        // inner_mut() allows mutation
        writer.inner_mut().extend_from_slice(b"raw");
        assert_eq!(writer.inner().len(), 3);
    }

    #[test]
    fn sse_writer_write_comment_custom_text() {
        let buffer = Vec::new();
        let mut writer = SseWriter::new(buffer);
        let cx = Cx::for_testing();

        writer.write_comment(&cx, "hello world").unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert_eq!(output, ": hello world\n");
    }

    #[test]
    fn sse_reader_read_endpoint_skips_message_events() {
        // read_endpoint should skip message events and return the endpoint
        let input = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n\
event: endpoint\ndata: http://localhost/post\n\n";
        let reader = Cursor::new(input.to_vec());
        let mut sse_reader = SseReader::new(reader);
        let cx = Cx::for_testing();

        let url = sse_reader.read_endpoint(&cx).unwrap().unwrap();
        assert_eq!(url, "http://localhost/post");
    }

    #[test]
    fn sse_server_transport_close_flushes() {
        let requests: Vec<JsonRpcRequest> = vec![];
        let buffer = Vec::new();
        let mut transport =
            SseServerTransport::new(buffer, requests.into_iter(), "http://localhost/post");

        // close() should succeed (flushes the underlying writer)
        let cx = Cx::for_testing();
        transport.close(&cx).unwrap();
        transport.close(&cx).unwrap();
    }

    #[test]
    fn partial_sse_client_request_write_latches_transport_closed() {
        let incoming = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":null,\"id\":1}\n\n";
        let mut transport = SseClientTransport::new(
            Cursor::new(incoming.to_vec()),
            PartialWriteThenFail::default(),
        );
        let cx = Cx::for_testing();
        let request = JsonRpcRequest::new("tools/list", None, 1_i64);

        assert!(matches!(
            transport.send(&cx, &JsonRpcMessage::Request(request)),
            Err(TransportError::Io(_))
        ));
        assert!(transport.closed);
        assert_ne!(transport.request_sink.bytes.len(), 0);
        assert!(matches!(transport.recv(&cx), Err(TransportError::Closed)));
        assert!(transport.close(&cx).is_ok());
        assert!(transport.close(&cx).is_ok());
    }

    #[test]
    fn terminal_sse_reader_error_latches_client_transport_closed() {
        let input = b"event: message\ndata: too-long\n\n";
        let mut transport = SseClientTransport::new(Cursor::new(input.to_vec()), Vec::<u8>::new());
        transport.reader.max_line_size = 8;
        let cx = Cx::for_testing();

        assert!(matches!(transport.recv(&cx), Err(TransportError::Io(_))));
        assert!(transport.closed);
        assert!(matches!(transport.recv(&cx), Err(TransportError::Closed)));
    }

    #[test]
    fn sse_client_prewrite_codec_failure_is_recoverable() {
        let mut transport =
            SseClientTransport::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new());
        let cx = Cx::for_testing();
        let invalid = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        };

        assert!(matches!(
            transport.send(&cx, &JsonRpcMessage::Response(invalid)),
            Err(TransportError::Codec(CodecError::Json(_)))
        ));
        assert!(!transport.closed);

        transport
            .send(
                &cx,
                &JsonRpcMessage::Request(JsonRpcRequest::new("still-open", None, 2_i64)),
            )
            .unwrap();
        assert_ne!(transport.request_sink.len(), 0);
    }

    #[test]
    fn sse_client_transport_send_cancelled() {
        let sse_input = b"";
        let reader = Cursor::new(sse_input.to_vec());
        let mut request_buffer = Vec::new();

        let mut transport = SseClientTransport::new(reader, &mut request_buffer);
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = transport.send(&cx, &JsonRpcMessage::Request(request));
        assert!(matches!(result, Err(TransportError::Cancelled)));
    }
}
