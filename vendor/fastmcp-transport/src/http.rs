//! HTTP transport for FastMCP.
//!
//! This module provides HTTP-based transport for MCP servers, enabling
//! web-based deployments without relying on stdio or WebSockets.
//!
//! # Modes
//!
//! The HTTP transport supports two modes:
//!
//! - **Stateless**: Each HTTP request contains a single JSON-RPC message and receives
//!   a single response. No session state is maintained between requests.
//!
//! - **Streamable**: Long-lived connections using HTTP streaming (chunked transfer)
//!   for bidirectional communication. Supports Server-Sent Events (SSE) for
//!   server-to-client notifications.
//!
//! # Integration
//!
//! This transport is designed to integrate with any HTTP server framework.
//! It provides:
//!
//! - [`HttpRequestHandler`]: Processes incoming HTTP requests containing JSON-RPC messages
//! - [`HttpTransport`]: Full transport implementation for HTTP connections
//! - [`StreamableHttpTransport`]: Streaming transport for long-lived connections
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_transport::http::{HttpRequestHandler, HttpRequest, HttpResponse};
//!
//! let handler = HttpRequestHandler::new();
//!
//! // In your HTTP server's request handler:
//! fn handle_mcp_request(http_req: YourHttpRequest) -> YourHttpResponse {
//!     let request = HttpRequest {
//!         method: http_req.method(),
//!         path: http_req.path(),
//!         headers: http_req.headers(),
//!         body: http_req.body(),
//!     };
//!
//!     let mcp_response = handler.handle(&cx, request)?;
//!
//!     YourHttpResponse::new()
//!         .status(mcp_response.status)
//!         .header("Content-Type", &mcp_response.content_type)
//!         .body(mcp_response.body)
//! }
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{Read, Write};
#[cfg(feature = "legacy-2024-11-05")]
use std::net::TcpStream as StdTcpStream;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
// Not #[cfg(test)]: `GuardedHttpResolver` is a shipped public trait whose
// method returns `Pin<Box<dyn Future<..>>>`, so `Pin` must be in scope in
// ordinary builds too. It was test-gated while that trait was test-only.
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, TryLockError,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::Poll;
use std::time::{Duration, Instant};

use asupersync::{
    Cx,
    channel::mpsc,
    http::h1::{Http1Client as NativeHttp1Client, Request as NativeHttpRequest},
    net::{
        TcpStream as NativeTcpStream,
        dns::{Resolver as NativeDnsResolver, ResolverConfig},
    },
    time,
    tls::TlsConnector,
    types::{CancelKind, CancelReason},
};
use fastmcp_core::{McpRequestCancellation, draw_security_identifier, sha256_bounded};
#[cfg(feature = "legacy-2024-11-05")]
use fastmcp_protocol::methods::{
    Legacy2024Direction, Legacy2024Envelope, decode_legacy_2024_11_05_envelope_classified,
};
use fastmcp_protocol::protocol_version::{
    FinalHttpRequestMetadata, RequestAdmissionError, RequestVersionMetadata,
    admit_final_http_request,
};
use fastmcp_protocol::{
    JsonRpcAdmissionError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
    JsonRpcResponseAdmission, RequestId, decode_strict_jsonrpc_response,
};

#[cfg(feature = "legacy-2024-11-05")]
use crate::sse::SseEvent;
use crate::sse::{
    ModernSseDecoder, ModernSseEndOfStream, ModernSseLimits, ModernSseParseError,
    ModernSsePushError,
};
use crate::{Codec, CodecError, Transport, TransportError};

/// Result of consuming one finite modern HTTP SSE response body.
///
/// The collector binds every terminal JSON-RPC response to one outgoing
/// request ID. Server notifications are passed to the supplied callback in
/// wire order before the terminal response is returned at EOF.
#[derive(Debug)]
pub struct ModernHttpSseCollector {
    request_id: RequestId,
    decoder: Option<ModernSseDecoder>,
    codec: Codec,
    terminal: Option<JsonRpcResponseAdmission>,
}

/// Errors while collecting one request-scoped modern HTTP SSE response body.
#[derive(Debug)]
pub enum ModernHttpSseCollectorError {
    /// The request ID cannot be used as a JSON-RPC correlation key.
    InvalidRequestId,
    /// Caller cancellation stopped collection before a terminal response.
    Cancelled,
    /// Bounded SSE framing refused the response body.
    Sse(ModernSseParseError),
    /// A completed SSE payload was not one valid bounded JSON-RPC message.
    Codec(CodecError),
    /// A response could not retain the exact source JSON of its `result`
    /// member after ordinary JSON-RPC admission classified it as a response.
    ResponseAdmission(JsonRpcAdmissionError),
    /// The transport context expired while consuming the response body.
    Transport(TransportError),
    /// Notification delivery returned an application transport error.
    NotificationDelivery(TransportError),
    /// A server-to-client request carried an ID and is therefore not a notification.
    NonNotificationRequest {
        /// The request ID unexpectedly carried by the server message.
        request_id: RequestId,
    },
    /// A terminal response belongs to a request other than this SSE body.
    TerminalResponseIdMismatch {
        /// The outgoing request ID bound to this body.
        expected: RequestId,
        /// The response ID observed on the body, including absent IDs.
        actual: Option<RequestId>,
    },
    /// A second terminal response arrived after the body's first terminal response.
    DuplicateTerminalResponse {
        /// The request ID bound to this body.
        request_id: RequestId,
    },
    /// A notification arrived after this finite response had terminated.
    NotificationAfterTerminal {
        /// The request ID bound to this body.
        request_id: RequestId,
    },
    /// The HTTP body ended before a correlated terminal response.
    EndOfStream {
        /// Exact incomplete SSE framing discarded at EOF.
        framing: ModernSseEndOfStream,
    },
    /// The response body was already finished or refused.
    Closed,
}

impl std::fmt::Display for ModernHttpSseCollectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequestId => {
                formatter.write_str("modern HTTP SSE collector has invalid request ID")
            }
            Self::Cancelled => formatter.write_str("modern HTTP SSE collection was cancelled"),
            Self::Sse(error) => error.fmt(formatter),
            Self::Codec(error) => error.fmt(formatter),
            Self::ResponseAdmission(error) => {
                write!(
                    formatter,
                    "modern HTTP SSE response admission failed: {error}"
                )
            }
            Self::Transport(error) => error.fmt(formatter),
            Self::NotificationDelivery(error) => error.fmt(formatter),
            Self::NonNotificationRequest { request_id } => write!(
                formatter,
                "modern HTTP SSE server request {request_id:?} is not a notification"
            ),
            Self::TerminalResponseIdMismatch { expected, actual } => write!(
                formatter,
                "modern HTTP SSE response ID {actual:?} does not match request {expected:?}"
            ),
            Self::DuplicateTerminalResponse { request_id } => write!(
                formatter,
                "modern HTTP SSE body for request {request_id:?} emitted a duplicate terminal response"
            ),
            Self::NotificationAfterTerminal { request_id } => write!(
                formatter,
                "modern HTTP SSE body for request {request_id:?} emitted a notification after its terminal response"
            ),
            Self::EndOfStream { .. } => formatter
                .write_str("modern HTTP SSE body ended before its correlated terminal response"),
            Self::Closed => formatter.write_str("modern HTTP SSE collector is closed"),
        }
    }
}

impl std::error::Error for ModernHttpSseCollectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sse(error) => Some(error),
            Self::Codec(error) => Some(error),
            Self::ResponseAdmission(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::NotificationDelivery(error) => Some(error),
            Self::InvalidRequestId
            | Self::Cancelled
            | Self::NonNotificationRequest { .. }
            | Self::TerminalResponseIdMismatch { .. }
            | Self::DuplicateTerminalResponse { .. }
            | Self::NotificationAfterTerminal { .. }
            | Self::EndOfStream { .. }
            | Self::Closed => None,
        }
    }
}

impl ModernHttpSseCollector {
    /// Creates a bounded collector for exactly one outgoing request.
    ///
    /// The decoder uses WHATWG SSE framing in replacement mode and the
    /// existing strict transport codec for each completed JSON-RPC payload.
    pub fn new(
        request_id: RequestId,
        limits: ModernSseLimits,
    ) -> Result<Self, ModernHttpSseCollectorError> {
        if request_id.validate().is_err() {
            return Err(ModernHttpSseCollectorError::InvalidRequestId);
        }
        let mut codec = Codec::new();
        codec.set_max_message_size(limits.max_event_bytes());
        Ok(Self {
            request_id,
            decoder: Some(ModernSseDecoder::new(limits)),
            codec,
            terminal: None,
        })
    }

    /// Incrementally consumes an HTTP response-body chunk.
    ///
    /// Each server notification is delivered immediately in wire order. A
    /// response is retained only if its ID mathematically correlates with this
    /// collector's request ID. Continue feeding chunks through EOF so duplicate
    /// terminals cannot be hidden after an otherwise valid first response.
    pub fn push(
        &mut self,
        cx: &Cx,
        chunk: &[u8],
        mut deliver_notification: impl FnMut(JsonRpcRequest) -> Result<(), TransportError>,
    ) -> Result<(), ModernHttpSseCollectorError> {
        if let Err(error) = Self::checkpoint(cx) {
            return Err(self.refuse(error));
        }

        let request_id = self.request_id.clone();
        let result = {
            let decoder = self
                .decoder
                .as_mut()
                .ok_or(ModernHttpSseCollectorError::Closed)?;
            let codec = &mut self.codec;
            let terminal = &mut self.terminal;
            decoder.push_with(chunk, |event| {
                Self::checkpoint(cx)?;
                let message = codec
                    .decode_complete_message(event.as_bytes())
                    .map_err(ModernHttpSseCollectorError::Codec)?;
                match message {
                    JsonRpcMessage::Request(notification) => {
                        if terminal.is_some() {
                            return Err(ModernHttpSseCollectorError::NotificationAfterTerminal {
                                request_id: request_id.clone(),
                            });
                        }
                        if let Some(request_id) = notification.id.clone() {
                            return Err(ModernHttpSseCollectorError::NonNotificationRequest {
                                request_id,
                            });
                        }
                        deliver_notification(notification)
                            .map_err(ModernHttpSseCollectorError::NotificationDelivery)
                    }
                    JsonRpcMessage::Response(response) => {
                        if terminal.is_some() {
                            return Err(ModernHttpSseCollectorError::DuplicateTerminalResponse {
                                request_id: request_id.clone(),
                            });
                        }
                        if !response
                            .id
                            .as_ref()
                            .is_some_and(|actual| actual.correlates_with(&request_id))
                        {
                            return Err(ModernHttpSseCollectorError::TerminalResponseIdMismatch {
                                expected: request_id.clone(),
                                actual: response.id,
                            });
                        }
                        // `JsonRpcResponse` holds a deserialized `Value`, so it
                        // cannot retain numeric spelling or member ordering in
                        // `result`. Re-admit this response and retain its exact
                        // result source for final-result consumers.
                        let admission = decode_strict_jsonrpc_response(
                            event.as_bytes(),
                            codec.max_message_size(),
                        )
                        .map_err(ModernHttpSseCollectorError::ResponseAdmission)?;
                        debug_assert_eq!(admission.response(), &response);
                        *terminal = Some(admission);
                        Ok(())
                    }
                }
            })
        };
        match result {
            Ok(()) => {}
            Err(ModernSsePushError::Parse(error)) => {
                return Err(self.refuse(ModernHttpSseCollectorError::Sse(error)));
            }
            Err(ModernSsePushError::Consumer(error)) => return Err(self.refuse(error)),
        }

        if let Err(error) = Self::checkpoint(cx) {
            return Err(self.refuse(error));
        }
        Ok(())
    }

    /// Ends the finite HTTP body and returns its one correlated terminal response.
    ///
    /// EOF without a terminal response is an error even if the SSE framing was
    /// otherwise clean. An unfinished final SSE event is reported in the EOF
    /// error instead of being synthesized into a JSON-RPC message.
    pub fn finish(&mut self, cx: &Cx) -> Result<JsonRpcResponse, ModernHttpSseCollectorError> {
        self.finish_admission(cx)
            .map(|admission| admission.into_parts().0)
    }

    /// Ends the finite HTTP body and retains the exact JSON source of the
    /// terminal response's `result` member.
    ///
    /// Consumers that perform a final result-algebra decode must use
    /// [`JsonRpcResponseAdmission::raw_result`] rather than serializing the
    /// typed `Value` again. [`Self::finish`] remains the compatibility method
    /// for consumers needing only the ordinary typed response.
    pub fn finish_admission(
        &mut self,
        cx: &Cx,
    ) -> Result<JsonRpcResponseAdmission, ModernHttpSseCollectorError> {
        if let Err(error) = Self::checkpoint(cx) {
            return Err(self.refuse(error));
        }
        let decoder = self
            .decoder
            .take()
            .ok_or(ModernHttpSseCollectorError::Closed)?;
        let framing = decoder.finish().map_err(ModernHttpSseCollectorError::Sse)?;
        if framing.discarded_pending_event || framing.discarded_partial_line {
            self.terminal = None;
            return Err(ModernHttpSseCollectorError::EndOfStream { framing });
        }
        self.terminal
            .take()
            .ok_or(ModernHttpSseCollectorError::EndOfStream { framing })
    }

    fn checkpoint(cx: &Cx) -> Result<(), ModernHttpSseCollectorError> {
        match http_checkpoint(cx) {
            Ok(()) => Ok(()),
            Err(TransportError::Cancelled) => Err(ModernHttpSseCollectorError::Cancelled),
            Err(error) => Err(ModernHttpSseCollectorError::Transport(error)),
        }
    }

    fn refuse(&mut self, error: ModernHttpSseCollectorError) -> ModernHttpSseCollectorError {
        self.decoder = None;
        self.terminal = None;
        error
    }
}

// =============================================================================
// HTTP Request/Response Types
// =============================================================================

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Options,
    Head,
    Patch,
}

impl HttpMethod {
    /// Parses an HTTP method from a string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "OPTIONS" => Some(Self::Options),
            "HEAD" => Some(Self::Head),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }

    /// Returns the method as a string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
            Self::Head => "HEAD",
            Self::Patch => "PATCH",
        }
    }
}

/// HTTP status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpStatus(pub u16);

impl HttpStatus {
    pub const OK: Self = Self(200);
    pub const ACCEPTED: Self = Self(202);
    pub const BAD_REQUEST: Self = Self(400);
    pub const UNAUTHORIZED: Self = Self(401);
    pub const FORBIDDEN: Self = Self(403);
    pub const NOT_FOUND: Self = Self(404);
    pub const METHOD_NOT_ALLOWED: Self = Self(405);
    pub const NOT_ACCEPTABLE: Self = Self(406);
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);
    pub const SERVICE_UNAVAILABLE: Self = Self(503);

    /// Returns true if this is a success status (2xx).
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.0)
    }

    /// Returns true if this is a client error (4xx).
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        (400..500).contains(&self.0)
    }

    /// Returns true if this is a server error (5xx).
    #[must_use]
    pub fn is_server_error(&self) -> bool {
        (500..600).contains(&self.0)
    }
}

/// Incoming HTTP request.
#[derive(Clone)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: HttpMethod,
    /// Request path (e.g., "/mcp/v1").
    pub path: String,
    /// Request headers.
    pub headers: HashMap<String, String>,
    /// Request body.
    pub body: Vec<u8>,
    /// Query parameters.
    pub query: HashMap<String, String>,
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("path_bytes", &self.path.len())
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .field("query_parameter_count", &self.query.len())
            .finish()
    }
}

impl HttpRequest {
    /// Creates a new HTTP request.
    #[must_use]
    pub fn new(method: HttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: HashMap::new(),
            body: Vec::new(),
            query: HashMap::new(),
        }
    }

    /// Adds a header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.into().to_lowercase(), value.into());
        self
    }

    /// Sets the body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Adds a query parameter.
    #[must_use]
    pub fn with_query(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.insert(name.into(), value.into());
        self
    }

    /// Gets a header value (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(header_name, value)| {
            header_name
                .eq_ignore_ascii_case(name)
                .then_some(value.as_str())
        })
    }

    /// Gets the Content-Type header.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Gets the Authorization header.
    #[must_use]
    pub fn authorization(&self) -> Option<&str> {
        self.header("authorization")
    }

    /// Parses the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

/// Concrete HTTP POST sink for the exact MCP 2024-11-05 SSE endpoint event.
///
/// This narrow adapter opens one plain-HTTP connection to a numeric socket
/// authority advertised by the legacy SSE stream and writes one JSON-RPC
/// message POST. It never derives a `/sse` or `/messages` route and never adds
/// modern session headers. DNS is deliberately outside this synchronous,
/// caller-deadline-aware sink; hostname authorities fail closed before contact.
/// TLS, credential, redirect, and origin policy are intentionally owned by the
/// corresponding security and adapter layers rather than this transport slice.
#[cfg(feature = "legacy-2024-11-05")]
#[derive(Debug, Default)]
pub struct LegacySseHttpPostSink;

#[cfg(feature = "legacy-2024-11-05")]
const LEGACY_SSE_HTTP_POST_RESPONSE_HEAD_BYTES: usize = 8 * 1024;
#[cfg(feature = "legacy-2024-11-05")]
const LEGACY_SSE_HTTP_POST_OPERATION_BOUND: Duration = Duration::from_secs(5);
#[cfg(feature = "legacy-2024-11-05")]
const LEGACY_SSE_HTTP_POST_IO_POLL_BOUND: Duration = Duration::from_millis(10);
#[cfg(feature = "legacy-2024-11-05")]
const LEGACY_SSE_HTTP_POST_RETRY_BACKOFF: Duration = Duration::from_millis(1);

#[cfg(feature = "legacy-2024-11-05")]
impl LegacySseHttpPostSink {
    /// Creates a concrete legacy SSE message-POST sink.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl crate::sse::LegacySsePostSink for LegacySseHttpPostSink {
    fn post(
        &mut self,
        cx: &Cx,
        post: crate::sse::LegacySseMessagePost,
    ) -> Result<(), TransportError> {
        let deadline = legacy_sse_http_post_deadline(cx);
        legacy_sse_http_post_checkpoint(cx, deadline)?;
        let (authority, address, target) = legacy_sse_http_post_target(post.endpoint())?;
        let mut stream = legacy_sse_http_post_connect(cx, deadline, address)?;
        let request = format!(
            "POST {target} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            post.body().len(),
        );
        legacy_sse_http_post_write_all(cx, deadline, &mut stream, request.as_bytes())?;
        legacy_sse_http_post_write_all(cx, deadline, &mut stream, post.body())?;
        legacy_sse_http_post_flush(cx, deadline, &mut stream)?;
        legacy_sse_http_post_checkpoint(cx, deadline)?;
        legacy_sse_http_post_requires_accepted(cx, deadline, &mut stream)
    }
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_deadline(cx: &Cx) -> asupersync::Time {
    let bounded = cx.now() + LEGACY_SSE_HTTP_POST_OPERATION_BOUND;
    cx.budget()
        .deadline
        .map_or(bounded, |deadline| deadline.min(bounded))
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_checkpoint(
    cx: &Cx,
    deadline: asupersync::Time,
) -> Result<(), TransportError> {
    http_checkpoint(cx)?;
    if cx.now() >= deadline {
        return Err(TransportError::Timeout);
    }
    Ok(())
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_poll_timeout(
    cx: &Cx,
    deadline: asupersync::Time,
) -> Result<Duration, TransportError> {
    legacy_sse_http_post_checkpoint(cx, deadline)?;
    let wait = Duration::from_nanos(deadline.duration_since(cx.now()))
        .min(LEGACY_SSE_HTTP_POST_IO_POLL_BOUND);
    if wait.is_zero() {
        return Err(TransportError::Timeout);
    }
    Ok(wait)
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_retry_backoff(
    cx: &Cx,
    deadline: asupersync::Time,
) -> Result<(), TransportError> {
    let backoff =
        legacy_sse_http_post_poll_timeout(cx, deadline)?.min(LEGACY_SSE_HTTP_POST_RETRY_BACKOFF);
    std::thread::sleep(backoff);
    legacy_sse_http_post_checkpoint(cx, deadline)
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_connect(
    cx: &Cx,
    deadline: asupersync::Time,
    address: SocketAddr,
) -> Result<StdTcpStream, TransportError> {
    legacy_sse_http_post_checkpoint(cx, deadline)?;
    loop {
        let timeout = legacy_sse_http_post_poll_timeout(cx, deadline)?;
        match StdTcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => {
                legacy_sse_http_post_checkpoint(cx, deadline)?;
                return Ok(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                ) =>
            {
                legacy_sse_http_post_retry_backoff(cx, deadline)?;
            }
            Err(error) => return Err(TransportError::Io(error)),
        }
    }
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_write_all(
    cx: &Cx,
    deadline: asupersync::Time,
    stream: &mut StdTcpStream,
    bytes: &[u8],
) -> Result<(), TransportError> {
    let mut offset = 0;
    while offset < bytes.len() {
        let timeout = legacy_sse_http_post_poll_timeout(cx, deadline)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(TransportError::Io)?;
        match stream.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "legacy SSE message POST stopped before its request body was written",
                )));
            }
            Ok(written) => {
                offset = offset.saturating_add(written);
                legacy_sse_http_post_checkpoint(cx, deadline)?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                ) =>
            {
                legacy_sse_http_post_retry_backoff(cx, deadline)?;
            }
            Err(error) => {
                legacy_sse_http_post_checkpoint(cx, deadline)?;
                return Err(TransportError::Io(error));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_flush(
    cx: &Cx,
    deadline: asupersync::Time,
    stream: &mut StdTcpStream,
) -> Result<(), TransportError> {
    loop {
        let timeout = legacy_sse_http_post_poll_timeout(cx, deadline)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(TransportError::Io)?;
        match stream.flush() {
            Ok(()) => return legacy_sse_http_post_checkpoint(cx, deadline),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                ) =>
            {
                legacy_sse_http_post_retry_backoff(cx, deadline)?;
            }
            Err(error) => {
                legacy_sse_http_post_checkpoint(cx, deadline)?;
                return Err(TransportError::Io(error));
            }
        }
    }
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_requires_accepted(
    cx: &Cx,
    deadline: asupersync::Time,
    stream: &mut StdTcpStream,
) -> Result<(), TransportError> {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0_u8; 512];

    while head.len() < LEGACY_SSE_HTTP_POST_RESPONSE_HEAD_BYTES {
        let remaining = LEGACY_SSE_HTTP_POST_RESPONSE_HEAD_BYTES - head.len();
        let chunk_len = remaining.min(chunk.len());
        let read = loop {
            let timeout = legacy_sse_http_post_poll_timeout(cx, deadline)?;
            stream
                .set_read_timeout(Some(timeout))
                .map_err(TransportError::Io)?;
            match stream.read(&mut chunk[..chunk_len]) {
                Ok(read) => break read,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    legacy_sse_http_post_retry_backoff(cx, deadline)?;
                }
                Err(error) => {
                    legacy_sse_http_post_checkpoint(cx, deadline)?;
                    return Err(TransportError::Io(error));
                }
            }
        };
        legacy_sse_http_post_checkpoint(cx, deadline)?;
        if read == 0 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "legacy SSE message POST closed before a complete HTTP response head",
            )));
        }
        head.extend_from_slice(&chunk[..read]);

        if let Some(head_end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = std::str::from_utf8(&head[..head_end]).map_err(|_| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "legacy SSE message POST response head is not UTF-8",
                ))
            })?;
            let mut lines = head.split("\r\n");
            let status_line = lines.next().expect("a completed head has a status line");
            let mut status_fields = status_line.splitn(3, ' ');
            let version = status_fields.next();
            let status = status_fields.next();
            let reason = status_fields.next();
            let headers_are_well_formed = lines.all(|line| {
                let Some((name, value)) = line.split_once(':') else {
                    return false;
                };
                let value = value.trim_matches([' ', '\t']);
                is_http_token(name) && (value.is_empty() || is_valid_http_header_value(value))
            });
            if version == Some("HTTP/1.1")
                && status == Some("202")
                && reason.is_some_and(|reason| {
                    !reason.is_empty() && is_valid_http_header_value(reason)
                })
                && headers_are_well_formed
            {
                return Ok(());
            }
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "legacy SSE message POST requires an HTTP/1.1 202 Accepted response",
            )));
        }
    }

    Err(TransportError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "legacy SSE message POST response head exceeds its byte limit",
    )))
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_sse_http_post_target(endpoint: &str) -> Result<(&str, SocketAddr, &str), TransportError> {
    let authority_and_target = endpoint.strip_prefix("http://").ok_or_else(|| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy SSE advertised endpoint must be an absolute HTTP URI",
        ))
    })?;
    if authority_and_target
        .bytes()
        .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
    {
        return Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy SSE advertised endpoint contains an invalid control byte",
        )));
    }
    let (authority, target) = authority_and_target
        .split_once('/')
        .map_or((authority_and_target, "/"), |(authority, _target)| {
            (authority, &authority_and_target[authority.len()..])
        });
    if authority.is_empty() {
        return Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy SSE advertised endpoint omits its authority",
        )));
    }
    let address = authority.parse::<SocketAddr>().map_err(|_| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy SSE advertised endpoint authority must be a numeric SocketAddr",
        ))
    })?;
    Ok((authority, address, target))
}

/// Outgoing HTTP response.
///
/// Diagnostics expose only status and sizes because headers and bodies may
/// contain OAuth codes, bearer credentials, or cookies.
#[derive(Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: HttpStatus,
    /// Response headers.
    pub headers: HashMap<String, String>,
    /// Response body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

const JSON_ENCODING_ERROR_BODY: &[u8] =
    br#"{"error":{"code":-32603,"message":"Failed to encode JSON response"}}"#;

impl HttpResponse {
    /// Creates a new HTTP response with the given status.
    #[must_use]
    pub fn new(status: HttpStatus) -> Self {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        Self {
            status,
            headers,
            body: Vec::new(),
        }
    }

    /// Creates a 200 OK response.
    #[must_use]
    pub fn ok() -> Self {
        Self::new(HttpStatus::OK)
    }

    /// Creates a 400 Bad Request response.
    #[must_use]
    pub fn bad_request() -> Self {
        Self::new(HttpStatus::BAD_REQUEST)
    }

    /// Creates a 500 Internal Server Error response.
    #[must_use]
    pub fn internal_error() -> Self {
        Self::new(HttpStatus::INTERNAL_SERVER_ERROR)
    }

    /// Adds a header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.into().to_lowercase(), value.into());
        self
    }

    /// Sets the body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Sets the body as JSON.
    ///
    /// Serialization failures are converted into a deterministic 500 response
    /// with a nonempty JSON error body. Use [`Self::try_with_json`] when the
    /// caller needs the typed serialization error instead.
    #[must_use]
    pub fn with_json<T: serde::Serialize>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => self.body = body,
            Err(_) => {
                self.status = HttpStatus::INTERNAL_SERVER_ERROR;
                self.body = JSON_ENCODING_ERROR_BODY.to_vec();
            }
        }
        self.headers
            .insert("content-type".to_string(), "application/json".to_string());
        self
    }

    /// Tries to set the body as JSON, preserving serialization failure as a
    /// typed [`HttpError`].
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::JsonError`] when `value` cannot be serialized.
    pub fn try_with_json<T: serde::Serialize>(mut self, value: &T) -> Result<Self, HttpError> {
        self.body = serde_json::to_vec(value)?;
        self.headers
            .insert("content-type".to_string(), "application/json".to_string());
        Ok(self)
    }

    /// Sets CORS headers for cross-origin requests.
    #[must_use]
    pub fn with_cors(mut self, origin: &str) -> Self {
        if !is_valid_http_field_value("origin", origin) {
            return self;
        }
        self.headers.insert(
            "access-control-allow-origin".to_string(),
            origin.to_string(),
        );
        self.headers.insert(
            "access-control-allow-methods".to_string(),
            "POST, OPTIONS".to_string(),
        );
        self.headers.insert(
            "access-control-allow-headers".to_string(),
            "Content-Type, Authorization".to_string(),
        );
        self.headers.insert(
            "vary".to_string(),
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_string(),
        );
        self
    }
}

// =============================================================================
// HTTP Error
// =============================================================================

/// HTTP transport error.
#[derive(Debug)]
pub enum HttpError {
    /// Invalid HTTP method.
    InvalidMethod(String),
    /// Invalid HTTP request line.
    InvalidRequestLine(String),
    /// Invalid HTTP header syntax or framing.
    InvalidHeader(String),
    /// Invalid Content-Type.
    InvalidContentType(String),
    /// The request accepts neither modern JSON nor request-scoped SSE.
    NotAcceptable,
    /// The final MCP header/body admission boundary rejected the request.
    ProtocolAdmission(RequestAdmissionError),
    /// Request path does not match the configured MCP endpoint.
    InvalidPath(String),
    /// Request Origin is not admitted by the configured policy.
    OriginNotAllowed(String),
    /// HTTP headers exceeded the maximum allowed size.
    HeadersTooLarge { size: usize, max: usize },
    /// HTTP body exceeded the maximum allowed size.
    BodyTooLarge { size: usize, max: usize },
    /// Unsupported Transfer-Encoding.
    UnsupportedTransferEncoding(String),
    /// Unsupported request Content-Encoding.
    UnsupportedContentEncoding(String),
    /// JSON encoding or decoding error.
    JsonError(serde_json::Error),
    /// Codec error.
    CodecError(CodecError),
    /// Request timeout.
    Timeout,
    /// Connection closed.
    Closed,
    /// Transport error.
    Transport(TransportError),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMethod(_) => write!(f, "invalid HTTP method"),
            Self::InvalidRequestLine(_) => write!(f, "invalid HTTP request line"),
            Self::InvalidHeader(_) => write!(f, "invalid HTTP header"),
            Self::InvalidContentType(_) => write!(f, "invalid content type"),
            Self::NotAcceptable => {
                write!(f, "no supported MCP response representation is acceptable")
            }
            Self::ProtocolAdmission(_) => write!(f, "final MCP request admission rejected"),
            Self::InvalidPath(_) => write!(f, "invalid MCP endpoint path"),
            Self::OriginNotAllowed(_) => write!(f, "origin is not allowed"),
            Self::HeadersTooLarge { size, max } => {
                write!(f, "headers too large: {size} > {max} bytes")
            }
            Self::BodyTooLarge { size, max } => write!(f, "body too large: {size} > {max} bytes"),
            Self::UnsupportedTransferEncoding(_) => write!(f, "unsupported transfer encoding"),
            Self::UnsupportedContentEncoding(_) => write!(f, "unsupported content encoding"),
            Self::JsonError(e) => write!(f, "JSON error: {}", e),
            Self::CodecError(e) => write!(f, "codec error: {}", e),
            Self::Timeout => write!(f, "request timeout"),
            Self::Closed => write!(f, "connection closed"),
            Self::Transport(e) => write!(f, "transport error: {}", e),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<serde_json::Error> for HttpError {
    fn from(err: serde_json::Error) -> Self {
        Self::JsonError(err)
    }
}

impl From<CodecError> for HttpError {
    fn from(err: CodecError) -> Self {
        Self::CodecError(err)
    }
}

impl From<TransportError> for HttpError {
    fn from(err: TransportError) -> Self {
        Self::Transport(err)
    }
}

// =============================================================================
// Guarded outbound HTTPS fetcher
// =============================================================================

/// Maximum URL bytes accepted by [`GuardedHttpsUrl`].
pub const MAX_GUARDED_HTTPS_URL_BYTES: usize = 2 * 1024;
/// Maximum root-policy revision bytes retained in fetch provenance.
pub const MAX_GUARDED_ROOT_POLICY_REVISION_BYTES: usize = 128;
/// The HTTP/1 header ceiling enforced by the native one-shot client.
pub const MAX_GUARDED_RESPONSE_HEADER_BYTES: usize = 64 * 1024;
/// Maximum response body ceiling accepted by this fetcher.
pub const MAX_GUARDED_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Maximum leaf certificate bytes admitted to the core-owned digest primitive.
pub const MAX_GUARDED_LEAF_CERTIFICATE_BYTES: usize = 128 * 1024;
/// Maximum DNS addresses admitted from one complete resolver answer set.
pub const MAX_GUARDED_RESOLVED_ADDRESSES: usize = 64;
/// Longest full URL-resolution through response-body deadline this vertical admits.
pub const MAX_GUARDED_FETCH_DEADLINE: Duration = Duration::from_secs(30);
/// Longest TLS handshake timeout this vertical admits.
pub const MAX_GUARDED_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum body bytes one [`GuardedHttpRequest`] may carry.
pub const MAX_GUARDED_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// Maximum bytes of a [`GuardedHttpRequest`] content-type or authorization value.
pub const MAX_GUARDED_REQUEST_HEADER_VALUE_BYTES: usize = 8 * 1024;
/// Maximum certificates supplied to one [`GuardedRootSet`].
pub const MAX_GUARDED_ROOT_SET_CERTIFICATES: usize = 32;

/// Test-only resolver seam whose complete answer set is checked before any connection.
///
/// The DNS seam every guarded fetch resolves through, production included.
///
/// SHIPPED PUBLIC SEAM (bd-ho7of, 2026-09-17). This was previously
/// `#[cfg(test)]`, which made DNS-hook behavior unprovable through the public
/// surface and left two resolution code paths where there should be one.
/// `GuardedHttpFetcher` now holds an `Arc<dyn GuardedHttpResolver>` and both
/// production and tests travel the same path.
///
/// CUSTODY IS NOT WIDENED BY THIS SEAM, and that is why it can be public.
/// Answers returned here are not trusted: every address is fenced downstream
/// by `guarded_select_address`, which canonicalizes it, rejects an empty set,
/// rejects more than `MAX_GUARDED_RESOLVED_ADDRESSES`, and requires
/// `is_public_guarded_ip`. TLS then verifies the certificate against the
/// ORIGINAL request host, not against whatever address was returned. A
/// supplied resolver can therefore choose only among addresses the fence
/// already admits, and it cannot reach a private or loopback peer, cannot
/// weaken WebPKI root custody, and cannot cause a certificate for another
/// name to be accepted.
///
/// The lower wire seam (`GuardedHttpTestExchange`) and the loopback-authority
/// escape remain `#[cfg(test)]` deliberately: those DO bypass the address
/// fence and real TLS, so publishing them would widen a production bound.
///
/// WHAT THAT GATING COSTS, stated where a reader meets it rather than left to
/// be discovered. A test driving the wire seam takes `provenance` from its
/// fixture; production DERIVES it from the live TLS session in
/// `guarded_peer_provenance`. Such a test therefore asserts admission behaviour
/// GIVEN a provenance, and can never prove that provenance was computed
/// correctly from a peer.
///
/// Derivation is covered by the loopback-authority tests instead — but only
/// PARTIALLY, and the remainder is an open gap rather than a documented
/// boundary. `guarded_leaf_certificate_sha256`'s bounds are proven directly by
/// `rh5_guarded_leaf_provenance_requires_admitted_certificate_and_valid_retry`:
/// absent leaf, oversized leaf, and the unchanged-input proof on rejection. But
/// the only test that derives provenance from a LIVE session is
/// `guarded_loopback_real_wire_200_records_request_body_and_leaf_provenance`,
/// and of the six fields it checks `leaf_certificate_sha256` only for being
/// non-zero — which a constant or the wrong certificate would also satisfy —
/// while `host` and `selected_address` are passed in rather than derived.
/// `alpn` and `tls_protocol` are read from the live session and asserted
/// NOWHERE in the crate. Those three are UNPROVEN, not merely unstated.
pub trait GuardedHttpResolver: Send + Sync {
    /// Resolve all IP answers for the supplied canonical DNS host.
    ///
    /// The future receives the fetch phase's child context and therefore must
    /// observe its cancellation before returning. The fetcher applies its
    /// non-extendable full-fetch deadline to this operation, just like the
    /// subsequent TCP/TLS/HTTP exchange. A resolver must not hide blocking
    /// work behind this future: this transport deliberately owns no threads or
    /// runtime of its own.
    fn resolve_all(
        &self,
        cx: Cx,
        host: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>>;
}

/// The production resolver: the cancel-safe native DNS resolver, reached
/// through the same public seam any caller-supplied resolver uses.
///
/// This is the "thin wrapper" half of the one-code-path requirement. It adds
/// only the two cancellation checkpoints the fetcher has always applied around
/// a lookup; it changes no bound and no policy.
pub struct NativeGuardedResolver {
    inner: Arc<NativeDnsResolver>,
}

impl NativeGuardedResolver {
    /// Wrap a configured native resolver.
    #[must_use]
    pub fn new(inner: Arc<NativeDnsResolver>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for NativeGuardedResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("NativeGuardedResolver").finish()
    }
}

impl GuardedHttpResolver for NativeGuardedResolver {
    fn resolve_all(
        &self,
        cx: Cx,
        host: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>>
    {
        let resolver = Arc::clone(&self.inner);
        Box::pin(async move {
            guarded_fetch_checkpoint(&cx)?;
            let lookup = resolver
                .lookup_ip(&host)
                .await
                .map_err(|error| GuardedHttpFetchError::Resolution(error.to_string()))?;
            guarded_fetch_checkpoint(&cx)?;
            Ok(lookup.into_iter().collect())
        })
    }
}

#[cfg(test)]
struct StaticGuardedResolverForLoopback {
    address: IpAddr,
}

#[cfg(test)]
impl GuardedHttpResolver for StaticGuardedResolverForLoopback {
    fn resolve_all(
        &self,
        cx: Cx,
        _host: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>>
    {
        let address = self.address;
        Box::pin(async move {
            guarded_fetch_checkpoint(&cx)?;
            Ok(vec![address])
        })
    }
}

/// A bounded HTTPS URL accepted by [`GuardedHttpFetcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedHttpsUrl {
    host: String,
    port: u16,
    target: String,
}

impl GuardedHttpsUrl {
    /// Parse an absolute HTTPS URL with a DNS hostname and a bounded target.
    pub fn parse(url: &str) -> Result<Self, GuardedHttpFetchError> {
        if url.is_empty() || url.len() > MAX_GUARDED_HTTPS_URL_BYTES {
            return Err(GuardedHttpFetchError::InvalidUrl("URL length"));
        }
        if url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        {
            return Err(GuardedHttpFetchError::InvalidUrl("URL control byte"));
        }
        let remainder = url
            .strip_prefix("https://")
            .ok_or(GuardedHttpFetchError::InvalidUrl("HTTPS required"))?;
        let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        if authority.is_empty() || authority.contains('@') || authority.starts_with('[') {
            return Err(GuardedHttpFetchError::InvalidUrl("DNS authority required"));
        }
        let tail = &remainder[authority_end..];
        if tail.contains('#') {
            return Err(GuardedHttpFetchError::InvalidUrl("fragment"));
        }
        let (host, port) = parse_guarded_https_authority(authority)?;
        let target = if tail.is_empty() {
            "/".to_owned()
        } else if tail.starts_with('?') {
            format!("/{tail}")
        } else {
            tail.to_owned()
        };
        Ok(Self { host, port, target })
    }

    /// Canonical ASCII DNS hostname used for resolver lookup and TLS SNI.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Explicit or default HTTPS port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Origin-form request target, including query when present.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Finite limits and caller policy identity carried by a guarded fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedHttpFetchPolicy {
    response_body_bytes: usize,
    deadline: Duration,
    tls_handshake_timeout: Duration,
    root_policy_revision: String,
}

impl GuardedHttpFetchPolicy {
    /// Construct a finite policy for a WebPKI-rooted HTTP/1.1 fetcher.
    pub fn new(
        response_body_bytes: usize,
        deadline: Duration,
        tls_handshake_timeout: Duration,
        root_policy_revision: impl Into<String>,
    ) -> Result<Self, GuardedHttpFetchError> {
        let root_policy_revision = root_policy_revision.into();
        if response_body_bytes == 0 || response_body_bytes > MAX_GUARDED_RESPONSE_BODY_BYTES {
            return Err(GuardedHttpFetchError::InvalidPolicy("response body bound"));
        }
        if deadline.is_zero() || tls_handshake_timeout.is_zero() {
            return Err(GuardedHttpFetchError::InvalidPolicy(
                "finite timeout required",
            ));
        }
        if deadline > MAX_GUARDED_FETCH_DEADLINE {
            return Err(GuardedHttpFetchError::InvalidPolicy("full fetch deadline"));
        }
        if tls_handshake_timeout > MAX_GUARDED_TLS_HANDSHAKE_TIMEOUT {
            return Err(GuardedHttpFetchError::InvalidPolicy(
                "TLS handshake timeout",
            ));
        }
        if tls_handshake_timeout > deadline {
            return Err(GuardedHttpFetchError::InvalidPolicy(
                "TLS handshake exceeds full fetch deadline",
            ));
        }
        if root_policy_revision.is_empty()
            || root_policy_revision.len() > MAX_GUARDED_ROOT_POLICY_REVISION_BYTES
            || !root_policy_revision.bytes().all(is_http_token_byte)
        {
            return Err(GuardedHttpFetchError::InvalidPolicy("root policy revision"));
        }
        Ok(Self {
            response_body_bytes,
            deadline,
            tls_handshake_timeout,
            root_policy_revision,
        })
    }

    /// Maximum accepted decoded response bytes.
    #[must_use]
    pub const fn response_body_bytes(&self) -> usize {
        self.response_body_bytes
    }

    /// Non-extendable full-fetch deadline.
    #[must_use]
    pub const fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Finite TLS handshake ceiling configured on the connector.
    #[must_use]
    pub const fn tls_handshake_timeout(&self) -> Duration {
        self.tls_handshake_timeout
    }

    /// Caller policy identity recorded with provenance.
    ///
    /// This label does not select trust roots: the fetcher always uses its
    /// non-injectable WebPKI connector.
    #[must_use]
    pub fn root_policy_revision(&self) -> &str {
        &self.root_policy_revision
    }
}

/// TLS peer and route facts for one completed guarded fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedHttpPeerProvenance {
    /// Canonical hostname supplied to both resolver and TLS SNI validation.
    pub host: String,
    /// Deterministically selected, verified-public socket address.
    pub selected_address: SocketAddr,
    /// SHA-256 digest of the admitted leaf DER certificate.
    pub leaf_certificate_sha256: [u8; 32],
    /// Negotiated ALPN bytes, when present.
    pub alpn: Option<Vec<u8>>,
    /// TLS protocol version debug identity, when present.
    pub tls_protocol: Option<String>,
    /// Caller policy identity; the actual trust roots are fixed WebPKI roots.
    pub root_policy_revision: String,
}

/// Observed redirect response. Redirects are reported, never followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedHttpRedirect {
    /// 3xx status observed on the single request.
    pub status: u16,
    /// Raw `Location` response field, when present.
    pub location: Option<String>,
}

/// A fully bounded one-shot response from [`GuardedHttpFetcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedHttpFetchResponse {
    /// HTTP status returned by the single request.
    pub status: u16,
    /// Response fields in wire order.
    pub headers: Vec<(String, String)>,
    /// Identity-coded response body, bounded by policy.
    pub body: Vec<u8>,
    /// Typed redirect observation; no follow-up request occurs.
    pub redirect: Option<GuardedHttpRedirect>,
    /// Selected route and TLS facts.
    pub provenance: GuardedHttpPeerProvenance,
}

/// Failure from guarded URL admission, resolution, TLS, or one-shot HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardedHttpFetchError {
    /// URL is not a bounded HTTPS URL suitable for DNS/SNI handling.
    InvalidUrl(&'static str),
    /// Fetch policy omitted a finite or bounded requirement.
    InvalidPolicy(&'static str),
    /// Resolver returned no usable answers.
    ResolutionEmpty,
    /// The bounded native resolver could not return a complete address set.
    Resolution(String),
    /// The resolver answer set exceeded the fixed admission bound.
    ResolutionTooMany,
    /// At least one answer was private, special-purpose, or otherwise non-public.
    DisallowedResolvedAddress(IpAddr),
    /// Caller cancellation stopped the owned exchange.
    Cancelled,
    /// The finite fetch deadline elapsed.
    DeadlineExceeded,
    /// TCP connection to the selected address failed.
    Connect(String),
    /// TLS authentication or handshake failed.
    Tls(String),
    /// The authenticated TLS stack did not expose a leaf certificate.
    PeerCertificateUnavailable,
    /// The authenticated TLS leaf certificate exceeded the digest admission bound.
    PeerCertificateTooLarge,
    /// HTTP/1.1 framing or I/O failed.
    Http(String),
    /// The peer used a non-identity content coding.
    UnexpectedContentEncoding(String),
    /// Response fields exceeded the fixed transport header bound.
    ResponseHeadersTooLarge,
    /// A [`GuardedHttpRequest`] value failed validation. This happens at
    /// construction, so no fetcher method, resolver, or socket is reached.
    /// The reason is static text and never echoes the rejected value.
    InvalidRequest(&'static str),
}

impl std::fmt::Display for GuardedHttpFetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(reason) => write!(formatter, "invalid guarded HTTPS URL: {reason}"),
            Self::InvalidPolicy(reason) => {
                write!(formatter, "invalid guarded fetch policy: {reason}")
            }
            Self::ResolutionEmpty => formatter.write_str("guarded resolver returned no addresses"),
            Self::Resolution(error) => write!(formatter, "guarded resolver failed: {error}"),
            Self::ResolutionTooMany => {
                formatter.write_str("guarded resolver returned too many addresses")
            }
            Self::DisallowedResolvedAddress(address) => {
                write!(
                    formatter,
                    "guarded resolver returned non-public address {address}"
                )
            }
            Self::Cancelled => formatter.write_str("guarded fetch cancelled"),
            Self::DeadlineExceeded => formatter.write_str("guarded fetch deadline exceeded"),
            Self::Connect(error) => write!(formatter, "guarded TCP connect failed: {error}"),
            Self::Tls(error) => write!(formatter, "guarded TLS handshake failed: {error}"),
            Self::PeerCertificateUnavailable => {
                formatter.write_str("guarded TLS peer did not expose a leaf certificate")
            }
            Self::PeerCertificateTooLarge => {
                formatter.write_str("guarded TLS leaf certificate exceeded the digest bound")
            }
            Self::Http(error) => write!(formatter, "guarded HTTP exchange failed: {error}"),
            Self::UnexpectedContentEncoding(value) => {
                write!(
                    formatter,
                    "guarded response uses non-identity content encoding {value:?}"
                )
            }
            Self::ResponseHeadersTooLarge => {
                formatter.write_str("guarded response headers too large")
            }
            Self::InvalidRequest(reason) => write!(formatter, "invalid guarded request: {reason}"),
        }
    }
}

impl std::error::Error for GuardedHttpFetchError {}

/// A fresh-connection, WebPKI-rooted HTTPS fetcher with resolver-answer fencing.
pub struct GuardedHttpFetcher {
    resolver: Arc<dyn GuardedHttpResolver>,
    policy: GuardedHttpFetchPolicy,
    connector: TlsConnector,
    #[cfg(test)]
    test_loopback_authority: Option<SocketAddr>,
    #[cfg(test)]
    test_exchange: Option<Arc<dyn GuardedHttpTestExchange>>,
}

/// Test-only lower wire seam. Production construction never accepts a custom
/// exchange, so it cannot weaken selected-address TCP or WebPKI TLS custody.
#[cfg(test)]
trait GuardedHttpTestExchange: Send + Sync {
    fn exchange(
        &self,
        cx: Cx,
        request: GuardedHttpTestRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>>
                + Send
                + 'static,
        >,
    >;
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct GuardedHttpTestRequest {
    host: String,
    authority: String,
    target: String,
    selected_address: SocketAddr,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct GuardedHttpTestWireResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    provenance: GuardedHttpPeerProvenance,
}

impl std::fmt::Debug for GuardedHttpFetcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedHttpFetcher")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl GuardedHttpFetcher {
    /// Build a fetcher that uses WebPKI roots, SNI, certificate validation, and
    /// a finite handshake timeout. It never accepts a caller-supplied connector
    /// because that could silently weaken this vertical's root custody.
    pub fn new(policy: GuardedHttpFetchPolicy) -> Result<Self, GuardedHttpFetchError> {
        let connector = TlsConnector::builder()
            .with_webpki_roots()
            .alpn_protocols(vec![b"http/1.1".to_vec()])
            .enable_early_data(false)
            .handshake_timeout(policy.tls_handshake_timeout)
            .build()
            .map_err(|error| GuardedHttpFetchError::Tls(error.to_string()))?;
        let resolver = Arc::new(NativeGuardedResolver::new(Arc::new(
            NativeDnsResolver::with_config(ResolverConfig {
                timeout: policy.deadline,
                retries: 0,
                happy_eyeballs: false,
                ..ResolverConfig::default()
            }),
        )));
        Ok(Self {
            resolver,
            policy,
            connector,
            #[cfg(test)]
            test_loopback_authority: None,
            #[cfg(test)]
            test_exchange: None,
        })
    }

    /// Build a fetcher that resolves through a caller-supplied resolver.
    ///
    /// SHIPPED PUBLIC SEAM (bd-ho7of). Every other guarantee is unchanged: the
    /// supplied resolver's answers are fenced by `guarded_select_address`
    /// exactly as the native resolver's are, so a resolver cannot reach a
    /// private or loopback peer, cannot exceed
    /// `MAX_GUARDED_RESOLVED_ADDRESSES`, and cannot return an empty set that
    /// is then used. TLS still validates the certificate against the original
    /// request host with WebPKI roots and early data disabled, so choosing a
    /// different admitted address cannot cause a certificate for another name
    /// to be accepted. This widens no production bound; it only makes the
    /// resolution seam reachable, and it is the same seam production uses.
    pub fn with_resolver(
        policy: GuardedHttpFetchPolicy,
        resolver: Arc<dyn GuardedHttpResolver>,
    ) -> Result<Self, GuardedHttpFetchError> {
        let mut fetcher = Self::new(policy)?;
        fetcher.resolver = resolver;
        Ok(fetcher)
    }

    #[cfg(test)]
    fn new_with_test_exchange(
        resolver: Arc<dyn GuardedHttpResolver>,
        policy: GuardedHttpFetchPolicy,
        test_exchange: Arc<dyn GuardedHttpTestExchange>,
    ) -> Result<Self, GuardedHttpFetchError> {
        // Thin wrapper over the shipped seam: resolution now goes through
        // `with_resolver`, so there is no second resolution path. Only the
        // lower wire exchange stays test-only, because it bypasses the address
        // fence and real TLS.
        let mut fetcher = Self::with_resolver(policy, resolver)?;
        fetcher.test_exchange = Some(test_exchange);
        Ok(fetcher)
    }

    /// Builds a test-only real-wire fetcher for one loopback listener.
    ///
    /// The override is deliberately private and test-gated: it admits exactly
    /// one supplied loopback socket, resolves no production name, and trusts
    /// only the supplied fixture CA. Production construction retains its fixed
    /// WebPKI connector and public-address fence.
    #[cfg(test)]
    fn new_loopback_test_authority(
        policy: GuardedHttpFetchPolicy,
        loopback_authority: SocketAddr,
        fixture_ca: asupersync::tls::Certificate,
    ) -> Result<Self, GuardedHttpFetchError> {
        if !loopback_authority.ip().is_loopback() {
            return Err(GuardedHttpFetchError::InvalidPolicy(
                "test loopback authority",
            ));
        }
        let connector = TlsConnector::builder()
            .add_root_certificate(&fixture_ca)
            .alpn_protocols(vec![b"http/1.1".to_vec()])
            .enable_early_data(false)
            .handshake_timeout(policy.tls_handshake_timeout)
            .build()
            .map_err(|error| GuardedHttpFetchError::Tls(error.to_string()))?;
        Ok(Self {
            // Uses the shipped seam like everything else; only the loopback
            // authority escape below is test-only.
            resolver: Arc::new(StaticGuardedResolverForLoopback {
                address: loopback_authority.ip(),
            }),
            policy,
            connector,
            test_loopback_authority: Some(loopback_authority),
            test_exchange: None,
        })
    }

    /// Fetch one HTTPS resource over a fresh connection.
    ///
    /// The resolver's complete answer set is admitted before `TcpStream::connect`
    /// receives exactly one selected `SocketAddr`. TLS uses the original DNS host
    /// for SNI and certificate validation. Redirects, cookies, credentials,
    /// proxies, and connection reuse are deliberately absent.
    pub async fn fetch(
        &self,
        cx: &Cx,
        url: &GuardedHttpsUrl,
    ) -> Result<GuardedHttpFetchResponse, GuardedHttpFetchError> {
        guarded_fetch_checkpoint(cx)?;
        let fetch_deadline = time::wall_now() + self.policy.deadline;
        let answers = self
            .resolve_all(cx, fetch_deadline, url.host.clone())
            .await?;
        #[cfg(test)]
        let selected_address = guarded_select_address_with_test_loopback(
            answers,
            url.port(),
            self.test_loopback_authority,
        )?;
        #[cfg(not(test))]
        let selected_address = guarded_select_address(answers, url.port())?;
        guarded_fetch_checkpoint(cx)?;

        #[cfg(test)]
        if let Some(exchange) = self.test_exchange.as_ref() {
            let exchange = Arc::clone(exchange);
            let request = GuardedHttpTestRequest {
                host: url.host.clone(),
                authority: url.authority(),
                target: url.target.clone(),
                selected_address,
            };
            let body_limit = self.policy.response_body_bytes;
            let wire = guarded_await_phase(cx, fetch_deadline, move |phase_cx| {
                exchange.exchange(phase_cx, request)
            })
            .await?;
            return guarded_admit_native_response(
                wire.status,
                wire.headers,
                wire.body,
                body_limit,
                wire.provenance,
            );
        }

        let connector = self.connector.clone();
        let host = url.host.clone();
        let authority = url.authority();
        let target = url.target.clone();
        let policy = self.policy.clone();
        let response = guarded_await_phase(cx, fetch_deadline, move |phase_cx| async move {
            guarded_fetch_checkpoint(&phase_cx)?;
            let tcp = NativeTcpStream::connect(selected_address)
                .await
                .map_err(|error| GuardedHttpFetchError::Connect(error.to_string()))?;
            tcp.set_nodelay(true)
                .map_err(|error| GuardedHttpFetchError::Connect(error.to_string()))?;
            guarded_fetch_checkpoint(&phase_cx)?;
            let tls = connector
                .connect(&host, tcp)
                .await
                .map_err(|error| GuardedHttpFetchError::Tls(error.to_string()))?;
            let provenance = guarded_peer_provenance(&tls, host, selected_address, &policy)?;
            guarded_fetch_checkpoint(&phase_cx)?;
            let request = NativeHttpRequest::get(target)
                .header("Host", authority)
                .header("Accept-Encoding", "identity")
                .header("Connection", "close")
                .build();
            let (response, _stream, _body_withheld) =
                NativeHttp1Client::request_with_io_and_max_body_size(
                    tls,
                    request,
                    policy.response_body_bytes,
                )
                .await
                .map_err(|error| GuardedHttpFetchError::Http(error.to_string()))?;
            guarded_admit_native_response(
                response.status,
                response.headers,
                response.body,
                policy.response_body_bytes,
                provenance,
            )
        })
        .await?;
        guarded_fetch_checkpoint(cx)?;
        Ok(response)
    }

    /// The fixed policy used by this fetcher.
    #[must_use]
    pub fn policy(&self) -> &GuardedHttpFetchPolicy {
        &self.policy
    }

    async fn resolve_all(
        &self,
        cx: &Cx,
        deadline: asupersync::Time,
        host: String,
    ) -> Result<Vec<IpAddr>, GuardedHttpFetchError> {
        // ONE resolution path. Production and any caller-supplied resolver both
        // arrive here, and both are fenced by `guarded_select_address` after.
        let resolver = Arc::clone(&self.resolver);
        guarded_await_phase(cx, deadline, move |phase_cx| {
            resolver.resolve_all(phase_cx, host)
        })
        .await
    }
}

/// Domain separator for the root-set identity preimage. Changing it changes
/// every custom-root identity, so it is versioned.
const GUARDED_ROOT_SET_IDENTITY_DOMAIN: &[u8] = b"FND05ROOTSETv1\0";
/// Prefix of the root identity a [`GuardedRootSet`] fetcher reports.
const GUARDED_ROOT_SET_IDENTITY_PREFIX: &str = "custom-roots.sha256.";
/// Upper bound of the identity preimage: domain, length-prefixed caller
/// revision, certificate count, and every length-prefixed certificate at its
/// maximum size.
const MAX_GUARDED_ROOT_SET_IDENTITY_PREIMAGE_BYTES: usize = GUARDED_ROOT_SET_IDENTITY_DOMAIN.len()
    + 4
    + MAX_GUARDED_ROOT_POLICY_REVISION_BYTES
    + 4
    + MAX_GUARDED_ROOT_SET_CERTIFICATES * (4 + MAX_GUARDED_LEAF_CERTIFICATE_BYTES);

/// A transport-level request value for [`GuardedHttpFetcher::post`].
///
/// It carries exactly a content type, the body bytes, and an optional
/// authorization value, and nothing else. It holds no OAuth semantics: the
/// body is opaque bytes, and the authorization value is an opaque field value
/// that the caller composes.
///
/// Every field is validated at construction, so a value that exists is
/// already admissible: `post` cannot receive an invalid request, and no
/// refusal here can reach a resolver or a socket. A content type or
/// authorization value must be non-empty, at most
/// [`MAX_GUARDED_REQUEST_HEADER_VALUE_BYTES`], made only of visible ASCII,
/// SP, and HTAB (so CR, LF, NUL, other controls, DEL, and non-ASCII are
/// refused), and must not begin or end with SP or HTAB. The body is at most
/// [`MAX_GUARDED_REQUEST_BODY_BYTES`].
#[derive(Clone, PartialEq, Eq)]
pub struct GuardedHttpRequest {
    content_type: String,
    body: Vec<u8>,
    authorization: Option<String>,
}

impl GuardedHttpRequest {
    /// Build a request with a validated content type and a bounded body.
    pub fn new(
        content_type: impl Into<String>,
        body: impl Into<Vec<u8>>,
    ) -> Result<Self, GuardedHttpFetchError> {
        let content_type = content_type.into();
        let body = body.into();
        if body.len() > MAX_GUARDED_REQUEST_BODY_BYTES {
            return Err(GuardedHttpFetchError::InvalidRequest("request body bound"));
        }
        if !is_guarded_request_field_value(&content_type) {
            return Err(GuardedHttpFetchError::InvalidRequest("content type"));
        }
        Ok(Self {
            content_type,
            body,
            authorization: None,
        })
    }

    /// Attach a validated authorization field value.
    ///
    /// On refusal the value is dropped and never echoed; the typed error
    /// carries only static text.
    pub fn with_authorization(
        mut self,
        value: impl Into<String>,
    ) -> Result<Self, GuardedHttpFetchError> {
        let value = value.into();
        if !is_guarded_request_field_value(&value) {
            return Err(GuardedHttpFetchError::InvalidRequest("authorization value"));
        }
        self.authorization = Some(value);
        Ok(self)
    }

    /// The validated content type.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// The body bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Whether an authorization value is attached. The value itself is not
    /// readable back from this type.
    #[must_use]
    pub fn has_authorization(&self) -> bool {
        self.authorization.is_some()
    }
}

/// Shows the body by length only and the authorization value as a redaction
/// marker, so a credential never reaches a log through `Debug`.
impl std::fmt::Debug for GuardedHttpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedHttpRequest")
            .field("content_type", &self.content_type)
            .field("body_len", &self.body.len())
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn is_guarded_request_field_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_GUARDED_REQUEST_HEADER_VALUE_BYTES
        && bytes
            .iter()
            .all(|&byte| matches!(byte, b'\t' | b' ' | 0x21..=0x7e))
        && !matches!(bytes.first(), Some(b'\t' | b' '))
        && !matches!(bytes.last(), Some(b'\t' | b' '))
}

/// Encode the exact request bytes [`GuardedHttpFetcher::post`] writes for
/// `request` to `url`.
///
/// This is not a second serializer. `post` sends the request built by the
/// same private composer, and asupersync's `Http1Client` encodes it with the
/// same public `Http1ClientCodec` used here. The frozen shape is the request
/// line `POST <target> HTTP/1.1`, then `Host`, `Content-Type`,
/// `Content-Length` (the body's byte length in decimal), `Authorization`
/// (only when supplied), `Accept-Encoding: identity`, and `Connection: close`,
/// then the body. It never emits `Transfer-Encoding`.
pub fn guarded_encode_post_request(
    url: &GuardedHttpsUrl,
    request: &GuardedHttpRequest,
) -> Result<Vec<u8>, GuardedHttpFetchError> {
    let mut encoded = asupersync::bytes::BytesMut::new();
    asupersync::codec::Encoder::encode(
        &mut asupersync::http::h1::Http1ClientCodec::new(),
        guarded_compose_post_request(url, request),
        &mut encoded,
    )
    .map_err(|error| GuardedHttpFetchError::Http(error.to_string()))?;
    Ok(encoded.as_ref().to_vec())
}

/// The one composer for guarded POST requests, shared by
/// [`guarded_encode_post_request`] and [`GuardedHttpFetcher::post`].
fn guarded_compose_post_request(
    url: &GuardedHttpsUrl,
    request: &GuardedHttpRequest,
) -> NativeHttpRequest {
    let mut builder = NativeHttpRequest::post(url.target.clone())
        .header("Host", url.authority())
        .header("Content-Type", request.content_type.clone())
        .header("Content-Length", request.body.len().to_string());
    if let Some(authorization) = request.authorization.as_ref() {
        builder = builder.header("Authorization", authorization.clone());
    }
    builder
        .header("Accept-Encoding", "identity")
        .header("Connection", "close")
        .body(request.body.clone())
        .build()
}

/// A transport-owned, non-empty set of DER trust anchors that REPLACES the
/// WebPKI roots for one fetcher and never widens them.
///
/// A fetcher built from a root set trusts exactly these certificates: its
/// connector is built by adding only them, and it never adds WebPKI or native
/// roots. That matters because asupersync's connector builder accumulates
/// roots, so any such call would silently widen trust. No constructor accepts
/// a caller connector.
///
/// Construction checks every certificate the same way the connector's root
/// store will: each must be non-empty, at most
/// [`MAX_GUARDED_LEAF_CERTIFICATE_BYTES`], and accepted by an asupersync
/// [`RootCertStore`](asupersync::tls::RootCertStore). A certificate the store
/// would reject is refused here rather than silently dropped later, so the
/// identity a fetcher reports covers only anchors it actually trusts. At most
/// [`MAX_GUARDED_ROOT_SET_CERTIFICATES`] certificates may be supplied.
/// Certificates are kept sorted by DER bytes with duplicates removed, so the
/// set, and the identity folded from it, do not depend on supply order.
///
/// No `BasicConstraints CA:TRUE` gate is applied. asupersync's strict-CA mode
/// drops a non-CA certificate without reporting it, which would let the
/// reported identity name an anchor that is not trusted. A caller that
/// supplies a leaf certificate here is pinning it deliberately.
#[derive(Clone)]
pub struct GuardedRootSet {
    certificates: Vec<asupersync::tls::Certificate>,
}

impl GuardedRootSet {
    /// Build a non-empty root set from DER certificates.
    pub fn new(
        certificates: impl IntoIterator<Item = asupersync::tls::Certificate>,
    ) -> Result<Self, GuardedHttpFetchError> {
        let mut admitted: Vec<asupersync::tls::Certificate> = Vec::new();
        for certificate in certificates {
            if admitted.len() == MAX_GUARDED_ROOT_SET_CERTIFICATES {
                return Err(GuardedHttpFetchError::InvalidPolicy(
                    "root set certificate count",
                ));
            }
            let der_len = certificate.as_der().len();
            if der_len == 0 || der_len > MAX_GUARDED_LEAF_CERTIFICATE_BYTES {
                return Err(GuardedHttpFetchError::InvalidPolicy(
                    "root certificate size",
                ));
            }
            if asupersync::tls::RootCertStore::empty()
                .add(&certificate)
                .is_err()
            {
                return Err(GuardedHttpFetchError::InvalidPolicy("root certificate"));
            }
            admitted.push(certificate);
        }
        if admitted.is_empty() {
            return Err(GuardedHttpFetchError::InvalidPolicy("empty root set"));
        }
        admitted.sort_unstable_by(|left, right| left.as_der().cmp(right.as_der()));
        admitted.dedup_by(|left, right| left.as_der() == right.as_der());
        Ok(Self {
            certificates: admitted,
        })
    }

    /// Number of distinct certificates in the set.
    #[must_use]
    pub fn certificate_count(&self) -> usize {
        self.certificates.len()
    }
}

impl std::fmt::Debug for GuardedRootSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedRootSet")
            .field("certificate_count", &self.certificates.len())
            .finish()
    }
}

impl GuardedHttpFetcher {
    /// Build a fetcher that trusts exactly `roots` instead of WebPKI.
    ///
    /// Everything else matches [`Self::new`]: the native resolver with the
    /// policy deadline, the public-address fence, SNI and hostname validation
    /// against the original host, `http/1.1` ALPN, early data disabled, and
    /// the policy's finite handshake timeout.
    ///
    /// ROOT IDENTITY. The fetcher does not report the caller's
    /// `root_policy_revision` unchanged, because that label would then claim
    /// a trust configuration it does not have. It stores a policy whose
    /// revision is `custom-roots.sha256.` followed by the lowercase hex
    /// SHA-256 of `FND05ROOTSETv1` NUL, the caller revision (u32 big-endian
    /// length, then bytes), the certificate count (u32 big-endian), and each
    /// certificate in DER-sorted order (u32 big-endian length, then DER).
    /// [`Self::policy`], [`Self::root_identity`], and every fetch's
    /// [`GuardedHttpPeerProvenance::root_policy_revision`] report that value.
    /// For a fetcher from this constructor, the field doc's statement that the
    /// roots are fixed WebPKI roots does not apply.
    pub fn with_root_set(
        policy: GuardedHttpFetchPolicy,
        roots: &GuardedRootSet,
    ) -> Result<Self, GuardedHttpFetchError> {
        let resolver = guarded_native_resolver(&policy);
        Self::with_root_set_and_resolver(policy, roots, resolver)
    }

    /// [`Self::with_root_set`] resolving through a caller-supplied resolver.
    ///
    /// The resolver seam is the one [`Self::with_resolver`] exposes, with the
    /// same guarantees: its answers are fenced by the public-address check
    /// before any connection.
    pub fn with_root_set_and_resolver(
        policy: GuardedHttpFetchPolicy,
        roots: &GuardedRootSet,
        resolver: Arc<dyn GuardedHttpResolver>,
    ) -> Result<Self, GuardedHttpFetchError> {
        let connector = guarded_root_set_connector(roots, policy.tls_handshake_timeout)?;
        let policy = guarded_root_set_policy(&policy, roots)?;
        Ok(Self {
            resolver,
            policy,
            connector,
            #[cfg(test)]
            test_loopback_authority: None,
            #[cfg(test)]
            test_exchange: None,
        })
    }

    /// Test-only real-wire root-set fetcher for one loopback listener.
    ///
    /// It uses the shipped root-set connector and identity fold. Only the
    /// loopback authority escape is test-only, exactly as in
    /// `new_loopback_test_authority`.
    #[cfg(test)]
    fn new_loopback_test_authority_with_root_set(
        policy: GuardedHttpFetchPolicy,
        loopback_authority: SocketAddr,
        roots: &GuardedRootSet,
    ) -> Result<Self, GuardedHttpFetchError> {
        if !loopback_authority.ip().is_loopback() {
            return Err(GuardedHttpFetchError::InvalidPolicy(
                "test loopback authority",
            ));
        }
        let resolver = Arc::new(StaticGuardedResolverForLoopback {
            address: loopback_authority.ip(),
        });
        let mut fetcher = Self::with_root_set_and_resolver(policy, roots, resolver)?;
        fetcher.test_loopback_authority = Some(loopback_authority);
        Ok(fetcher)
    }

    /// The root identity this fetcher reports in provenance, observable
    /// before any fetch.
    ///
    /// For a WebPKI fetcher it is the caller's `root_policy_revision`. For a
    /// root-set fetcher it is the folded identity described on
    /// [`Self::with_root_set`].
    #[must_use]
    pub fn root_identity(&self) -> &str {
        self.policy.root_policy_revision()
    }

    /// POST one request over a fresh connection.
    ///
    /// Resolution, the public-address fence, the per-phase deadline, TCP, TLS,
    /// provenance, and response admission are the same as for
    /// [`Self::fetch`], which this method leaves unchanged. The response passes
    /// through [`guarded_admit_native_response`], the admission `fetch` uses.
    /// The request bytes are exactly [`guarded_encode_post_request`]'s output.
    ///
    /// A redirect is never followed or replayed. This method sends one request
    /// and returns; a 3xx comes back as [`GuardedHttpRedirect`] data.
    pub async fn post(
        &self,
        cx: &Cx,
        url: &GuardedHttpsUrl,
        request: &GuardedHttpRequest,
    ) -> Result<GuardedHttpFetchResponse, GuardedHttpFetchError> {
        guarded_fetch_checkpoint(cx)?;
        let fetch_deadline = time::wall_now() + self.policy.deadline;
        let answers = self
            .resolve_all(cx, fetch_deadline, url.host.clone())
            .await?;
        #[cfg(test)]
        let selected_address = guarded_select_address_with_test_loopback(
            answers,
            url.port(),
            self.test_loopback_authority,
        )?;
        #[cfg(not(test))]
        let selected_address = guarded_select_address(answers, url.port())?;
        guarded_fetch_checkpoint(cx)?;

        let connector = self.connector.clone();
        let host = url.host.clone();
        let native_request = guarded_compose_post_request(url, request);
        let policy = self.policy.clone();
        let response = guarded_await_phase(cx, fetch_deadline, move |phase_cx| async move {
            guarded_fetch_checkpoint(&phase_cx)?;
            let tcp = NativeTcpStream::connect(selected_address)
                .await
                .map_err(|error| GuardedHttpFetchError::Connect(error.to_string()))?;
            tcp.set_nodelay(true)
                .map_err(|error| GuardedHttpFetchError::Connect(error.to_string()))?;
            guarded_fetch_checkpoint(&phase_cx)?;
            let tls = connector
                .connect(&host, tcp)
                .await
                .map_err(|error| GuardedHttpFetchError::Tls(error.to_string()))?;
            let provenance = guarded_peer_provenance(&tls, host, selected_address, &policy)?;
            guarded_fetch_checkpoint(&phase_cx)?;
            let (response, _stream, _body_withheld) =
                NativeHttp1Client::request_with_io_and_max_body_size(
                    tls,
                    native_request,
                    policy.response_body_bytes,
                )
                .await
                .map_err(|error| GuardedHttpFetchError::Http(error.to_string()))?;
            guarded_admit_native_response(
                response.status,
                response.headers,
                response.body,
                policy.response_body_bytes,
                provenance,
            )
        })
        .await?;
        guarded_fetch_checkpoint(cx)?;
        Ok(response)
    }
}

/// The production resolver configuration [`GuardedHttpFetcher::new`] uses.
fn guarded_native_resolver(policy: &GuardedHttpFetchPolicy) -> Arc<dyn GuardedHttpResolver> {
    Arc::new(NativeGuardedResolver::new(Arc::new(
        NativeDnsResolver::with_config(ResolverConfig {
            timeout: policy.deadline,
            retries: 0,
            happy_eyeballs: false,
            ..ResolverConfig::default()
        }),
    )))
}

/// The root-set connector. It adds only the set's certificates and never
/// calls `with_webpki_roots` or `with_native_roots`, so trust is replaced,
/// never widened. ALPN, early data, and the handshake timeout match
/// [`GuardedHttpFetcher::new`].
fn guarded_root_set_connector(
    roots: &GuardedRootSet,
    tls_handshake_timeout: Duration,
) -> Result<TlsConnector, GuardedHttpFetchError> {
    TlsConnector::builder()
        .add_root_certificates(roots.certificates.iter().cloned())
        .alpn_protocols(vec![b"http/1.1".to_vec()])
        .enable_early_data(false)
        .handshake_timeout(tls_handshake_timeout)
        .build()
        .map_err(|error| GuardedHttpFetchError::Tls(error.to_string()))
}

/// The caller policy with its revision replaced by the folded root identity.
/// It goes back through [`GuardedHttpFetchPolicy::new`], so every policy
/// invariant is re-checked on the value the fetcher stores.
fn guarded_root_set_policy(
    policy: &GuardedHttpFetchPolicy,
    roots: &GuardedRootSet,
) -> Result<GuardedHttpFetchPolicy, GuardedHttpFetchError> {
    GuardedHttpFetchPolicy::new(
        policy.response_body_bytes,
        policy.deadline,
        policy.tls_handshake_timeout,
        guarded_root_set_identity(&policy.root_policy_revision, roots)?,
    )
}

fn guarded_root_set_identity(
    caller_revision: &str,
    roots: &GuardedRootSet,
) -> Result<String, GuardedHttpFetchError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    fn push_length_prefixed(
        preimage: &mut Vec<u8>,
        bytes: &[u8],
    ) -> Result<(), GuardedHttpFetchError> {
        let length = u32::try_from(bytes.len())
            .map_err(|_| GuardedHttpFetchError::InvalidPolicy("root set identity bound"))?;
        preimage.extend_from_slice(&length.to_be_bytes());
        preimage.extend_from_slice(bytes);
        Ok(())
    }

    let mut preimage = Vec::new();
    preimage.extend_from_slice(GUARDED_ROOT_SET_IDENTITY_DOMAIN);
    push_length_prefixed(&mut preimage, caller_revision.as_bytes())?;
    let count = u32::try_from(roots.certificates.len())
        .map_err(|_| GuardedHttpFetchError::InvalidPolicy("root set identity bound"))?;
    preimage.extend_from_slice(&count.to_be_bytes());
    for certificate in &roots.certificates {
        push_length_prefixed(&mut preimage, certificate.as_der())?;
    }
    let digest = sha256_bounded(&preimage, MAX_GUARDED_ROOT_SET_IDENTITY_PREIMAGE_BYTES)
        .map_err(|_| GuardedHttpFetchError::InvalidPolicy("root set identity bound"))?;
    let mut identity = String::with_capacity(GUARDED_ROOT_SET_IDENTITY_PREFIX.len() + 64);
    identity.push_str(GUARDED_ROOT_SET_IDENTITY_PREFIX);
    for byte in digest.as_bytes() {
        identity.push(char::from(HEX[usize::from(byte >> 4)]));
        identity.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(identity)
}

/// Admit one already-received HTTP response head and body, applying the whole
/// guarded response policy: header/body bounds, content-coding enforcement, and
/// **redirect interception**.
///
/// # Why this is public
///
/// FND-05 requires redirect interception to be proven from OUTSIDE this crate.
/// A `#[cfg(test)]` proof cannot do that (PL-3): `cfg(test)` is set only when
/// compiling this crate's own lib-test harness, so no integration test and no
/// downstream consumer can reach it. This function is therefore shipped.
///
/// # The fence, stated as a mechanism rather than a convention
///
/// This function is **pure**. It takes a status, headers, a body and the
/// provenance of a fetch that already happened, and returns typed data. It
/// holds no socket, resolver, connector or policy; it cannot perform DNS, open
/// a connection, or reach any address. Promoting it therefore grants a caller
/// **zero networking capability** — there is no fence here to bypass, because
/// every destination and TLS fence lives strictly upstream and has already run
/// by the time these bytes exist. Contrast the two seams that remain test-only:
/// `test_exchange` substitutes the wire itself and `test_loopback_authority`
/// admits a non-public peer, and both of those DO defeat an upstream fence.
///
/// # What it does and does not establish about redirects
///
/// `provenance` is passed through unchanged, so a redirect is reported as data
/// and never becomes the origin of the response — that is redirect-origin
/// interception, and it is observable here. It does NOT establish that
/// `fetch` declines to open a follow-up connection; that is a property of the
/// caller of this function, not of this function.
pub fn guarded_admit_native_response(
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_limit: usize,
    provenance: GuardedHttpPeerProvenance,
) -> Result<GuardedHttpFetchResponse, GuardedHttpFetchError> {
    let header_bytes = headers.iter().try_fold(0_usize, |total, (name, value)| {
        total
            .checked_add(name.len())
            .and_then(|total| total.checked_add(value.len()))
            .and_then(|total| total.checked_add(4))
            .ok_or(GuardedHttpFetchError::ResponseHeadersTooLarge)
    })?;
    if header_bytes > MAX_GUARDED_RESPONSE_HEADER_BYTES {
        return Err(GuardedHttpFetchError::ResponseHeadersTooLarge);
    }
    if body.len() > body_limit {
        return Err(GuardedHttpFetchError::Http(
            "response body exceeded guarded bound".to_owned(),
        ));
    }
    for (_, encoding) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
    {
        if !is_identity_content_coding(encoding) {
            return Err(GuardedHttpFetchError::UnexpectedContentEncoding(
                encoding.clone(),
            ));
        }
    }
    let redirect = (300..400).contains(&status).then(|| GuardedHttpRedirect {
        status,
        location: headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
            .map(|(_, value)| value.clone()),
    });
    Ok(GuardedHttpFetchResponse {
        status,
        headers,
        body,
        redirect,
        provenance,
    })
}

fn guarded_peer_provenance(
    tls: &asupersync::tls::TlsStream<NativeTcpStream>,
    host: String,
    selected_address: SocketAddr,
    policy: &GuardedHttpFetchPolicy,
) -> Result<GuardedHttpPeerProvenance, GuardedHttpFetchError> {
    let leaf_certificate_sha256 = guarded_leaf_certificate_sha256(tls.peer_leaf_certificate_der())?;
    Ok(GuardedHttpPeerProvenance {
        host,
        selected_address,
        leaf_certificate_sha256,
        alpn: tls.alpn_protocol().map(<[u8]>::to_vec),
        tls_protocol: tls.protocol_version().map(|version| format!("{version:?}")),
        root_policy_revision: policy.root_policy_revision.clone(),
    })
}

fn guarded_leaf_certificate_sha256(
    leaf_der: Option<Vec<u8>>,
) -> Result<[u8; 32], GuardedHttpFetchError> {
    let leaf_der = leaf_der.ok_or(GuardedHttpFetchError::PeerCertificateUnavailable)?;
    sha256_bounded(&leaf_der, MAX_GUARDED_LEAF_CERTIFICATE_BYTES)
        .map_err(|_| GuardedHttpFetchError::PeerCertificateTooLarge)
        .map(|digest| digest.into_bytes())
}

async fn guarded_await_phase<F, Fut, T>(
    cx: &Cx,
    deadline: asupersync::Time,
    phase: F,
) -> Result<T, GuardedHttpFetchError>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, GuardedHttpFetchError>> + Send + 'static,
    T: Send + 'static,
{
    if time::wall_now() >= deadline {
        return Err(GuardedHttpFetchError::DeadlineExceeded);
    }
    let mut task = cx
        .spawn(move |phase_cx| async move {
            let mut future = Box::pin(phase(phase_cx.clone()));
            std::future::poll_fn(|task_cx| {
                if let Err(error) = guarded_phase_checkpoint(&phase_cx) {
                    return Poll::Ready(Err(error));
                }
                future.as_mut().poll(task_cx)
            })
            .await
        })
        .map_err(|error| GuardedHttpFetchError::Http(error.to_string()))?;
    match time::timeout_at(
        deadline,
        task.join_with_drop_reason(cx, CancelReason::timeout()),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(guarded_join_error(error, cx, deadline)),
        Err(_) => {
            // `timeout` drops its join future, which requests cancellation.
            // Abort again explicitly, then settle the task before returning so
            // no phase (and no owned TCP/TLS stream) escapes this fetch.
            task.abort();
            match task.join(cx).await {
                Ok(Ok(_)) | Err(asupersync::runtime::JoinError::Cancelled(_)) => {
                    Err(GuardedHttpFetchError::DeadlineExceeded)
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(guarded_join_error(error, cx, deadline)),
            }
        }
    }
}

fn guarded_join_error(
    error: asupersync::runtime::JoinError,
    cx: &Cx,
    deadline: asupersync::Time,
) -> GuardedHttpFetchError {
    match error {
        asupersync::runtime::JoinError::Cancelled(reason) => {
            if let Err(error) = guarded_fetch_checkpoint(cx) {
                error
            } else if matches!(reason.kind, CancelKind::Deadline | CancelKind::Timeout)
                || time::wall_now() >= deadline
            {
                GuardedHttpFetchError::DeadlineExceeded
            } else {
                GuardedHttpFetchError::Cancelled
            }
        }
        asupersync::runtime::JoinError::Panicked(payload) => {
            GuardedHttpFetchError::Http(format!("guarded fetch phase panicked: {payload}"))
        }
        asupersync::runtime::JoinError::PolledAfterCompletion => {
            GuardedHttpFetchError::Http("guarded fetch phase join consumed unexpectedly".to_owned())
        }
    }
}

fn guarded_fetch_checkpoint(cx: &Cx) -> Result<(), GuardedHttpFetchError> {
    cx.checkpoint()
        .map_err(|_| GuardedHttpFetchError::Cancelled)
}

fn guarded_phase_checkpoint(cx: &Cx) -> Result<(), GuardedHttpFetchError> {
    cx.checkpoint().map_err(|_| {
        if cx
            .cancel_reason()
            .is_some_and(|reason| matches!(reason.kind, CancelKind::Deadline | CancelKind::Timeout))
        {
            GuardedHttpFetchError::DeadlineExceeded
        } else {
            GuardedHttpFetchError::Cancelled
        }
    })
}

fn parse_guarded_https_authority(authority: &str) -> Result<(String, u16), GuardedHttpFetchError> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or(GuardedHttpFetchError::InvalidUrl("port"))?;
            (host, port)
        }
        _ => (authority, 443),
    };
    let host = host.to_ascii_lowercase();
    if host.len() > 253
        || host.ends_with('.')
        || host.parse::<IpAddr>().is_ok()
        || !host.split('.').all(is_guarded_dns_label)
    {
        return Err(GuardedHttpFetchError::InvalidUrl("DNS host"));
    }
    Ok((host, port))
}

fn is_guarded_dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn guarded_select_address(
    answers: Vec<IpAddr>,
    port: u16,
) -> Result<SocketAddr, GuardedHttpFetchError> {
    guarded_select_address_inner(answers, port, None)
}

#[cfg(test)]
fn guarded_select_address_with_test_loopback(
    answers: Vec<IpAddr>,
    port: u16,
    allowed_loopback: Option<SocketAddr>,
) -> Result<SocketAddr, GuardedHttpFetchError> {
    guarded_select_address_inner(answers, port, allowed_loopback)
}

fn guarded_select_address_inner(
    answers: Vec<IpAddr>,
    port: u16,
    #[cfg_attr(not(test), allow(unused_variables))] allowed_loopback: Option<SocketAddr>,
) -> Result<SocketAddr, GuardedHttpFetchError> {
    if answers.is_empty() {
        return Err(GuardedHttpFetchError::ResolutionEmpty);
    }
    if answers.len() > MAX_GUARDED_RESOLVED_ADDRESSES {
        return Err(GuardedHttpFetchError::ResolutionTooMany);
    }
    let mut addresses = Vec::with_capacity(answers.len());
    for answer in answers {
        let canonical = canonical_guarded_ip(answer);
        let address = SocketAddr::new(canonical, port);
        #[cfg(test)]
        let is_exact_test_loopback = allowed_loopback == Some(address);
        #[cfg(not(test))]
        let is_exact_test_loopback = false;
        if !is_exact_test_loopback && !is_public_guarded_ip(canonical) {
            return Err(GuardedHttpFetchError::DisallowedResolvedAddress(canonical));
        }
        addresses.push(address);
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses
        .into_iter()
        .next()
        .ok_or(GuardedHttpFetchError::ResolutionEmpty)
}

fn canonical_guarded_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

fn is_public_guarded_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_guarded_ipv4(address),
        IpAddr::V6(address) => is_public_guarded_ipv6(address),
    }
}

fn is_public_guarded_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !matches!(
        (a, b, c),
        (0 | 10 | 127 | 224..=255, _, _)
            | (100, 64..=127, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0 | 2 | 168, _)
            | (192, 88, 99)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
    )
}

fn is_public_guarded_ipv6(address: Ipv6Addr) -> bool {
    if address.is_unspecified() || address.is_loopback() || address.is_multicast() {
        return false;
    }
    let segments = address.segments();
    // IANA special-purpose prefixes marked globally UNREACHABLE in the pinned
    // 2025-10-09 registry (evidence/fnd-05/iana/) that the arms below admit.
    // 2001::/23 is denied whole, which also denies its seven global=True
    // anycast/service sub-allocations. That fail-closed over-denial is ruled
    // and declared in provenance.toml (bd-fnd-05-implementation-b-ysez). Kept
    // as a separate block so every existing arm below stays unchanged.
    if matches!(
        segments,
        [0x0100, 0, 0, 1, ..]
            | [0x2001, 0x0000..=0x01FF, ..]
            | [0x3FFF, 0x0000..=0x0FFF, ..]
            | [0x5F00, ..]
    ) {
        return false;
    }
    !matches!(
        segments,
        [0x0000 | 0x2002 | 0xFC00..=0xFDFF | 0xFE80..=0xFEBF, ..]
            | [0x0064, 0xFF9B, 0, 0, 0, 0, ..]
            | [0x0064, 0xFF9B, 0x0001, ..]
            | [0x0100, 0, 0, 0, ..]
            | [0x2001, 0 | 0x0DB8, ..]
    )
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_valid_http_header_value(value: &str) -> bool {
    // RFC 9110 section 5.5: field-value = *field-content. Empty extension
    // fields are syntactically valid; field-specific requirements belong
    // below, not in the shared control-byte check.
    value
        .bytes()
        .all(|byte| byte == b'\t' || byte >= b' ' && byte != 0x7f)
}

fn is_valid_http_field_value(name: &str, value: &str) -> bool {
    if !is_valid_http_header_value(value) {
        return false;
    }
    if !value.trim_matches([' ', '\t']).is_empty() {
        return true;
    }

    // Preserve nonempty values for the framing, authentication, origin and
    // MCP metadata fields this transport relies on. This is a minimum bound,
    // not a replacement for their existing field-specific parsers/policies.
    // Match without allocating or depending on an adapter normalizing names.
    ![
        "authorization",
        "proxy-authorization",
        "www-authenticate",
        "proxy-authenticate",
        "content-type",
        "content-length",
        "content-encoding",
        "transfer-encoding",
        "host",
        "origin",
        "access-control-allow-origin",
        "access-control-request-method",
        "mcp-protocol-version",
        "mcp-session-id",
        "mcp-method",
        "mcp-name",
    ]
    .iter()
    .any(|required| name.eq_ignore_ascii_case(required))
}

/// Validates requests assembled through the public `HttpRequest` fields.
///
/// The wire parser normalizes names while parsing, but framework integrations
/// commonly construct `HttpRequest` directly. HTTP field names remain
/// case-insensitive on that path, and two differently-cased map keys must not
/// be allowed to smuggle conflicting security-sensitive values.
fn validate_http_request_headers(request: &HttpRequest) -> Result<(), HttpError> {
    let mut normalized_names = HashSet::with_capacity(request.headers.len());
    for (name, value) in &request.headers {
        if !is_http_token(name) {
            return Err(HttpError::InvalidHeader(format!(
                "invalid request header name: {name}"
            )));
        }
        if !is_valid_http_field_value(name, value) {
            return Err(HttpError::InvalidHeader(format!(
                "invalid value for request header {name}"
            )));
        }

        let normalized_name = name.to_ascii_lowercase();
        if !normalized_names.insert(normalized_name.clone()) {
            return Err(HttpError::InvalidHeader(format!(
                "duplicate request header: {normalized_name}"
            )));
        }
    }
    Ok(())
}

fn validate_mcp_request_metadata(request: &HttpRequest) -> Result<(), HttpError> {
    if request.method != HttpMethod::Post {
        return Err(HttpError::InvalidMethod(
            request.method.as_str().to_string(),
        ));
    }

    let content_type = request.content_type().unwrap_or("");
    if !is_modern_json_content_type(content_type) {
        return Err(HttpError::InvalidContentType(content_type.to_string()));
    }
    if let Some(coding) = request.header("content-encoding")
        && !is_identity_content_coding(coding)
    {
        return Err(HttpError::UnsupportedContentEncoding(coding.to_string()));
    }
    Ok(())
}

/// Accepts exactly `application/json`, optionally with one single
/// `charset=utf-8` parameter, ASCII-case-insensitively. A different charset
/// or any other parameter must be refused before body admission rather than
/// silently decoded as UTF-8 anyway.
fn is_modern_json_content_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("application/json") {
        return false;
    }
    let Some(parameter) = parts.next() else {
        return true;
    };
    if parts.next().is_some() {
        return false;
    }
    let Some((name, charset)) = parameter.trim().split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("charset") && charset.trim().eq_ignore_ascii_case("utf-8")
}

/// Maximum ignored empty RFC 9110 list elements in one request
/// `Content-Encoding` value; framing noise stays finite.
const MAX_IGNORED_REQUEST_CONTENT_ENCODING_EMPTY_ELEMENTS: usize = 16;

/// A present request `Content-Encoding` must reduce to exactly one semantic
/// `identity` token. Any compressed or unknown coding is a transport failure
/// with no JSON-RPC dispatch; without this check a coded body would reach
/// JSON admission and fail there with a misleading diagnostic.
fn is_identity_content_coding(value: &str) -> bool {
    let mut ignored_empty_elements = 0_usize;
    let mut semantic_codings = 0_usize;
    for element in value.split(',') {
        let element = element.trim_matches([' ', '\t']);
        if element.is_empty() {
            ignored_empty_elements += 1;
            if ignored_empty_elements > MAX_IGNORED_REQUEST_CONTENT_ENCODING_EMPTY_ELEMENTS {
                return false;
            }
            continue;
        }
        if !element.eq_ignore_ascii_case("identity") {
            return false;
        }
        semantic_codings += 1;
        if semantic_codings > 1 {
            return false;
        }
    }
    semantic_codings == 1
}

fn is_origin_allowed(config: &HttpHandlerConfig, origin: &str) -> bool {
    config.allow_cors
        && !origin.is_empty()
        && config.cors_origins.iter().any(|allowed| allowed == origin)
}

fn validate_mcp_request_policy(
    request: &HttpRequest,
    config: &HttpHandlerConfig,
) -> Result<(), HttpError> {
    validate_http_request_headers(request)?;
    if request.path != config.base_path {
        return Err(HttpError::InvalidPath(request.path.clone()));
    }
    if let Some(origin) = request.header("origin") {
        if !is_origin_allowed(config, origin) {
            return Err(HttpError::OriginNotAllowed(origin.to_string()));
        }
    }
    validate_mcp_request_metadata(request)?;
    if request.body.len() > config.max_body_size {
        return Err(HttpError::BodyTooLarge {
            size: request.body.len(),
            max: config.max_body_size,
        });
    }
    Ok(())
}

fn body_protocol_version(request: &JsonRpcRequest) -> Option<&str> {
    request
        .params
        .as_ref()?
        .as_object()?
        .get("_meta")?
        .as_object()?
        .get("io.modelcontextprotocol/protocolVersion")?
        .as_str()
}

fn body_mcp_name(request: &JsonRpcRequest) -> Option<&str> {
    let parameter_name = match request.method.as_str() {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        "tasks/get" | "tasks/update" | "tasks/cancel" => "taskId",
        _ => return None,
    };
    request
        .params
        .as_ref()?
        .as_object()?
        .get(parameter_name)?
        .as_str()
}

fn response_representation(request: &HttpRequest) -> Result<HttpResponseRepresentation, HttpError> {
    HttpResponsePreferences::from_headers(
        request
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )
    .map(HttpResponsePreferences::preferred)
}

const MAX_ACCEPT_MEMBERS: usize = 16;
const MAX_ACCEPT_PARAMETERS: usize = 16;
const MAX_IGNORED_ACCEPT_EMPTY_ELEMENTS: usize = 16;

/// Admitted preferences for the parameter-free JSON and SSE response types.
///
/// A single bounded parser supplies both ordinary response selection and the
/// acceptance check for a method that requires SSE. More-specific ranges take
/// precedence over wildcards, including an explicit quality of zero. Duplicate
/// equally specific ranges use the lower quality independent of field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpResponsePreferences {
    json_quality: u16,
    sse_quality: u16,
}

impl HttpResponsePreferences {
    /// Admits all `Accept` field lines without allocating combined header data.
    ///
    /// Callers apply their HTTP header byte/count limits before this operation.
    /// Negotiation additionally bounds media ranges, parameters, and ignored
    /// empty list members to 16 each. Quoted delimiters cannot introduce a new
    /// range, and malformed syntax never grants acceptance through a wildcard.
    /// An absent `Accept` permits both representations and prefers JSON.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::NotAcceptable`] for malformed or over-limit input,
    /// or when neither offered representation has a positive quality.
    pub fn from_headers<'a>(
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, HttpError> {
        let mut members = 0_usize;
        let mut empty_elements = 0_usize;
        let mut saw_accept = false;
        let mut json = AcceptPreference::default();
        let mut sse = AcceptPreference::default();

        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("accept") {
                continue;
            }
            saw_accept = true;
            for member in AcceptQuotedParts::new(value, b',') {
                let member = member
                    .map_err(|()| HttpError::NotAcceptable)?
                    .trim_matches([' ', '\t']);
                if member.is_empty() {
                    empty_elements += 1;
                    if empty_elements > MAX_IGNORED_ACCEPT_EMPTY_ELEMENTS {
                        return Err(HttpError::NotAcceptable);
                    }
                    continue;
                }
                members += 1;
                if members > MAX_ACCEPT_MEMBERS {
                    return Err(HttpError::NotAcceptable);
                }
                let range =
                    AcceptMediaRange::parse(member).map_err(|()| HttpError::NotAcceptable)?;
                if let Some(specificity) = range.specificity_for("application", "json") {
                    json.consider(specificity, range.quality);
                }
                if let Some(specificity) = range.specificity_for("text", "event-stream") {
                    sse.consider(specificity, range.quality);
                }
            }
        }

        let preferences = if saw_accept {
            Self {
                json_quality: json.quality(),
                sse_quality: sse.quality(),
            }
        } else {
            Self {
                json_quality: 1000,
                sse_quality: 1000,
            }
        };
        if preferences.json_quality == 0 && preferences.sse_quality == 0 {
            return Err(HttpError::NotAcceptable);
        }
        Ok(preferences)
    }

    /// Returns the highest-quality acceptable representation; JSON wins ties.
    #[must_use]
    pub const fn preferred(self) -> HttpResponseRepresentation {
        if self.json_quality >= self.sse_quality {
            HttpResponseRepresentation::Json
        } else {
            HttpResponseRepresentation::Sse
        }
    }

    /// Whether a required representation is acceptable, independently of which
    /// representation has the higher quality.
    #[must_use]
    pub const fn accepts(self, representation: HttpResponseRepresentation) -> bool {
        match representation {
            HttpResponseRepresentation::Json => self.json_quality > 0,
            HttpResponseRepresentation::Sse => self.sse_quality > 0,
        }
    }
}

#[derive(Default)]
struct AcceptPreference {
    matched: Option<(u8, u16)>,
}

impl AcceptPreference {
    fn consider(&mut self, specificity: u8, quality: u16) {
        self.matched = Some(match self.matched {
            Some((previous, weight)) if previous > specificity => (previous, weight),
            Some((previous, weight)) if previous == specificity => (previous, weight.min(quality)),
            _ => (specificity, quality),
        });
    }

    fn quality(&self) -> u16 {
        self.matched.map_or(0, |(_, quality)| quality)
    }
}

struct AcceptMediaRange<'a> {
    media_type: &'a str,
    subtype: &'a str,
    quality: u16,
    requires_parameters: bool,
}

impl<'a> AcceptMediaRange<'a> {
    fn parse(member: &'a str) -> Result<Self, ()> {
        let mut parts = AcceptQuotedParts::new(member, b';');
        let essence = parts.next().ok_or(())??.trim_matches([' ', '\t']);
        let (media_type, subtype) = essence.split_once('/').ok_or(())?;
        if !is_http_token(media_type)
            || !is_http_token(subtype)
            || (media_type == "*" && subtype != "*")
        {
            return Err(());
        }
        let mut quality = None;
        let mut requires_parameters = false;
        for (index, parameter) in parts.enumerate() {
            if index >= MAX_ACCEPT_PARAMETERS {
                return Err(());
            }
            let parameter = parameter?.trim_matches([' ', '\t']);
            // RFC 9110's parameters production permits empty parameter slots.
            if parameter.is_empty() {
                continue;
            }
            let (name, value) = parameter.split_once('=').ok_or(())?;
            if !is_http_token(name) || !is_accept_parameter_value(value) {
                return Err(());
            }
            if name.eq_ignore_ascii_case("q") {
                if quality.is_some() {
                    return Err(());
                }
                quality = Some(parse_accept_quality(value).ok_or(())?);
            } else {
                requires_parameters = true;
            }
        }
        Ok(Self {
            media_type,
            subtype,
            quality: quality.unwrap_or(1000),
            requires_parameters,
        })
    }

    fn specificity_for(&self, media_type: &str, subtype: &str) -> Option<u8> {
        if self.requires_parameters {
            return None;
        }
        if self.media_type == "*" && self.subtype == "*" {
            Some(0)
        } else if !self.media_type.eq_ignore_ascii_case(media_type) {
            None
        } else if self.subtype == "*" {
            Some(1)
        } else if self.subtype.eq_ignore_ascii_case(subtype) {
            Some(2)
        } else {
            None
        }
    }
}

/// Exact thousandths preserve RFC 9110's quality grammar without rounding.
fn parse_accept_quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !matches!(whole, "0" | "1")
        || fraction.len() > 3
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if whole == "1" {
        return fraction.bytes().all(|byte| byte == b'0').then_some(1000);
    }
    let mut quality = 0_u16;
    for byte in fraction.bytes() {
        quality = quality * 10 + u16::from(byte - b'0');
    }
    for _ in fraction.len()..3 {
        quality *= 10;
    }
    Some(quality)
}

fn is_accept_parameter_value(value: &str) -> bool {
    if is_http_token(value) {
        return true;
    }
    let Some(quoted) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return false;
    };
    let mut escaped = false;
    for byte in quoted.bytes() {
        if byte < b' ' && byte != b'\t' || byte == 0x7f {
            return false;
        }
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return false;
        }
    }
    !escaped
}

/// Splits on unquoted ASCII delimiters without allocation or UTF-8 slicing.
struct AcceptQuotedParts<'a> {
    rest: Option<&'a str>,
    separator: u8,
}

impl<'a> AcceptQuotedParts<'a> {
    fn new(value: &'a str, separator: u8) -> Self {
        Self {
            rest: Some(value),
            separator,
        }
    }
}

impl<'a> Iterator for AcceptQuotedParts<'a> {
    type Item = Result<&'a str, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.rest.take()?;
        let mut quoted = false;
        let mut escaped = false;
        for (index, byte) in value.bytes().enumerate() {
            if byte < b' ' && byte != b'\t' || byte == 0x7f {
                return Some(Err(()));
            }
            if escaped {
                escaped = false;
            } else if quoted && byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = !quoted;
            } else if !quoted && byte == self.separator {
                self.rest = Some(&value[index + 1..]);
                return Some(Ok(&value[..index]));
            }
        }
        Some(if quoted || escaped {
            Err(())
        } else {
            Ok(value)
        })
    }
}

// =============================================================================
// HTTP Request Handler
// =============================================================================

/// Configuration for the HTTP request handler.
#[derive(Debug, Clone)]
pub struct HttpHandlerConfig {
    /// Base path for MCP endpoints (e.g., "/mcp/v1").
    pub base_path: String,
    /// Whether to allow CORS requests.
    pub allow_cors: bool,
    /// Exact allowed CORS origins.
    ///
    /// Wildcards are deliberately unsupported because MCP requests can carry
    /// credentials. Each cross-origin deployment must name every trusted
    /// origin explicitly.
    pub cors_origins: Vec<String>,
    /// Maximum request body size in bytes.
    pub max_body_size: usize,
}

/// The response representation selected independently for one admitted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpResponseRepresentation {
    /// A finite `application/json` response body.
    Json,
    /// A finite `text/event-stream` response body bound to this request only.
    Sse,
}

/// A modern HTTP request admitted before authentication or application dispatch.
///
/// The response representation is immutable and request-local. In particular,
/// admitting this value does not create an SSE response body; callers must
/// explicitly bind one only after downstream dispatch is ready to own its
/// cancellation guard.
#[derive(Debug, Clone)]
pub struct ModernHttpRequestAdmission {
    request: JsonRpcRequest,
    response_representation: HttpResponseRepresentation,
}

impl ModernHttpRequestAdmission {
    /// Returns the bounded, strictly decoded JSON-RPC request for downstream dispatch.
    #[must_use]
    pub const fn request(&self) -> &JsonRpcRequest {
        &self.request
    }

    /// Returns this request's immutable response representation.
    #[must_use]
    pub const fn response_representation(&self) -> HttpResponseRepresentation {
        self.response_representation
    }

    /// Binds the selected SSE response body to this request's JSON-RPC ID.
    ///
    /// Calling this for a JSON-selected request or a notification is rejected
    /// before response-stream state is allocated. Dropping the returned body
    /// cancels its paired request guard and releases the registry entry.
    pub fn bind_sse_response_body(
        &self,
        responses: &StreamableHttpResponseStream,
    ) -> Result<StreamableHttpRequestResponseStream, TransportError> {
        if self.response_representation != HttpResponseRepresentation::Sse {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the admitted HTTP request selected a JSON response",
            )));
        }
        let request_id = self.request.id.clone().ok_or_else(|| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a JSON-RPC notification cannot own an SSE response body",
            ))
        })?;
        responses.for_request(request_id)
    }
}

impl Default for HttpHandlerConfig {
    fn default() -> Self {
        Self {
            base_path: "/mcp/v1".to_string(),
            allow_cors: false,
            cors_origins: Vec::new(),
            max_body_size: 10 * 1024 * 1024, // 10 MB
        }
    }
}

/// Handles HTTP requests containing MCP JSON-RPC messages.
///
/// This handler is designed to be integrated with any HTTP server framework.
/// It processes incoming HTTP requests, extracts JSON-RPC messages, and returns
/// appropriate HTTP responses.
///
/// [`HttpRequest`] already owns its body. Integrations must also enforce a
/// streaming body limit before constructing that value; this handler's size
/// check prevents parsing but cannot undo allocation performed upstream.
pub struct HttpRequestHandler {
    config: HttpHandlerConfig,
    codec: Codec,
}

impl HttpRequestHandler {
    /// Creates a new HTTP request handler with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(HttpHandlerConfig::default())
    }

    /// Creates a new HTTP request handler with the given configuration.
    #[must_use]
    pub fn with_config(config: HttpHandlerConfig) -> Self {
        let mut codec = Codec::new();
        codec.set_max_message_size(config.max_body_size);
        Self { config, codec }
    }

    /// Returns the handler configuration.
    #[must_use]
    pub fn config(&self) -> &HttpHandlerConfig {
        &self.config
    }

    /// Handles a CORS preflight OPTIONS request.
    #[must_use]
    pub fn handle_options(&self, request: &HttpRequest) -> HttpResponse {
        if validate_http_request_headers(request).is_err() {
            return HttpResponse::new(HttpStatus::BAD_REQUEST);
        }
        if request.path != self.config.base_path {
            return HttpResponse::new(HttpStatus::NOT_FOUND);
        }
        if request.method != HttpMethod::Options {
            return HttpResponse::new(HttpStatus::METHOD_NOT_ALLOWED);
        }
        if !self.config.allow_cors {
            return HttpResponse::new(HttpStatus::METHOD_NOT_ALLOWED);
        }

        let Some(origin) = request.header("origin") else {
            return HttpResponse::new(HttpStatus::FORBIDDEN);
        };
        if request.header("access-control-request-method") != Some("POST") {
            return HttpResponse::new(HttpStatus::FORBIDDEN);
        }
        let allowed = self.is_origin_allowed(origin);

        if !allowed {
            return HttpResponse::new(HttpStatus::FORBIDDEN);
        }

        HttpResponse::new(HttpStatus::OK)
            .with_cors(origin)
            .with_header("access-control-max-age", "86400")
    }

    /// Checks if the origin is allowed for CORS.
    #[must_use]
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        is_origin_allowed(&self.config, origin)
    }

    /// Parses a JSON-RPC request from an HTTP request.
    pub fn parse_request(&self, request: &HttpRequest) -> Result<JsonRpcRequest, HttpError> {
        validate_mcp_request_policy(request, &self.config)?;

        // HTTP framing already supplies one complete body. Route it through
        // the shared strict JSON admission boundary before typed decoding.
        Ok(self.codec.decode_complete_request(&request.body)?)
    }

    /// Admits one final MCP 2026-07-28 request before authentication or dispatch.
    ///
    /// This applies the fixed endpoint/method/media/bounds boundary, strict
    /// raw JSON-RPC object decoding (including batch rejection), PRT-03's
    /// header/body mirrors, and request-local response selection. It does not
    /// allocate a response body, authenticate, resolve a method, or mutate
    /// application state.
    pub fn admit_modern_request(
        &self,
        request: &HttpRequest,
    ) -> Result<ModernHttpRequestAdmission, HttpError> {
        validate_mcp_request_policy(request, &self.config)?;
        let json_rpc = self.codec.decode_complete_request(&request.body)?;
        admit_final_http_request(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: request.header("MCP-Protocol-Version"),
                body_version: body_protocol_version(&json_rpc),
            },
            header_method: request.header("Mcp-Method"),
            body_method: Some(&json_rpc.method),
            header_name: request.header("Mcp-Name"),
            body_name: body_mcp_name(&json_rpc),
        })
        .map_err(HttpError::ProtocolAdmission)?;
        let response_representation = response_representation(request)?;

        Ok(ModernHttpRequestAdmission {
            request: json_rpc,
            response_representation,
        })
    }

    /// Creates an HTTP response from a JSON-RPC response.
    ///
    /// Encoding failures are converted into a deterministic 500 response with
    /// a nonempty JSON error body. Use [`Self::try_create_response`] when the
    /// caller needs the typed codec error instead.
    #[must_use]
    pub fn create_response(
        &self,
        response: &JsonRpcResponse,
        origin: Option<&str>,
    ) -> HttpResponse {
        self.create_response_from_encoding(self.codec.encode_response(response), origin)
    }

    /// Tries to create an HTTP response from a JSON-RPC response.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CodecError`] when the JSON-RPC response cannot be
    /// encoded.
    pub fn try_create_response(
        &self,
        response: &JsonRpcResponse,
        origin: Option<&str>,
    ) -> Result<HttpResponse, HttpError> {
        self.try_create_response_from_encoding(self.codec.encode_response(response), origin)
    }

    fn create_response_from_encoding(
        &self,
        encoded: Result<Vec<u8>, CodecError>,
        origin: Option<&str>,
    ) -> HttpResponse {
        match self.try_create_response_from_encoding(encoded, origin) {
            Ok(response) => response,
            Err(_) => self.with_allowed_origin(
                HttpResponse::internal_error().with_body(JSON_ENCODING_ERROR_BODY),
                origin,
            ),
        }
    }

    fn try_create_response_from_encoding(
        &self,
        encoded: Result<Vec<u8>, CodecError>,
        origin: Option<&str>,
    ) -> Result<HttpResponse, HttpError> {
        let body = encoded?;

        let http_response = HttpResponse::ok()
            .with_body(body)
            .with_header("content-type", "application/json");

        Ok(self.with_allowed_origin(http_response, origin))
    }

    fn with_allowed_origin(
        &self,
        mut http_response: HttpResponse,
        origin: Option<&str>,
    ) -> HttpResponse {
        if self.config.allow_cors {
            if let Some(origin) = origin {
                if self.is_origin_allowed(origin) {
                    http_response = http_response.with_cors(origin);
                }
            }
        }

        http_response
    }

    /// Creates an error HTTP response.
    #[must_use]
    pub fn error_response(&self, status: HttpStatus, message: &str) -> HttpResponse {
        let error = serde_json::json!({
            "error": {
                "code": -32600,
                "message": message
            }
        });

        HttpResponse::new(status).with_json(&error)
    }
}

impl Default for HttpRequestHandler {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// HTTP Transport
// =============================================================================

/// HTTP transport for stateless MCP communication.
///
/// In stateless mode, each HTTP request contains a single JSON-RPC message
/// and receives a single response. This is suitable for simple integrations
/// where session state is not needed.
///
/// Context-aware receive checks cancellation/budget state before and after
/// every completed incremental read and again after parse/decode. The generic
/// `R: Read` boundary cannot preempt an underlying synchronous read that is
/// already blocked; callers requiring bounded cancellation while a peer is
/// silent must supply a readiness-aware or asynchronous host boundary.
pub struct HttpTransport<R, W> {
    reader: R,
    writer: W,
    codec: Codec,
    config: HttpHandlerConfig,
    closed: bool,
    /// Exact admitted Origin for the request awaiting its response.
    response_origin: Option<String>,
    /// Whether one admitted HTTP request is still awaiting its sole response.
    response_pending: bool,
    /// Whether the pending exchange's body failed JSON-RPC decoding. The
    /// server may still answer it with a correlated JSON-RPC error response;
    /// if it declines and receives again instead, the next receive completes
    /// the abandoned exchange with 400 Bad Request so the connection keeps
    /// serving (the HTTP framing consumed exactly that request's bytes).
    pending_body_rejected: bool,
}

fn read_retry_interrupted<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    cx: Option<&Cx>,
) -> Result<usize, HttpError> {
    loop {
        if let Some(cx) = cx {
            // Check again immediately before each potentially blocking read,
            // including reads separated by parsing or buffer growth.
            http_checkpoint(cx).map_err(HttpError::Transport)?;
        }
        match reader.read(buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                if let Some(cx) = cx {
                    http_checkpoint(cx).map_err(HttpError::Transport)?;
                }
            }
            Ok(read) => {
                if let Some(cx) = cx {
                    // Incremental parsing can span many successful reads. A
                    // single entry checkpoint is insufficient for a slow peer:
                    // recheck after every completed read before another read
                    // may block or newly consumed bytes can be admitted.
                    http_checkpoint(cx).map_err(HttpError::Transport)?;
                }
                return Ok(read);
            }
            Err(error) => return Err(HttpError::Transport(error.into())),
        }
    }
}

fn read_exact_retry_interrupted<R: Read>(
    reader: &mut R,
    mut buffer: &mut [u8],
    cx: Option<&Cx>,
) -> Result<(), HttpError> {
    while !buffer.is_empty() {
        let read = read_retry_interrupted(reader, buffer, cx)?;
        if read == 0 {
            return Err(HttpError::Transport(
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into(),
            ));
        }
        let (_, remaining) = buffer.split_at_mut(read);
        buffer = remaining;
    }
    Ok(())
}

impl<R: Read, W: Write> HttpTransport<R, W> {
    /// Creates a new HTTP transport.
    #[must_use]
    pub fn new(reader: R, writer: W) -> Self {
        Self::with_config(reader, writer, HttpHandlerConfig::default())
    }

    /// Creates an HTTP transport with an explicit endpoint and Origin policy.
    #[must_use]
    pub fn with_config(reader: R, writer: W, config: HttpHandlerConfig) -> Self {
        let mut codec = Codec::new();
        codec.set_max_message_size(config.max_body_size);
        Self {
            reader,
            writer,
            codec,
            config,
            closed: false,
            response_origin: None,
            response_pending: false,
            pending_body_rejected: false,
        }
    }

    /// Returns the transport's HTTP admission policy.
    #[must_use]
    pub fn config(&self) -> &HttpHandlerConfig {
        &self.config
    }

    /// Reads an HTTP request from the reader.
    ///
    /// Any error is terminal for this transport. The incremental parser may
    /// already have consumed a request line, headers, chunk metadata, or a body
    /// prefix, so the unread suffix can never be admitted as a new request.
    pub fn read_request(&mut self) -> Result<HttpRequest, HttpError> {
        self.read_request_with_context(None)
    }

    fn read_request_with_context(&mut self, cx: Option<&Cx>) -> Result<HttpRequest, HttpError> {
        if self.closed {
            return Err(HttpError::Closed);
        }

        let result = self.read_request_inner(cx);
        if result.is_err() {
            self.closed = true;
            self.response_pending = false;
            self.response_origin = None;
        }
        result
    }

    fn read_request_inner(&mut self, cx: Option<&Cx>) -> Result<HttpRequest, HttpError> {
        const MAX_HEADERS_SIZE: usize = 64 * 1024;
        let max_body_size = self.config.max_body_size;

        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];

        // Read headers until \r\n\r\n
        loop {
            if read_retry_interrupted(&mut self.reader, &mut byte, cx)? == 0 {
                return Err(HttpError::Closed);
            }
            buffer.push(byte[0]);

            if buffer.len() > MAX_HEADERS_SIZE {
                return Err(HttpError::HeadersTooLarge {
                    size: buffer.len(),
                    max: MAX_HEADERS_SIZE,
                });
            }
            if buffer.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        let header_str = std::str::from_utf8(&buffer)
            .map_err(|_| HttpError::InvalidHeader("headers are not valid UTF-8".to_string()))?;
        let header_block = header_str.strip_suffix("\r\n\r\n").ok_or_else(|| {
            HttpError::InvalidHeader("headers are not terminated by CRLF CRLF".to_string())
        })?;
        let mut lines = header_block.split("\r\n");

        // Parse request line
        let request_line = lines
            .next()
            .ok_or_else(|| HttpError::InvalidRequestLine("missing request line".to_string()))?;
        let mut request_parts = request_line.split(' ');
        let method_token = request_parts.next().unwrap_or("");
        let full_path = request_parts.next().unwrap_or("");
        let version = request_parts.next().unwrap_or("");
        if method_token.is_empty()
            || full_path.is_empty()
            || !full_path.bytes().all(|byte| byte.is_ascii_graphic())
            || version != "HTTP/1.1"
            || request_parts.next().is_some()
        {
            return Err(HttpError::InvalidRequestLine(request_line.to_string()));
        }

        let method = HttpMethod::parse(method_token)
            .filter(|method| method.as_str() == method_token)
            .ok_or_else(|| HttpError::InvalidMethod(method_token.to_string()))?;

        let (path, query_str) = full_path
            .split_once('?')
            .map_or((full_path.to_string(), None), |(p, q)| {
                (p.to_string(), Some(q))
            });

        let mut query = HashMap::new();
        if let Some(qs) = query_str {
            for pair in qs.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                query.insert(k.to_string(), v.to_string());
            }
        }

        // Parse headers
        let mut headers: HashMap<String, String> = HashMap::new();
        for line in lines {
            if line.is_empty() {
                return Err(HttpError::InvalidHeader(
                    "unexpected empty header line".to_string(),
                ));
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                return Err(HttpError::InvalidHeader(
                    "obsolete folded header line".to_string(),
                ));
            }
            let (name, raw_value) = line.split_once(':').ok_or_else(|| {
                HttpError::InvalidHeader("header is missing ':' separator".to_string())
            })?;
            if !is_http_token(name) {
                return Err(HttpError::InvalidHeader(format!(
                    "invalid header name: {name}"
                )));
            }
            let value = raw_value.trim_matches([' ', '\t']);
            if !is_valid_http_field_value(name, value) {
                return Err(HttpError::InvalidHeader(format!(
                    "invalid value for header {name}"
                )));
            }
            let normalized_name = name.to_ascii_lowercase();
            if normalized_name == "accept" {
                // `Accept` is a list-valued HTTP field. Preserve every field
                // line in wire order by joining with the list delimiter before
                // reducing it into `HttpRequest`'s one-value map. MCP binding
                // headers deliberately do not use this path: duplicated
                // singletons must remain an admission failure.
                if let Some(previous) = headers.get_mut(&normalized_name) {
                    previous.push(',');
                    previous.push_str(value);
                } else {
                    headers.insert(normalized_name, value.to_string());
                }
            } else if headers
                .insert(normalized_name.clone(), value.to_string())
                .is_some()
            {
                return Err(HttpError::InvalidHeader(format!(
                    "duplicate header: {normalized_name}"
                )));
            }
        }

        if headers.contains_key("content-length") && headers.contains_key("transfer-encoding") {
            return Err(HttpError::InvalidHeader(
                "content-length and transfer-encoding cannot be combined".to_string(),
            ));
        }

        // Read body.
        //
        // We support Content-Length or Transfer-Encoding: chunked. This is sufficient for MCP's
        // JSON-RPC-over-HTTP payloads and avoids pulling in a full HTTP server stack here.
        let mut body = Vec::new();

        if let Some(te) = headers.get("transfer-encoding") {
            if te.trim().eq_ignore_ascii_case("chunked") {
                // Chunked transfer encoding
                loop {
                    // Read chunk size line (hex), terminated by CRLF.
                    let mut line = Vec::new();
                    loop {
                        if read_retry_interrupted(&mut self.reader, &mut byte, cx)? == 0 {
                            return Err(HttpError::Closed);
                        }
                        line.push(byte[0]);
                        if line.len() > 1024 {
                            return Err(HttpError::InvalidHeader(
                                "invalid chunk size line".to_string(),
                            ));
                        }
                        if line.ends_with(b"\r\n") {
                            break;
                        }
                    }

                    let line_str = std::str::from_utf8(&line).map_err(|_| {
                        HttpError::InvalidHeader("chunk size is not valid UTF-8".to_string())
                    })?;
                    let size_str = line_str.strip_suffix("\r\n").ok_or_else(|| {
                        HttpError::InvalidHeader(
                            "chunk size line is not CRLF terminated".to_string(),
                        )
                    })?;
                    if size_str.contains(';') {
                        return Err(HttpError::InvalidHeader(
                            "chunk extensions are not supported".to_string(),
                        ));
                    }
                    let size = usize::from_str_radix(size_str, 16)
                        .map_err(|_| HttpError::InvalidHeader("invalid chunk size".to_string()))?;

                    if size == 0 {
                        // Consume the empty line that terminates a chunked body.
                        // Trailer fields are deliberately unsupported below.
                        let mut trailer = Vec::new();
                        loop {
                            if read_retry_interrupted(&mut self.reader, &mut byte, cx)? == 0 {
                                return Err(HttpError::Closed);
                            }
                            trailer.push(byte[0]);
                            if trailer.len() > MAX_HEADERS_SIZE {
                                return Err(HttpError::HeadersTooLarge {
                                    size: trailer.len(),
                                    max: MAX_HEADERS_SIZE,
                                });
                            }
                            if trailer.ends_with(b"\r\n") {
                                break;
                            }
                        }
                        if trailer != b"\r\n" {
                            return Err(HttpError::InvalidHeader(
                                "HTTP trailer fields are not supported".to_string(),
                            ));
                        }
                        break;
                    }

                    // Reject the declared aggregate length before allocating
                    // the chunk. Allocating `size` first would let an
                    // attacker trigger an OOM with only a large hexadecimal
                    // chunk header on the wire.
                    let body_start = body.len();
                    let projected_body_size = body_start.saturating_add(size);
                    if projected_body_size > max_body_size {
                        return Err(HttpError::BodyTooLarge {
                            size: projected_body_size,
                            max: max_body_size,
                        });
                    }
                    body.resize(projected_body_size, 0);
                    read_exact_retry_interrupted(&mut self.reader, &mut body[body_start..], cx)?;

                    // Consume trailing CRLF after the chunk.
                    let mut crlf = [0u8; 2];
                    read_exact_retry_interrupted(&mut self.reader, &mut crlf, cx)?;
                    if &crlf != b"\r\n" {
                        return Err(HttpError::InvalidHeader(
                            "invalid chunk terminator".to_string(),
                        ));
                    }
                }
            } else {
                return Err(HttpError::UnsupportedTransferEncoding(te.clone()));
            }
        } else {
            // Content-Length (if present)
            let content_length = match headers.get("content-length") {
                Some(value) => {
                    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(HttpError::InvalidHeader(
                            "invalid content-length".to_string(),
                        ));
                    }
                    value.parse::<usize>().map_err(|_| {
                        HttpError::InvalidHeader("content-length is out of range".to_string())
                    })?
                }
                None => 0,
            };

            if content_length > max_body_size {
                return Err(HttpError::BodyTooLarge {
                    size: content_length,
                    max: max_body_size,
                });
            }

            body.resize(content_length, 0);
            if content_length > 0 {
                read_exact_retry_interrupted(&mut self.reader, &mut body, cx)?;
            }
        }

        if let Some(cx) = cx {
            // Parsing and allocation follow the final read. Recheck once more
            // before exposing the fully consumed request to the transport.
            http_checkpoint(cx).map_err(HttpError::Transport)?;
        }

        Ok(HttpRequest {
            method,
            path,
            headers,
            body,
            query,
        })
    }

    /// Writes an HTTP response to the writer.
    pub fn write_response(&mut self, response: &HttpResponse) -> Result<(), HttpError> {
        // Validate the complete header set before emitting the status line so
        // a rejected field cannot leave a partial response on the wire.
        let mut normalized_names = HashSet::new();
        let mut has_content_length = false;
        for (name, value) in &response.headers {
            if !is_http_token(name) {
                return Err(HttpError::InvalidHeader(format!(
                    "invalid response header name: {name}"
                )));
            }
            if !is_valid_http_field_value(name, value) {
                return Err(HttpError::InvalidHeader(format!(
                    "invalid value for response header {name}"
                )));
            }
            let normalized_name = name.to_ascii_lowercase();
            if !normalized_names.insert(normalized_name.clone()) {
                return Err(HttpError::InvalidHeader(format!(
                    "duplicate response header: {normalized_name}"
                )));
            }
            if normalized_name == "transfer-encoding" {
                return Err(HttpError::InvalidHeader(
                    "HttpTransport does not encode transfer-encoding responses".to_string(),
                ));
            }
            if normalized_name == "content-length" {
                let length = value.parse::<usize>().map_err(|_| {
                    HttpError::InvalidHeader("invalid response content-length".to_string())
                })?;
                if length != response.body.len() {
                    return Err(HttpError::InvalidHeader(
                        "response content-length does not match body".to_string(),
                    ));
                }
                has_content_length = true;
            }
        }

        let status_text = match response.status.0 {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            406 => "Not Acceptable",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Unknown",
        };

        // Write status line
        write!(
            self.writer,
            "HTTP/1.1 {} {}\r\n",
            response.status.0, status_text
        )
        .map_err(|e| HttpError::Transport(e.into()))?;

        // Write headers
        for (name, value) in &response.headers {
            write!(self.writer, "{}: {}\r\n", name, value)
                .map_err(|e| HttpError::Transport(e.into()))?;
        }

        // Write content-length if not present
        if !has_content_length {
            write!(self.writer, "content-length: {}\r\n", response.body.len())
                .map_err(|e| HttpError::Transport(e.into()))?;
        }

        // End headers
        write!(self.writer, "\r\n").map_err(|e| HttpError::Transport(e.into()))?;

        // Write body
        self.writer
            .write_all(&response.body)
            .map_err(|e| HttpError::Transport(e.into()))?;
        self.writer
            .flush()
            .map_err(|e| HttpError::Transport(e.into()))?;

        Ok(())
    }
}

impl<R: Read, W: Write> Transport for HttpTransport<R, W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        http_checkpoint(cx)?;

        let response = match message {
            JsonRpcMessage::Response(r) => r.clone(),
            JsonRpcMessage::Request(r) => {
                // For HTTP transport, requests from server to client
                // are typically sent as notifications or SSE events.
                // This transport is request/response only and cannot deliver server-to-client
                // requests. Returning Ok() would silently drop messages and can deadlock
                // bidirectional protocols, so we fail explicitly.
                let _ = r;
                return Err(TransportError::Io(std::io::Error::other(
                    "HttpTransport cannot send server-to-client requests",
                )));
            }
        };

        if !self.response_pending {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no admitted HTTP request is awaiting a response",
            )));
        }

        let encoded = self.codec.encode_response(&response)?;
        let mut http_response = HttpResponse::ok()
            .with_body(encoded)
            .with_header("content-type", "application/json");
        if let Some(origin) = self.response_origin.as_deref() {
            http_response = http_response.with_cors(origin);
        }

        if self.write_response(&http_response).is_err() {
            // A partially written HTTP response cannot be retried safely on
            // the same byte stream.
            self.closed = true;
            return Err(TransportError::Io(std::io::Error::other("write error")));
        }
        self.response_pending = false;
        self.response_origin = None;
        self.pending_body_rejected = false;

        Ok(())
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        http_checkpoint(cx)?;
        if self.response_pending {
            if self.pending_body_rejected {
                // The previous exchange's body failed JSON-RPC decoding and
                // the server declined to answer it with a JSON-RPC error
                // response (era admission skips malformed opening frames).
                // The HTTP framing consumed exactly that request's bytes, so
                // the stream is still in sync: complete the abandoned
                // exchange with 400 Bad Request and keep the connection
                // serving subsequent exchanges.
                let mut rejection = HttpResponse::new(HttpStatus::BAD_REQUEST);
                if let Some(origin) = self.response_origin.as_deref() {
                    rejection = rejection.with_cors(origin);
                }
                if self.write_response(&rejection).is_err() {
                    self.closed = true;
                    self.response_pending = false;
                    self.response_origin = None;
                    self.pending_body_rejected = false;
                    return Err(TransportError::Io(std::io::Error::other("write error")));
                }
                self.response_pending = false;
                self.response_origin = None;
                self.pending_body_rejected = false;
            } else {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "previous HTTP request is still awaiting a response",
                )));
            }
        }

        let http_request = match self.read_request_with_context(Some(cx)) {
            Ok(request) => request,
            Err(error) => {
                // The incremental HTTP parser may already have consumed a
                // request-line, headers, chunk metadata, or body prefix. It is
                // never safe to treat the remaining suffix as a new request.
                self.closed = true;
                self.response_pending = false;
                self.response_origin = None;
                return Err(match error {
                    HttpError::Closed => TransportError::Closed,
                    HttpError::Timeout => TransportError::Timeout,
                    HttpError::Transport(error) => error,
                    _ => TransportError::Io(std::io::Error::other(error.to_string())),
                });
            }
        };
        validate_mcp_request_policy(&http_request, &self.config).map_err(|error| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            ))
        })?;
        self.response_origin = http_request.header("origin").map(ToOwned::to_owned);
        self.response_pending = true;

        // Parse JSON-RPC from the complete HTTP body through the same bounded
        // admission policy used by every other transport. A decode failure
        // keeps the response slot open: the server may answer this exchange
        // with a correlated JSON-RPC error (for example -32700 Parse error).
        // If it declines and receives again, the guard above completes the
        // exchange with 400 Bad Request instead of failing the connection.
        let json_rpc = match self.codec.decode_complete_request(&http_request.body) {
            Ok(json_rpc) => json_rpc,
            Err(error) => {
                self.pending_body_rejected = true;
                return Err(error.into());
            }
        };

        if let Err(error) = http_checkpoint(cx) {
            // The complete HTTP exchange has already left the byte stream. A
            // failed post-decode checkpoint is terminal rather than authority
            // to retry and silently skip that request.
            self.closed = true;
            self.response_pending = false;
            self.response_origin = None;
            return Err(error);
        }

        if json_rpc.is_notification() {
            // Streamable HTTP acknowledges notification POSTs at the HTTP
            // layer; JSON-RPC itself does not produce a response. Completing
            // that exchange here prevents the one-outstanding-request guard
            // from permanently blocking the next request.
            let mut accepted = HttpResponse::new(HttpStatus::ACCEPTED);
            if let Some(origin) = self.response_origin.as_deref() {
                accepted = accepted.with_cors(origin);
            }
            if self.write_response(&accepted).is_err() {
                self.closed = true;
                return Err(TransportError::Io(std::io::Error::other("write error")));
            }
            self.response_pending = false;
            self.response_origin = None;
        }

        Ok(JsonRpcMessage::Request(json_rpc))
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        self.response_pending = false;
        self.response_origin = None;
        self.pending_body_rejected = false;
        Ok(())
    }
}

// =============================================================================
// Streaming HTTP Transport
// =============================================================================

const DEFAULT_STREAMABLE_QUEUE_CAPACITY: usize = 64;
const MAX_STREAMABLE_QUEUE_CAPACITY: usize = 1_024;
const MAX_STREAMABLE_QUEUED_BYTES_PER_DIRECTION: usize = 16 * 1024 * 1024;

struct QueuedRequest {
    message: JsonRpcRequest,
    serialized_bytes: usize,
}

/// One JSON-RPC message emitted through a request-owned modern SSE body.
///
/// A body may carry any number of server-to-client notifications and exactly
/// one terminal response. Notifications retain the request body that owns
/// their delivery, so independent modern requests cannot observe, consume, or
/// cancel one another's outbound messages.
#[derive(Debug, Clone)]
pub enum StreamableHttpRequestResponseMessage {
    /// A server-to-client JSON-RPC notification sent before the terminal response.
    Notification(JsonRpcRequest),
    /// A server-to-client JSON-RPC reverse request sent before the terminal response.
    Request(JsonRpcRequest),
    /// The single terminal JSON-RPC response for the owning request.
    Response(JsonRpcResponse),
}

struct QueuedResponse {
    request_id: Option<RequestId>,
    message: StreamableHttpRequestResponseMessage,
    serialized_bytes: usize,
}

struct StreamableResponseMailbox {
    queue: VecDeque<QueuedResponse>,
    retained_bytes: usize,
}

struct StreamableAdmissionGuard<'a> {
    active: &'a AtomicUsize,
}

impl Drop for StreamableAdmissionGuard<'_> {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "streamable admission count underflow");
    }
}

fn begin_streamable_admission<'a>(
    open: &AtomicBool,
    active: &'a AtomicUsize,
) -> Result<StreamableAdmissionGuard<'a>, TransportError> {
    // These two atomics form one admission gate. Sequential consistency is
    // intentional: with only acquire/release ordering, close could observe the
    // old active count while an entrant observed the old open flag (the
    // store-buffering outcome), allowing an admission to outlive close.
    active.fetch_add(1, Ordering::SeqCst);
    if !open.load(Ordering::SeqCst) {
        let previous = active.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "streamable admission count underflow");
        return Err(TransportError::Closed);
    }
    Ok(StreamableAdmissionGuard { active })
}

fn close_streamable_admissions(open: &AtomicBool, active: &AtomicUsize) {
    open.store(false, Ordering::SeqCst);
    while active.load(Ordering::SeqCst) != 0 {
        std::thread::yield_now();
    }
}

impl StreamableResponseMailbox {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            retained_bytes: 0,
        }
    }
}

/// Cloneable, accounting-aware ingress for streamable HTTP requests.
///
/// This handle is intended for HTTP request-handler threads while the owning
/// [`StreamableHttpTransport`] is blocked in [`Transport::recv`]. Every clone
/// shares the transport's count and serialized-byte limits; the raw channel is
/// deliberately not exposed because sending through it would bypass those
/// admission checks.
pub struct StreamableHttpRequestIngress {
    codec: Codec,
    sender: mpsc::Sender<QueuedRequest>,
    retained_bytes: Arc<AtomicUsize>,
    max_queued_bytes: usize,
    endpoint_count: Arc<AtomicUsize>,
    admissions_open: Arc<AtomicBool>,
    active_admissions: Arc<AtomicUsize>,
}

impl Clone for StreamableHttpRequestIngress {
    fn clone(&self) -> Self {
        self.endpoint_count.fetch_add(1, Ordering::Relaxed);
        let mut codec = Codec::new();
        codec.set_max_message_size(self.codec.max_message_size());
        Self {
            codec,
            sender: self.sender.clone(),
            retained_bytes: Arc::clone(&self.retained_bytes),
            max_queued_bytes: self.max_queued_bytes,
            endpoint_count: Arc::clone(&self.endpoint_count),
            admissions_open: Arc::clone(&self.admissions_open),
            active_admissions: Arc::clone(&self.active_admissions),
        }
    }
}

impl Drop for StreamableHttpRequestIngress {
    fn drop(&mut self) {
        let previous = self.endpoint_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "request-ingress endpoint count underflow");
        if previous == 1 {
            close_streamable_admissions(&self.admissions_open, &self.active_admissions);
        }
    }
}

impl StreamableHttpRequestIngress {
    /// Admits one request to the transport's bounded request queue.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` when either the count or serialized-byte budget is
    /// full, and returns [`TransportError::Closed`] after transport shutdown.
    pub fn push_request(&self, cx: &Cx, request: JsonRpcRequest) -> Result<(), TransportError> {
        enqueue_streamable_request(
            &self.codec,
            &self.sender,
            &self.retained_bytes,
            self.max_queued_bytes,
            &self.admissions_open,
            &self.active_admissions,
            cx,
            request,
        )
    }

    /// Returns the hard request-count capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    /// Closes the shared request-admission endpoint.
    ///
    /// Dropping the final ingress handle has the same effect. Closing one clone
    /// is intentionally ingress-wide; all clones stop admitting requests while
    /// an independent response stream may still drain prior work. This is a
    /// synchronization barrier: it waits for bounded admissions that already
    /// entered the gate to commit or abort.
    pub fn close(&self) {
        close_streamable_admissions(&self.admissions_open, &self.active_admissions);
    }

    /// Returns whether request admission or the owning receiver has closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !self.admissions_open.load(Ordering::Acquire) || self.sender.is_closed()
    }
}

/// Cloneable, accounting-aware consumer for streamable HTTP outbound messages.
///
/// Every receive operation names its expected JSON-RPC request ID. Clones can
/// therefore wait for different unowned responses without consuming one
/// another's messages. Request-owned SSE bodies use their dedicated consumer
/// instead, which preserves notification and terminal-response ordering.
/// Dequeues release the corresponding count and serialized-byte reservations
/// exactly once.
pub struct StreamableHttpResponseStream {
    codec: Codec,
    mailbox: Arc<Mutex<StreamableResponseMailbox>>,
    request_states: Arc<Mutex<HashMap<RequestId, Arc<StreamableHttpRequestCancellationState>>>>,
    pending_count: Arc<AtomicUsize>,
    endpoint_count: Arc<AtomicUsize>,
    active_admissions: Arc<AtomicUsize>,
    capacity: usize,
    max_queued_bytes: usize,
    owner_open: Arc<AtomicBool>,
    admissions_open: Arc<AtomicBool>,
    poll_interval: Duration,
    #[cfg(test)]
    empty_polls: Arc<AtomicUsize>,
    #[cfg(test)]
    entered_empty_waits: Arc<AtomicUsize>,
}

impl Clone for StreamableHttpResponseStream {
    fn clone(&self) -> Self {
        self.endpoint_count.fetch_add(1, Ordering::Relaxed);
        Self {
            codec: response_stream_codec(&self.codec),
            mailbox: Arc::clone(&self.mailbox),
            request_states: Arc::clone(&self.request_states),
            pending_count: Arc::clone(&self.pending_count),
            endpoint_count: Arc::clone(&self.endpoint_count),
            active_admissions: Arc::clone(&self.active_admissions),
            capacity: self.capacity,
            max_queued_bytes: self.max_queued_bytes,
            owner_open: Arc::clone(&self.owner_open),
            admissions_open: Arc::clone(&self.admissions_open),
            poll_interval: self.poll_interval,
            #[cfg(test)]
            empty_polls: Arc::clone(&self.empty_polls),
            #[cfg(test)]
            entered_empty_waits: Arc::clone(&self.entered_empty_waits),
        }
    }
}

/// Builds the encoding-only codec held by an external response stream.
///
/// `Codec` deliberately does not implement `Clone`: its decode buffer is
/// mutable per direction. Response streams only encode, so they retain the
/// originating transport's exact message-size admission limit while starting
/// with an independent, empty decode buffer.
fn response_stream_codec(codec: &Codec) -> Codec {
    let mut response_codec = Codec::new();
    response_codec.set_max_message_size(codec.max_message_size());
    response_codec
}

impl Drop for StreamableHttpResponseStream {
    fn drop(&mut self) {
        let previous = self.endpoint_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "response-stream endpoint count underflow");
        if previous == 1 {
            self.terminate();
        }
    }
}

impl StreamableHttpResponseStream {
    /// Pops an unowned response for `request_id` without blocking.
    ///
    /// A null-ID JSON-RPC error can be selected with `None`. If another clone
    /// currently owns the mailbox lock, this returns `WouldBlock` rather than
    /// blocking the calling thread. A live request-owned SSE body must consume
    /// its own messages through [`StreamableHttpRequestResponseStream`].
    pub fn pop_response(
        &self,
        request_id: Option<&RequestId>,
    ) -> Result<Option<JsonRpcResponse>, TransportError> {
        if let Some(request_id) = request_id {
            if self.request_is_live(request_id)? {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "request-owned Streamable HTTP messages must use their bound SSE response body",
                )));
            }
        }
        let queued = self.pop_matching(|queued| {
            matches!(
                &queued.message,
                StreamableHttpRequestResponseMessage::Response(response)
                    if response.id.as_ref() == request_id
            )
        })?;
        Ok(queued.map(|queued| match queued.message {
            StreamableHttpRequestResponseMessage::Response(response) => response,
            StreamableHttpRequestResponseMessage::Notification(_)
            | StreamableHttpRequestResponseMessage::Request(_) => {
                unreachable!(
                    "the response matcher cannot dequeue a notification or reverse request"
                )
            }
        }))
    }

    fn pop_matching(
        &self,
        matches: impl Fn(&QueuedResponse) -> bool,
    ) -> Result<Option<QueuedResponse>, TransportError> {
        if let Some(message) =
            try_pop_streamable_message(&self.mailbox, &self.pending_count, &matches)?
        {
            return Ok(Some(message));
        }
        if !self.admissions_open.load(Ordering::Acquire)
            && self.active_admissions.load(Ordering::SeqCst) == 0
        {
            // An admitted producer can commit between the first empty mailbox
            // check and dropping its admission guard. Once the gate is closed
            // and the active count reaches zero, recheck under the mailbox
            // lock before declaring terminal closure.
            match try_pop_streamable_message(&self.mailbox, &self.pending_count, &matches)? {
                Some(message) => Ok(Some(message)),
                None => Err(TransportError::Closed),
            }
        } else {
            Ok(None)
        }
    }

    /// Waits for the response matching `request_id` while observing the full
    /// context checkpoint contract, including masking and budget exhaustion.
    pub fn recv_response(
        &self,
        cx: &Cx,
        request_id: Option<&RequestId>,
    ) -> Result<JsonRpcResponse, TransportError> {
        #[cfg(test)]
        let mut entered_empty_wait = false;
        loop {
            http_checkpoint(cx)?;
            match self.pop_response(request_id) {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(error) if is_would_block(&error) => {}
                Err(error) => return Err(error),
            }
            #[cfg(test)]
            {
                self.empty_polls.fetch_add(1, Ordering::Release);
                if !entered_empty_wait {
                    self.entered_empty_waits.fetch_add(1, Ordering::Release);
                    entered_empty_wait = true;
                }
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    fn pop_request_message(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<StreamableHttpRequestResponseMessage>, TransportError> {
        Ok(self
            .pop_matching(|queued| queued.request_id.as_ref() == Some(request_id))?
            .map(|queued| queued.message))
    }

    fn pop_request_response(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<JsonRpcResponse>, TransportError> {
        if let Some(response) =
            try_pop_streamable_request_response(&self.mailbox, &self.pending_count, request_id)?
        {
            return Ok(Some(response));
        }
        if !self.admissions_open.load(Ordering::Acquire)
            && self.active_admissions.load(Ordering::SeqCst) == 0
        {
            match try_pop_streamable_request_response(
                &self.mailbox,
                &self.pending_count,
                request_id,
            )? {
                Some(response) => Ok(Some(response)),
                None => Err(TransportError::Closed),
            }
        } else {
            Ok(None)
        }
    }

    fn request_is_live(&self, request_id: &RequestId) -> Result<bool, TransportError> {
        let request_states = match self.request_states.try_lock() {
            Ok(request_states) => request_states,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP request-response registry is busy",
                ));
            }
        };
        Ok(request_states.contains_key(request_id))
    }

    fn request_response_guard_is_active(
        &self,
        cancellation: &StreamableHttpRequestCancellation,
    ) -> Result<bool, TransportError> {
        let request_states = match self.request_states.try_lock() {
            Ok(request_states) => request_states,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP request-response registry is busy",
                ));
            }
        };
        Ok(request_states
            .get(cancellation.request_id())
            .is_some_and(|state| Arc::ptr_eq(state, &cancellation.state)))
    }

    fn enqueue_request_message(
        &self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        message: StreamableHttpRequestResponseMessage,
    ) -> Result<(), TransportError> {
        if !self.owner_open.load(Ordering::Acquire) || !self.admissions_open.load(Ordering::Acquire)
        {
            return Err(TransportError::Closed);
        }
        cancellation.checkpoint(cx)?;
        let _admission =
            begin_streamable_admission(&self.admissions_open, &self.active_admissions)?;
        let serialized_bytes = match &message {
            StreamableHttpRequestResponseMessage::Notification(notification)
            | StreamableHttpRequestResponseMessage::Request(notification) => {
                self.codec.encode_request(notification)?.len()
            }
            StreamableHttpRequestResponseMessage::Response(response) => {
                self.codec.encode_response(response)?.len()
            }
        };
        cancellation.checkpoint(cx)?;
        let mut mailbox = match self.mailbox.try_lock() {
            Ok(mailbox) => mailbox,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable response mailbox is busy",
                ));
            }
        };
        if !self.owner_open.load(Ordering::Acquire) || !self.admissions_open.load(Ordering::Acquire)
        {
            return Err(TransportError::Closed);
        }
        cancellation.checkpoint(cx)?;
        if mailbox.queue.len() >= self.capacity {
            return Err(streamable_queue_full_error(
                "streamable response queue is full",
            ));
        }
        let prospective = mailbox
            .retained_bytes
            .checked_add(serialized_bytes)
            .filter(|bytes| *bytes <= self.max_queued_bytes)
            .ok_or_else(|| {
                streamable_queue_full_error("streamable response byte budget is full")
            })?;
        mailbox.queue.push_back(QueuedResponse {
            request_id: Some(cancellation.request_id().clone()),
            message,
            serialized_bytes,
        });
        mailbox.retained_bytes = prospective;
        self.pending_count.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn send_response_for_request(
        &self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        response: JsonRpcResponse,
    ) -> Result<(), TransportError> {
        if response.id.as_ref() != Some(cancellation.request_id()) {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP response ID does not match its request response body",
            )));
        }
        cancellation.checkpoint(cx)?;
        if !self.request_response_guard_is_active(cancellation)? {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP response guard does not belong to this live transport request",
            )));
        }
        cancellation.with_message_commit(cx, true, || {
            self.enqueue_request_message(
                cx,
                cancellation,
                StreamableHttpRequestResponseMessage::Response(response),
            )
        })
    }

    fn send_notification_for_request(
        &self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        notification: JsonRpcRequest,
    ) -> Result<(), TransportError> {
        if !notification.is_notification() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP request-owned messages must be JSON-RPC notifications",
            )));
        }
        cancellation.checkpoint(cx)?;
        if !self.request_response_guard_is_active(cancellation)? {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP notification guard does not belong to this live transport request",
            )));
        }
        cancellation.with_message_commit(cx, false, || {
            self.enqueue_request_message(
                cx,
                cancellation,
                StreamableHttpRequestResponseMessage::Notification(notification),
            )
        })
    }

    fn send_request_for_request(
        &self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        request: JsonRpcRequest,
    ) -> Result<(), TransportError> {
        if request.is_notification() || request.id.is_none() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP reverse requests must carry a JSON-RPC ID",
            )));
        }
        cancellation.checkpoint(cx)?;
        if !self.request_response_guard_is_active(cancellation)? {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP reverse-request guard does not belong to this live transport request",
            )));
        }
        cancellation.with_message_commit(cx, false, || {
            self.enqueue_request_message(
                cx,
                cancellation,
                StreamableHttpRequestResponseMessage::Request(request),
            )
        })
    }

    fn discard_request_messages(&self, request_id: &RequestId) {
        let mut mailbox = self
            .mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut discarded_count = 0;
        let mut discarded_bytes = 0;
        mailbox.queue.retain(|queued| {
            if queued.request_id.as_ref() == Some(request_id) {
                discarded_count += 1;
                discarded_bytes += queued.serialized_bytes;
                false
            } else {
                true
            }
        });
        if discarded_count != 0 {
            debug_assert!(mailbox.retained_bytes >= discarded_bytes);
            mailbox.retained_bytes = mailbox.retained_bytes.saturating_sub(discarded_bytes);
            let previous = self
                .pending_count
                .fetch_sub(discarded_count, Ordering::AcqRel);
            debug_assert!(
                previous >= discarded_count,
                "response pending-count underflow while cancelling a request body"
            );
        }
    }

    /// Returns whether at least one outbound message is awaiting consumption.
    #[must_use]
    pub fn has_responses(&self) -> bool {
        self.pending_count.load(Ordering::Acquire) > 0
    }

    /// Returns the number of outbound messages awaiting consumption.
    #[must_use]
    pub fn pending_responses(&self) -> usize {
        self.pending_count.load(Ordering::Acquire)
    }

    /// Returns the number of request-owned response bodies that remain live.
    ///
    /// This count excludes queued messages: a body remains live while it can
    /// still accept notifications and its one terminal response, or be
    /// cancelled by its HTTP response teardown.
    pub fn live_request_bodies(&self) -> Result<usize, TransportError> {
        let request_states = match self.request_states.try_lock() {
            Ok(request_states) => request_states,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP request-response registry is busy",
                ));
            }
        };
        Ok(request_states.len())
    }

    /// Returns the hard response-count capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Closes the shared response-admission endpoint.
    ///
    /// Already-admitted outbound messages remain available to their matching
    /// consumers. Closing any clone seals production for every clone. Dropping
    /// the final response-stream handle additionally discards responses that
    /// can no longer have a consumer. This is a synchronization barrier for
    /// bounded response admissions already inside the gate.
    pub fn close(&self) {
        close_streamable_admissions(&self.admissions_open, &self.active_admissions);
    }

    fn terminate(&self) {
        close_streamable_admissions(&self.admissions_open, &self.active_admissions);

        let request_states = {
            let mut request_states = self
                .request_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *request_states)
        };
        for state in request_states.into_values() {
            StreamableHttpRequestCancellation { state }.cancel();
        }

        let mut mailbox = self
            .mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mailbox.queue.clear();
        mailbox.retained_bytes = 0;
        self.pending_count.store(0, Ordering::Release);
    }

    /// Returns whether the owner or shared response producer has closed.
    ///
    /// Matching responses admitted before closure remain drainable.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        !self.owner_open.load(Ordering::Acquire) || !self.admissions_open.load(Ordering::Acquire)
    }

    /// Binds an SSE response consumer to one request's outbound messages.
    ///
    /// The returned stream carries the request ID internally, so an HTTP
    /// response body cannot accidentally consume another in-flight request's
    /// notifications or terminal response. Dropping it requests cancellation
    /// through the paired guard; request handlers retain that guard and
    /// checkpoint it before committing further work or writes.
    pub fn for_request(
        &self,
        request_id: RequestId,
    ) -> Result<StreamableHttpRequestResponseStream, TransportError> {
        // Registering a request body is itself a response-side admission. It
        // must share the close barrier with response commits: otherwise a
        // listener shutdown could leave a newly registered body waiting on a
        // stream that can no longer produce its terminal response.
        let _admission =
            begin_streamable_admission(&self.admissions_open, &self.active_admissions)?;
        if !self.owner_open.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }

        let state = Arc::new(StreamableHttpRequestCancellationState {
            request_id: request_id.clone(),
            cancelled: AtomicBool::new(false),
            terminal_committed: AtomicBool::new(false),
            response_commit_gate: Mutex::new(()),
            request_cancellation: McpRequestCancellation::new(),
        });
        let mut request_states = match self.request_states.try_lock() {
            Ok(request_states) => request_states,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP request-response registry is busy",
                ));
            }
        };
        if request_states.contains_key(&request_id) {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "streamable HTTP request already has a live response body",
            )));
        }
        let mailbox = match self.mailbox.try_lock() {
            Ok(mailbox) => mailbox,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP response mailbox is busy",
                ));
            }
        };
        if mailbox
            .queue
            .iter()
            .any(|queued| queued.request_id.as_ref() == Some(&request_id))
        {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "streamable HTTP response is already queued for this request ID",
            )));
        }
        drop(mailbox);
        request_states.insert(request_id.clone(), Arc::clone(&state));
        Ok(StreamableHttpRequestResponseStream {
            responses: self.clone(),
            request_id,
            request_states: Arc::clone(&self.request_states),
            cancellation: StreamableHttpRequestCancellation { state },
            finished: AtomicBool::new(false),
        })
    }
}

/// Request-owned cancellation guard paired with a response stream.
///
/// A handler keeps a clone of this guard while it performs request work. A
/// peer disconnect drops the response body, which marks the guard cancelled;
/// the handler then observes [`TransportError::Cancelled`] at its next
/// explicit checkpoint before it can commit another response-side effect.
#[derive(Clone, Debug)]
pub struct StreamableHttpRequestCancellation {
    state: Arc<StreamableHttpRequestCancellationState>,
}

#[derive(Debug)]
struct StreamableHttpRequestCancellationState {
    request_id: RequestId,
    cancelled: AtomicBool,
    terminal_committed: AtomicBool,
    response_commit_gate: Mutex<()>,
    request_cancellation: McpRequestCancellation,
}

impl StreamableHttpRequestCancellation {
    fn cancel(&self) {
        let _commit_gate = self
            .state
            .response_commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state.cancelled.store(true, Ordering::Release);
        self.state.request_cancellation.cancel();
    }

    fn with_message_commit<T>(
        &self,
        cx: &Cx,
        terminal: bool,
        commit: impl FnOnce() -> Result<T, TransportError>,
    ) -> Result<T, TransportError> {
        self.checkpoint(cx)?;
        if self.is_terminal_committed() {
            return Err(TransportError::Closed);
        }
        let _commit_gate = match self.state.response_commit_gate.try_lock() {
            Ok(commit_gate) => commit_gate,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable request response commit is busy",
                ));
            }
        };
        self.checkpoint(cx)?;
        if self.is_terminal_committed() {
            return Err(TransportError::Closed);
        }
        let committed = commit()?;
        if terminal {
            self.state.terminal_committed.store(true, Ordering::Release);
        }
        Ok(committed)
    }

    /// Returns whether the request response body has been dropped or finished.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Returns whether a terminal response has already been committed.
    #[must_use]
    pub fn is_terminal_committed(&self) -> bool {
        self.state.terminal_committed.load(Ordering::Acquire)
    }

    /// Returns the only JSON-RPC response ID this request guard may commit.
    #[must_use]
    pub fn request_id(&self) -> &RequestId {
        &self.state.request_id
    }

    /// Returns the server request-cancellation domain paired with this HTTP
    /// response body. Dropping the body cancels this domain before it can
    /// admit a later handler effect.
    #[must_use]
    pub fn request_cancellation(&self) -> McpRequestCancellation {
        self.state.request_cancellation.clone()
    }

    /// Observes caller cancellation and the request response body's lifetime.
    ///
    /// Callers must checkpoint again after any independently cancellable work
    /// and immediately before committing a response-side effect.
    pub fn checkpoint(&self, cx: &Cx) -> Result<(), TransportError> {
        http_checkpoint(cx)?;
        if self.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        Ok(())
    }
}

/// Per-request consumer for one Streamable HTTP response body.
///
/// This abstraction owns the response's correlation ID and its cancellation
/// guard. It intentionally has no `Clone` implementation: one dropped HTTP
/// response body has one cancellation decision for its request, while handlers
/// can clone only the accompanying [`StreamableHttpRequestCancellation`] guard.
pub struct StreamableHttpRequestResponseStream {
    responses: StreamableHttpResponseStream,
    request_id: RequestId,
    request_states: Arc<Mutex<HashMap<RequestId, Arc<StreamableHttpRequestCancellationState>>>>,
    cancellation: StreamableHttpRequestCancellation,
    finished: AtomicBool,
}

/// Cloneable producer for one request-owned Streamable HTTP response body.
///
/// Producers can emit ordered notifications and the one terminal response
/// without owning the body itself. Once the body is dropped, the paired
/// cancellation state rejects every later producer effect.
#[derive(Clone)]
pub struct StreamableHttpRequestResponseSender {
    responses: StreamableHttpResponseStream,
    cancellation: StreamableHttpRequestCancellation,
}

impl Drop for StreamableHttpRequestResponseStream {
    fn drop(&mut self) {
        self.finish();
    }
}

impl StreamableHttpRequestResponseStream {
    fn finish(&self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.cancellation.cancel();
            let mut request_states = self
                .request_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if request_states
                .get(&self.request_id)
                .is_some_and(|state| Arc::ptr_eq(state, &self.cancellation.state))
            {
                request_states.remove(&self.request_id);
            }
            drop(request_states);
            self.responses.discard_request_messages(&self.request_id);
        }
    }

    /// Returns a cancellation guard for the request handler that owns this
    /// response body.
    #[must_use]
    pub fn cancellation(&self) -> StreamableHttpRequestCancellation {
        self.cancellation.clone()
    }

    /// Returns a producer for this exact request body.
    ///
    /// The producer preserves the body's ordering and request ownership while
    /// allowing request work to run independently from socket streaming.
    #[must_use]
    pub fn sender(&self) -> StreamableHttpRequestResponseSender {
        StreamableHttpRequestResponseSender {
            responses: self.responses.clone(),
            cancellation: self.cancellation(),
        }
    }

    /// Returns the request ID bound to this response body.
    #[must_use]
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Returns whether this response body has reached its terminal state.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Pops the next notification or terminal response for this request body
    /// without blocking.
    ///
    /// Messages are emitted in commit order for this exact request. Receiving
    /// the terminal response finishes the body; subsequent calls fail closed.
    pub fn pop_message(
        &self,
    ) -> Result<Option<StreamableHttpRequestResponseMessage>, TransportError> {
        if self.is_finished() {
            return Err(TransportError::Closed);
        }
        if self.cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }

        let message = self.responses.pop_request_message(&self.request_id)?;
        if matches!(
            message,
            Some(StreamableHttpRequestResponseMessage::Response(_))
        ) {
            self.finish();
        }
        Ok(message)
    }

    /// Waits for the next notification or terminal response while observing
    /// both caller cancellation and this response body's lifetime.
    pub fn recv_message(
        &self,
        cx: &Cx,
    ) -> Result<StreamableHttpRequestResponseMessage, TransportError> {
        loop {
            self.cancellation.checkpoint(cx)?;
            match self.pop_message() {
                Ok(Some(message)) => return Ok(message),
                Ok(None) => {}
                Err(error) if is_would_block(&error) => {}
                Err(error) => return Err(error),
            }
            std::thread::sleep(self.responses.poll_interval);
        }
    }

    /// Pops the bound request's final response without blocking.
    ///
    /// A pending notification must be consumed through [`Self::pop_message`]
    /// before this compatibility method can receive the terminal response.
    /// A completed response is terminal for this body. Subsequent calls fail
    /// closed rather than allowing a second final response for the request.
    pub fn pop_response(&self) -> Result<Option<JsonRpcResponse>, TransportError> {
        if self.is_finished() {
            return Err(TransportError::Closed);
        }
        if self.cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }

        let response = self.responses.pop_request_response(&self.request_id)?;
        if response.is_some() {
            self.finish();
        }
        Ok(response)
    }

    /// Waits for the bound request's final response while observing both the
    /// caller context and response-body cancellation.
    pub fn recv_response(&self, cx: &Cx) -> Result<JsonRpcResponse, TransportError> {
        loop {
            self.cancellation.checkpoint(cx)?;
            match self.pop_response() {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(error) if is_would_block(&error) => {}
                Err(error) => return Err(error),
            }
            std::thread::sleep(self.responses.poll_interval);
        }
    }
}

impl StreamableHttpRequestResponseSender {
    /// Returns the request cancellation domain paired with this producer.
    #[must_use]
    pub fn request_cancellation(&self) -> McpRequestCancellation {
        self.cancellation.request_cancellation()
    }

    /// Commits one notification before this body's terminal response.
    pub fn send_notification(
        &self,
        cx: &Cx,
        notification: JsonRpcRequest,
    ) -> Result<(), TransportError> {
        self.responses
            .send_notification_for_request(cx, &self.cancellation, notification)
    }

    /// Commits one reverse request before this body's terminal response.
    pub fn send_request(&self, cx: &Cx, request: JsonRpcRequest) -> Result<(), TransportError> {
        self.responses
            .send_request_for_request(cx, &self.cancellation, request)
    }

    /// Commits this body's one terminal response.
    pub fn send_response(&self, cx: &Cx, response: JsonRpcResponse) -> Result<(), TransportError> {
        self.responses
            .send_response_for_request(cx, &self.cancellation, response)
    }
}

fn try_pop_streamable_message(
    mailbox: &Mutex<StreamableResponseMailbox>,
    pending_count: &AtomicUsize,
    matches: impl Fn(&QueuedResponse) -> bool,
) -> Result<Option<QueuedResponse>, TransportError> {
    let mut mailbox = match mailbox.try_lock() {
        Ok(mailbox) => mailbox,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            return Err(streamable_queue_full_error(
                "streamable response mailbox is busy",
            ));
        }
    };
    let Some(position) = mailbox.queue.iter().position(matches) else {
        return Ok(None);
    };
    let response = mailbox
        .queue
        .remove(position)
        .expect("response position was obtained from the same mailbox");
    debug_assert!(mailbox.retained_bytes >= response.serialized_bytes);
    mailbox.retained_bytes = mailbox
        .retained_bytes
        .saturating_sub(response.serialized_bytes);
    let previous = pending_count.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "response pending-count underflow");
    Ok(Some(response))
}

fn try_pop_streamable_response(
    mailbox: &Mutex<StreamableResponseMailbox>,
    pending_count: &AtomicUsize,
    matches: impl Fn(&JsonRpcResponse) -> bool,
) -> Result<Option<JsonRpcResponse>, TransportError> {
    let queued = try_pop_streamable_message(mailbox, pending_count, |queued| {
        matches!(
            &queued.message,
            StreamableHttpRequestResponseMessage::Response(response) if matches(response)
        )
    })?;
    Ok(queued.map(|queued| match queued.message {
        StreamableHttpRequestResponseMessage::Response(response) => response,
        StreamableHttpRequestResponseMessage::Notification(_)
        | StreamableHttpRequestResponseMessage::Request(_) => {
            unreachable!("the response matcher cannot dequeue a notification or reverse request")
        }
    }))
}

fn try_pop_streamable_request_response(
    mailbox: &Mutex<StreamableResponseMailbox>,
    pending_count: &AtomicUsize,
    request_id: &RequestId,
) -> Result<Option<JsonRpcResponse>, TransportError> {
    let mut mailbox = match mailbox.try_lock() {
        Ok(mailbox) => mailbox,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            return Err(streamable_queue_full_error(
                "streamable response mailbox is busy",
            ));
        }
    };
    let Some(position) = mailbox
        .queue
        .iter()
        .position(|queued| queued.request_id.as_ref() == Some(request_id))
    else {
        return Ok(None);
    };
    if matches!(
        &mailbox.queue[position].message,
        StreamableHttpRequestResponseMessage::Notification(_)
            | StreamableHttpRequestResponseMessage::Request(_)
    ) {
        return Err(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a request-owned notification or reverse request must be consumed before its terminal response",
        )));
    }
    let queued = mailbox
        .queue
        .remove(position)
        .expect("response position was obtained from the same mailbox");
    debug_assert!(mailbox.retained_bytes >= queued.serialized_bytes);
    mailbox.retained_bytes = mailbox
        .retained_bytes
        .saturating_sub(queued.serialized_bytes);
    let previous = pending_count.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "response pending-count underflow");
    match queued.message {
        StreamableHttpRequestResponseMessage::Response(response) => Ok(Some(response)),
        StreamableHttpRequestResponseMessage::Notification(_)
        | StreamableHttpRequestResponseMessage::Request(_) => {
            unreachable!("notification and reverse-request messages return before they are removed")
        }
    }
}

fn release_streamable_bytes(retained_bytes: &AtomicUsize, serialized_bytes: usize) {
    let _ = retained_bytes.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(serialized_bytes))
    });
}

fn http_checkpoint(cx: &Cx) -> Result<(), TransportError> {
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

fn is_would_block(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Io(error) if error.kind() == std::io::ErrorKind::WouldBlock
    )
}

fn streamable_queue_full_error(message: &'static str) -> TransportError {
    TransportError::Io(std::io::Error::new(std::io::ErrorKind::WouldBlock, message))
}

fn map_streamable_send_error<T>(
    error: mpsc::SendError<T>,
    full_message: &'static str,
) -> TransportError {
    match error {
        mpsc::SendError::Disconnected(_) => TransportError::Closed,
        mpsc::SendError::Cancelled(_) => TransportError::Cancelled,
        mpsc::SendError::Full(_) => TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            full_message,
        )),
    }
}

fn reserve_streamable_bytes(
    retained_bytes: &AtomicUsize,
    max_queued_bytes: usize,
    serialized_bytes: usize,
    admissions_open: &AtomicBool,
    cx: &Cx,
    full_message: &'static str,
) -> Result<(), TransportError> {
    let mut current = retained_bytes.load(Ordering::Acquire);
    loop {
        if !admissions_open.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        let prospective = current
            .checked_add(serialized_bytes)
            .filter(|bytes| *bytes <= max_queued_bytes)
            .ok_or_else(|| streamable_queue_full_error(full_message))?;
        match retained_bytes.compare_exchange_weak(
            current,
            prospective,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(observed) => {
                current = observed;
                http_checkpoint(cx)?;
            }
        }
    }
}

fn enqueue_streamable_request(
    codec: &Codec,
    sender: &mpsc::Sender<QueuedRequest>,
    retained_bytes: &AtomicUsize,
    max_queued_bytes: usize,
    admissions_open: &AtomicBool,
    active_admissions: &AtomicUsize,
    cx: &Cx,
    request: JsonRpcRequest,
) -> Result<(), TransportError> {
    if !admissions_open.load(Ordering::Acquire) {
        return Err(TransportError::Closed);
    }
    http_checkpoint(cx)?;
    let _admission = begin_streamable_admission(admissions_open, active_admissions)?;

    let serialized_bytes = codec.encode_request(&request)?.len();
    // Encoding is bounded but can still be substantial. Re-check after that
    // CPU work so a deadline or cancellation raised during encoding wins
    // before the request acquires queue reservations.
    http_checkpoint(cx)?;
    reserve_streamable_bytes(
        retained_bytes,
        max_queued_bytes,
        serialized_bytes,
        admissions_open,
        cx,
        "streamable request byte budget is full",
    )?;
    if let Err(error) = sender.try_send(QueuedRequest {
        message: request,
        serialized_bytes,
    }) {
        release_streamable_bytes(retained_bytes, serialized_bytes);
        let disconnected = matches!(&error, mpsc::SendError::Disconnected(_));
        let error = map_streamable_send_error(error, "streamable request queue is full");
        if disconnected {
            admissions_open.store(false, Ordering::Release);
        }
        return Err(error);
    }
    Ok(())
}

/// Streaming HTTP transport for long-lived MCP connections.
///
/// This transport uses HTTP streaming (chunked transfer encoding) for
/// server-to-client messages and regular POST requests for client-to-server
/// messages.
pub struct StreamableHttpTransport {
    /// Shared typed-message validation and serialized-size boundary.
    codec: Codec,
    /// Bounded request channel (from HTTP POST requests).
    request_sender: Option<mpsc::Sender<QueuedRequest>>,
    request_receiver: mpsc::Receiver<QueuedRequest>,
    request_retained_bytes: Arc<AtomicUsize>,
    request_endpoint_count: Arc<AtomicUsize>,
    request_admissions_open: Arc<AtomicBool>,
    request_active_admissions: Arc<AtomicUsize>,
    /// Bounded, correlation-aware response mailbox.
    response_mailbox: Arc<Mutex<StreamableResponseMailbox>>,
    /// Live request-owned response bodies keyed by their JSON-RPC request ID.
    request_response_states:
        Arc<Mutex<HashMap<RequestId, Arc<StreamableHttpRequestCancellationState>>>>,
    response_pending_count: Arc<AtomicUsize>,
    response_endpoint_count: Arc<AtomicUsize>,
    response_active_admissions: Arc<AtomicUsize>,
    response_admissions_open: Arc<AtomicBool>,
    response_externalized: bool,
    capacity: usize,
    max_queued_bytes_per_direction: usize,
    /// Whether the transport owner remains open.
    owner_open: Arc<AtomicBool>,
    /// Poll interval for checking new messages.
    poll_interval: Duration,
    #[cfg(test)]
    request_empty_polls: Arc<AtomicUsize>,
    #[cfg(test)]
    response_empty_polls: Arc<AtomicUsize>,
    #[cfg(test)]
    response_entered_empty_waits: Arc<AtomicUsize>,
}

impl StreamableHttpTransport {
    /// Creates a new streaming HTTP transport.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_STREAMABLE_QUEUE_CAPACITY)
            .expect("the built-in streamable HTTP queue capacity must be valid")
    }

    /// Creates a streaming HTTP transport with bounded queues in both directions.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when `capacity` is zero or exceeds the
    /// hard queue-count limit.
    pub fn with_capacity(capacity: usize) -> Result<Self, TransportError> {
        Self::with_queue_limits(capacity, MAX_STREAMABLE_QUEUED_BYTES_PER_DIRECTION)
    }

    /// Creates bounded queues using an endpoint's configured message-size limit.
    ///
    /// The limit is captured by both handles before they are externalized. Each
    /// direction retains its existing byte budget or enough for one maximum-size
    /// message plus its NDJSON newline, whichever is larger. Queue-count and
    /// serialized-byte accounting remain enforced.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for an invalid capacity, a zero message
    /// limit, or a message limit that leaves no room for its framing byte.
    pub fn with_capacity_and_max_message_size(
        capacity: usize,
        max_message_size: usize,
    ) -> Result<Self, TransportError> {
        let framed_message_size = max_message_size
            .checked_add(1)
            .filter(|_| max_message_size != 0)
            .ok_or_else(|| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "streamable HTTP message limit must be nonzero and leave room for framing",
                ))
            })?;
        let mut transport = Self::with_capacity(capacity)?;
        transport.codec.set_max_message_size(max_message_size);
        transport.max_queued_bytes_per_direction =
            MAX_STREAMABLE_QUEUED_BYTES_PER_DIRECTION.max(framed_message_size);
        Ok(transport)
    }

    fn with_queue_limits(
        capacity: usize,
        max_queued_bytes_per_direction: usize,
    ) -> Result<Self, TransportError> {
        if capacity == 0 || capacity > MAX_STREAMABLE_QUEUE_CAPACITY {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP queue capacity is outside the supported range",
            )));
        }
        if max_queued_bytes_per_direction == 0
            || max_queued_bytes_per_direction > MAX_STREAMABLE_QUEUED_BYTES_PER_DIRECTION
        {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP queue byte limit is outside the supported range",
            )));
        }

        let (request_sender, request_receiver) = mpsc::channel(capacity);
        Ok(Self {
            codec: Codec::new(),
            request_sender: Some(request_sender),
            request_receiver,
            request_retained_bytes: Arc::new(AtomicUsize::new(0)),
            request_endpoint_count: Arc::new(AtomicUsize::new(0)),
            request_admissions_open: Arc::new(AtomicBool::new(true)),
            request_active_admissions: Arc::new(AtomicUsize::new(0)),
            response_mailbox: Arc::new(Mutex::new(StreamableResponseMailbox::new())),
            request_response_states: Arc::new(Mutex::new(HashMap::new())),
            response_pending_count: Arc::new(AtomicUsize::new(0)),
            response_endpoint_count: Arc::new(AtomicUsize::new(0)),
            response_active_admissions: Arc::new(AtomicUsize::new(0)),
            response_admissions_open: Arc::new(AtomicBool::new(true)),
            response_externalized: false,
            capacity,
            max_queued_bytes_per_direction,
            owner_open: Arc::new(AtomicBool::new(true)),
            poll_interval: Duration::from_millis(10),
            #[cfg(test)]
            request_empty_polls: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            response_empty_polls: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            response_entered_empty_waits: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Returns a cloneable request-ingress handle for HTTP handler threads.
    ///
    /// The handle captures the transport's current message-size limit and
    /// shares its queue count, byte accounting, and close state. Each admission
    /// observes cancellation through the supplied [`Cx`].
    ///
    /// # Errors
    ///
    /// Returns `AlreadyExists` after the request endpoint has already been
    /// externalized, or [`TransportError::Closed`] after owner shutdown.
    pub fn request_ingress(&mut self) -> Result<StreamableHttpRequestIngress, TransportError> {
        if !self.owner_open.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        let sender = self.request_sender.take().ok_or_else(|| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "streamable HTTP request ingress has already been externalized",
            ))
        })?;
        self.request_endpoint_count.store(1, Ordering::Release);
        let mut codec = Codec::new();
        codec.set_max_message_size(self.codec.max_message_size());
        Ok(StreamableHttpRequestIngress {
            codec,
            sender,
            retained_bytes: Arc::clone(&self.request_retained_bytes),
            max_queued_bytes: self.max_queued_bytes_per_direction,
            endpoint_count: Arc::clone(&self.request_endpoint_count),
            admissions_open: Arc::clone(&self.request_admissions_open),
            active_admissions: Arc::clone(&self.request_active_admissions),
        })
    }

    /// Returns a cloneable response consumer for HTTP streaming threads.
    ///
    /// Multiple clones may wait for different request IDs without consuming
    /// one another's responses.
    ///
    /// # Errors
    ///
    /// Returns `AlreadyExists` after the response endpoint has already been
    /// externalized, or [`TransportError::Closed`] after owner shutdown.
    pub fn response_stream(&mut self) -> Result<StreamableHttpResponseStream, TransportError> {
        if !self.owner_open.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        if self.response_externalized {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "streamable HTTP response stream has already been externalized",
            )));
        }
        self.response_externalized = true;
        self.response_endpoint_count.store(1, Ordering::Release);
        Ok(StreamableHttpResponseStream {
            codec: response_stream_codec(&self.codec),
            mailbox: Arc::clone(&self.response_mailbox),
            request_states: Arc::clone(&self.request_response_states),
            pending_count: Arc::clone(&self.response_pending_count),
            endpoint_count: Arc::clone(&self.response_endpoint_count),
            active_admissions: Arc::clone(&self.response_active_admissions),
            capacity: self.capacity(),
            max_queued_bytes: self.max_queued_bytes_per_direction,
            owner_open: Arc::clone(&self.owner_open),
            admissions_open: Arc::clone(&self.response_admissions_open),
            poll_interval: self.poll_interval,
            #[cfg(test)]
            empty_polls: Arc::clone(&self.response_empty_polls),
            #[cfg(test)]
            entered_empty_waits: Arc::clone(&self.response_entered_empty_waits),
        })
    }

    /// Returns the external producer/consumer handles used around a running
    /// transport.
    ///
    /// # Errors
    ///
    /// Returns `AlreadyExists` unless both endpoints remain owner-held, or
    /// [`TransportError::Closed`] after owner shutdown.
    pub fn split_handles(
        &mut self,
    ) -> Result<(StreamableHttpRequestIngress, StreamableHttpResponseStream), TransportError> {
        if self.request_sender.is_none() || self.response_externalized {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "streamable HTTP endpoints have already been externalized",
            )));
        }
        let ingress = self.request_ingress()?;
        let response_stream = self.response_stream()?;
        Ok((ingress, response_stream))
    }

    /// Pushes a request into the bounded queue (from an HTTP handler).
    ///
    /// The operation rejects overload with `WouldBlock`; it never grows the
    /// queue beyond the configured capacity.
    pub fn push_request(&self, cx: &Cx, request: JsonRpcRequest) -> Result<(), TransportError> {
        let sender = self.request_sender.as_ref().ok_or(TransportError::Closed)?;
        enqueue_streamable_request(
            &self.codec,
            sender,
            &self.request_retained_bytes,
            self.max_queued_bytes_per_direction,
            &self.request_admissions_open,
            &self.request_active_admissions,
            cx,
            request,
        )
    }

    /// Pops a response from the queue (for HTTP streaming).
    pub fn pop_response(&self) -> Result<Option<JsonRpcResponse>, TransportError> {
        if self.response_externalized {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the response consumer has been externalized",
            )));
        }
        if let Some(response) = try_pop_streamable_response(
            &self.response_mailbox,
            &self.response_pending_count,
            |_| true,
        )? {
            return Ok(Some(response));
        }
        if !self.response_admissions_open.load(Ordering::Acquire)
            && self.response_active_admissions.load(Ordering::SeqCst) == 0
        {
            // Stabilize the empty observation against a producer that
            // committed immediately before dropping the final admission.
            match try_pop_streamable_response(
                &self.response_mailbox,
                &self.response_pending_count,
                |_| true,
            )? {
                Some(response) => Ok(Some(response)),
                None => Err(TransportError::Closed),
            }
        } else {
            Ok(None)
        }
    }

    /// Checks if there are pending responses.
    #[must_use]
    pub fn has_responses(&self) -> bool {
        self.response_pending_count.load(Ordering::Acquire) > 0
    }

    /// Returns the number of admitted requests awaiting dispatch.
    #[must_use]
    pub fn pending_requests(&self) -> usize {
        self.request_receiver.len()
    }

    /// Returns the number of admitted responses awaiting streaming.
    #[must_use]
    pub fn pending_responses(&self) -> usize {
        self.response_pending_count.load(Ordering::Acquire)
    }

    /// Returns the hard queue capacity used in each direction.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn request_response_guard_is_active(
        &self,
        cancellation: &StreamableHttpRequestCancellation,
    ) -> Result<bool, TransportError> {
        let request_states = match self.request_response_states.try_lock() {
            Ok(request_states) => request_states,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP request-response registry is busy",
                ));
            }
        };
        Ok(request_states
            .get(cancellation.request_id())
            .is_some_and(|state| Arc::ptr_eq(state, &cancellation.state)))
    }

    fn response_is_bound_to_live_request(
        &self,
        response: &JsonRpcResponse,
    ) -> Result<bool, TransportError> {
        let Some(request_id) = response.id.as_ref() else {
            return Ok(false);
        };
        let request_states = match self.request_response_states.try_lock() {
            Ok(request_states) => request_states,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable HTTP request-response registry is busy",
                ));
            }
        };
        Ok(request_states.contains_key(request_id))
    }

    /// Commits a final response for one live request response body.
    ///
    /// The guard binds the response ID to its request and closes the request's
    /// response-commit gate before a dropped body returns. This prevents a
    /// disconnected request from committing a late response while retaining
    /// the transport's existing count and byte backpressure limits.
    pub fn send_response_for_request(
        &mut self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        response: JsonRpcResponse,
    ) -> Result<(), TransportError> {
        if response.id.as_ref() != Some(cancellation.request_id()) {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP response ID does not match its request response body",
            )));
        }
        cancellation.checkpoint(cx)?;
        if !self.request_response_guard_is_active(cancellation)? {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP response guard does not belong to this live transport request",
            )));
        }
        cancellation.with_message_commit(cx, true, || {
            self.enqueue_message(
                cx,
                Some(cancellation.request_id().clone()),
                StreamableHttpRequestResponseMessage::Response(response),
                Some(cancellation),
            )
        })
    }

    /// Commits one server-to-client notification for one live modern SSE body.
    ///
    /// The request cancellation guard identifies the only body that can emit
    /// this notification. The shared bounded outbound queue provides
    /// backpressure, while the request-local commit gate keeps notifications
    /// ordered before the body's one terminal response.
    pub fn send_notification_for_request(
        &mut self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        notification: JsonRpcRequest,
    ) -> Result<(), TransportError> {
        if !notification.is_notification() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP request-owned messages must be JSON-RPC notifications",
            )));
        }
        cancellation.checkpoint(cx)?;
        if !self.request_response_guard_is_active(cancellation)? {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP notification guard does not belong to this live transport request",
            )));
        }
        cancellation.with_message_commit(cx, false, || {
            self.enqueue_message(
                cx,
                Some(cancellation.request_id().clone()),
                StreamableHttpRequestResponseMessage::Notification(notification),
                Some(cancellation),
            )
        })
    }

    /// Commits one server-to-client reverse request for one live modern SSE body.
    pub fn send_request_for_request(
        &mut self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        request: JsonRpcRequest,
    ) -> Result<(), TransportError> {
        if request.is_notification() || request.id.is_none() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP reverse requests must carry a JSON-RPC ID",
            )));
        }
        cancellation.checkpoint(cx)?;
        if !self.request_response_guard_is_active(cancellation)? {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "streamable HTTP reverse-request guard does not belong to this live transport request",
            )));
        }
        cancellation.with_message_commit(cx, false, || {
            self.enqueue_message(
                cx,
                Some(cancellation.request_id().clone()),
                StreamableHttpRequestResponseMessage::Request(request),
                Some(cancellation),
            )
        })
    }

    fn enqueue_message(
        &mut self,
        cx: &Cx,
        request_id: Option<RequestId>,
        message: StreamableHttpRequestResponseMessage,
        request_cancellation: Option<&StreamableHttpRequestCancellation>,
    ) -> Result<(), TransportError> {
        if !self.owner_open.load(Ordering::Acquire)
            || !self.response_admissions_open.load(Ordering::Acquire)
        {
            return Err(TransportError::Closed);
        }
        http_checkpoint(cx)?;
        if let Some(cancellation) = request_cancellation {
            cancellation.checkpoint(cx)?;
        }
        // Validate before retaining a clone in the bounded queue. The codec
        // bounds each message's serialized size, while the retained-byte
        // counter bounds the aggregate queue footprint.
        let _admission = begin_streamable_admission(
            &self.response_admissions_open,
            &self.response_active_admissions,
        )?;
        let serialized_bytes = match &message {
            StreamableHttpRequestResponseMessage::Notification(notification)
            | StreamableHttpRequestResponseMessage::Request(notification) => {
                self.codec.encode_request(notification)?.len()
            }
            StreamableHttpRequestResponseMessage::Response(response) => {
                self.codec.encode_response(response)?.len()
            }
        };
        // Do not commit a response after cancellation or a deadline that
        // became observable during bounded serialization.
        http_checkpoint(cx)?;
        if let Some(cancellation) = request_cancellation {
            cancellation.checkpoint(cx)?;
        }
        let mut mailbox = match self.response_mailbox.try_lock() {
            Ok(mailbox) => mailbox,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(streamable_queue_full_error(
                    "streamable response mailbox is busy",
                ));
            }
        };
        if !self.owner_open.load(Ordering::Acquire)
            || !self.response_admissions_open.load(Ordering::Acquire)
        {
            return Err(TransportError::Closed);
        }
        if let Some(cancellation) = request_cancellation {
            cancellation.checkpoint(cx)?;
        }
        if mailbox.queue.len() >= self.capacity {
            return Err(streamable_queue_full_error(
                "streamable response queue is full",
            ));
        }
        let prospective = mailbox
            .retained_bytes
            .checked_add(serialized_bytes)
            .filter(|bytes| *bytes <= self.max_queued_bytes_per_direction)
            .ok_or_else(|| {
                streamable_queue_full_error("streamable response byte budget is full")
            })?;
        mailbox.queue.push_back(QueuedResponse {
            request_id,
            message,
            serialized_bytes,
        });
        mailbox.retained_bytes = prospective;
        self.response_pending_count.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn release_request_bytes(&self, serialized_bytes: usize) {
        release_streamable_bytes(&self.request_retained_bytes, serialized_bytes);
    }

    fn close_queues(&mut self) {
        self.owner_open.store(false, Ordering::Release);
        close_streamable_admissions(
            &self.request_admissions_open,
            &self.request_active_admissions,
        );
        close_streamable_admissions(
            &self.response_admissions_open,
            &self.response_active_admissions,
        );
        self.request_sender.take();
        self.request_receiver.close();
        while let Ok(request) = self.request_receiver.try_recv() {
            self.release_request_bytes(request.serialized_bytes);
        }
        // Outbound responses are deliberately retained. `Transport::close`
        // first settles bounded in-flight admissions and seals production;
        // extant response-stream handles can then drain the bounded mailbox.
    }
}

impl Drop for StreamableHttpTransport {
    fn drop(&mut self) {
        self.close_queues();
    }
}

impl Default for StreamableHttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for StreamableHttpTransport {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if !self.owner_open.load(Ordering::Acquire)
            || !self.response_admissions_open.load(Ordering::Acquire)
        {
            return Err(TransportError::Closed);
        }
        http_checkpoint(cx)?;

        match message {
            JsonRpcMessage::Response(response) => {
                if self.response_is_bound_to_live_request(response)? {
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "request-bound Streamable HTTP responses must use send_response_for_request",
                    )));
                }
                self.enqueue_message(
                    cx,
                    response.id.clone(),
                    StreamableHttpRequestResponseMessage::Response(response.clone()),
                    None,
                )?;
            }
            JsonRpcMessage::Request(_) => {
                // A server-to-client notification needs a request-owned modern
                // SSE body. The generic transport trait has no such owner, so
                // fail closed rather than routing it to another active stream.
                return Err(TransportError::Io(std::io::Error::other(
                    "StreamableHttpTransport requires a request-owned guard for server-to-client notifications",
                )));
            }
        }

        Ok(())
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        // Poll for requests
        loop {
            if !self.owner_open.load(Ordering::Acquire) {
                return Err(TransportError::Closed);
            }
            http_checkpoint(cx)?;

            match self.request_receiver.try_recv() {
                Ok(request) => {
                    self.release_request_bytes(request.serialized_bytes);
                    return Ok(JsonRpcMessage::Request(request.message));
                }
                Err(mpsc::RecvError::Empty) => {}
                Err(mpsc::RecvError::Disconnected) => {
                    self.request_admissions_open.store(false, Ordering::Release);
                    return Err(TransportError::Closed);
                }
                Err(mpsc::RecvError::Cancelled) => return Err(TransportError::Cancelled),
            }

            if !self.request_admissions_open.load(Ordering::Acquire)
                && self.request_active_admissions.load(Ordering::SeqCst) == 0
            {
                // A producer admitted before closure can enqueue between the
                // first empty observation and dropping its admission guard.
                // With the gate closed and no active producers, one final
                // receive makes the terminal decision stable.
                return match self.request_receiver.try_recv() {
                    Ok(request) => {
                        self.release_request_bytes(request.serialized_bytes);
                        Ok(JsonRpcMessage::Request(request.message))
                    }
                    Err(mpsc::RecvError::Empty | mpsc::RecvError::Disconnected) => {
                        Err(TransportError::Closed)
                    }
                    Err(mpsc::RecvError::Cancelled) => Err(TransportError::Cancelled),
                };
            }

            #[cfg(test)]
            self.request_empty_polls.fetch_add(1, Ordering::Release);

            // Sleep briefly before polling again
            std::thread::sleep(self.poll_interval);
        }
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.close_queues();
        Ok(())
    }
}

// =============================================================================
// Dual-era HTTP/SSE endpoint composition
// =============================================================================

/// Configuration for one endpoint that serves modern Streamable HTTP and
/// exact MCP 2024-11-05 SSE clients side by side.
#[cfg(feature = "legacy-2024-11-05")]
#[derive(Debug, Clone)]
pub struct DualEraHttpEndpointConfig {
    /// GET route for the legacy SSE event stream.
    pub legacy_sse_path: String,
    /// POST route advertised to the legacy SSE client by its first event.
    pub legacy_message_path: String,
    /// Plain-HTTP origin used to construct the opaque legacy POST URI.
    pub legacy_origin: String,
    /// Maximum legacy POST requests retained before application dispatch.
    pub legacy_request_capacity: usize,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpEndpointConfig {
    /// Creates a configuration with bounded defaults for the two legacy routes.
    #[must_use]
    pub fn new(
        legacy_sse_path: impl Into<String>,
        legacy_message_path: impl Into<String>,
        legacy_origin: impl Into<String>,
    ) -> Self {
        Self {
            legacy_sse_path: legacy_sse_path.into(),
            legacy_message_path: legacy_message_path.into(),
            legacy_origin: legacy_origin.into(),
            legacy_request_capacity: DEFAULT_STREAMABLE_QUEUE_CAPACITY,
        }
    }
}

/// Failure while constructing or operating a [`DualEraHttpEndpoint`].
#[cfg(feature = "legacy-2024-11-05")]
#[derive(Debug)]
pub enum DualEraHttpEndpointError {
    /// The endpoint configuration does not provide disjoint, valid routes.
    InvalidConfiguration(String),
    /// Modern HTTP admission failed.
    Http(HttpError),
    /// A bounded transport operation failed.
    Transport(TransportError),
    /// The session identifier could not be generated.
    Session(HttpSessionError),
    /// The session has been closed and can no longer admit work.
    Closed,
}

#[cfg(feature = "legacy-2024-11-05")]
impl std::fmt::Display for DualEraHttpEndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(
                    formatter,
                    "invalid dual-era HTTP endpoint configuration: {message}"
                )
            }
            Self::Http(error) => write!(formatter, "modern HTTP admission failed: {error}"),
            Self::Transport(error) => write!(formatter, "HTTP transport failed: {error}"),
            Self::Session(error) => write!(formatter, "HTTP session setup failed: {error}"),
            Self::Closed => formatter.write_str("dual-era HTTP session is closed"),
        }
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl std::error::Error for DualEraHttpEndpointError {}

#[cfg(feature = "legacy-2024-11-05")]
impl From<HttpError> for DualEraHttpEndpointError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl From<TransportError> for DualEraHttpEndpointError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl From<CodecError> for DualEraHttpEndpointError {
    fn from(error: CodecError) -> Self {
        Self::Transport(TransportError::Codec(error))
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl From<HttpSessionError> for DualEraHttpEndpointError {
    fn from(error: HttpSessionError) -> Self {
        Self::Session(error)
    }
}

/// A public endpoint composition for modern Streamable HTTP and legacy SSE.
///
/// The modern route is owned by the supplied [`HttpRequestHandler`]. The two
/// legacy routes remain method-disjoint: GET opens the SSE stream, while the
/// exact POST URI advertised in its first event is the only ingress for legacy
/// client requests. The legacy GET route may share the modern POST target.
#[cfg(feature = "legacy-2024-11-05")]
pub struct DualEraHttpEndpoint {
    handler: Arc<HttpRequestHandler>,
    config: DualEraHttpEndpointConfig,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpEndpoint {
    /// Validates a dual-era endpoint configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when any route is malformed or the legacy POST route
    /// overlaps a forbidden target, when the legacy origin cannot be used by
    /// [`LegacySseHttpPostSink`], or when the configured legacy request queue
    /// bound is invalid.
    pub fn new(
        handler: HttpRequestHandler,
        config: DualEraHttpEndpointConfig,
    ) -> Result<Self, DualEraHttpEndpointError> {
        validate_dual_era_path("legacy SSE route", &config.legacy_sse_path)?;
        validate_dual_era_path("legacy message route", &config.legacy_message_path)?;
        validate_legacy_http_origin(&config.legacy_origin)?;
        if config.legacy_request_capacity == 0
            || config.legacy_request_capacity > MAX_STREAMABLE_QUEUE_CAPACITY
        {
            return Err(DualEraHttpEndpointError::InvalidConfiguration(
                "legacy request capacity is outside the supported range".to_string(),
            ));
        }
        let modern_path = &handler.config().base_path;
        if config.legacy_sse_path == config.legacy_message_path
            || config.legacy_message_path == *modern_path
        {
            return Err(DualEraHttpEndpointError::InvalidConfiguration(
                "legacy POST route must differ from the legacy SSE route and modern POST route"
                    .to_string(),
            ));
        }
        Ok(Self {
            handler: Arc::new(handler),
            config,
        })
    }

    /// Opens one independently bounded endpoint session.
    ///
    /// The session owns its modern request/response transport, legacy request
    /// queue, and one live legacy SSE stream. Dropping or closing it clears all
    /// three deterministically.
    pub fn open_session(&self) -> Result<DualEraHttpSession, DualEraHttpEndpointError> {
        let session_id = generate_session_id()?;
        let mut transport = StreamableHttpTransport::with_capacity_and_max_message_size(
            self.config.legacy_request_capacity,
            self.handler.config().max_body_size,
        )?;
        let (modern_ingress, modern_responses) = transport.split_handles()?;
        let mut legacy_codec = Codec::new();
        legacy_codec.set_max_message_size(self.handler.config().max_body_size);
        let legacy_message_endpoint = format!(
            "{}{}?session_id={session_id}",
            self.config.legacy_origin, self.config.legacy_message_path
        );

        Ok(DualEraHttpSession {
            handler: Arc::clone(&self.handler),
            legacy_sse_path: self.config.legacy_sse_path.clone(),
            legacy_message_path: self.config.legacy_message_path.clone(),
            legacy_origin: self.config.legacy_origin.clone(),
            session_id: session_id.clone(),
            legacy_message_endpoint,
            legacy_request_capacity: self.config.legacy_request_capacity,
            legacy_requests: VecDeque::new(),
            legacy_responses: VecDeque::new(),
            legacy_stream_generation: 0,
            legacy_codec,
            legacy_live_sender: None,
            legacy_live_active: Arc::new(AtomicBool::new(false)),
            legacy_live_pending: Arc::new(AtomicUsize::new(0)),
            legacy_post_lifecycle_guard: Arc::new(Mutex::new(())),
            modern_transport: transport,
            modern_ingress,
            modern_responses,
            closed: false,
        })
    }
}

#[cfg(feature = "legacy-2024-11-05")]
fn validate_dual_era_path(label: &str, path: &str) -> Result<(), DualEraHttpEndpointError> {
    if !path.starts_with('/')
        || path.len() == 1
        || path
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0' | b'?' | b'#'))
    {
        return Err(DualEraHttpEndpointError::InvalidConfiguration(format!(
            "{label} must be a non-root absolute path without query, fragment, or control bytes"
        )));
    }
    Ok(())
}

/// The exact 2024-11-05 lane has no TLS form: [`LegacySseHttpPostSink`] is
/// plaintext-only, and the native TLS listener serves only MCP 2026-07-28.
#[cfg(feature = "legacy-2024-11-05")]
const LEGACY_ORIGIN_REQUIRES_PLAIN_HTTP: &str = "legacy origin must use http://: the exact \
     2024-11-05 SSE lane has no TLS support (its POST sink is plaintext-only and native TLS \
     listeners serve only MCP 2026-07-28)";

#[cfg(feature = "legacy-2024-11-05")]
fn validate_legacy_http_origin(origin: &str) -> Result<(), DualEraHttpEndpointError> {
    let Some(authority) = origin.strip_prefix("http://") else {
        return Err(DualEraHttpEndpointError::InvalidConfiguration(
            LEGACY_ORIGIN_REQUIRES_PLAIN_HTTP.to_string(),
        ));
    };
    if authority.is_empty()
        || authority.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || matches!(byte, b'/' | b'?' | b'#' | b'\\' | b'\r' | b'\n' | b'\0')
        })
    {
        return Err(DualEraHttpEndpointError::InvalidConfiguration(
            "legacy origin must contain exactly one nonempty HTTP authority".to_string(),
        ));
    }
    Ok(())
}

#[cfg(feature = "legacy-2024-11-05")]
fn sse_http_response(body: Vec<u8>) -> HttpResponse {
    HttpResponse::new(HttpStatus::OK)
        .with_header("content-type", "text/event-stream")
        .with_header("cache-control", "no-cache")
        .with_header("connection", "keep-alive")
        .with_header("x-accel-buffering", "no")
        .with_body(body)
}

#[cfg(feature = "legacy-2024-11-05")]
fn method_rejection(allowed: &str) -> HttpResponse {
    HttpResponse::new(HttpStatus::METHOD_NOT_ALLOWED).with_header("allow", allowed)
}

#[cfg(feature = "legacy-2024-11-05")]
fn has_modern_http_binding_headers(request: &HttpRequest) -> bool {
    [
        "mcp-protocol-version",
        "mcp-method",
        "mcp-name",
        "mcp-session-id",
    ]
    .iter()
    .any(|name| request.header(name).is_some())
}

/// One exact MCP 2024-11-05 message admitted by the legacy HTTP POST boundary.
#[cfg(feature = "legacy-2024-11-05")]
pub enum Legacy2024HttpPostEnvelope {
    /// A client-to-server request or notification.
    ClientMessage(JsonRpcRequest),
    /// A response to a server-originated exact-2024 request.
    Response(JsonRpcResponse),
}

/// Applies the complete exact-2024 HTTP POST admission without mutating session state.
#[cfg(feature = "legacy-2024-11-05")]
pub fn admit_legacy_2024_http_post(
    request: &HttpRequest,
    expected_path: &str,
    expected_session_id: &str,
    max_body_size: usize,
) -> Result<Legacy2024HttpPostEnvelope, HttpResponse> {
    if validate_http_request_headers(request).is_err() {
        return Err(HttpResponse::bad_request());
    }
    if request.method != HttpMethod::Post {
        return Err(method_rejection("POST"));
    }
    if request.path != expected_path
        || request.query.len() != 1
        || request.query.get("session_id").map(String::as_str) != Some(expected_session_id)
    {
        return Err(HttpResponse::new(HttpStatus::NOT_FOUND));
    }
    let content_type = request.content_type().unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
        || has_modern_http_binding_headers(request)
        || request.body.len() > max_body_size
        || request
            .header("content-encoding")
            .is_some_and(|coding| !is_identity_content_coding(coding))
    {
        return Err(HttpResponse::bad_request());
    }

    let mut codec = Codec::new();
    codec.set_max_message_size(max_body_size);
    let message = codec
        .decode_complete_message(&request.body)
        .map_err(|_| HttpResponse::bad_request())?;
    let raw = serde_json::from_slice(&request.body).map_err(|_| HttpResponse::bad_request())?;
    let exact = decode_legacy_2024_11_05_envelope_classified(raw)
        .map_err(|_| HttpResponse::bad_request())?;
    match (message, exact) {
        (
            JsonRpcMessage::Request(request),
            Legacy2024Envelope::Request { method, .. }
            | Legacy2024Envelope::Notification { method, .. },
        ) if matches!(
            method.direction,
            Legacy2024Direction::ClientToServer | Legacy2024Direction::Bidirectional
        ) =>
        {
            Ok(Legacy2024HttpPostEnvelope::ClientMessage(request))
        }
        (
            JsonRpcMessage::Response(response),
            Legacy2024Envelope::Response { .. } | Legacy2024Envelope::Error { .. },
        ) => Ok(Legacy2024HttpPostEnvelope::Response(response)),
        _ => Err(HttpResponse::bad_request()),
    }
}

/// Shared live-body authority for exact-2024 POST side effects.
#[cfg(feature = "legacy-2024-11-05")]
#[derive(Clone)]
pub struct DualEraHttpLegacyLifecycle {
    active: Arc<AtomicBool>,
    commit_guard: Arc<Mutex<()>>,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpLegacyLifecycle {
    /// Runs `commit` only while the owning legacy SSE body is live.
    pub fn commit_if_live<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let _commit_guard = self
            .commit_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.active.load(Ordering::Acquire).then(commit)
    }

    /// Returns whether the owning legacy SSE body is currently live.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// The externally renderable result of one [`DualEraHttpSession`] request.
#[cfg(feature = "legacy-2024-11-05")]
pub enum DualEraHttpEndpointResponse {
    /// A complete HTTP response, including legacy POST responses.
    Immediate(HttpResponse),
    /// A modern JSON response that becomes available after application dispatch.
    ModernJson(DualEraHttpJsonResponse),
    /// A modern request-scoped SSE response body.
    ModernSse(DualEraHttpSseResponse),
    /// A live legacy SSE stream that begins with its endpoint.
    LegacySse(DualEraHttpLegacySseResponse),
}

/// One admitted modern JSON response awaiting its matching JSON-RPC response.
#[cfg(feature = "legacy-2024-11-05")]
pub struct DualEraHttpJsonResponse {
    handler: Arc<HttpRequestHandler>,
    responses: StreamableHttpResponseStream,
    request_id: RequestId,
    origin: Option<String>,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpJsonResponse {
    /// Tries to render the one response bound to this modern request.
    ///
    /// `Ok(None)` means application dispatch has not yet committed its final
    /// response. The returned HTTP value has the normal JSON response headers.
    pub fn try_response(&self) -> Result<Option<HttpResponse>, DualEraHttpEndpointError> {
        let Some(response) = self.responses.pop_response(Some(&self.request_id))? else {
            return Ok(None);
        };
        Ok(Some(
            self.handler
                .try_create_response(&response, self.origin.as_deref())?,
        ))
    }
}

/// One finite modern SSE response body bound to its exact JSON-RPC request.
#[cfg(feature = "legacy-2024-11-05")]
pub struct DualEraHttpSseResponse {
    response: HttpResponse,
    body: StreamableHttpRequestResponseStream,
    codec: Codec,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpSseResponse {
    /// Returns the HTTP status and headers to send before the finite SSE body.
    #[must_use]
    pub fn response(&self) -> &HttpResponse {
        &self.response
    }

    /// Returns the request-owned cancellation guard for application dispatch.
    #[must_use]
    pub fn cancellation(&self) -> StreamableHttpRequestCancellation {
        self.body.cancellation()
    }

    /// Returns a producer for notifications and the terminal response of this
    /// exact request-owned SSE body.
    #[must_use]
    pub fn sender(&self) -> StreamableHttpRequestResponseSender {
        self.body.sender()
    }

    /// Returns whether this body has emitted its terminal response.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.body.is_finished()
    }

    /// Tries to frame the next notification or terminal response as an SSE
    /// `message` event without blocking the caller's runtime worker.
    pub fn pop_event(&self) -> Result<Option<SseEvent>, DualEraHttpEndpointError> {
        let Some(message) = self.body.pop_message()? else {
            return Ok(None);
        };
        Self::frame_message(&self.codec, message).map(Some)
    }

    /// Receives and frames the next notification or terminal response as an
    /// SSE `message` event.
    ///
    /// Modern notifications and the final response are ordered within this
    /// request body. These events deliberately have no resumable ID because
    /// this request-scoped body is not resumable.
    pub fn recv_event(&self, cx: &Cx) -> Result<SseEvent, DualEraHttpEndpointError> {
        Self::frame_message(&self.codec, self.body.recv_message(cx)?)
    }

    fn frame_message(
        codec: &Codec,
        message: StreamableHttpRequestResponseMessage,
    ) -> Result<SseEvent, DualEraHttpEndpointError> {
        let mut encoded = match message {
            StreamableHttpRequestResponseMessage::Notification(notification)
            | StreamableHttpRequestResponseMessage::Request(notification) => {
                codec.encode_request(&notification)?
            }
            StreamableHttpRequestResponseMessage::Response(response) => {
                codec.encode_response(&response)?
            }
        };
        if encoded.pop() != Some(b'\n') {
            return Err(DualEraHttpEndpointError::Transport(TransportError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "JSON-RPC codec omitted its NDJSON delimiter",
                ),
            )));
        }
        let data = String::from_utf8(encoded).map_err(|error| {
            DualEraHttpEndpointError::Transport(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })?;
        Ok(SseEvent::message(data))
    }
}

#[cfg(feature = "legacy-2024-11-05")]
struct LegacySseLiveMessage {
    data: String,
}

#[cfg(feature = "legacy-2024-11-05")]
struct LegacyQueuedRequest {
    generation: u64,
    request: JsonRpcRequest,
}

#[cfg(feature = "legacy-2024-11-05")]
struct LegacyQueuedResponse {
    generation: u64,
    response: JsonRpcResponse,
}

/// One live legacy SSE response body for an exact MCP 2024-11-05 session.
///
/// The stream starts with its endpoint event. Subsequent server messages arrive
/// only through the session that admitted this stream. Waiting for them checks
/// the caller context between bounded nonblocking channel polls.
#[cfg(feature = "legacy-2024-11-05")]
pub struct DualEraHttpLegacySseResponse {
    response: HttpResponse,
    initial_events: VecDeque<SseEvent>,
    receiver: mpsc::Receiver<LegacySseLiveMessage>,
    active: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    post_lifecycle_guard: Arc<Mutex<()>>,
    poll_interval: Duration,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpLegacySseResponse {
    fn deactivate(&self) {
        let _lifecycle_guard = self
            .post_lifecycle_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.active.store(false, Ordering::Release);
        self.pending.store(0, Ordering::Release);
    }

    /// Returns the HTTP status and headers to send before the live SSE body.
    #[must_use]
    pub fn response(&self) -> &HttpResponse {
        &self.response
    }

    /// Receives the next endpoint or live legacy SSE event.
    ///
    /// Once the session or this response body closes, the method returns
    /// [`TransportError::Closed`].
    /// Non-blocking variant of [`Self::recv_event`]: returns `Ok(None)` when
    /// no event is ready, so async pumps can poll between yields without
    /// parking a blocking thread per live stream.
    pub fn try_recv_event(
        &mut self,
        cx: &Cx,
    ) -> Result<Option<SseEvent>, DualEraHttpEndpointError> {
        if let Err(error) = http_checkpoint(cx) {
            self.deactivate();
            return Err(DualEraHttpEndpointError::Transport(error));
        }
        if !self.active.load(Ordering::Acquire) {
            return Err(DualEraHttpEndpointError::Transport(TransportError::Closed));
        }
        if let Some(event) = self.initial_events.pop_front() {
            return Ok(Some(event));
        }
        match self.receiver.try_recv() {
            Ok(message) => {
                let previous = self.pending.fetch_sub(1, Ordering::AcqRel);
                debug_assert!(previous > 0, "legacy SSE live-event count underflow");
                Ok(Some(SseEvent::message(message.data)))
            }
            Err(mpsc::RecvError::Empty) => Ok(None),
            Err(mpsc::RecvError::Disconnected) => {
                Err(DualEraHttpEndpointError::Transport(TransportError::Closed))
            }
            Err(mpsc::RecvError::Cancelled) => Err(DualEraHttpEndpointError::Transport(
                TransportError::Cancelled,
            )),
        }
    }

    /// Asynchronously receives the next endpoint or live legacy SSE event.
    ///
    /// Unlike [`Self::try_recv_event`], this registers the current task's
    /// waker with the live-message channel. A server-side publisher therefore
    /// wakes a parked SSE writer immediately instead of relying on timer
    /// polling to make progress.
    pub async fn recv_event_async(
        &mut self,
        cx: &Cx,
    ) -> Result<SseEvent, DualEraHttpEndpointError> {
        if let Err(error) = http_checkpoint(cx) {
            self.deactivate();
            return Err(DualEraHttpEndpointError::Transport(error));
        }
        if !self.active.load(Ordering::Acquire) {
            return Err(DualEraHttpEndpointError::Transport(TransportError::Closed));
        }
        if let Some(event) = self.initial_events.pop_front() {
            return Ok(event);
        }

        match self.receiver.recv(cx).await {
            Ok(message) => {
                let previous = self.pending.fetch_sub(1, Ordering::AcqRel);
                debug_assert!(previous > 0, "legacy SSE live-event count underflow");
                Ok(SseEvent::message(message.data))
            }
            Err(mpsc::RecvError::Disconnected) => {
                Err(DualEraHttpEndpointError::Transport(TransportError::Closed))
            }
            Err(mpsc::RecvError::Cancelled) => Err(DualEraHttpEndpointError::Transport(
                TransportError::Cancelled,
            )),
            Err(mpsc::RecvError::Empty) => unreachable!("a waiting receive cannot return Empty"),
        }
    }

    pub fn recv_event(&mut self, cx: &Cx) -> Result<SseEvent, DualEraHttpEndpointError> {
        if let Err(error) = http_checkpoint(cx) {
            self.deactivate();
            return Err(DualEraHttpEndpointError::Transport(error));
        }
        if !self.active.load(Ordering::Acquire) {
            return Err(DualEraHttpEndpointError::Transport(TransportError::Closed));
        }
        if let Some(event) = self.initial_events.pop_front() {
            return Ok(event);
        }

        loop {
            if let Err(error) = http_checkpoint(cx) {
                self.deactivate();
                return Err(DualEraHttpEndpointError::Transport(error));
            }
            if !self.active.load(Ordering::Acquire) {
                return Err(DualEraHttpEndpointError::Transport(TransportError::Closed));
            }
            match self.receiver.try_recv() {
                Ok(message) => {
                    let previous = self.pending.fetch_sub(1, Ordering::AcqRel);
                    debug_assert!(previous > 0, "legacy SSE live-event count underflow");
                    return Ok(SseEvent::message(message.data));
                }
                Err(mpsc::RecvError::Empty) => {
                    // This poll loop runs on a blocking thread; without a
                    // pause it spins one core at 100% between events.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(mpsc::RecvError::Disconnected) => {
                    return Err(DualEraHttpEndpointError::Transport(TransportError::Closed));
                }
                Err(mpsc::RecvError::Cancelled) => {
                    return Err(DualEraHttpEndpointError::Transport(
                        TransportError::Cancelled,
                    ));
                }
            }
            std::thread::sleep(self.poll_interval);
        }
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl Drop for DualEraHttpLegacySseResponse {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// A session combining modern Streamable HTTP with exact legacy SSE/POST flow.
#[cfg(feature = "legacy-2024-11-05")]
pub struct DualEraHttpSession {
    handler: Arc<HttpRequestHandler>,
    legacy_sse_path: String,
    legacy_message_path: String,
    legacy_origin: String,
    session_id: String,
    legacy_message_endpoint: String,
    legacy_request_capacity: usize,
    legacy_requests: VecDeque<LegacyQueuedRequest>,
    legacy_responses: VecDeque<LegacyQueuedResponse>,
    legacy_stream_generation: u64,
    legacy_codec: Codec,
    legacy_live_sender: Option<mpsc::Sender<LegacySseLiveMessage>>,
    legacy_live_active: Arc<AtomicBool>,
    legacy_live_pending: Arc<AtomicUsize>,
    legacy_post_lifecycle_guard: Arc<Mutex<()>>,
    modern_transport: StreamableHttpTransport,
    modern_ingress: StreamableHttpRequestIngress,
    modern_responses: StreamableHttpResponseStream,
    closed: bool,
}

#[cfg(feature = "legacy-2024-11-05")]
impl DualEraHttpSession {
    /// Returns the opaque session value required by the advertised legacy POST URI.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the exact legacy POST URI advertised in the first SSE event.
    #[must_use]
    pub fn legacy_message_endpoint(&self) -> &str {
        &self.legacy_message_endpoint
    }

    /// Returns a cloneable authority that serializes effects with legacy body closure.
    #[must_use]
    pub fn legacy_lifecycle(&self) -> DualEraHttpLegacyLifecycle {
        DualEraHttpLegacyLifecycle {
            active: Arc::clone(&self.legacy_live_active),
            commit_guard: Arc::clone(&self.legacy_post_lifecycle_guard),
        }
    }

    /// Returns whether this session has stopped admitting work.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Handles one request against exactly one modern or legacy route.
    ///
    /// Modern requests are admitted through the existing final HTTP boundary.
    /// Legacy GET returns a fresh live stream beginning with its endpoint;
    /// legacy POST accepts only the URI advertised for this exact session.
    pub fn handle(
        &mut self,
        cx: &Cx,
        request: HttpRequest,
    ) -> Result<DualEraHttpEndpointResponse, DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        http_checkpoint(cx)?;

        let modern_path = &self.handler.config().base_path;
        let modern_target = request.path == *modern_path;
        let legacy_sse_target = request.path == self.legacy_sse_path;
        let legacy_post_target = request.path == self.legacy_message_path;

        if modern_target && request.method == HttpMethod::Post {
            return self.handle_modern(cx, request);
        }
        if legacy_sse_target && request.method == HttpMethod::Get {
            return self.handle_legacy_sse(request);
        }
        if legacy_post_target && request.method == HttpMethod::Post {
            return Ok(self.handle_legacy_post(request));
        }
        if modern_target || legacy_sse_target || legacy_post_target {
            let allow = if modern_target && legacy_sse_target {
                "GET, POST"
            } else if modern_target || legacy_post_target {
                "POST"
            } else {
                "GET"
            };
            return Ok(DualEraHttpEndpointResponse::Immediate(method_rejection(
                allow,
            )));
        }
        Ok(DualEraHttpEndpointResponse::Immediate(HttpResponse::new(
            HttpStatus::NOT_FOUND,
        )))
    }

    fn handle_modern(
        &mut self,
        cx: &Cx,
        request: HttpRequest,
    ) -> Result<DualEraHttpEndpointResponse, DualEraHttpEndpointError> {
        let origin = request.header("origin").map(str::to_owned);
        let admission = self.handler.admit_modern_request(&request)?;
        let json_rpc = admission.request().clone();

        match admission.response_representation() {
            HttpResponseRepresentation::Json => {
                self.modern_ingress.push_request(cx, json_rpc.clone())?;
                let Some(request_id) = json_rpc.id else {
                    return Ok(DualEraHttpEndpointResponse::Immediate(HttpResponse::new(
                        HttpStatus::ACCEPTED,
                    )));
                };
                Ok(DualEraHttpEndpointResponse::ModernJson(
                    DualEraHttpJsonResponse {
                        handler: Arc::clone(&self.handler),
                        responses: self.modern_responses.clone(),
                        request_id,
                        origin,
                    },
                ))
            }
            HttpResponseRepresentation::Sse => {
                let body = admission.bind_sse_response_body(&self.modern_responses)?;
                self.modern_ingress.push_request(cx, json_rpc)?;
                Ok(DualEraHttpEndpointResponse::ModernSse(
                    DualEraHttpSseResponse {
                        response: sse_http_response(Vec::new()),
                        body,
                        codec: Codec::new(),
                    },
                ))
            }
        }
    }

    fn handle_legacy_sse(
        &mut self,
        request: HttpRequest,
    ) -> Result<DualEraHttpEndpointResponse, DualEraHttpEndpointError> {
        validate_http_request_headers(&request)?;
        if request.method != HttpMethod::Get {
            return Ok(DualEraHttpEndpointResponse::Immediate(method_rejection(
                "GET",
            )));
        }
        if !request.body.is_empty() || has_modern_http_binding_headers(&request) {
            return Ok(DualEraHttpEndpointResponse::Immediate(
                HttpResponse::bad_request(),
            ));
        }

        // Exact MCP 2024-11-05 reconnects establish a fresh SSE endpoint
        // lifecycle. `Last-Event-ID` is intentionally ignored: it neither
        // replays messages nor selects or mutates session state.
        let rotated_session = (self.legacy_stream_generation != 0)
            .then(generate_session_id)
            .transpose()?;
        let Some(next_generation) = self.legacy_stream_generation.checked_add(1) else {
            return Ok(DualEraHttpEndpointResponse::Immediate(HttpResponse::new(
                HttpStatus::SERVICE_UNAVAILABLE,
            )));
        };
        if rotated_session.is_some() {
            {
                let _old_lifecycle_guard = self
                    .legacy_post_lifecycle_guard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if self.legacy_live_active.load(Ordering::Acquire) {
                    return Ok(DualEraHttpEndpointResponse::Immediate(HttpResponse::new(
                        HttpStatus::SERVICE_UNAVAILABLE,
                    )));
                }
            }
            // A reconnect is a new capability generation, not a revival of
            // the old body authority. Retained lifecycle clones stay bound to
            // the closed generation and therefore remain permanently false.
            self.legacy_live_active = Arc::new(AtomicBool::new(true));
            self.legacy_live_pending = Arc::new(AtomicUsize::new(0));
            self.legacy_post_lifecycle_guard = Arc::new(Mutex::new(()));
        } else {
            let _lifecycle_guard = self
                .legacy_post_lifecycle_guard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self
                .legacy_live_active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Ok(DualEraHttpEndpointResponse::Immediate(HttpResponse::new(
                    HttpStatus::SERVICE_UNAVAILABLE,
                )));
            }
        }
        self.legacy_stream_generation = next_generation;
        if let Some(session_id) = rotated_session {
            self.session_id = session_id;
            self.legacy_message_endpoint = format!(
                "{}{}?session_id={}",
                self.legacy_origin, self.legacy_message_path, self.session_id
            );
        }
        self.legacy_requests.clear();
        self.legacy_responses.clear();
        self.legacy_live_pending.store(0, Ordering::Release);
        let (sender, receiver) = mpsc::channel(self.legacy_request_capacity);
        self.legacy_live_sender = Some(sender);
        let mut initial_events = VecDeque::new();
        initial_events.push_back(SseEvent::endpoint(&self.legacy_message_endpoint));
        Ok(DualEraHttpEndpointResponse::LegacySse(
            DualEraHttpLegacySseResponse {
                response: sse_http_response(Vec::new()),
                initial_events,
                receiver,
                active: Arc::clone(&self.legacy_live_active),
                pending: Arc::clone(&self.legacy_live_pending),
                post_lifecycle_guard: Arc::clone(&self.legacy_post_lifecycle_guard),
                poll_interval: Duration::from_millis(10),
            },
        ))
    }

    fn handle_legacy_post(&mut self, request: HttpRequest) -> DualEraHttpEndpointResponse {
        let legacy_message = match admit_legacy_2024_http_post(
            &request,
            &self.legacy_message_path,
            &self.session_id,
            self.handler.config().max_body_size,
        ) {
            Ok(message) => message,
            Err(response) => return DualEraHttpEndpointResponse::Immediate(response),
        };

        let _lifecycle_guard = self
            .legacy_post_lifecycle_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.legacy_live_active.load(Ordering::Acquire)
            || self.legacy_requests.len() + self.legacy_responses.len()
                >= self.legacy_request_capacity
        {
            return DualEraHttpEndpointResponse::Immediate(HttpResponse::new(
                HttpStatus::SERVICE_UNAVAILABLE,
            ));
        }
        match legacy_message {
            Legacy2024HttpPostEnvelope::ClientMessage(request) => {
                self.legacy_requests.push_back(LegacyQueuedRequest {
                    generation: self.legacy_stream_generation,
                    request,
                });
            }
            Legacy2024HttpPostEnvelope::Response(response) => {
                self.legacy_responses.push_back(LegacyQueuedResponse {
                    generation: self.legacy_stream_generation,
                    response,
                });
            }
        }
        DualEraHttpEndpointResponse::Immediate(HttpResponse::new(HttpStatus::ACCEPTED))
    }

    /// Receives one modern request after final HTTP admission.
    pub fn recv_modern_request(
        &mut self,
        cx: &Cx,
    ) -> Result<JsonRpcRequest, DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        match self.modern_transport.recv(cx)? {
            JsonRpcMessage::Request(request) => Ok(request),
            JsonRpcMessage::Response(_) => Err(DualEraHttpEndpointError::Transport(
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "modern request ingress yielded a JSON-RPC response",
                )),
            )),
        }
    }

    /// Removes the next legacy client POST request in FIFO order.
    #[must_use]
    pub fn take_legacy_request(&mut self) -> Option<JsonRpcRequest> {
        if self.closed {
            return None;
        }
        let _lifecycle_guard = self
            .legacy_post_lifecycle_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.legacy_live_active.load(Ordering::Acquire) {
            self.legacy_requests.clear();
            self.legacy_responses.clear();
            return None;
        }
        while let Some(queued) = self.legacy_requests.pop_front() {
            if queued.generation == self.legacy_stream_generation {
                return Some(queued.request);
            }
        }
        None
    }

    /// Removes the next exact-2024 client response in FIFO order.
    ///
    /// The transport admits only a structurally exact response; its live
    /// server adapter remains responsible for correlating it to an owned
    /// reverse request before mutating pending-request state.
    #[must_use]
    pub fn take_legacy_response(&mut self) -> Option<JsonRpcResponse> {
        if self.closed {
            return None;
        }
        let _lifecycle_guard = self
            .legacy_post_lifecycle_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.legacy_live_active.load(Ordering::Acquire) {
            self.legacy_requests.clear();
            self.legacy_responses.clear();
            return None;
        }
        while let Some(queued) = self.legacy_responses.pop_front() {
            if queued.generation == self.legacy_stream_generation {
                return Some(queued.response);
            }
        }
        None
    }

    /// Sends one finite JSON response for a modern JSON-selected request.
    pub fn send_modern_json_response(
        &mut self,
        cx: &Cx,
        response: JsonRpcResponse,
    ) -> Result<(), DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        self.modern_transport
            .send(cx, &JsonRpcMessage::Response(response))?;
        Ok(())
    }

    /// Sends one response for the exact modern SSE request owned by `cancellation`.
    pub fn send_modern_sse_response(
        &mut self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        response: JsonRpcResponse,
    ) -> Result<(), DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        self.modern_transport
            .send_response_for_request(cx, cancellation, response)?;
        Ok(())
    }

    /// Sends one request-owned notification through a modern SSE response body.
    ///
    /// The cancellation guard prevents a notification from being routed to a
    /// different in-flight request or committed after its HTTP body closes.
    pub fn send_modern_sse_notification(
        &mut self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        notification: JsonRpcRequest,
    ) -> Result<(), DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        self.modern_transport
            .send_notification_for_request(cx, cancellation, notification)?;
        Ok(())
    }

    /// Sends one request-owned reverse request through a modern SSE response body.
    pub fn send_modern_sse_request(
        &mut self,
        cx: &Cx,
        cancellation: &StreamableHttpRequestCancellation,
        request: JsonRpcRequest,
    ) -> Result<(), DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        self.modern_transport
            .send_request_for_request(cx, cancellation, request)?;
        Ok(())
    }

    /// Publishes one legacy server-to-client message to the live SSE stream.
    ///
    /// Exact MCP 2024-11-05 does not retain legacy SSE events or assign event
    /// IDs. If no legacy stream is live, the message is not persisted for a
    /// later GET.
    pub fn publish_legacy_message(
        &self,
        message: &JsonRpcMessage,
    ) -> Result<(), DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        let data = self.encode_legacy_live_message(message)?;
        // Serialize liveness observation and the channel commit with body
        // teardown. A stale response-body capability cannot publish after its
        // stream has transitioned out of the live generation.
        let _lifecycle_guard = self
            .legacy_post_lifecycle_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.commit_legacy_live_message(data)
    }

    /// Publishes one live-generation message while the caller already holds
    /// this generation's [`DualEraHttpLegacyLifecycle`] commit guard.
    ///
    /// `commit_if_live` and [`Self::publish_legacy_message`] share
    /// `legacy_post_lifecycle_guard`. Re-entering the locking publisher from
    /// a live commit deadlocks handler `list_changed`, progress, log, and
    /// reverse-request send during POST dispatch.
    pub fn publish_legacy_message_committed(
        &self,
        message: &JsonRpcMessage,
    ) -> Result<(), DualEraHttpEndpointError> {
        if self.closed {
            return Err(DualEraHttpEndpointError::Closed);
        }
        let data = self.encode_legacy_live_message(message)?;
        self.commit_legacy_live_message(data)
    }

    fn encode_legacy_live_message(
        &self,
        message: &JsonRpcMessage,
    ) -> Result<String, DualEraHttpEndpointError> {
        let mut encoded = match message {
            JsonRpcMessage::Request(request) => self.legacy_codec.encode_request(request)?,
            JsonRpcMessage::Response(response) => self.legacy_codec.encode_response(response)?,
        };
        if encoded.pop() != Some(b'\n') {
            return Err(DualEraHttpEndpointError::Transport(TransportError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "JSON-RPC codec omitted its NDJSON delimiter",
                ),
            )));
        }
        String::from_utf8(encoded).map_err(|error| {
            DualEraHttpEndpointError::Transport(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })
    }

    fn commit_legacy_live_message(&self, data: String) -> Result<(), DualEraHttpEndpointError> {
        let reserves_live_delivery = self.reserve_legacy_live_delivery()?;
        if reserves_live_delivery {
            let Some(sender) = self.legacy_live_sender.as_ref() else {
                self.release_legacy_live_delivery();
                return Ok(());
            };
            if let Err(error) = sender.try_send(LegacySseLiveMessage { data }) {
                self.release_legacy_live_delivery();
                if matches!(&error, mpsc::SendError::Disconnected(_)) {
                    self.legacy_live_active.store(false, Ordering::Release);
                    return Ok(());
                } else {
                    return Err(DualEraHttpEndpointError::Transport(
                        map_streamable_send_error(error, "legacy SSE live queue is full"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn reserve_legacy_live_delivery(&self) -> Result<bool, DualEraHttpEndpointError> {
        if !self.legacy_live_active.load(Ordering::Acquire) {
            return Ok(false);
        }

        let mut pending = self.legacy_live_pending.load(Ordering::Acquire);
        loop {
            if !self.legacy_live_active.load(Ordering::Acquire) {
                return Ok(false);
            }
            if pending >= self.legacy_request_capacity {
                return Err(DualEraHttpEndpointError::Transport(
                    streamable_queue_full_error("legacy SSE live queue is full"),
                ));
            }
            match self.legacy_live_pending.compare_exchange_weak(
                pending,
                pending + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(true),
                Err(observed) => pending = observed,
            }
        }
    }

    fn release_legacy_live_delivery(&self) {
        let _ =
            self.legacy_live_pending
                .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                    pending.checked_sub(1)
                });
    }

    /// Closes the session, cancels all live modern response bodies, and clears
    /// queued modern work plus legacy request and live-stream state.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.modern_ingress.close();
        self.modern_responses.terminate();
        // `Transport::close` now takes the caller's `&Cx`, and this method is
        // reachable from `Drop for DualEraHttpSession` (below), where no `Cx`
        // exists and none can be conjured. The trait method was never needed
        // here: `StreamableHttpTransport::close` is exactly `close_queues()`
        // plus `Ok(())`, and that type's own `Drop` already calls
        // `close_queues()`. Calling it directly keeps the destructor path
        // synchronous and loses no behaviour.
        self.modern_transport.close_queues();
        let _lifecycle_guard = self
            .legacy_post_lifecycle_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.legacy_live_active.store(false, Ordering::Release);
        self.legacy_live_pending.store(0, Ordering::Release);
        self.legacy_live_sender = None;
        self.legacy_requests.clear();
        self.legacy_responses.clear();
    }
}

#[cfg(feature = "legacy-2024-11-05")]
impl Drop for DualEraHttpSession {
    fn drop(&mut self) {
        self.close();
    }
}

// =============================================================================
// Session Support
// =============================================================================

const MAX_HTTP_SESSIONS: usize = 1_024;
const MAX_HTTP_SESSION_ID_BYTES: usize = 128;
const MAX_HTTP_SESSION_ENTRIES: usize = 64;
const MAX_HTTP_SESSION_KEY_BYTES: usize = 256;
const MAX_HTTP_SESSION_VALUE_BYTES: usize = 256 * 1024;
const MAX_HTTP_SESSION_RETAINED_BYTES: usize = 1024 * 1024;

/// Bounded HTTP session admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpSessionError {
    InvalidSessionId,
    InvalidCapacity,
    KeyTooLarge,
    ValueTooLarge,
    CapacityExceeded,
    SessionNotFound,
    RandomnessUnavailable,
}

impl std::fmt::Display for HttpSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidSessionId => "invalid HTTP session identifier",
            Self::InvalidCapacity => "HTTP session capacity is outside the supported range",
            Self::KeyTooLarge => "HTTP session key exceeds byte limit",
            Self::ValueTooLarge => "HTTP session value exceeds byte limit",
            Self::CapacityExceeded => "HTTP session capacity exhausted",
            Self::SessionNotFound => "HTTP session not found",
            Self::RandomnessUnavailable => "HTTP session identifier generation failed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for HttpSessionError {}

#[derive(Debug, Clone)]
struct HttpSessionEntry {
    value: serde_json::Value,
    retained_bytes: usize,
}

struct SessionValueByteCounter {
    bytes: usize,
    limit: usize,
}

impl Write for SessionValueByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self.bytes.checked_add(buffer.len()) else {
            return Err(std::io::Error::other("session value byte limit exceeded"));
        };
        if next > self.limit {
            return Err(std::io::Error::other("session value byte limit exceeded"));
        }
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn measure_session_value(value: &serde_json::Value) -> Result<usize, HttpSessionError> {
    let mut counter = SessionValueByteCounter {
        bytes: 0,
        limit: MAX_HTTP_SESSION_VALUE_BYTES,
    };
    serde_json::to_writer(&mut counter, value).map_err(|_| HttpSessionError::ValueTooLarge)?;
    Ok(counter.bytes)
}

/// HTTP session for maintaining state across requests.
#[derive(Debug, Clone)]
pub struct HttpSession {
    /// Session ID.
    id: String,
    /// Session creation time.
    created_at: Instant,
    /// Last activity time.
    last_activity: Instant,
    /// Session data.
    data: HashMap<String, HttpSessionEntry>,
    retained_bytes: usize,
}

impl HttpSession {
    /// Creates a new session with the given ID.
    pub fn new(id: impl Into<String>) -> Result<Self, HttpSessionError> {
        let id = id.into();
        if id.is_empty()
            || id.len() > MAX_HTTP_SESSION_ID_BYTES
            || !id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(HttpSessionError::InvalidSessionId);
        }
        let now = Instant::now();
        Ok(Self {
            id,
            created_at: now,
            last_activity: now,
            data: HashMap::new(),
            retained_bytes: 0,
        })
    }

    /// Returns this session's immutable identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns when the session was created.
    #[must_use]
    pub fn created_at(&self) -> Instant {
        self.created_at
    }

    /// Updates the last activity time.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Checks if the session has expired.
    #[must_use]
    pub fn is_expired(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    /// Gets a session value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.data.get(key).map(|entry| &entry.value)
    }

    /// Sets a session value.
    pub fn set(
        &mut self,
        key: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<(), HttpSessionError> {
        let key = key.into();
        if key.is_empty() || key.len() > MAX_HTTP_SESSION_KEY_BYTES {
            return Err(HttpSessionError::KeyTooLarge);
        }
        if !self.data.contains_key(&key) && self.data.len() >= MAX_HTTP_SESSION_ENTRIES {
            return Err(HttpSessionError::CapacityExceeded);
        }
        let value_bytes = measure_session_value(&value)?;
        let retained_bytes = key
            .len()
            .checked_add(value_bytes)
            .ok_or(HttpSessionError::CapacityExceeded)?;
        let prior_bytes = self.data.get(&key).map_or(0, |entry| entry.retained_bytes);
        let prospective = self
            .retained_bytes
            .checked_sub(prior_bytes)
            .and_then(|bytes| bytes.checked_add(retained_bytes))
            .ok_or(HttpSessionError::CapacityExceeded)?;
        if prospective > MAX_HTTP_SESSION_RETAINED_BYTES {
            return Err(HttpSessionError::CapacityExceeded);
        }
        self.data.insert(
            key,
            HttpSessionEntry {
                value,
                retained_bytes,
            },
        );
        self.retained_bytes = prospective;
        self.touch();
        Ok(())
    }

    /// Removes a session value.
    pub fn remove(&mut self, key: &str) -> Option<serde_json::Value> {
        self.touch();
        self.data.remove(key).map(|entry| {
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
            entry.value
        })
    }
}

/// Session store for HTTP sessions.
#[derive(Debug)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, HttpSession>>,
    timeout: Duration,
    max_sessions: usize,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl SessionStore {
    /// Creates a new session store with the given timeout.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self::with_capacity(timeout, MAX_HTTP_SESSIONS)
            .expect("the built-in HTTP session capacity must be valid")
    }

    /// Creates a session store with a hard global session limit.
    ///
    /// # Errors
    ///
    /// Returns [`HttpSessionError::InvalidCapacity`] when `max_sessions` is
    /// zero or exceeds the hard global session limit.
    pub fn with_capacity(timeout: Duration, max_sessions: usize) -> Result<Self, HttpSessionError> {
        if max_sessions == 0 || max_sessions > MAX_HTTP_SESSIONS {
            return Err(HttpSessionError::InvalidCapacity);
        }
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            timeout,
            max_sessions,
        })
    }

    /// Creates a new session store with default 1-hour timeout.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(Duration::from_secs(3600))
    }

    /// Creates a new session.
    pub fn create(&self) -> Result<String, HttpSessionError> {
        let id = generate_session_id()?;
        let session = HttpSession::new(&id)?;
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|_, existing| !existing.is_expired(self.timeout));
        if guard.len() >= self.max_sessions {
            return Err(HttpSessionError::CapacityExceeded);
        }
        if guard.contains_key(&id) {
            return Err(HttpSessionError::CapacityExceeded);
        }
        guard.insert(id.clone(), session);
        Ok(id)
    }

    /// Gets a session by ID.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<HttpSession> {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = guard.get_mut(id)?;

        if session.is_expired(self.timeout) {
            guard.remove(id);
            return None;
        }

        session.touch();
        Some(session.clone())
    }

    /// Updates a session.
    pub fn update(&self, session: HttpSession) -> Result<(), HttpSessionError> {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|_, existing| !existing.is_expired(self.timeout));
        if !guard.contains_key(&session.id) {
            return Err(HttpSessionError::SessionNotFound);
        }
        guard.insert(session.id.clone(), session);
        Ok(())
    }

    /// Removes a session.
    pub fn remove(&self, id: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// Removes expired sessions.
    pub fn cleanup(&self) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, session| !session.is_expired(self.timeout));
    }

    /// Returns the number of active sessions.
    #[must_use]
    pub fn count(&self) -> usize {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|_, session| !session.is_expired(self.timeout));
        guard.len()
    }
}

/// Generates a fresh 256-bit session ID from the process-wide OS randomness
/// boundary and encodes it as fixed-width lowercase hexadecimal.
fn generate_session_id() -> Result<String, HttpSessionError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let identifier =
        draw_security_identifier().map_err(|_| HttpSessionError::RandomnessUnavailable)?;
    let mut encoded = String::with_capacity(identifier.as_bytes().len() * 2);
    for byte in identifier.as_bytes() {
        encoded.push(char::from(HEX[usize::from(*byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(*byte & 0x0f)]));
    }
    Ok(encoded)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        tls::{Certificate, CertificateChain, PrivateKey, TlsAcceptor},
    };
    use std::io::Cursor;

    #[derive(Clone)]
    struct StaticGuardedResolver {
        answers: Vec<IpAddr>,
    }

    const GUARDED_LOOPBACK_ROOT_PEM: &[u8] = br"-----BEGIN CERTIFICATE-----
MIIBgDCCASegAwIBAgIUPHDUu9WL36yvTmFeNFZVe/qhClcwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSUnVzdGxzIFJvYnVzdCBSb290MCAXDTc1MDEwMTAwMDAwMFoY
DzQwOTYwMTAxMDAwMDAwWjAdMRswGQYDVQQDDBJSdXN0bHMgUm9idXN0IFJvb3Qw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASW/VkDFs5iGDQvH8jaXYT4jMx66jo+
5CWKyMt4OlTDdBfKfnmQ9LYeK/PsYfJ8wVizuSlPzXi9je8SnyYejGP3o0MwQTAP
BgNVHQ8BAf8EBQMDB4QAMB0GA1UdDgQWBBRqY/oMENJbNo7y39iL6GW3tDs0rzAP
BgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIEUbrmSUjANju9nNpFop
PAl9Wh8tBxI5IY+BPh466+aUAiA1/9+prypt6s3Doo0GDsnoFGJi1UBivUg1qdik
cy4eNw==
-----END CERTIFICATE-----";
    const GUARDED_LOOPBACK_CHAIN_PEM: &[u8] = br"-----BEGIN CERTIFICATE-----
MIIBszCCAVmgAwIBAgIUUg3keFcU1xXWK8BNVb1KynPulV8wCgYIKoZIzj0EAwIw
JjEkMCIGA1UEAwwbUnVzdGxzIFJvYnVzdCBSb290IC0gUnVuZyAyMCAXDTc1MDEw
MTAwMDAwMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxm
IHNpZ25lZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEud6w4gtZ0xbw
J3E69SSMy5TZfdIifl9L5ZY+hgEe4UiUsBWS32f6Y5NR5Jo8FO1f6o13b3+FvVHR
EHCGdvppL6NoMGYwFQYDVR0RBA4wDIIKZm9vYmFyLmNvbTAdBgNVHSUEFjAUBggr
BgEFBQcDAQYIKwYBBQUHAwIwHQYDVR0OBBYEFELvxbj5tD75n4pYFvJyr+c8qVEi
MA8GA1UdEwEB/wQFMAMBAQAwCgYIKoZIzj0EAwIDSAAwRQIhALxSSdUsrRFnwNMu
/doBqI8i8u5HdohVAheFTDwObkOMAiASSjULUtkWSD15u/7Sr01Wm9J1MpqW1pob
BVqU3CNRlA==
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
MIIBiTCCATCgAwIBAgIUHWiVYIvMMWoZEFYvSz46COf2FqowCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSUnVzdGxzIFJvYnVzdCBSb290MCAXDTc1MDEwMTAwMDAwMFoY
DzQwOTYwMTAxMDAwMDAwWjAmMSQwIgYDVQQDDBtSdXN0bHMgUm9idXN0IFJvb3Qg
LSBSdW5nIDIwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATAOCcBD7dXjmAZ3te5
D47cCJ9ec93PWv7BKYIL826CJsKfXQOGrBTthLm77hXLhHu6uv8E5QXNLZpfowLQ
Do1ao0MwQTAPBgNVHQ8BAf8EBQMDB4QAMB0GA1UdDgQWBBRdza76r11Ok9vRmlg6
Nn/wL/N+jTAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIFmZrXeK
hnfkahocvkhhNT3cDv1LWf6WBoFaCiBwZXFPAiARaKRiSCMG7PCHmSqFe82TBVmL
odHGogAVax1Dh/aYAA==
-----END CERTIFICATE-----";
    const GUARDED_LOOPBACK_KEY_PEM: &[u8] = br"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgTbAQpfjAT46fgF4B
mP15n37woNG5ZNJmwcqsred/7tmhRANCAAS53rDiC1nTFvAncTr1JIzLlNl90iJ+
X0vllj6GAR7hSJSwFZLfZ/pjk1HkmjwU7V/qjXdvf4W9UdEQcIZ2+mkv
-----END PRIVATE KEY-----";

    fn guarded_loopback_acceptor() -> TlsAcceptor {
        let chain = CertificateChain::from_pem(GUARDED_LOOPBACK_CHAIN_PEM)
            .expect("bounded loopback certificate chain");
        let key =
            PrivateKey::from_pem(GUARDED_LOOPBACK_KEY_PEM).expect("bounded loopback private key");
        TlsAcceptor::builder(chain, key)
            .alpn_protocols(vec![b"http/1.1".to_vec()])
            .handshake_timeout(Duration::from_millis(100))
            .build()
            .expect("bounded loopback TLS acceptor")
    }

    fn guarded_loopback_root() -> Certificate {
        Certificate::from_pem(GUARDED_LOOPBACK_ROOT_PEM)
            .expect("bounded loopback fixture CA")
            .into_iter()
            .next()
            .expect("one loopback fixture CA")
    }

    async fn guarded_loopback_read_request(
        tls: &mut asupersync::tls::TlsStream<NativeTcpStream>,
    ) -> Result<Vec<u8>, String> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 512];
        while request.len() < 8 * 1024 {
            let read = tls
                .read(&mut chunk)
                .await
                .map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(request);
            }
        }
        Err("loopback HTTP request exceeded header bound or ended before terminator".to_owned())
    }

    /// Bounds every phase INDEPENDENTLY, with `phase_budget` restarted at each
    /// phase, rather than spending one budget across the whole sequence.
    ///
    /// The property a test server owes its caller is "no single phase may block
    /// forever". That property is per-phase by nature. The previous signature
    /// took one absolute `Time` and applied it to N accepts, N handshakes, N
    /// reads and N writes, so a slow first fetch stole the budget of every fetch
    /// after it and the server stopped accepting mid-sequence. That is a
    /// wall-clock bound spanning a multi-step sequence: it fails on a loaded
    /// host while the code under test is correct, which is precisely backwards.
    ///
    /// Total runtime stays finite (phases x budget), so the anti-hang guarantee
    /// is preserved exactly; only the false-red mode is removed.
    async fn guarded_loopback_serve(
        cx: Cx,
        phase_budget: Duration,
        listener: TcpListener,
        responses: Vec<Vec<u8>>,
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
    ) -> Result<(), String> {
        let acceptor = guarded_loopback_acceptor();
        for response in responses {
            cx.checkpoint()
                .map_err(|_| "loopback server cancelled before accept".to_owned())?;
            let (stream, _) = time::timeout_at(time::wall_now() + phase_budget, listener.accept())
                .await
                .map_err(|_| format!("loopback TEST HARNESS bound: accept phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))?
                .map_err(|error| error.to_string())?;
            cx.checkpoint()
                .map_err(|_| "loopback server cancelled before TLS".to_owned())?;
            let tls = time::timeout_at(time::wall_now() + phase_budget, acceptor.accept(stream))
                .await
                .map_err(|_| format!("loopback TEST HARNESS bound: TLS-handshake phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))?;
            let Ok(mut tls) = tls else {
                // A hostname refusal can terminate TLS before HTTP bytes are
                // available. The next accepted connection is the valid retry.
                continue;
            };
            cx.checkpoint()
                .map_err(|_| "loopback server cancelled before HTTP read".to_owned())?;
            let request = time::timeout_at(time::wall_now() + phase_budget, guarded_loopback_read_request(&mut tls))
                .await
                .map_err(|_| format!("loopback TEST HARNESS bound: HTTP-read phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))??;
            requests
                .lock()
                .expect("loopback request record lock")
                .push(request);
            cx.checkpoint()
                .map_err(|_| "loopback server cancelled before HTTP write".to_owned())?;
            time::timeout_at(time::wall_now() + phase_budget, tls.write_all(&response))
                .await
                .map_err(|_| format!("loopback TEST HARNESS bound: HTTP-write phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))?
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn guarded_loopback_fetches(
        hosts: &[&str],
        responses: Vec<Vec<u8>>,
    ) -> (
        Vec<Result<GuardedHttpFetchResponse, GuardedHttpFetchError>>,
        Vec<Vec<u8>>,
    ) {
        fastmcp_core::block_on(async {
            let cx = Cx::current().expect("runtime installs loopback context");
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .expect("bind loopback listener");
            let authority = listener.local_addr().expect("loopback listener address");
            // A PER-PHASE budget, not a budget for the whole sequence. See
            // `guarded_loopback_serve`. 10s is ~200x the observed cost of a
            // loopback phase, so it never fires on a healthy run at any load we
            // have measured, while still making a genuine hang terminate.
            let phase_budget = Duration::from_secs(10);
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = Arc::clone(&requests);
            let mut server = cx
                .spawn(move |server_cx| {
                    guarded_loopback_serve(
                        server_cx,
                        phase_budget,
                        listener,
                        responses,
                        server_requests,
                    )
                })
                .expect("spawn loopback TLS server");
            let fetcher = GuardedHttpFetcher::new_loopback_test_authority(
                guarded_test_policy_with_handshake(
                    1024,
                    Duration::from_secs(1),
                    Duration::from_millis(100),
                ),
                authority,
                guarded_loopback_root(),
            )
            .expect("private loopback fetcher");
            let mut results = Vec::with_capacity(hosts.len());
            for host in hosts {
                let url =
                    GuardedHttpsUrl::parse(&format!("https://{host}:{}/proof", authority.port()))
                        .expect("loopback proof URL");
                results.push(fetcher.fetch(&cx, &url).await);
            }
            // The join gets its own fresh bound too. By now the server has either
            // finished its response list or is parked in one phase, so one full
            // phase budget plus slack is the tightest sound bound available.
            let join_deadline = time::wall_now() + phase_budget + Duration::from_secs(1);
            match time::timeout_at(join_deadline, server.join(&cx)).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => panic!("loopback TLS server: {error}"),
                Ok(Err(error)) => panic!("loopback TLS server task: {error}"),
                Err(_) => {
                    // The dropped timed join has already requested task
                    // cancellation; make that explicit and give settlement a
                    // second, independent finite bound before panicking.
                    server.abort();
                    let settlement_deadline = time::wall_now() + Duration::from_millis(250);
                    match time::timeout_at(settlement_deadline, server.join(&cx)).await {
                        Ok(Ok(Ok(()))) => panic!("loopback TLS server exceeded its join deadline"),
                        Ok(Ok(Err(error))) => {
                            panic!("loopback TLS server exceeded join deadline: {error}")
                        }
                        Ok(Err(error)) => {
                            panic!("loopback TLS server cancellation settled: {error}")
                        }
                        Err(error) => {
                            panic!(
                                "loopback TLS server failed bounded cancellation settlement: {error:?}"
                            )
                        }
                    }
                }
            }
            let requests = requests
                .lock()
                .expect("loopback request record lock")
                .clone();
            (results, requests)
        })
    }

    /// An independent self-signed CA (`FastMCP OAuth TEST ONLY Root`), copied
    /// from the fastmcp-client OAuth fixture. The loopback chain does not
    /// chain to it, so it is the "CA outside the root set" for X7(ii).
    const GUARDED_OUTSIDE_ROOT_PEM: &[u8] = br"-----BEGIN CERTIFICATE-----
MIIBgzCCASmgAwIBAgICA+kwCgYIKoZIzj0EAwIwJzElMCMGA1UEAwwcRmFzdE1D
UCBPQXV0aCBURVNUIE9OTFkgUm9vdDAeFw0yMDAxMDEwMDAwMDBaFw00OTEyMzEw
MDAwMDBaMCcxJTAjBgNVBAMMHEZhc3RNQ1AgT0F1dGggVEVTVCBPTkxZIFJvb3Qw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAS5t2O8JZ0hNjgI38E9Ov6i6mKoDRGo
ApMsykFkvgb6Zm9/5gCZ90eIKw7aWgK6iNs7lbtVY9mysZBIqm6pKQO2o0UwQzAS
BgNVHRMBAf8ECDAGAQH/AgEAMA4GA1UdDwEB/wQEAwIBhjAdBgNVHQ4EFgQU6QNI
rmvMiLoV3jIoCyohXARwI8gwCgYIKoZIzj0EAwIDSAAwRQIgCKOrW3vhzUJ2EyuY
vQUTdqGFhy0zEHj4ITFLvXPz1X8CIQCLKD4EKCvS/zkBSu/6uee1WV9d97UpK3yW
X/aCEJ5+hA==
-----END CERTIFICATE-----";

    fn guarded_outside_root() -> Certificate {
        Certificate::from_pem(GUARDED_OUTSIDE_ROOT_PEM)
            .expect("outside fixture CA")
            .into_iter()
            .next()
            .expect("one outside fixture CA")
    }

    /// Reads one complete request: the header block, then exactly
    /// `Content-Length` body bytes. The header-only reader above cannot see a
    /// POST body, which X7(i) compares byte for byte.
    async fn guarded_loopback_read_full_request(
        tls: &mut asupersync::tls::TlsStream<NativeTcpStream>,
    ) -> Result<Vec<u8>, String> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if request.len() > 8 * 1024 {
                return Err("loopback POST header block exceeded its bound".to_owned());
            }
            let read = tls
                .read(&mut chunk)
                .await
                .map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("loopback POST ended before the header terminator".to_owned());
            }
            request.extend_from_slice(&chunk[..read]);
        };
        let head = std::str::from_utf8(&request[..header_end])
            .map_err(|_| "loopback POST header block is not UTF-8".to_owned())?;
        let content_length = head
            .split("\r\n")
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>())
            .transpose()
            .map_err(|_| "loopback POST Content-Length is not decimal".to_owned())?
            .unwrap_or(0);
        let total = header_end + content_length;
        if total > MAX_GUARDED_REQUEST_BODY_BYTES + 8 * 1024 {
            return Err("loopback POST exceeded the request bound".to_owned());
        }
        while request.len() < total {
            let read = tls
                .read(&mut chunk)
                .await
                .map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("loopback POST ended before its declared body".to_owned());
            }
            request.extend_from_slice(&chunk[..read]);
        }
        if request.len() != total {
            return Err("loopback POST sent bytes past its declared body".to_owned());
        }
        Ok(request)
    }

    /// What the POST loopback server observed.
    #[derive(Debug, Default)]
    struct GuardedLoopbackPostObservation {
        /// TCP connections accepted, including refused TLS handshakes and any
        /// connection that arrives during the trailing quiet window.
        accepted_connections: usize,
        /// Complete requests read after a successful TLS handshake.
        requests: Vec<Vec<u8>>,
    }

    /// The POST counterpart of `guarded_loopback_serve`, with the same
    /// PER-PHASE budgets (#2678). One response slot is consumed per accepted
    /// connection. A connection whose TLS handshake fails is counted, gets no
    /// HTTP bytes, and consumes its slot. After the last slot the server keeps
    /// accepting for `quiet_window`, so a follow-up connection (a followed
    /// redirect, say) is counted rather than missed.
    ///
    /// Each response is written, flushed, and closed with close_notify. A
    /// write alone can leave encrypted bytes buffered in the TLS session when
    /// the socket is not writable, and dropping the stream then loses them.
    async fn guarded_loopback_serve_posts(
        cx: Cx,
        phase_budget: Duration,
        quiet_window: Duration,
        listener: TcpListener,
        responses: Vec<Vec<u8>>,
        observation: Arc<Mutex<GuardedLoopbackPostObservation>>,
    ) -> Result<(), String> {
        let acceptor = guarded_loopback_acceptor();
        for response in responses {
            cx.checkpoint()
                .map_err(|_| "loopback POST server cancelled before accept".to_owned())?;
            let (stream, _) = time::timeout_at(time::wall_now() + phase_budget, listener.accept())
                .await
                .map_err(|_| format!("loopback TEST HARNESS bound: accept phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))?
                .map_err(|error| error.to_string())?;
            observation
                .lock()
                .expect("loopback POST observation lock")
                .accepted_connections += 1;
            let tls = time::timeout_at(time::wall_now() + phase_budget, acceptor.accept(stream))
                .await
                .map_err(|_| format!("loopback TEST HARNESS bound: TLS-handshake phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))?;
            let Ok(mut tls) = tls else {
                continue;
            };
            let request = time::timeout_at(
                time::wall_now() + phase_budget,
                guarded_loopback_read_full_request(&mut tls),
            )
            .await
            .map_err(|_| format!("loopback TEST HARNESS bound: HTTP-read phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))??;
            observation
                .lock()
                .expect("loopback POST observation lock")
                .requests
                .push(request);
            time::timeout_at(time::wall_now() + phase_budget, async {
                tls.write_all(&response).await?;
                tls.flush().await?;
                tls.shutdown().await
            })
            .await
            .map_err(|_| format!("loopback TEST HARNESS bound: HTTP-write phase exceeded its {phase_budget:?} per-phase budget; this is the harness deadline, not a failure of the code under test"))?
            .map_err(|error| error.to_string())?;
        }
        if let Ok(accepted) =
            time::timeout_at(time::wall_now() + quiet_window, listener.accept()).await
        {
            accepted.map_err(|error| error.to_string())?;
            observation
                .lock()
                .expect("loopback POST observation lock")
                .accepted_connections += 1;
        }
        Ok(())
    }

    /// One root-set POST attempt: the set the fetcher trusts, the request, and
    /// the host used in the URL.
    struct GuardedLoopbackPostAttempt {
        roots: GuardedRootSet,
        request: GuardedHttpRequest,
        host: &'static str,
    }

    /// The results of a POST loopback run, with each attempt's URL and the
    /// root identity its fetcher reported before posting.
    struct GuardedLoopbackPostRun {
        results: Vec<Result<GuardedHttpFetchResponse, GuardedHttpFetchError>>,
        urls: Vec<GuardedHttpsUrl>,
        root_identities: Vec<String>,
        observation: GuardedLoopbackPostObservation,
    }

    fn guarded_loopback_posts(
        attempts: Vec<GuardedLoopbackPostAttempt>,
        responses: Vec<Vec<u8>>,
    ) -> GuardedLoopbackPostRun {
        fastmcp_core::block_on(async {
            let cx = Cx::current().expect("runtime installs loopback context");
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .expect("bind loopback listener");
            let authority = listener.local_addr().expect("loopback listener address");
            // Per-phase, as in `guarded_loopback_fetches`.
            let phase_budget = Duration::from_secs(10);
            let quiet_window = Duration::from_millis(500);
            let observation = Arc::new(Mutex::new(GuardedLoopbackPostObservation::default()));
            let server_observation = Arc::clone(&observation);
            let mut server = cx
                .spawn(move |server_cx| {
                    guarded_loopback_serve_posts(
                        server_cx,
                        phase_budget,
                        quiet_window,
                        listener,
                        responses,
                        server_observation,
                    )
                })
                .expect("spawn loopback POST server");
            let mut results = Vec::with_capacity(attempts.len());
            let mut urls = Vec::with_capacity(attempts.len());
            let mut root_identities = Vec::with_capacity(attempts.len());
            for attempt in attempts {
                let fetcher = GuardedHttpFetcher::new_loopback_test_authority_with_root_set(
                    guarded_test_policy_with_handshake(
                        1024,
                        Duration::from_secs(1),
                        Duration::from_millis(100),
                    ),
                    authority,
                    &attempt.roots,
                )
                .expect("private loopback root-set fetcher");
                let url = GuardedHttpsUrl::parse(&format!(
                    "https://{}:{}/token",
                    attempt.host,
                    authority.port()
                ))
                .expect("loopback POST URL");
                root_identities.push(fetcher.root_identity().to_owned());
                results.push(fetcher.post(&cx, &url, &attempt.request).await);
                urls.push(url);
            }
            let join_deadline =
                time::wall_now() + phase_budget + quiet_window + Duration::from_secs(1);
            match time::timeout_at(join_deadline, server.join(&cx)).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => panic!("loopback POST server: {error}"),
                Ok(Err(error)) => panic!("loopback POST server task: {error}"),
                Err(_) => {
                    server.abort();
                    let settlement_deadline = time::wall_now() + Duration::from_millis(250);
                    match time::timeout_at(settlement_deadline, server.join(&cx)).await {
                        Ok(Ok(Ok(()))) => panic!("loopback POST server exceeded its join deadline"),
                        Ok(Ok(Err(error))) => {
                            panic!("loopback POST server exceeded join deadline: {error}")
                        }
                        Ok(Err(error)) => {
                            panic!("loopback POST server cancellation settled: {error}")
                        }
                        Err(error) => panic!(
                            "loopback POST server failed bounded cancellation settlement: {error:?}"
                        ),
                    }
                }
            }
            let observation =
                std::mem::take(&mut *observation.lock().expect("loopback POST observation lock"));
            GuardedLoopbackPostRun {
                results,
                urls,
                root_identities,
                observation,
            }
        })
    }

    fn guarded_loopback_member_roots() -> GuardedRootSet {
        GuardedRootSet::new([guarded_loopback_root()]).expect("member root set")
    }

    /// X7(i), lower proof class (cfg(test) loopback, PL-3): the bytes the
    /// server receives equal `guarded_encode_post_request`'s output.
    #[test]
    fn fnd_05_guarded_post_wire_bytes_match_encoder() {
        let request = GuardedHttpRequest::new(
            "application/x-www-form-urlencoded",
            b"grant_type=opaque&scope=wire".to_vec(),
        )
        .expect("admissible POST body")
        .with_authorization("Basic d2lyZS1wcm9vZg==")
        .expect("admissible authorization value");
        let run = guarded_loopback_posts(
            vec![GuardedLoopbackPostAttempt {
                roots: guarded_loopback_member_roots(),
                request: request.clone(),
                host: "foobar.com",
            }],
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()],
        );

        let expected = guarded_encode_post_request(&run.urls[0], &request)
            .expect("encoder output for the posted request");
        assert_eq!(
            run.observation.requests,
            vec![expected],
            "wire bytes must equal the public encoder's bytes"
        );
        assert_eq!(run.observation.accepted_connections, 1);
        let response = run
            .results
            .into_iter()
            .next()
            .expect("one result")
            .expect("POST over the member root set succeeds");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        assert_eq!(response.redirect, None);
        assert_eq!(
            response.provenance.root_policy_revision, run.root_identities[0],
            "provenance reports the folded root identity"
        );
        assert!(
            run.root_identities[0].starts_with("custom-roots.sha256."),
            "a root-set fetcher must not report the caller's WebPKI label: {}",
            run.root_identities[0]
        );
    }

    /// X7(ii), lower proof class: the same chain is accepted under a set that
    /// holds its CA and refused, with a typed TLS error and no HTTP bytes,
    /// under a set that holds only an unrelated CA.
    #[test]
    fn fnd_05_guarded_root_set_wire_admits_member_refuses_nonmember() {
        let request =
            GuardedHttpRequest::new("application/json", b"{}".to_vec()).expect("admissible body");
        let run = guarded_loopback_posts(
            vec![
                GuardedLoopbackPostAttempt {
                    roots: GuardedRootSet::new([guarded_outside_root()])
                        .expect("non-member root set"),
                    request: request.clone(),
                    host: "foobar.com",
                },
                GuardedLoopbackPostAttempt {
                    roots: guarded_loopback_member_roots(),
                    request: request.clone(),
                    host: "foobar.com",
                },
            ],
            vec![
                b"HTTP/1.1 500 Unreachable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ],
        );

        // A handshake TIMEOUT is also `Tls(_)` ("TLS operation timed out
        // after ..."). With a 100 ms budget and this attempt running first on a
        // cold stack, a slow host could green this arm for the wrong reason,
        // so the refusal must be a verification failure, not a timeout.
        match &run.results[0] {
            Err(GuardedHttpFetchError::Tls(message)) => assert!(
                !message.contains("timed out"),
                "the non-member refusal must be certificate verification, not a handshake timeout: {message}"
            ),
            other => panic!("a chain outside the root set must be a typed TLS refusal: {other:?}"),
        }
        assert_eq!(
            run.results[1].as_ref().map(|response| response.status),
            Ok(200),
            "the same chain under its own CA is admitted"
        );
        assert_eq!(
            run.observation.accepted_connections, 2,
            "both attempts reached the listener"
        );
        let expected = guarded_encode_post_request(&run.urls[1], &request)
            .expect("encoder output for the admitted request");
        assert_eq!(
            run.observation.requests,
            vec![expected],
            "the refused attempt wrote no HTTP bytes; only the admitted one did"
        );
        assert_ne!(
            run.root_identities[0], run.root_identities[1],
            "different root sets report different identities"
        );
    }

    /// X7(iii), lower proof class: a 3xx answer to POST is reported as data,
    /// and the server sees exactly one connection and one request, including
    /// during a quiet window after the response.
    #[test]
    fn fnd_05_guarded_post_redirect_is_reported_not_followed() {
        let request =
            GuardedHttpRequest::new("application/json", b"{\"a\":1}".to_vec()).expect("body");
        let run = guarded_loopback_posts(
            vec![GuardedLoopbackPostAttempt {
                roots: guarded_loopback_member_roots(),
                request,
                host: "foobar.com",
            }],
            vec![b"HTTP/1.1 307 Temporary Redirect\r\nLocation: https://foobar.com/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()],
        );

        let response = run
            .results
            .into_iter()
            .next()
            .expect("one result")
            .expect("redirect response is data, not an error");
        assert_eq!(
            response.redirect,
            Some(GuardedHttpRedirect {
                status: 307,
                location: Some("https://foobar.com/elsewhere".to_owned()),
            })
        );
        assert_eq!(
            run.observation.requests.len(),
            1,
            "POST must not replay after a redirect"
        );
        assert_eq!(
            run.observation.accepted_connections, 1,
            "no follow-up connection may open, even after the response"
        );
    }

    impl GuardedHttpResolver for StaticGuardedResolver {
        fn resolve_all(
            &self,
            cx: Cx,
            _host: String,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>,
        > {
            let answers = self.answers.clone();
            Box::pin(async move {
                guarded_fetch_checkpoint(&cx)?;
                Ok(answers)
            })
        }
    }

    struct ScriptedGuardedResolver {
        answers: Arc<Mutex<VecDeque<Vec<IpAddr>>>>,
    }

    impl GuardedHttpResolver for ScriptedGuardedResolver {
        fn resolve_all(
            &self,
            cx: Cx,
            _host: String,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>,
        > {
            let answers = self
                .answers
                .lock()
                .expect("scripted resolver lock")
                .pop_front()
                .expect("one answer set per public fetch");
            Box::pin(async move {
                guarded_fetch_checkpoint(&cx)?;
                Ok(answers)
            })
        }
    }

    struct DelayedGuardedResolver {
        delay: Duration,
        answers: Vec<IpAddr>,
    }

    impl GuardedHttpResolver for DelayedGuardedResolver {
        fn resolve_all(
            &self,
            cx: Cx,
            _host: String,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>,
        > {
            let answers = self.answers.clone();
            let delay = self.delay;
            Box::pin(async move {
                time::sleep(time::wall_now(), delay).await;
                guarded_fetch_checkpoint(&cx)?;
                Ok(answers)
            })
        }
    }

    struct PendingGuardedResolver {
        dropped: Arc<AtomicUsize>,
        cancel_on_first_poll: bool,
    }

    impl GuardedHttpResolver for PendingGuardedResolver {
        fn resolve_all(
            &self,
            cx: Cx,
            _host: String,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>,
        > {
            Box::pin(PendingGuardedResolverFuture {
                dropped: Arc::clone(&self.dropped),
                cx,
                cancel_on_first_poll: self.cancel_on_first_poll,
            })
        }
    }

    struct PendingGuardedResolverFuture {
        dropped: Arc<AtomicUsize>,
        cx: Cx,
        cancel_on_first_poll: bool,
    }

    impl Future for PendingGuardedResolverFuture {
        type Output = Result<Vec<IpAddr>, GuardedHttpFetchError>;

        fn poll(
            mut self: Pin<&mut Self>,
            task_cx: &mut std::task::Context<'_>,
        ) -> Poll<Self::Output> {
            if self.cancel_on_first_poll {
                self.cx.set_cancel_requested(true);
                self.cancel_on_first_poll = false;
                task_cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    }

    impl Drop for PendingGuardedResolverFuture {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct RecordingGuardedExchange {
        requests: Arc<Mutex<Vec<GuardedHttpTestRequest>>>,
        responses: Arc<Mutex<VecDeque<Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>>>>,
    }

    impl GuardedHttpTestExchange for RecordingGuardedExchange {
        fn exchange(
            &self,
            cx: Cx,
            request: GuardedHttpTestRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>>
                    + Send
                    + 'static,
            >,
        > {
            let requests = Arc::clone(&self.requests);
            let responses = Arc::clone(&self.responses);
            Box::pin(async move {
                guarded_fetch_checkpoint(&cx)?;
                requests
                    .lock()
                    .expect("test exchange request lock")
                    .push(request.clone());
                let mut response = responses
                    .lock()
                    .expect("test exchange response lock")
                    .pop_front()
                    .expect("one test response per public fetch")?;
                response.provenance.host = request.host;
                response.provenance.selected_address = request.selected_address;
                Ok(response)
            })
        }
    }

    struct PendingGuardedExchange {
        dropped: Arc<AtomicUsize>,
        started: Arc<AtomicUsize>,
    }

    impl GuardedHttpTestExchange for PendingGuardedExchange {
        fn exchange(
            &self,
            _cx: Cx,
            _request: GuardedHttpTestRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>>
                    + Send
                    + 'static,
            >,
        > {
            self.started.fetch_add(1, Ordering::AcqRel);
            Box::pin(PendingGuardedExchangeFuture {
                dropped: Arc::clone(&self.dropped),
            })
        }
    }

    struct PendingGuardedExchangeFuture {
        dropped: Arc<AtomicUsize>,
    }

    impl Future for PendingGuardedExchangeFuture {
        type Output = Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>;

        fn poll(self: Pin<&mut Self>, _task_cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingGuardedExchangeFuture {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct DelayedGuardedExchange {
        delay: Duration,
        response: GuardedHttpTestWireResponse,
        started: Arc<AtomicUsize>,
    }

    impl GuardedHttpTestExchange for DelayedGuardedExchange {
        fn exchange(
            &self,
            cx: Cx,
            _request: GuardedHttpTestRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>>
                    + Send
                    + 'static,
            >,
        > {
            let delay = self.delay;
            let response = self.response.clone();
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                started.fetch_add(1, Ordering::AcqRel);
                time::sleep(time::wall_now(), delay).await;
                guarded_fetch_checkpoint(&cx)?;
                Ok(response)
            })
        }
    }

    fn guarded_test_policy(body_limit: usize, deadline: Duration) -> GuardedHttpFetchPolicy {
        guarded_test_policy_with_handshake(body_limit, deadline, Duration::from_millis(50))
    }

    fn guarded_test_policy_with_handshake(
        body_limit: usize,
        deadline: Duration,
        tls_handshake_timeout: Duration,
    ) -> GuardedHttpFetchPolicy {
        GuardedHttpFetchPolicy::new(body_limit, deadline, tls_handshake_timeout, "webpki-r1")
            .expect("finite guarded test policy")
    }

    fn guarded_test_wire_response(
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> GuardedHttpTestWireResponse {
        GuardedHttpTestWireResponse {
            status,
            headers,
            body,
            provenance: guarded_test_provenance(),
        }
    }

    fn guarded_test_fetcher(
        answers: Vec<IpAddr>,
        body_limit: usize,
        responses: Vec<Result<GuardedHttpTestWireResponse, GuardedHttpFetchError>>,
    ) -> (GuardedHttpFetcher, Arc<Mutex<Vec<GuardedHttpTestRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let exchange = Arc::new(RecordingGuardedExchange {
            requests: Arc::clone(&requests),
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
        });
        let fetcher = GuardedHttpFetcher::new_with_test_exchange(
            Arc::new(StaticGuardedResolver { answers }),
            guarded_test_policy(body_limit, Duration::from_secs(1)),
            exchange,
        )
        .expect("test fetcher");
        (fetcher, requests)
    }

    fn guarded_fetch_for_test(
        fetcher: &GuardedHttpFetcher,
        url: &GuardedHttpsUrl,
    ) -> Result<GuardedHttpFetchResponse, GuardedHttpFetchError> {
        fastmcp_core::block_on(async {
            let cx = Cx::current().expect("runtime installs fetch context");
            fetcher.fetch(&cx, url).await
        })
    }

    fn guarded_test_provenance() -> GuardedHttpPeerProvenance {
        GuardedHttpPeerProvenance {
            host: "public.example".to_owned(),
            selected_address: SocketAddr::from(([93, 184, 216, 34], 443)),
            leaf_certificate_sha256: [0; 32],
            alpn: Some(b"http/1.1".to_vec()),
            tls_protocol: Some("TLSv1_3".to_owned()),
            root_policy_revision: "webpki-r1".to_owned(),
        }
    }

    #[test]
    fn guarded_fetch_public_path_fences_all_answers_and_records_selected_route() {
        let url =
            GuardedHttpsUrl::parse("https://PUBLIC.example:8443/a?b=c").expect("bounded HTTPS URL");
        let (fetcher, requests) = guarded_test_fetcher(
            vec![
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)),
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            ],
            16,
            vec![Ok(guarded_test_wire_response(
                200,
                Vec::new(),
                b"public".to_vec(),
            ))],
        );

        let response = guarded_fetch_for_test(&fetcher, &url).expect("public guarded fetch");
        let calls = requests.lock().expect("test exchange request lock");

        assert_eq!(response.body, b"public");
        assert_eq!(response.provenance.host, "public.example");
        assert_eq!(
            response.provenance.selected_address,
            SocketAddr::from(([93, 184, 216, 34], 8443))
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].authority, "public.example:8443");
        assert_eq!(calls[0].target, "/a?b=c");
        assert_eq!(
            calls[0].selected_address,
            response.provenance.selected_address
        );
    }

    #[test]
    fn rh5_guarded_fetch_private_answer_fences_connect_then_valid_retry_uses_same_fetcher() {
        let public = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
        let private = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
        let resolver = Arc::new(ScriptedGuardedResolver {
            answers: Arc::new(Mutex::new(VecDeque::from([
                vec![public, private],
                vec![public],
            ]))),
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        let exchange = Arc::new(RecordingGuardedExchange {
            requests: Arc::clone(&requests),
            responses: Arc::new(Mutex::new(VecDeque::from([Ok(
                guarded_test_wire_response(200, Vec::new(), b"retry".to_vec()),
            )]))),
        });
        let fetcher = GuardedHttpFetcher::new_with_test_exchange(
            resolver,
            guarded_test_policy(16, Duration::from_secs(1)),
            exchange,
        )
        .expect("test fetcher");
        let url = GuardedHttpsUrl::parse("https://public.example/").expect("bounded HTTPS URL");
        let public_input = vec![public, private];
        let input_snapshot = public_input.clone();

        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url),
            Err(GuardedHttpFetchError::DisallowedResolvedAddress(private))
        );
        assert_eq!(public_input, input_snapshot);
        assert!(
            requests
                .lock()
                .expect("test exchange request lock")
                .is_empty()
        );

        let retry =
            guarded_fetch_for_test(&fetcher, &url).expect("valid retry after fenced answer");
        assert_eq!(retry.body, b"retry");
        assert_eq!(
            requests.lock().expect("test exchange request lock").len(),
            1
        );
    }

    #[test]
    fn guarded_fetch_redirect_is_typed_and_never_follows_a_second_request() {
        let (fetcher, requests) = guarded_test_fetcher(
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            16,
            vec![Ok(guarded_test_wire_response(
                302,
                vec![(
                    "Location".to_owned(),
                    "https://other.example/next".to_owned(),
                )],
                Vec::new(),
            ))],
        );
        let url = GuardedHttpsUrl::parse("https://public.example/first").expect("bounded URL");

        let response = guarded_fetch_for_test(&fetcher, &url).expect("redirect observation");

        assert_eq!(
            response.redirect,
            Some(GuardedHttpRedirect {
                status: 302,
                location: Some("https://other.example/next".to_owned()),
            })
        );
        assert_eq!(
            requests.lock().expect("test exchange request lock").len(),
            1
        );
    }

    #[test]
    fn guarded_fetch_two_public_calls_open_two_fresh_exchanges() {
        let (fetcher, requests) = guarded_test_fetcher(
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            16,
            vec![
                Ok(guarded_test_wire_response(200, Vec::new(), b"one".to_vec())),
                Ok(guarded_test_wire_response(200, Vec::new(), b"two".to_vec())),
            ],
        );
        let url = GuardedHttpsUrl::parse("https://public.example/").expect("bounded URL");

        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url).expect("first").body,
            b"one"
        );
        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url).expect("second").body,
            b"two"
        );
        assert_eq!(
            requests.lock().expect("test exchange request lock").len(),
            2
        );
    }

    #[test]
    fn rh5_guarded_fetch_body_n_and_n_plus_one_and_content_encoding_retry() {
        let body = vec![0xA5; 8];
        let body_snapshot = body.clone();
        let (fetcher, requests) = guarded_test_fetcher(
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            8,
            vec![
                Ok(guarded_test_wire_response(200, Vec::new(), body.clone())),
                Ok(guarded_test_wire_response(200, Vec::new(), vec![0xA5; 9])),
                Ok(guarded_test_wire_response(
                    200,
                    vec![
                        ("Content-Encoding".to_owned(), "identity".to_owned()),
                        ("Content-Encoding".to_owned(), "gzip".to_owned()),
                    ],
                    Vec::new(),
                )),
                Ok(guarded_test_wire_response(
                    200,
                    Vec::new(),
                    b"retry".to_vec(),
                )),
            ],
        );
        let url = GuardedHttpsUrl::parse("https://public.example/").expect("bounded URL");

        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url)
                .expect("N bytes")
                .body
                .len(),
            8
        );
        assert_eq!(body, body_snapshot);
        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url),
            Err(GuardedHttpFetchError::Http(
                "response body exceeded guarded bound".to_owned()
            ))
        );
        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url),
            Err(GuardedHttpFetchError::UnexpectedContentEncoding(
                "gzip".to_owned()
            ))
        );
        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url)
                .expect("valid retry")
                .body,
            b"retry"
        );
        assert_eq!(
            requests.lock().expect("test exchange request lock").len(),
            4
        );
    }

    #[test]
    fn guarded_fetch_pending_resolver_cancellation_settles_and_drops_phase() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let exchange = Arc::new(PendingGuardedExchange {
            dropped: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(AtomicUsize::new(0)),
        });
        let fetcher = GuardedHttpFetcher::new_with_test_exchange(
            Arc::new(PendingGuardedResolver {
                dropped: Arc::clone(&dropped),
                cancel_on_first_poll: true,
            }),
            guarded_test_policy(16, Duration::from_millis(100)),
            exchange,
        )
        .expect("test fetcher");
        let url = GuardedHttpsUrl::parse("https://public.example/").expect("bounded URL");

        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url),
            Err(GuardedHttpFetchError::Cancelled)
        );
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn guarded_fetch_pending_resolver_deadline_settles_and_drops_phase() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let exchange_started = Arc::new(AtomicUsize::new(0));
        let deadline = Duration::from_millis(5);
        let fetcher = GuardedHttpFetcher::new_with_test_exchange(
            Arc::new(PendingGuardedResolver {
                dropped: Arc::clone(&dropped),
                cancel_on_first_poll: false,
            }),
            // Only the helper's TLS-handshake bound changes: the planted
            // resolver deadline, pending future, and zero-exchange assertion
            // remain exactly the same.
            guarded_test_policy_with_handshake(16, deadline, deadline),
            Arc::new(PendingGuardedExchange {
                dropped: Arc::new(AtomicUsize::new(0)),
                started: Arc::clone(&exchange_started),
            }),
        )
        .expect("test fetcher");
        let url = GuardedHttpsUrl::parse("https://public.example/").expect("bounded URL");

        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url),
            Err(GuardedHttpFetchError::DeadlineExceeded)
        );
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        assert_eq!(exchange_started.load(Ordering::Acquire), 0);
    }

    #[test]
    fn guarded_fetch_resolver_delay_consumes_the_single_exchange_budget() {
        let exchange_started = Arc::new(AtomicUsize::new(0));
        let deadline = Duration::from_millis(500);
        let fetcher = GuardedHttpFetcher::new_with_test_exchange(
            Arc::new(DelayedGuardedResolver {
                delay: Duration::from_millis(25),
                answers: vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
            }),
            // Only the helper's TLS-handshake bound changes. The resolver has
            // ample room to start the exchange even under suite load, while
            // the exchange still cannot finish inside this one fetch budget.
            guarded_test_policy_with_handshake(16, deadline, deadline),
            Arc::new(DelayedGuardedExchange {
                delay: Duration::from_secs(1),
                response: guarded_test_wire_response(200, Vec::new(), b"late".to_vec()),
                started: Arc::clone(&exchange_started),
            }),
        )
        .expect("test fetcher");
        let url = GuardedHttpsUrl::parse("https://public.example/").expect("bounded URL");

        assert_eq!(
            guarded_fetch_for_test(&fetcher, &url),
            Err(GuardedHttpFetchError::DeadlineExceeded)
        );
        assert_eq!(exchange_started.load(Ordering::Acquire), 1);
    }

    #[test]
    fn guarded_loopback_real_wire_200_records_request_body_and_leaf_provenance() {
        let (results, requests) = guarded_loopback_fetches(
            &["foobar.com"],
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()],
        );
        let response = results
            .into_iter()
            .next()
            .expect("one response")
            .expect("200 response");

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        // The digest must be OF THE PEER'S LEAF, not merely non-zero. The old
        // `!= [0; 32]` here was the weakest predicate available: a constant, a
        // digest of the issuing CA, or a digest of the wrong certificate
        // entirely all satisfied it. Comparing against the fixture leaf's own
        // DER is what makes this prove derivation rather than presence.
        let fixture_chain = Certificate::from_pem(GUARDED_LOOPBACK_CHAIN_PEM)
            .expect("bounded loopback certificate chain");
        let fixture_leaf_digest =
            guarded_leaf_certificate_sha256(Some(fixture_chain[0].as_der().to_vec()))
                .expect("fixture leaf digest");
        assert_eq!(
            response.provenance.leaf_certificate_sha256, fixture_leaf_digest,
            "recorded provenance must carry the digest of the PEER'S LEAF certificate"
        );
        // bd-0a6lr: `alpn` and `tls_protocol` are DERIVED from the live session
        // in `guarded_peer_provenance` and were asserted NOWHERE in the crate, so
        // an implementation that reported `None` for both passed every test.
        // The fixture acceptor pins ALPN to `http/1.1`, which makes the expected
        // value fixed by the fixture rather than guessed.
        assert_eq!(
            response.provenance.alpn.as_deref(),
            Some(&b"http/1.1"[..]),
            "provenance must report the ALPN the acceptor actually negotiated"
        );
        // `tls_protocol` is checked for presence and non-emptiness, NOT for an
        // exact version string, and that restraint is deliberate. The negotiated
        // version is mutable content: pinning it would fail this test on a
        // legitimate TLS-library upgrade while proving nothing extra about
        // derivation. `None` is what a non-deriving implementation returns, so
        // presence is the discriminating half; an exact match would be a frozen
        // measurement that rots.
        let tls_protocol = response
            .provenance
            .tls_protocol
            .as_deref()
            .expect("provenance must report the negotiated TLS version, not None");
        assert!(
            !tls_protocol.is_empty(),
            "the negotiated TLS version must be a real identity, not an empty placeholder"
        );
        // And not simply some certificate from the chain: the issuer is present
        // in the same PEM, so an implementation digesting the wrong chain
        // element would still have passed the equality above by accident had we
        // not pinned which element.
        let fixture_issuer_digest =
            guarded_leaf_certificate_sha256(Some(fixture_chain[1].as_der().to_vec()))
                .expect("fixture issuer digest");
        assert_ne!(
            response.provenance.leaf_certificate_sha256, fixture_issuer_digest,
            "the leaf digest must not be the issuing certificate's digest"
        );
        assert_eq!(response.provenance.host, "foobar.com");
        assert!(response.provenance.selected_address.ip().is_loopback());
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(b"GET /proof HTTP/1.1\r\n"));
        assert!(
            requests[0]
                .windows(b"Host: foobar.com:".len())
                .any(|window| window == b"Host: foobar.com:")
        );
        assert!(
            requests[0]
                .windows(b"Accept-Encoding: identity".len())
                .any(|window| window == b"Accept-Encoding: identity")
        );
    }

    #[test]
    fn rh5_guarded_loopback_wrong_host_has_zero_http_bytes_then_valid_retry() {
        let (results, requests) = guarded_loopback_fetches(
            &["wrong.example", "foobar.com"],
            vec![
                Vec::new(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
            ],
        );

        assert!(matches!(results[0], Err(GuardedHttpFetchError::Tls(_))));

        // THE SECURITY PROPERTY, on its own, so that a fence leak is reported AS
        // a fence leak. Previously this was folded into `requests.len() == 1`,
        // which conflated two independent claims -- "the refused host wrote
        // nothing" and "the valid retry wrote one" -- into a single number. A
        // failure could not be read from its left/right values: 0 meant the
        // RETRY was lost (fence held) while 2 meant the fence LEAKED, and the
        // message blamed the fence either way. Identify the leak by the Host
        // header, so this holds whatever the total count happens to be.
        let leaked = requests
            .iter()
            .filter(|request| {
                request
                    .windows(b"Host: wrong.example:".len())
                    .any(|window| window == b"Host: wrong.example:")
            })
            .count();
        assert_eq!(
            leaked, 0,
            "FENCE LEAK: the hostname-refused fetch wrote {leaked} HTTP request(s); \
             hostname refusal must precede any HTTP bytes"
        );

        // THE LIVENESS PROPERTY, separately: a refusal must not poison the same
        // fetcher for a subsequent valid host. Failing any assertion below does
        // NOT mean the fence leaked -- the assertion above already proved it did
        // not -- it means the retry did not complete.
        assert_eq!(
            results[1].as_ref().expect("same fetcher valid retry").body,
            b"ok"
        );
        assert_eq!(
            requests.len(),
            1,
            "the valid retry must be the only request that reached the server"
        );
        assert!(
            requests[0]
                .windows(b"Host: foobar.com:".len())
                .any(|window| window == b"Host: foobar.com:")
        );
    }

    #[test]
    fn rh5_guarded_loopback_real_wire_redirect_is_typed_without_follow_up() {
        let (results, requests) = guarded_loopback_fetches(
            &["foobar.com"],
            vec![b"HTTP/1.1 302 Found\r\nLocation: https://other.example/next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()],
        );
        let response = results
            .into_iter()
            .next()
            .expect("one response")
            .expect("redirect response");

        assert_eq!(
            response.redirect,
            Some(GuardedHttpRedirect {
                status: 302,
                location: Some("https://other.example/next".to_owned()),
            })
        );
        assert_eq!(
            requests.len(),
            1,
            "redirect must not open a follow-up connection"
        );
    }

    #[test]
    fn rh5_guarded_policy_time_bounds_accept_n_and_reject_n_plus_one() {
        let root = "webpki-r1";
        assert!(
            GuardedHttpFetchPolicy::new(
                1,
                MAX_GUARDED_FETCH_DEADLINE,
                MAX_GUARDED_TLS_HANDSHAKE_TIMEOUT,
                root,
            )
            .is_ok()
        );
        assert_eq!(
            GuardedHttpFetchPolicy::new(
                1,
                MAX_GUARDED_FETCH_DEADLINE + Duration::from_nanos(1),
                MAX_GUARDED_TLS_HANDSHAKE_TIMEOUT,
                root,
            ),
            Err(GuardedHttpFetchError::InvalidPolicy("full fetch deadline"))
        );
        assert_eq!(
            GuardedHttpFetchPolicy::new(
                1,
                MAX_GUARDED_FETCH_DEADLINE,
                MAX_GUARDED_TLS_HANDSHAKE_TIMEOUT + Duration::from_nanos(1),
                root,
            ),
            Err(GuardedHttpFetchError::InvalidPolicy(
                "TLS handshake timeout"
            ))
        );
        assert_eq!(
            GuardedHttpFetchPolicy::new(
                1,
                Duration::from_secs(1),
                Duration::from_secs(1) + Duration::from_nanos(1),
                root,
            ),
            Err(GuardedHttpFetchError::InvalidPolicy(
                "TLS handshake exceeds full fetch deadline"
            ))
        );
    }

    #[test]
    fn rh5_guarded_leaf_provenance_requires_admitted_certificate_and_valid_retry() {
        let leaf = vec![0xA5; MAX_GUARDED_LEAF_CERTIFICATE_BYTES];
        let oversized = vec![0xA5; MAX_GUARDED_LEAF_CERTIFICATE_BYTES + 1];
        let oversized_snapshot = oversized.clone();

        assert_eq!(
            guarded_leaf_certificate_sha256(None),
            Err(GuardedHttpFetchError::PeerCertificateUnavailable)
        );
        assert_eq!(
            guarded_leaf_certificate_sha256(Some(oversized.clone())),
            Err(GuardedHttpFetchError::PeerCertificateTooLarge)
        );
        assert_eq!(oversized, oversized_snapshot);
        assert_ne!(
            guarded_leaf_certificate_sha256(Some(leaf)).expect("admitted leaf digest"),
            [0; 32]
        );
    }

    #[test]
    fn guarded_https_url_public_address_selects_deterministically() {
        let url =
            GuardedHttpsUrl::parse("https://PUBLIC.example:8443/a?b=c").expect("bounded HTTPS URL");
        let selected = guarded_select_address(
            vec![
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)),
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            ],
            url.port(),
        )
        .expect("all answers are public");

        assert_eq!(url.host(), "public.example");
        assert_eq!(url.target(), "/a?b=c");
        assert_eq!(selected, SocketAddr::from(([93, 184, 216, 34], 8443)));
    }

    #[test]
    fn rh5_guarded_private_or_mapped_answer_rejects_before_any_connect_selection() {
        let public = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
        let private = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
        let mapped_private = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 7));
        let well_known_nat64 = IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808));
        let local_use_nat64 = IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0x0808, 0x0808));
        let adjacent_public = IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 2, 0, 0, 0, 0x0808, 0x0808));
        for prohibited in [private, mapped_private, well_known_nat64, local_use_nat64] {
            let answers = vec![public, prohibited];
            let input_snapshot = answers.clone();

            assert_eq!(
                guarded_select_address(answers.clone(), 443),
                Err(GuardedHttpFetchError::DisallowedResolvedAddress(
                    canonical_guarded_ip(prohibited)
                ))
            );
            assert_eq!(answers, input_snapshot);
        }
        assert_eq!(
            guarded_select_address(vec![public], 443),
            Ok(SocketAddr::from(([93, 184, 216, 34], 443)))
        );
        assert!(is_public_guarded_ip(adjacent_public));
    }

    #[test]
    fn rh5_guarded_loopback_exception_is_exact_socket_only() {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let allowed = SocketAddr::from((Ipv4Addr::LOCALHOST, 9443));

        assert_eq!(
            guarded_select_address_with_test_loopback(vec![loopback], 9443, Some(allowed)),
            Ok(allowed)
        );
        assert_eq!(
            guarded_select_address_with_test_loopback(vec![loopback], 9444, Some(allowed)),
            Err(GuardedHttpFetchError::DisallowedResolvedAddress(loopback))
        );
    }

    #[test]
    fn guarded_redirect_is_observed_without_follow_up_request_state() {
        let response = guarded_admit_native_response(
            302,
            vec![(
                "Location".to_owned(),
                "https://other.example/next".to_owned(),
            )],
            Vec::new(),
            64,
            guarded_test_provenance(),
        )
        .expect("single redirect response is observable");

        assert_eq!(
            response.redirect,
            Some(GuardedHttpRedirect {
                status: 302,
                location: Some("https://other.example/next".to_owned()),
            })
        );
        assert_eq!(response.provenance.host, "public.example");
    }

    #[test]
    fn rh5_guarded_response_body_bound_accepts_n_and_rejects_n_plus_one() {
        let provenance = guarded_test_provenance();
        let accepted =
            guarded_admit_native_response(200, Vec::new(), vec![0xA5; 8], 8, provenance.clone())
                .expect("exact body bound admits");
        let body = vec![0xA5; 9];
        let input_snapshot = body.clone();

        assert_eq!(accepted.body.len(), 8);
        assert_eq!(
            guarded_admit_native_response(200, Vec::new(), body.clone(), 8, provenance),
            Err(GuardedHttpFetchError::Http(
                "response body exceeded guarded bound".to_owned()
            ))
        );
        assert_eq!(body, input_snapshot);
    }

    #[test]
    fn rh5_guarded_nonidentity_response_encoding_rejects_without_changing_input() {
        let headers = vec![("Content-Encoding".to_owned(), "gzip".to_owned())];
        let headers_snapshot = headers.clone();
        let body = b"identity-looking-but-not-decoded".to_vec();
        let body_snapshot = body.clone();

        assert_eq!(
            guarded_admit_native_response(
                200,
                headers.clone(),
                body.clone(),
                64,
                guarded_test_provenance(),
            ),
            Err(GuardedHttpFetchError::UnexpectedContentEncoding(
                "gzip".to_owned()
            ))
        );
        assert_eq!(headers, headers_snapshot);
        assert_eq!(body, body_snapshot);
    }

    struct InterruptEveryOtherRead {
        inner: Cursor<Vec<u8>>,
        interrupt_next: bool,
        interruptions: Arc<AtomicUsize>,
        cancel_on_interrupt: Option<Cx>,
    }

    impl Read for InterruptEveryOtherRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.interrupt_next {
                self.interrupt_next = false;
                self.interruptions.fetch_add(1, Ordering::AcqRel);
                if let Some(cx) = self.cancel_on_interrupt.as_ref() {
                    cx.set_cancel_requested(true);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "deterministic transient read interruption",
                ));
            }
            self.interrupt_next = true;
            std::io::Read::read(&mut self.inner, buffer)
        }
    }

    struct CancelAfterSuccessfulRead {
        inner: Cursor<Vec<u8>>,
        cx: Cx,
        reads: Arc<AtomicUsize>,
    }

    impl Read for CancelAfterSuccessfulRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = std::io::Read::read(&mut self.inner, buffer)?;
            if read > 0 && self.reads.fetch_add(1, Ordering::AcqRel) == 0 {
                self.cx.set_cancel_requested(true);
            }
            Ok(read)
        }
    }

    #[derive(Debug)]
    struct FailingSerialize;

    impl serde::Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(<S::Error as serde::ser::Error>::custom(
                "intentional HTTP response encoding failure",
            ))
        }
    }

    fn wait_for_counter(counter: &AtomicUsize, expected: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        while counter.load(Ordering::Acquire) < expected {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    #[test]
    fn test_http_method_parse() {
        assert_eq!(HttpMethod::parse("GET"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::parse("POST"), Some(HttpMethod::Post));
        assert_eq!(HttpMethod::parse("get"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::parse("INVALID"), None);
    }

    #[test]
    fn test_http_status() {
        assert!(HttpStatus::OK.is_success());
        assert!(HttpStatus::BAD_REQUEST.is_client_error());
        assert!(HttpStatus::INTERNAL_SERVER_ERROR.is_server_error());
    }

    #[test]
    fn test_http_request_builder() {
        let request = HttpRequest::new(HttpMethod::Post, "/api/mcp")
            .with_header("Content-Type", "application/json")
            .with_body(b"{\"test\": true}".to_vec())
            .with_query("version", "1");

        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/api/mcp");
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.query.get("version"), Some(&"1".to_string()));
    }

    #[test]
    fn directly_constructed_request_headers_are_case_insensitive_at_admission() {
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["https://trusted.example".to_string()],
            ..HttpHandlerConfig::default()
        };
        let handler = HttpRequestHandler::with_config(config);
        let mut request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_body(r#"{"jsonrpc":"2.0","method":"test","id":1}"#);
        request
            .headers
            .insert("Content-Type".to_string(), "application/json".to_string());
        request
            .headers
            .insert("Origin".to_string(), "https://trusted.example".to_string());

        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.header("ORIGIN"), Some("https://trusted.example"));
        assert!(handler.parse_request(&request).is_ok());

        request
            .headers
            .insert("Origin".to_string(), "https://denied.example".to_string());
        assert!(matches!(
            handler.parse_request(&request),
            Err(HttpError::OriginNotAllowed(origin)) if origin == "https://denied.example"
        ));
    }

    #[test]
    fn directly_constructed_case_insensitive_duplicate_headers_are_rejected() {
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["https://trusted.example".to_string()],
            ..HttpHandlerConfig::default()
        };
        let handler = HttpRequestHandler::with_config(config);
        let mut request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_body(r#"{"jsonrpc":"2.0","method":"test","id":1}"#);
        request
            .headers
            .insert("Origin".to_string(), "https://trusted.example".to_string());
        request
            .headers
            .insert("origin".to_string(), "https://denied.example".to_string());

        assert!(matches!(
            handler.parse_request(&request),
            Err(HttpError::InvalidHeader(message)) if message.contains("duplicate")
        ));

        request.method = HttpMethod::Options;
        request.headers.insert(
            "Access-Control-Request-Method".to_string(),
            "POST".to_string(),
        );
        assert_eq!(
            handler.handle_options(&request).status,
            HttpStatus::BAD_REQUEST
        );
    }

    #[test]
    fn test_http_response_builder() {
        let response = HttpResponse::ok()
            .with_header("X-Custom", "value")
            .with_body(b"Hello".to_vec());

        assert_eq!(response.status, HttpStatus::OK);
        assert_eq!(response.headers.get("x-custom"), Some(&"value".to_string()));
        assert_eq!(response.body, b"Hello");
    }

    #[test]
    fn test_http_response_json() {
        let data = serde_json::json!({"result": "ok"});
        let response = HttpResponse::ok().with_json(&data);

        assert_ne!(response.body.len(), 0);
        assert_eq!(
            response.headers.get("content-type"),
            Some(&"application/json".to_string())
        );
    }

    #[test]
    fn http_response_json_encoding_failure_is_typed_and_fails_closed() {
        let error = HttpResponse::ok()
            .try_with_json(&FailingSerialize)
            .expect_err("fallible JSON response builder must preserve serializer failure");
        assert!(matches!(error, HttpError::JsonError(_)));

        let response = HttpResponse::ok()
            .with_header("x-response-policy", "preserved")
            .with_json(&FailingSerialize);
        assert_eq!(response.status, HttpStatus::INTERNAL_SERVER_ERROR);
        assert_eq!(response.body, JSON_ENCODING_ERROR_BODY);
        assert_ne!(response.body.len(), 0);
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            response
                .headers
                .get("x-response-policy")
                .map(String::as_str),
            Some("preserved")
        );
    }

    #[test]
    fn test_http_response_cors() {
        let response = HttpResponse::ok().with_cors("https://example.com");

        assert_eq!(
            response.headers.get("access-control-allow-origin"),
            Some(&"https://example.com".to_string())
        );
        assert_eq!(
            response.headers.get("access-control-allow-methods"),
            Some(&"POST, OPTIONS".to_string())
        );
        assert_eq!(
            response.headers.get("vary").map(String::as_str),
            Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
        );

        let rejected = HttpResponse::ok().with_cors("https://example.com\r\nx-injected: yes");
        assert!(!rejected.headers.contains_key("access-control-allow-origin"));
        assert!(!rejected.headers.contains_key("vary"));
    }

    #[test]
    fn default_http_handler_rejects_cross_origin_preflight() {
        let handler = HttpRequestHandler::new();
        let request = HttpRequest::new(HttpMethod::Options, "/mcp/v1")
            .with_header("Origin", "https://example.com")
            .with_header("Access-Control-Request-Method", "POST");

        let response = handler.handle_options(&request);
        assert_eq!(response.status, HttpStatus::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn test_http_handler_parse_request() {
        let handler = HttpRequestHandler::new();

        // Valid request
        let json_rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "test",
            "id": 1
        });
        let request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_body(serde_json::to_vec(&json_rpc).unwrap());

        let result = handler.parse_request(&request);
        assert!(result.is_ok());

        // Invalid method
        let request = HttpRequest::new(HttpMethod::Get, "/mcp/v1");
        assert!(handler.parse_request(&request).is_err());

        // Invalid content type
        let request =
            HttpRequest::new(HttpMethod::Post, "/mcp/v1").with_header("Content-Type", "text/plain");
        assert!(handler.parse_request(&request).is_err());

        let request = HttpRequest::new(HttpMethod::Post, "/wrong")
            .with_header("Content-Type", "application/json")
            .with_body(serde_json::to_vec(&json_rpc).unwrap());
        assert!(matches!(
            handler.parse_request(&request),
            Err(HttpError::InvalidPath(path)) if path == "/wrong"
        ));
    }

    #[test]
    fn http_handler_response_encoding_failure_is_typed_and_returns_500() {
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["https://allowed.example".to_string()],
            ..HttpHandlerConfig::default()
        };
        let handler = HttpRequestHandler::with_config(config);

        let typed_error = serde_json::to_vec(&FailingSerialize)
            .expect_err("fixture serializer must fail response encoding");
        let typed_result = handler.try_create_response_from_encoding(
            Err(CodecError::from(typed_error)),
            Some("https://allowed.example"),
        );
        assert!(matches!(typed_result, Err(HttpError::CodecError(_))));

        let fallback_error = serde_json::to_vec(&FailingSerialize)
            .expect_err("fixture serializer must fail response encoding");
        let response = handler.create_response_from_encoding(
            Err(CodecError::from(fallback_error)),
            Some("https://allowed.example"),
        );
        assert_eq!(response.status, HttpStatus::INTERNAL_SERVER_ERROR);
        assert_eq!(response.body, JSON_ENCODING_ERROR_BODY);
        assert_ne!(response.body.len(), 0);
        assert_eq!(
            response
                .headers
                .get("access-control-allow-origin")
                .map(String::as_str),
            Some("https://allowed.example")
        );
    }

    #[test]
    fn test_http_session() {
        let mut session = HttpSession::new("test-session").unwrap();
        assert_eq!(session.id(), "test-session");

        session.set("key", serde_json::json!("value")).unwrap();
        assert_eq!(session.get("key"), Some(&serde_json::json!("value")));

        session.remove("key");
        assert!(session.get("key").is_none());

        assert!(!session.is_expired(Duration::from_secs(3600)));
    }

    #[test]
    fn http_session_id_requires_visible_ascii() {
        for invalid in ["", "contains space", "line\nbreak", "nul\0byte", "é"] {
            assert_eq!(
                HttpSession::new(invalid).unwrap_err(),
                HttpSessionError::InvalidSessionId
            );
        }
        assert!(HttpSession::new("!visible-session~").is_ok());
    }

    #[test]
    fn http_session_state_is_hard_bounded() {
        let mut session = HttpSession::new("bounded-session").unwrap();
        for index in 0..MAX_HTTP_SESSION_ENTRIES {
            session
                .set(format!("key-{index}"), serde_json::json!(index))
                .expect("entry at or below the count limit");
        }
        assert_eq!(
            session.set("one-too-many", serde_json::json!(true)),
            Err(HttpSessionError::CapacityExceeded)
        );
        session
            .set("key-0", serde_json::json!("replacement"))
            .expect("replacement does not consume another entry");

        let oversized = "x".repeat(MAX_HTTP_SESSION_VALUE_BYTES);
        assert_eq!(
            session.set("key-0", serde_json::Value::String(oversized)),
            Err(HttpSessionError::ValueTooLarge)
        );
        assert!(matches!(
            HttpSession::new("x".repeat(MAX_HTTP_SESSION_ID_BYTES + 1)),
            Err(HttpSessionError::InvalidSessionId)
        ));
    }

    #[test]
    fn test_session_store() {
        let store = SessionStore::with_defaults();

        let id = store.create().unwrap();
        assert_ne!(id.len(), 0);

        let session = store.get(&id);
        assert!(session.is_some());

        store.remove(&id);
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn session_store_rejects_above_global_capacity() {
        let store = SessionStore::with_capacity(Duration::from_secs(3600), 1)
            .expect("capacity one is valid");
        let first = store.create().expect("first session admitted");
        assert_eq!(store.count(), 1);
        assert_eq!(store.create(), Err(HttpSessionError::CapacityExceeded));
        store.remove(&first);
        assert!(store.create().is_ok());
    }

    #[test]
    fn session_store_rejects_invalid_capacity_without_panicking() {
        assert!(matches!(
            SessionStore::with_capacity(Duration::from_secs(3600), 0),
            Err(HttpSessionError::InvalidCapacity)
        ));
        assert!(matches!(
            SessionStore::with_capacity(Duration::from_secs(3600), MAX_HTTP_SESSIONS + 1),
            Err(HttpSessionError::InvalidCapacity)
        ));
    }

    #[test]
    fn test_streamable_transport() {
        let transport = StreamableHttpTransport::new();
        let cx = Cx::for_testing();

        // Push a request
        let request = JsonRpcRequest::new("test", None, 1i64);
        transport.push_request(&cx, request).unwrap();

        // Should have a request in queue
        assert_eq!(transport.pending_requests(), 1);
    }

    #[test]
    fn test_http_error_display() {
        let err = HttpError::InvalidMethod("PATCH".to_string());
        assert_eq!(err.to_string(), "invalid HTTP method");

        let err = HttpError::Timeout;
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn test_generate_session_id() {
        let id1 = generate_session_id().unwrap();
        let id2 = generate_session_id().unwrap();

        assert_ne!(id1, id2);
        assert_eq!(id1.len(), 64);
        assert!(id1.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(id1.bytes().all(|byte| !byte.is_ascii_uppercase()));
    }

    #[test]
    fn test_http_transport_read_request_chunked_body_and_query() {
        use std::io::Cursor;

        let body = br#"{"jsonrpc":"2.0","method":"test","id":1}"#;
        let body1 = &body[..10];
        let body2 = &body[10..];

        let raw = format!(
            "POST /mcp/v1?foo=bar&x=y HTTP/1.1\r\n\
Host: example.com\r\n\
Content-Type: application/json\r\n\
Transfer-Encoding: chunked\r\n\
\r\n\
{:x}\r\n\
{}\r\n\
{:x}\r\n\
{}\r\n\
0\r\n\
\r\n",
            body1.len(),
            std::str::from_utf8(body1).unwrap(),
            body2.len(),
            std::str::from_utf8(body2).unwrap(),
        );

        let reader = Cursor::new(raw.into_bytes());
        let mut output = Vec::new();
        let mut transport = HttpTransport::new(reader, &mut output);

        let req = transport.read_request().unwrap();
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.path, "/mcp/v1");
        assert_eq!(req.query.get("foo"), Some(&"bar".to_string()));
        assert_eq!(req.query.get("x"), Some(&"y".to_string()));
        assert_eq!(req.body, body);
    }

    #[test]
    fn chunked_request_rejects_non_empty_trailer_fields_and_latches_closed() {
        let raw = b"POST /mcp/v1 HTTP/1.1\r\n\
Transfer-Encoding: chunked\r\n\
\r\n\
0\r\n\
X-Checksum: abc123\r\n\
\r\n";
        let mut transport = HttpTransport::new(Cursor::new(raw.to_vec()), Vec::new());

        let error = transport.read_request().unwrap_err();

        assert!(matches!(
            error,
            HttpError::InvalidHeader(detail)
                if detail == "HTTP trailer fields are not supported"
        ));
        assert!(transport.closed);
        assert!(matches!(
            transport.recv(&Cx::for_testing()),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn chunked_request_bounds_oversized_trailer_before_line_termination() {
        const HEADER_LIMIT: usize = 64 * 1024;
        const PREFIX: &[u8] = b"X-Trailer: ";

        let mut raw = b"POST /mcp/v1 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_vec();
        raw.extend_from_slice(PREFIX);
        raw.extend(vec![b'a'; HEADER_LIMIT + 1 - PREFIX.len()]);

        let mut transport = HttpTransport::new(Cursor::new(raw), Vec::new());
        let error = transport.read_request().unwrap_err();

        assert!(matches!(
            error,
            HttpError::HeadersTooLarge {
                size,
                max: HEADER_LIMIT
            } if size == HEADER_LIMIT + 1
        ));
        assert!(transport.closed);
    }

    #[test]
    fn chunked_request_rejects_oversized_declaration_before_body_read() {
        let declared_size = 10 * 1024 * 1024 + 1;
        let raw = format!(
            "POST /mcp/v1 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{declared_size:x}\r\n"
        );
        let mut transport = HttpTransport::new(Cursor::new(raw.into_bytes()), Vec::new());

        let error = transport.read_request().unwrap_err();

        let HttpError::BodyTooLarge { size, max } = error else {
            panic!("expected body-too-large error");
        };
        assert_eq!(size, declared_size);
        assert_eq!(max, 10 * 1024 * 1024);
    }

    #[test]
    fn chunked_request_rejects_unimplemented_transfer_coding_chain() {
        let raw = b"POST /mcp/v1 HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        let mut transport = HttpTransport::new(Cursor::new(raw.to_vec()), Vec::new());

        let error = transport.read_request().unwrap_err();

        assert!(matches!(
            error,
            HttpError::UnsupportedTransferEncoding(value) if value == "gzip, chunked"
        ));
    }

    #[test]
    fn http_request_head_is_strict_utf8_and_http_11_three_token_syntax() {
        let malformed_heads = [
            b"POST /mcp/v1 HTTP/1.0\r\n\r\n".to_vec(),
            b"POST /mcp/v1 HTTP/1.1 extra\r\n\r\n".to_vec(),
            b"POST  /mcp/v1 HTTP/1.1\r\n\r\n".to_vec(),
        ];
        for raw in malformed_heads {
            let mut transport = HttpTransport::new(Cursor::new(raw), Vec::new());
            assert!(matches!(
                transport.read_request(),
                Err(HttpError::InvalidRequestLine(_))
            ));
        }

        let mut invalid_utf8 = b"POST /mcp/v1 HTTP/1.1\r\nX-Test: ".to_vec();
        invalid_utf8.push(0xff);
        invalid_utf8.extend_from_slice(b"\r\n\r\n");
        let mut transport = HttpTransport::new(Cursor::new(invalid_utf8), Vec::new());
        assert!(matches!(
            transport.read_request(),
            Err(HttpError::InvalidHeader(_))
        ));
    }

    #[test]
    fn http_request_rejects_malformed_folded_duplicate_and_ambiguous_headers() {
        let malformed = [
            "POST /mcp/v1 HTTP/1.1\r\nContent-Length: nope\r\n\r\n",
            "POST /mcp/v1 HTTP/1.1\r\nHost: one\r\nhost: two\r\n\r\n",
            "POST /mcp/v1 HTTP/1.1\r\nX-Test: one\r\n two\r\n\r\n",
            "POST /mcp/v1 HTTP/1.1\r\nBad Name: value\r\n\r\n",
            "POST /mcp/v1 HTTP/1.1\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
        ];

        for raw in malformed {
            let mut transport =
                HttpTransport::new(Cursor::new(raw.as_bytes().to_vec()), Vec::new());
            assert!(matches!(
                transport.read_request(),
                Err(HttpError::InvalidHeader(_))
            ));
        }
    }

    #[cfg(feature = "legacy-2024-11-05")]
    fn public_modern_listener_wire(duplicate_mcp_method: bool) -> Vec<u8> {
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "weather",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            })),
            2_501_i64,
        );
        let body = serde_json::to_vec(&request).expect("modern listener request serializes");
        let repeated_method = duplicate_mcp_method.then_some("Mcp-Method: tools/call\r\n");
        format!(
            "POST /mcp HTTP/1.1\r\n\
Content-Type: application/json\r\n\
Accept: text/event-stream\r\n\
Accept: application/json\r\n\
MCP-Protocol-Version: 2026-07-28\r\n\
Mcp-Method: tools/call\r\n\
{}\
Mcp-Name: weather\r\n\
Content-Length: {}\r\n\
\r\n",
            repeated_method.unwrap_or(""),
            body.len(),
        )
        .into_bytes()
        .into_iter()
        .chain(body)
        .collect()
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn public_http_listener_preserves_two_accept_lines_and_admits_the_request() {
        use std::io::Cursor;

        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let mut listener =
            HttpTransport::new(Cursor::new(public_modern_listener_wire(false)), Vec::new());

        let request = listener
            .read_request()
            .expect("the public HTTP listener admits repeated Accept fields");
        assert_eq!(
            request.header("accept"),
            Some("text/event-stream,application/json"),
            "both list-valued Accept field lines remain visible to response selection"
        );
        let response = session
            .handle(&Cx::for_testing(), request)
            .expect("the parsed request reaches the public modern endpoint");
        assert!(matches!(
            response,
            DualEraHttpEndpointResponse::ModernJson(_)
        ));
        assert_eq!(
            session
                .recv_modern_request(&Cx::for_testing())
                .expect("accepted listener request reaches downstream dispatch")
                .id,
            Some(RequestId::Number(2_501))
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn public_http_listener_rejects_duplicate_singleton_mcp_method_without_downstream_state() {
        use std::io::Cursor;

        let endpoint = dual_era_endpoint();
        let session = endpoint.open_session().expect("endpoint opens a session");
        // This differs from the admitted listener wire only by one repeated
        // singleton MCP binding header.
        let mut listener =
            HttpTransport::new(Cursor::new(public_modern_listener_wire(true)), Vec::new());

        let error = listener
            .read_request()
            .expect_err("duplicate Mcp-Method must fail before endpoint admission");
        assert!(matches!(
            error,
            HttpError::InvalidHeader(detail) if detail == "duplicate header: mcp-method"
        ));
        assert!(listener.closed, "a refused listener request is terminal");
        assert_eq!(
            session.modern_transport.pending_requests(),
            0,
            "the duplicate singleton leaves downstream dispatch state unchanged"
        );
    }

    fn http_response_diagnostic_fixture(value: &str) -> HttpResponse {
        HttpResponse::ok()
            .with_header(
                "location",
                format!("https://client.example/callback?code={value}"),
            )
            .with_header("set-cookie", format!("session={value}; HttpOnly; Secure"))
            .with_json(&serde_json::json!({
                "access_token": value,
                "refresh_token": format!("{value}-refresh")
            }))
    }

    fn assert_http_response_diagnostics_preserve_wire(value: &str) {
        const PUBLIC_VALUE: &str = "public-value-0123456789";
        assert_eq!(value.len(), PUBLIC_VALUE.len());
        let response = http_response_diagnostic_fixture(value);
        let public_response = http_response_diagnostic_fixture(PUBLIC_VALUE);
        let original = response.clone();
        let normal = format!("{response:?}");
        let pretty = format!("{response:#?}");

        assert_eq!(response.body.len(), public_response.body.len());
        assert_eq!(response.headers.len(), public_response.headers.len());
        assert_eq!(normal, format!("{public_response:?}"));
        assert_eq!(pretty, format!("{public_response:#?}"));
        for diagnostic in [&normal, &pretty] {
            assert!(diagnostic.contains("HttpResponse"));
            assert!(diagnostic.contains("status: HttpStatus"));
            assert!(diagnostic.contains("200"));
            assert!(diagnostic.contains("header_count: 3"));
            assert!(diagnostic.contains(&format!("body_bytes: {}", response.body.len())));
            assert!(!diagnostic.contains(value));
            for header_value in response.headers.values() {
                assert!(!diagnostic.contains(header_value));
            }
            assert!(!diagnostic.contains(&format!("{:?}", response.body)));
            assert!(!diagnostic.contains(&format!("{:#?}", response.body)));
        }

        let mut transport = HttpTransport::new(Cursor::new(Vec::<u8>::new()), Vec::new());
        transport
            .write_response(&response)
            .expect("the public writer preserves admitted response contents");
        let wire = String::from_utf8(transport.writer)
            .expect("ASCII headers and the JSON body remain valid UTF-8");
        let (head, body) = wire
            .split_once("\r\n\r\n")
            .expect("the actual writer separates headers from the body");
        let mut lines = head.split("\r\n");
        assert_eq!(lines.next(), Some("HTTP/1.1 200 OK"));
        let headers: HashMap<String, String> = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").expect("valid written header");
                (name.to_owned(), value.to_owned())
            })
            .collect();
        let mut expected_headers = original.headers.clone();
        expected_headers.insert("content-length".to_owned(), original.body.len().to_string());
        assert_eq!(headers, expected_headers);
        assert_eq!(body.as_bytes(), original.body.as_slice());
        assert_eq!(response.status, original.status);
        assert_eq!(response.headers, original.headers);
        assert_eq!(response.body, original.body);
    }

    #[test]
    fn auth_01_http_response_public_values_remain_writable() {
        assert_http_response_diagnostics_preserve_wire("public-value-0123456789");
    }

    #[test]
    fn auth_01_http_response_credential_values_are_not_diagnostics() {
        // Only the equally sized header/body values change from the public case.
        assert_http_response_diagnostics_preserve_wire("secret-value-9876543210");
    }

    #[test]
    fn http_response_rejects_header_injection_before_writing() {
        for (name, value) in [
            ("x-test", "safe\r\nx-injected: yes"),
            ("x-test\ninvalid", "safe"),
        ] {
            let response = HttpResponse::ok().with_header(name, value);
            let mut transport = HttpTransport::new(Cursor::new(Vec::<u8>::new()), Vec::new());

            let error = transport.write_response(&response).unwrap_err();

            assert!(matches!(error, HttpError::InvalidHeader(_)));
            assert_eq!(transport.writer.len(), 0);
        }

        let mut response = HttpResponse::ok();
        response
            .headers
            .insert("X-Duplicate".to_string(), "one".to_string());
        response
            .headers
            .insert("x-duplicate".to_string(), "two".to_string());
        let mut transport = HttpTransport::new(Cursor::new(Vec::<u8>::new()), Vec::new());
        assert!(matches!(
            transport.write_response(&response),
            Err(HttpError::InvalidHeader(_))
        ));
        assert_eq!(transport.writer.len(), 0);
    }

    #[test]
    fn http_response_writes_reason_phrases_for_every_declared_status() {
        for (status, expected_line) in [
            (HttpStatus::OK, "HTTP/1.1 200 OK\r\n"),
            (HttpStatus::ACCEPTED, "HTTP/1.1 202 Accepted\r\n"),
            (HttpStatus::BAD_REQUEST, "HTTP/1.1 400 Bad Request\r\n"),
            (HttpStatus::UNAUTHORIZED, "HTTP/1.1 401 Unauthorized\r\n"),
            (HttpStatus::FORBIDDEN, "HTTP/1.1 403 Forbidden\r\n"),
            (HttpStatus::NOT_FOUND, "HTTP/1.1 404 Not Found\r\n"),
            (
                HttpStatus::METHOD_NOT_ALLOWED,
                "HTTP/1.1 405 Method Not Allowed\r\n",
            ),
            (
                HttpStatus::INTERNAL_SERVER_ERROR,
                "HTTP/1.1 500 Internal Server Error\r\n",
            ),
            (
                HttpStatus::SERVICE_UNAVAILABLE,
                "HTTP/1.1 503 Service Unavailable\r\n",
            ),
        ] {
            let mut transport = HttpTransport::new(Cursor::new(Vec::<u8>::new()), Vec::new());
            transport
                .write_response(&HttpResponse::new(status))
                .expect("declared status must serialize");

            assert!(transport.writer.starts_with(expected_line.as_bytes()));
        }
    }

    #[test]
    fn http_transport_recv_requires_post_and_json_media_type() {
        let body = br#"{"jsonrpc":"2.0","method":"test","id":1}"#;
        for (method, content_type) in [
            ("GET", "application/json"),
            ("POST", "application/jsonevil"),
            ("POST", "text/plain"),
        ] {
            let head = format!(
                "{method} /mcp/v1 HTTP/1.1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let mut raw = head.into_bytes();
            raw.extend_from_slice(body);
            let mut transport = HttpTransport::new(Cursor::new(raw), Vec::new());

            let error = transport.recv(&Cx::for_testing()).unwrap_err();

            assert!(matches!(
                error,
                TransportError::Io(ref source)
                    if source.kind() == std::io::ErrorKind::InvalidData
            ));
        }
    }

    #[test]
    fn http_transport_enforces_exact_origin_policy_without_reflecting_input() {
        let body = br#"{"jsonrpc":"2.0","method":"test","id":1}"#;
        let request = |origin: &str| {
            let head = format!(
                "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nOrigin: {origin}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let mut raw = head.into_bytes();
            raw.extend_from_slice(body);
            raw
        };
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["https://trusted.example".to_string()],
            ..HttpHandlerConfig::default()
        };

        let mut allowed = HttpTransport::with_config(
            Cursor::new(request("https://trusted.example")),
            Vec::new(),
            config.clone(),
        );
        assert!(allowed.recv(&Cx::for_testing()).is_ok());

        let secret_origin = "https://secret-canary.invalid";
        let mut denied =
            HttpTransport::with_config(Cursor::new(request(secret_origin)), Vec::new(), config);
        let error = denied.recv(&Cx::for_testing()).unwrap_err();
        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert!(!error.to_string().contains(secret_origin));

        let wildcard = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["*".to_string()],
            ..HttpHandlerConfig::default()
        };
        let mut wildcard_transport = HttpTransport::with_config(
            Cursor::new(request("https://untrusted.example")),
            Vec::new(),
            wildcard,
        );
        assert!(wildcard_transport.recv(&Cx::for_testing()).is_err());
    }

    #[test]
    fn http_transport_config_bounds_body_before_allocation() {
        let config = HttpHandlerConfig {
            max_body_size: 8,
            ..HttpHandlerConfig::default()
        };
        let raw =
            b"POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n";
        let mut transport =
            HttpTransport::with_config(Cursor::new(raw.to_vec()), Vec::new(), config);

        assert!(matches!(
            transport.read_request(),
            Err(HttpError::BodyTooLarge { size: 9, max: 8 })
        ));
    }

    #[test]
    fn reality_check_regression_http_retries_interrupted_header_and_body_reads() {
        let body = br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut wire = head.into_bytes();
        wire.extend_from_slice(body);
        let interruptions = Arc::new(AtomicUsize::new(0));
        let reader = InterruptEveryOtherRead {
            inner: Cursor::new(wire),
            interrupt_next: true,
            interruptions: Arc::clone(&interruptions),
            cancel_on_interrupt: None,
        };
        let mut transport = HttpTransport::new(reader, Vec::new());

        let message = transport
            .recv(&Cx::for_testing())
            .expect("transient interruptions must not terminate HTTP framing");

        assert!(matches!(
            message,
            JsonRpcMessage::Request(ref request) if request.method == "tools/list"
        ));
        assert!(interruptions.load(Ordering::Acquire) > body.len());
        assert!(!transport.closed);
        assert!(transport.response_pending);
    }

    #[test]
    fn reality_check_regression_http_checks_context_between_interrupted_read_retries() {
        let cx = Cx::for_testing();
        let reader = InterruptEveryOtherRead {
            inner: Cursor::new(b"POST /mcp/v1 HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_vec()),
            interrupt_next: false,
            interruptions: Arc::new(AtomicUsize::new(0)),
            cancel_on_interrupt: Some(cx.clone()),
        };
        let mut transport = HttpTransport::new(reader, Vec::new());

        assert!(matches!(
            transport.recv(&cx),
            Err(TransportError::Cancelled)
        ));
        assert!(transport.closed);
        assert!(!transport.response_pending);
    }

    #[test]
    fn reality_check_regression_http_checks_context_after_successful_incremental_read() {
        let cx = Cx::for_testing();
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = CancelAfterSuccessfulRead {
            inner: Cursor::new(
                b"POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"
                    .to_vec(),
            ),
            cx: cx.clone(),
            reads: Arc::clone(&reads),
        };
        let mut transport = HttpTransport::new(reader, Vec::new());

        assert!(matches!(
            transport.recv(&cx),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(reads.load(Ordering::Acquire), 1);
        assert!(transport.closed);
        assert!(!transport.response_pending);
    }

    #[test]
    fn reality_check_regression_http_recv_uses_full_checkpoint_and_preserves_masking() {
        let body = br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut wire = head.into_bytes();
        wire.extend_from_slice(body);
        let mut transport = HttpTransport::new(Cursor::new(wire), Vec::new());

        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert!(matches!(
            transport.recv(&deadline_cx),
            Err(TransportError::Timeout)
        ));

        let poll_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_poll_quota(0));
        assert!(matches!(
            transport.recv(&poll_cx),
            Err(TransportError::Cancelled)
        ));

        let cost_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_cost_quota(0));
        assert!(matches!(
            transport.recv(&cost_cx),
            Err(TransportError::Cancelled)
        ));
        assert!(!transport.closed);
        assert!(!transport.response_pending);

        let cancelled_cx = Cx::for_testing();
        cancelled_cx.set_cancel_requested(true);
        let message = cancelled_cx
            .masked(|| transport.recv(&cancelled_cx))
            .expect("masking defers explicit cancellation at HTTP receive admission");
        assert!(matches!(
            message,
            JsonRpcMessage::Request(ref request) if request.method == "tools/list"
        ));
        assert!(transport.response_pending);
    }

    #[test]
    fn reality_check_regression_http_send_uses_full_checkpoint_without_losing_response_ownership() {
        let body = br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut wire = head.into_bytes();
        wire.extend_from_slice(body);
        let mut transport = HttpTransport::new(Cursor::new(wire), Vec::new());
        transport
            .recv(&Cx::for_testing())
            .expect("establish one pending HTTP response");
        let response = JsonRpcMessage::Response(JsonRpcResponse::success(
            RequestId::Number(1),
            serde_json::Value::Null,
        ));

        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert!(matches!(
            transport.send(&deadline_cx, &response),
            Err(TransportError::Timeout)
        ));
        let poll_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_poll_quota(0));
        assert!(matches!(
            transport.send(&poll_cx, &response),
            Err(TransportError::Cancelled)
        ));
        let cost_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_cost_quota(0));
        assert!(matches!(
            transport.send(&cost_cx, &response),
            Err(TransportError::Cancelled)
        ));
        assert!(transport.response_pending);
        assert_eq!(transport.writer.len(), 0);

        let masked_deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        masked_deadline_cx.masked(|| {
            transport
                .send(&masked_deadline_cx, &response)
                .expect("masking defers deadline enforcement at HTTP send admission");
        });
        assert!(!transport.response_pending);
        assert!(transport.writer.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }

    // =========================================================================
    // E2E HTTP Transport Tests (bd-2kv / bd-3fq1)
    // =========================================================================

    #[test]
    fn e2e_http_request_response_flow() {
        use fastmcp_protocol::RequestId;
        use std::io::Cursor;

        // Build an HTTP request with JSON-RPC body
        let json_rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 1
        });
        let body = serde_json::to_vec(&json_rpc_request).unwrap();

        let http_request = format!(
            "POST /mcp/v1 HTTP/1.1\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             \r\n",
            body.len()
        );

        let mut input = http_request.into_bytes();
        input.extend(body);

        let reader = Cursor::new(input);
        let mut output = Vec::new();

        let cx = Cx::for_testing();

        {
            let mut transport = HttpTransport::new(reader, &mut output);

            // Receive the request
            let msg = transport.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Request(_)),
                "Expected request"
            );
            let JsonRpcMessage::Request(req) = msg else {
                return;
            };

            assert_eq!(req.method, "tools/list");
            assert_eq!(req.id, Some(RequestId::Number(1)));

            // Send response
            let response = JsonRpcResponse {
                jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
                result: Some(serde_json::json!({"tools": []})),
                error: None,
                id: Some(RequestId::Number(1)),
            };
            transport
                .send(&cx, &JsonRpcMessage::Response(response))
                .unwrap();
        }

        // Verify HTTP response
        let response_str = String::from_utf8(output).unwrap();
        assert!(response_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response_str.contains("content-type: application/json"));
        assert!(response_str.contains("\"tools\":[]"));
    }

    #[test]
    fn http_transport_response_emits_json_and_exact_admitted_origin_headers() {
        let body = br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let origin = "https://trusted.example";
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nOrigin: {origin}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut input = head.into_bytes();
        input.extend_from_slice(body);
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec![origin.to_string()],
            ..HttpHandlerConfig::default()
        };
        let mut output = Vec::new();

        {
            let mut transport = HttpTransport::with_config(Cursor::new(input), &mut output, config);
            transport.recv(&Cx::for_testing()).unwrap();
            transport
                .send(
                    &Cx::for_testing(),
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        fastmcp_protocol::RequestId::Number(1),
                        serde_json::json!({"tools": []}),
                    )),
                )
                .unwrap();
        }

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("content-type: application/json\r\n"));
        assert!(response.contains(&format!("access-control-allow-origin: {origin}\r\n")));
        assert!(response.contains(
            "vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers\r\n"
        ));
    }

    #[test]
    fn http_transport_does_not_overwrite_origin_while_a_response_is_pending() {
        let first_origin = "https://first.example";
        let second_origin = "https://second.example";
        let request = |origin: &str, id: i64| {
            let body = format!(r#"{{"jsonrpc":"2.0","method":"tools/list","id":{id}}}"#);
            let head = format!(
                "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nOrigin: {origin}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let mut framed = head.into_bytes();
            framed.extend_from_slice(body.as_bytes());
            framed
        };
        let mut input = request(first_origin, 1);
        input.extend(request(second_origin, 2));
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec![first_origin.to_string(), second_origin.to_string()],
            ..HttpHandlerConfig::default()
        };
        let mut output = Vec::new();

        {
            let cx = Cx::for_testing();
            let mut transport = HttpTransport::with_config(Cursor::new(input), &mut output, config);
            transport.recv(&cx).expect("first request is admitted");

            let pending = transport
                .recv(&cx)
                .expect_err("a second request cannot overwrite pending response ownership");
            assert!(matches!(
                pending,
                TransportError::Io(ref error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
            ));

            transport
                .send(
                    &cx,
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        fastmcp_protocol::RequestId::Number(1),
                        serde_json::json!({"sequence": 1}),
                    )),
                )
                .expect("first response is written");
            transport.recv(&cx).expect("second request remains unread");
            transport
                .send(
                    &cx,
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        fastmcp_protocol::RequestId::Number(2),
                        serde_json::json!({"sequence": 2}),
                    )),
                )
                .expect("second response is written");
        }

        let wire = String::from_utf8(output).unwrap();
        let responses: Vec<_> = wire
            .split("HTTP/1.1 ")
            .filter(|response| !response.is_empty())
            .collect();
        assert_eq!(responses.len(), 2);
        assert!(responses[0].contains(first_origin));
        assert!(!responses[0].contains(second_origin));
        assert!(responses[0].contains(r#""sequence":1"#));
        assert!(responses[1].contains(second_origin));
        assert!(!responses[1].contains(first_origin));
        assert!(responses[1].contains(r#""sequence":2"#));
    }

    #[test]
    fn http_transport_acknowledges_notification_and_releases_origin_slot() {
        let origin = "https://trusted.example";
        let body = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nOrigin: {origin}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut input = head.into_bytes();
        input.extend_from_slice(body);
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec![origin.to_string()],
            ..HttpHandlerConfig::default()
        };
        let mut output = Vec::new();

        {
            let mut transport = HttpTransport::with_config(Cursor::new(input), &mut output, config);
            let message = transport.recv(&Cx::for_testing()).unwrap();
            assert!(
                matches!(message, JsonRpcMessage::Request(ref request) if request.is_notification())
            );
            assert!(!transport.response_pending);
            assert!(transport.response_origin.is_none());
        }

        let wire = String::from_utf8(output).unwrap();
        assert!(wire.starts_with("HTTP/1.1 202 Accepted\r\n"));
        assert!(wire.contains(&format!("access-control-allow-origin: {origin}\r\n")));
        assert!(wire.ends_with("\r\n\r\n"));
    }

    #[test]
    fn http_framing_or_eof_failure_latches_terminal_and_closed_wins_over_cancellation() {
        let body_prefix = br#"{"jsonrpc":"2.0""#;
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body_prefix.len() + 10
        );
        let mut input = head.into_bytes();
        input.extend_from_slice(body_prefix);
        let mut transport = HttpTransport::new(Cursor::new(input), Vec::new());
        let cx = Cx::for_testing();

        assert!(matches!(transport.recv(&cx), Err(TransportError::Io(_))));
        assert!(transport.closed);
        assert!(!transport.response_pending);
        assert!(transport.response_origin.is_none());

        cx.set_cancel_requested(true);
        assert!(matches!(transport.recv(&cx), Err(TransportError::Closed)));
        assert!(matches!(
            transport.send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    fastmcp_protocol::RequestId::Number(1),
                    serde_json::Value::Null,
                )),
            ),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn complete_http_body_codec_error_keeps_response_slot_for_jsonrpc_error() {
        let body = b"{not-json";
        let head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut input = head.into_bytes();
        input.extend_from_slice(body);
        let mut transport = HttpTransport::new(Cursor::new(input), Vec::new());
        let cx = Cx::for_testing();

        assert!(matches!(
            transport.recv(&cx),
            Err(TransportError::Codec(CodecError::Json(_)))
        ));
        assert!(!transport.closed);
        assert!(transport.response_pending);

        let response = JsonRpcResponse::error(
            None,
            fastmcp_protocol::JsonRpcError {
                code: (-32700).into(),
                message: "Parse error".to_string(),
                data: None,
            },
        );
        transport
            .send(&cx, &JsonRpcMessage::Response(response))
            .unwrap();
        assert!(!transport.closed);
        assert!(!transport.response_pending);
        assert!(transport.writer.starts_with(b"HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn declined_codec_error_exchange_flushes_400_and_admits_the_next_request() {
        // First exchange: well-framed POST with a malformed JSON body.
        let bad_body = b"{not-json";
        let mut input = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            bad_body.len()
        )
        .into_bytes();
        input.extend_from_slice(bad_body);
        // Second exchange: a valid ping request.
        let good_body = br#"{"jsonrpc":"2.0","method":"ping","id":7}"#;
        input.extend_from_slice(
            format!(
                "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                good_body.len()
            )
            .as_bytes(),
        );
        input.extend_from_slice(good_body);

        let mut transport = HttpTransport::new(Cursor::new(input), Vec::new());
        let cx = Cx::for_testing();

        // The codec failure keeps the response slot so the server may still
        // answer with a correlated JSON-RPC error if it chooses to.
        assert!(matches!(
            transport.recv(&cx),
            Err(TransportError::Codec(CodecError::Json(_)))
        ));
        assert!(!transport.closed);
        assert!(transport.response_pending);

        // The server declines (era admission skips malformed opening frames)
        // and receives again: the abandoned exchange completes with 400 Bad
        // Request and the next request is admitted on the same connection.
        let message = transport
            .recv(&cx)
            .expect("the connection must keep serving after a declined malformed body");
        let JsonRpcMessage::Request(request) = message else {
            panic!("second exchange must decode as a request");
        };
        assert_eq!(request.method, "ping");
        assert!(transport.response_pending);
        assert!(!transport.pending_body_rejected);
        assert!(
            transport
                .writer
                .starts_with(b"HTTP/1.1 400 Bad Request\r\n"),
            "declined malformed exchange must be answered with 400"
        );
    }

    #[test]
    fn http_handler_rejects_escaped_duplicate_object_member() {
        let handler = HttpRequestHandler::new();
        let request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_body(r#"{"jsonrpc":"2.0","method":"first","m\u0065thod":"second","id":1}"#);

        let error = handler.parse_request(&request).unwrap_err();

        assert!(matches!(
            error,
            HttpError::CodecError(CodecError::InvalidMessage {
                kind: crate::InvalidMessageKind::Request,
                ..
            })
        ));
        assert!(error.to_string().contains("duplicate JSON object member"));
    }

    #[test]
    fn http_transport_rejects_escaped_duplicate_object_member() {
        let body = br#"{"jsonrpc":"2.0","method":"first","m\u0065thod":"second","id":1}"#;
        let request_head = format!(
            "POST /mcp/v1 HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut input = request_head.into_bytes();
        input.extend_from_slice(body);
        let reader = Cursor::new(input);
        let writer = Vec::new();
        let mut transport = HttpTransport::new(reader, writer);

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

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
    fn e2e_http_error_status_codes() {
        let handler = HttpRequestHandler::new();

        // Invalid method should return error
        let request = HttpRequest::new(HttpMethod::Get, "/mcp/v1")
            .with_header("Content-Type", "application/json");
        let result = handler.parse_request(&request);
        assert!(matches!(result, Err(HttpError::InvalidMethod(_))));

        // Invalid content type
        let request =
            HttpRequest::new(HttpMethod::Post, "/mcp/v1").with_header("Content-Type", "text/xml");
        let result = handler.parse_request(&request);
        assert!(matches!(result, Err(HttpError::InvalidContentType(_))));

        // Create error response
        let response = handler.error_response(HttpStatus::BAD_REQUEST, "Invalid request format");
        assert_eq!(response.status, HttpStatus::BAD_REQUEST);
        let body_str = String::from_utf8(response.body).unwrap();
        assert!(body_str.contains("\"error\""));
    }

    #[test]
    fn modern_json_content_type_admits_only_a_utf8_charset_parameter() {
        assert!(is_modern_json_content_type("application/json"));
        assert!(is_modern_json_content_type("Application/JSON"));
        assert!(is_modern_json_content_type(
            "application/json; charset=utf-8"
        ));
        assert!(is_modern_json_content_type(
            "application/json; charset=UTF-8"
        ));

        // The changed variable in each rejection is one parameter detail.
        assert!(!is_modern_json_content_type(
            "application/json; charset=utf-16"
        ));
        assert!(!is_modern_json_content_type(
            "application/json; charset=utf-8; boundary=x"
        ));
        assert!(!is_modern_json_content_type("application/json; nonsense"));
        assert!(!is_modern_json_content_type("text/json"));
        assert!(!is_modern_json_content_type(""));
    }

    #[test]
    fn request_content_coding_admits_only_singleton_identity() {
        assert!(is_identity_content_coding("identity"));
        assert!(is_identity_content_coding("Identity"));
        assert!(is_identity_content_coding(", identity"));

        assert!(!is_identity_content_coding("gzip"));
        assert!(!is_identity_content_coding("identity, identity"));
        assert!(!is_identity_content_coding(""));
        assert!(!is_identity_content_coding(",,,"));
    }

    #[test]
    fn coded_request_bodies_are_refused_before_json_admission() {
        let handler = HttpRequestHandler::new();

        // The identical request without the coding header parses; the sole
        // changed variable is the compressed content coding, which must be
        // a typed transport refusal rather than a JSON diagnostic.
        let admitted = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_body(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec());
        assert!(handler.parse_request(&admitted).is_ok());

        let coded = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_header("Content-Encoding", "gzip")
            .with_body(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec());
        let result = handler.parse_request(&coded);
        assert!(matches!(
            result,
            Err(HttpError::UnsupportedContentEncoding(value)) if value == "gzip"
        ));
    }

    #[test]
    fn e2e_http_content_type_handling() {
        let handler = HttpRequestHandler::new();

        // Standard JSON content type
        let request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_body(r#"{"jsonrpc":"2.0","method":"test","id":1}"#);
        assert!(handler.parse_request(&request).is_ok());

        // JSON with charset
        let request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json; charset=utf-8")
            .with_body(r#"{"jsonrpc":"2.0","method":"test","id":1}"#);
        assert!(handler.parse_request(&request).is_ok());

        // Response content type is always application/json
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({})),
            error: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        };
        let http_response = handler.create_response(&response, None);
        assert_eq!(
            http_response.headers.get("content-type"),
            Some(&"application/json".to_string())
        );
    }

    #[test]
    fn e2e_http_cors_handling() {
        let config = HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["https://allowed.com".to_string()],
            ..Default::default()
        };
        let handler = HttpRequestHandler::with_config(config);

        // Allowed origin
        assert!(handler.is_origin_allowed("https://allowed.com"));

        // Disallowed origin
        assert!(!handler.is_origin_allowed("https://evil.com"));

        // OPTIONS request from allowed origin
        let request = HttpRequest::new(HttpMethod::Options, "/mcp/v1")
            .with_header("Origin", "https://allowed.com")
            .with_header("Access-Control-Request-Method", "POST");
        let response = handler.handle_options(&request);
        assert_eq!(response.status, HttpStatus::OK);
        assert_eq!(
            response.headers.get("access-control-allow-origin"),
            Some(&"https://allowed.com".to_string())
        );

        // OPTIONS request from disallowed origin
        let request = HttpRequest::new(HttpMethod::Options, "/mcp/v1")
            .with_header("Origin", "https://evil.com")
            .with_header("Access-Control-Request-Method", "POST");
        let response = handler.handle_options(&request);
        assert_eq!(response.status, HttpStatus::FORBIDDEN);

        let wrong_preflight_method = HttpRequest::new(HttpMethod::Options, "/mcp/v1")
            .with_header("Origin", "https://allowed.com")
            .with_header("Access-Control-Request-Method", "GET");
        assert_eq!(
            handler.handle_options(&wrong_preflight_method).status,
            HttpStatus::FORBIDDEN
        );

        let post = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_header("Origin", "https://evil.com")
            .with_body(r#"{"jsonrpc":"2.0","method":"test","id":1}"#);
        assert!(matches!(
            handler.parse_request(&post),
            Err(HttpError::OriginNotAllowed(origin)) if origin == "https://evil.com"
        ));

        let missing_origin = HttpRequest::new(HttpMethod::Options, "/mcp/v1");
        assert_eq!(
            handler.handle_options(&missing_origin).status,
            HttpStatus::FORBIDDEN
        );

        let wrong_path = HttpRequest::new(HttpMethod::Options, "/wrong")
            .with_header("Origin", "https://allowed.com")
            .with_header("Access-Control-Request-Method", "POST");
        assert_eq!(
            handler.handle_options(&wrong_path).status,
            HttpStatus::NOT_FOUND
        );

        let wildcard_handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            allow_cors: true,
            cors_origins: vec!["*".to_string()],
            ..HttpHandlerConfig::default()
        });
        assert!(!wildcard_handler.is_origin_allowed("https://allowed.com"));
    }

    #[test]
    fn e2e_http_streaming_transport() {
        use fastmcp_protocol::RequestId;

        let mut transport = StreamableHttpTransport::new();
        let cx = Cx::for_testing();

        // Simulate multiple requests being pushed (from HTTP handlers)
        let req1 = JsonRpcRequest::new("method1", None, 1i64);
        let req2 = JsonRpcRequest::new("method2", None, 2i64);
        transport.push_request(&cx, req1).unwrap();
        transport.push_request(&cx, req2).unwrap();

        // Transport should receive requests in FIFO order.
        let msg = transport.recv(&cx).unwrap();
        if let JsonRpcMessage::Request(req) = msg {
            assert_eq!(req.method, "method1");
        }

        // Send a response
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({})),
            error: None,
            id: Some(RequestId::Number(2)),
        };
        transport
            .send(&cx, &JsonRpcMessage::Response(response))
            .unwrap();

        // Response should be available for streaming
        assert!(transport.has_responses());
        let resp = transport.pop_response().unwrap().unwrap();
        assert_eq!(resp.id, Some(RequestId::Number(2)));
    }

    #[test]
    fn streamable_http_request_response_body_routes_only_its_bound_final_response() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_id = RequestId::Number(701);
        let request_response = response_stream
            .for_request(request_id.clone())
            .expect("each request receives one response body");
        assert!(matches!(
            response_stream.for_request(request_id.clone()),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        let other_request_id = RequestId::Number(703);
        let other_request_response = response_stream
            .for_request(other_request_id.clone())
            .expect("independent requests receive independent response bodies");
        let request_cancellation = request_response.cancellation();
        let other_cancellation = other_request_response.cancellation();
        let cx = Cx::for_testing();

        request_cancellation
            .checkpoint(&cx)
            .expect("a live response body admits request work");
        transport
            .send_response_for_request(
                &cx,
                &other_cancellation,
                JsonRpcResponse::success(
                    other_request_id.clone(),
                    serde_json::json!({"response": "other"}),
                ),
            )
            .expect("an independent request response remains queued for its own body");
        transport
            .send_response_for_request(
                &cx,
                &request_cancellation,
                JsonRpcResponse::success(
                    request_id.clone(),
                    serde_json::json!({"response": "bound"}),
                ),
            )
            .expect("a live request response body accepts its final response");

        let response = request_response
            .recv_response(&cx)
            .expect("the request response body receives only its bound response");
        assert_eq!(response.id, Some(request_id));
        assert!(request_response.is_finished());
        assert!(request_cancellation.is_cancelled());
        assert_eq!(response_stream.pending_responses(), 1);
        assert!(matches!(
            request_response.pop_response(),
            Err(TransportError::Closed)
        ));
        let other_response = other_request_response
            .recv_response(&cx)
            .expect("the second body receives the response retained for its request ID");
        assert_eq!(other_response.id, Some(other_request_id));
        assert_eq!(response_stream.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_request_owned_notifications_are_ordered_bounded_and_terminal() {
        let mut transport =
            StreamableHttpTransport::with_capacity(2).expect("capacity two is valid");
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_id = RequestId::Number(704);
        let request_body = response_stream
            .for_request(request_id.clone())
            .expect("the request receives one response body");
        let cancellation = request_body.cancellation();
        let cx = Cx::for_testing();

        transport
            .send_notification_for_request(
                &cx,
                &cancellation,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 1})),
                ),
            )
            .expect("the first notification is admitted");
        transport
            .send_notification_for_request(
                &cx,
                &cancellation,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 2})),
                ),
            )
            .expect("the second notification is admitted");
        assert_eq!(response_stream.pending_responses(), 2);
        assert!(matches!(
            transport.send_notification_for_request(
                &cx,
                &cancellation,
                JsonRpcRequest::notification("notifications/progress", None),
            ),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        assert!(matches!(
            request_body
                .recv_message(&cx)
                .expect("the first queued message is readable"),
            StreamableHttpRequestResponseMessage::Notification(notification)
                if notification.method == "notifications/progress"
                    && notification.params == Some(serde_json::json!({"progress": 1}))
        ));
        assert!(!request_body.is_finished());
        transport
            .send_response_for_request(
                &cx,
                &cancellation,
                JsonRpcResponse::success(request_id.clone(), serde_json::json!({"complete": true})),
            )
            .expect("draining one notification makes room for the terminal response");
        assert!(cancellation.is_terminal_committed());

        assert!(matches!(
            request_body
                .recv_message(&cx)
                .expect("the second notification remains ahead of the terminal response"),
            StreamableHttpRequestResponseMessage::Notification(notification)
                if notification.params == Some(serde_json::json!({"progress": 2}))
        ));
        assert!(matches!(
            request_body
                .recv_message(&cx)
                .expect("the final message is the terminal response"),
            StreamableHttpRequestResponseMessage::Response(response)
                if response.id == Some(request_id)
        ));
        assert!(request_body.is_finished());
        assert_eq!(response_stream.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_notification_rejects_a_foreign_request_owner_without_mutation() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_id = RequestId::Number(705);
        let request_body = response_stream
            .for_request(request_id.clone())
            .expect("the local request body is registered");
        let local_cancellation = request_body.cancellation();

        let mut foreign_transport = StreamableHttpTransport::new();
        let foreign_stream = foreign_transport
            .response_stream()
            .expect("the foreign response stream can be externalized once");
        let foreign_body = foreign_stream
            .for_request(request_id)
            .expect("the foreign request has the same ID but a distinct owner");
        let foreign_cancellation = foreign_body.cancellation();
        let cx = Cx::for_testing();
        let pending_before = response_stream.pending_responses();
        let retained_before = transport
            .response_mailbox
            .lock()
            .expect("response mailbox is available")
            .retained_bytes;

        // Planted forbidden dimension: only the guard belongs to a different
        // transport; the JSON-RPC ID and notification are otherwise valid.
        assert!(matches!(
            transport.send_notification_for_request(
                &cx,
                &foreign_cancellation,
                JsonRpcRequest::notification("notifications/progress", None),
            ),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert_eq!(response_stream.pending_responses(), pending_before);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes,
            retained_before,
            "a foreign owner must not mutate the local response stream"
        );
        assert!(
            request_body
                .pop_message()
                .expect("the local body remains readable")
                .is_none()
        );
        local_cancellation
            .checkpoint(&cx)
            .expect("the local body remains live after a foreign-owner rejection");
    }

    #[test]
    fn streamable_http_rejects_sse_binding_over_an_unowned_queued_response() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_id = RequestId::Number(707);
        let cx = Cx::for_testing();
        transport
            .send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    request_id.clone(),
                    serde_json::json!({"unowned": true}),
                )),
            )
            .expect("an unowned response is queued before SSE binding");
        let pending_before = response_stream.pending_responses();
        let retained_before = transport
            .response_mailbox
            .lock()
            .expect("response mailbox is available")
            .retained_bytes;

        // Planted forbidden dimension: only a generic response for this ID
        // was queued before the otherwise valid SSE body registration.
        assert!(matches!(
            response_stream.for_request(request_id.clone()),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(response_stream.pending_responses(), pending_before);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes,
            retained_before,
            "rejected SSE binding must preserve the generic response byte reservation"
        );
        assert_eq!(
            response_stream
                .pop_response(Some(&request_id))
                .expect("the generic response remains independently consumable")
                .expect("the generic response remains queued")
                .id,
            Some(request_id)
        );
        assert_eq!(response_stream.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_notification_rejects_a_closed_request_body_without_mutation() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_body = response_stream
            .for_request(RequestId::Number(706))
            .expect("the request body is registered");
        let cancellation = request_body.cancellation();
        let cx = Cx::for_testing();
        transport
            .send_notification_for_request(
                &cx,
                &cancellation,
                JsonRpcRequest::notification("notifications/progress", None),
            )
            .expect("the live request body accepts its notification");
        assert_eq!(response_stream.pending_responses(), 1);
        assert!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes
                > 0
        );

        // Planted forbidden dimension: only the request body is dropped before
        // the otherwise identical notification commit.
        drop(request_body);

        assert_eq!(
            response_stream.pending_responses(),
            0,
            "closing a request body releases its queued notifications"
        );
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes,
            0,
            "closing a request body releases its notification byte reservation"
        );

        assert!(matches!(
            transport.send_notification_for_request(
                &cx,
                &cancellation,
                JsonRpcRequest::notification("notifications/progress", None),
            ),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(response_stream.pending_responses(), 0);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes,
            0,
            "a closed request body must not retain a notification"
        );
        assert!(!response_stream.is_closed());
    }

    #[test]
    fn streamable_http_request_response_body_disconnect_cancels_before_commit() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_response = response_stream
            .for_request(RequestId::Number(702))
            .expect("the response body is registered before dispatch");
        let request_cancellation = request_response.cancellation();
        let cx = Cx::for_testing();
        let pending_before = response_stream.pending_responses();

        // Planted forbidden dimension: the otherwise live request response
        // body is dropped, modeling peer disconnect before handler commit.
        drop(request_response);

        assert!(request_cancellation.is_cancelled());
        assert!(matches!(
            request_cancellation.checkpoint(&cx),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(
            response_stream.pending_responses(),
            pending_before,
            "disconnect cancellation must not enqueue or consume another request's response"
        );
        assert!(
            !response_stream.is_closed(),
            "one request-body disconnect must not close independent response bodies"
        );
    }

    #[test]
    fn streamable_http_request_response_body_enforces_backpressure_and_terminal_commit() {
        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let first_id = RequestId::Number(801);
        let first_body = response_stream
            .for_request(first_id.clone())
            .expect("the first response body is registered");
        let first_cancellation = first_body.cancellation();
        let second_id = RequestId::Number(802);
        let second_body = response_stream
            .for_request(second_id.clone())
            .expect("the second response body is registered");
        let second_cancellation = second_body.cancellation();
        let cx = Cx::for_testing();

        transport
            .send_response_for_request(
                &cx,
                &first_cancellation,
                JsonRpcResponse::success(first_id.clone(), serde_json::json!({"sequence": 1})),
            )
            .expect("the first bounded response is admitted");
        assert!(first_cancellation.is_terminal_committed());
        assert!(matches!(
            transport.send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    first_id.clone(),
                    serde_json::json!({"raw_bypass": true}),
                )),
            ),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        let retained_before_backpressure = transport
            .response_mailbox
            .lock()
            .expect("response mailbox is available")
            .retained_bytes;
        assert!(matches!(
            transport.send_response_for_request(
                &cx,
                &second_cancellation,
                JsonRpcResponse::success(second_id.clone(), serde_json::json!({"sequence": 2})),
            ),
            Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert_eq!(response_stream.pending_responses(), 1);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes,
            retained_before_backpressure,
            "a slow consumer must not grow the bounded response mailbox"
        );

        assert_eq!(
            first_body
                .recv_response(&cx)
                .expect("the first body drains its final response")
                .id,
            Some(first_id.clone())
        );
        assert!(first_body.is_finished());
        assert!(first_cancellation.is_cancelled());
        assert!(matches!(
            transport.send_response_for_request(
                &cx,
                &first_cancellation,
                JsonRpcResponse::success(first_id, serde_json::json!({"late": true})),
            ),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(response_stream.pending_responses(), 0);

        transport
            .send_response_for_request(
                &cx,
                &second_cancellation,
                JsonRpcResponse::success(second_id.clone(), serde_json::json!({"sequence": 2})),
            )
            .expect("draining the first body releases exactly one response slot");
        assert_eq!(
            second_body
                .recv_response(&cx)
                .expect("the second body receives the response admitted after backpressure")
                .id,
            Some(second_id)
        );
        assert_eq!(response_stream.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_request_response_body_rejects_commit_after_disconnect() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let request_id = RequestId::Number(803);
        let request_body = response_stream
            .for_request(request_id.clone())
            .expect("the response body is registered before dispatch");
        let request_cancellation = request_body.cancellation();
        let cx = Cx::for_testing();
        let pending_before = response_stream.pending_responses();
        let retained_before = transport
            .response_mailbox
            .lock()
            .expect("response mailbox is available")
            .retained_bytes;

        // Planted forbidden dimension: only the request body is dropped before
        // the handler attempts its otherwise identical response commit.
        drop(request_body);

        assert!(matches!(
            transport.send_response_for_request(
                &cx,
                &request_cancellation,
                JsonRpcResponse::success(request_id, serde_json::json!({"late": true})),
            ),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(response_stream.pending_responses(), pending_before);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .expect("response mailbox is available")
                .retained_bytes,
            retained_before,
            "a cancelled request must leave queued-response accounting unchanged"
        );
        assert!(!response_stream.is_closed());
    }

    #[test]
    fn streamable_http_response_body_admission_closes_with_the_shared_stream() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");

        let live_body = response_stream
            .for_request(RequestId::Number(804))
            .expect("an open response stream admits a request-owned body");
        assert_eq!(
            response_stream
                .live_request_bodies()
                .expect("live body registry is observable"),
            1
        );
        drop(live_body);
        assert_eq!(
            response_stream
                .live_request_bodies()
                .expect("dropped body releases the registry entry"),
            0
        );

        response_stream.close();

        // Planted forbidden dimension: only the shared response stream has
        // closed. Registration must fail before it allocates a request body.
        assert!(matches!(
            response_stream.for_request(RequestId::Number(805)),
            Err(TransportError::Closed)
        ));
        assert_eq!(
            response_stream
                .live_request_bodies()
                .expect("closed admission leaves the registry unchanged"),
            0
        );
    }

    #[test]
    fn streamable_http_request_ingress_feeds_transport_owned_by_recv_thread() {
        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let empty_polls = Arc::clone(&transport.request_empty_polls);
        let ingress = transport
            .request_ingress()
            .expect("request ingress can be externalized once");
        let receive_cx = Cx::for_testing();
        let cancel_cx = receive_cx.clone();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();

        let worker = std::thread::spawn(move || {
            let mut transport = transport;
            let result = transport.recv(&receive_cx);
            result_sender
                .send(result)
                .expect("test result receiver remains available");
        });

        if !wait_for_counter(&empty_polls, 1) {
            cancel_cx.set_cancel_requested(true);
            worker.join().expect("receive thread cancels cleanly");
            panic!("transport did not enter its empty receive wait");
        }
        ingress
            .push_request(
                &Cx::for_testing(),
                JsonRpcRequest::new("concurrent/ingress", None, 17_i64),
            )
            .expect("independent ingress can feed the owned transport");

        let received = match result_receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result.expect("transport receives the concurrent request"),
            Err(error) => {
                cancel_cx.set_cancel_requested(true);
                let _ = worker.join();
                panic!("transport did not receive concurrent ingress: {error}");
            }
        };
        worker.join().expect("receive thread completes");

        let JsonRpcMessage::Request(request) = received else {
            panic!("expected request");
        };
        assert_eq!(request.method, "concurrent/ingress");
    }

    #[test]
    fn streamable_http_response_stream_consumes_while_transport_is_owned_elsewhere() {
        use fastmcp_protocol::RequestId;

        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let entered_empty_waits = Arc::clone(&response_stream.entered_empty_waits);
        let receive_cx = Cx::for_testing();
        let cancel_cx = receive_cx.clone();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let request_id = RequestId::Number(23);
        let worker_request_id = request_id.clone();

        let worker = std::thread::spawn(move || {
            let result = response_stream.recv_response(&receive_cx, Some(&worker_request_id));
            result_sender
                .send(result)
                .expect("test result receiver remains available");
        });

        if !wait_for_counter(&entered_empty_waits, 1) {
            cancel_cx.set_cancel_requested(true);
            worker.join().expect("response thread cancels cleanly");
            panic!("response consumer did not enter its empty wait");
        }
        transport
            .send(
                &Cx::for_testing(),
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    request_id,
                    serde_json::json!({"concurrent": true}),
                )),
            )
            .expect("transport can produce for an independent response consumer");

        let response = match result_receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result.expect("response stream receives the concurrent response"),
            Err(error) => {
                cancel_cx.set_cancel_requested(true);
                let _ = worker.join();
                panic!("response stream did not receive transport output: {error}");
            }
        };
        worker.join().expect("response thread completes");
        assert_eq!(response.id, Some(RequestId::Number(23)));
        assert_eq!(transport.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_correlates_two_concurrent_consumers_exactly_once() {
        let mut transport =
            StreamableHttpTransport::with_capacity(2).expect("capacity two is valid");
        let first_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let second_stream = first_stream.clone();
        let entered_empty_waits = Arc::clone(&first_stream.entered_empty_waits);
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let first_cx = Cx::for_testing();
        let first_cancel_cx = first_cx.clone();
        let second_cx = Cx::for_testing();
        let second_cancel_cx = second_cx.clone();

        let first_sender = result_sender.clone();
        let first_worker = std::thread::spawn(move || {
            let id = RequestId::Number(101);
            first_sender
                .send((id.clone(), first_stream.recv_response(&first_cx, Some(&id))))
                .expect("test result receiver remains available");
        });
        let second_worker = std::thread::spawn(move || {
            let id = RequestId::Number(202);
            result_sender
                .send((
                    id.clone(),
                    second_stream.recv_response(&second_cx, Some(&id)),
                ))
                .expect("test result receiver remains available");
        });

        if !wait_for_counter(&entered_empty_waits, 2) {
            first_cancel_cx.set_cancel_requested(true);
            second_cancel_cx.set_cancel_requested(true);
            first_worker.join().expect("first consumer cancels cleanly");
            second_worker
                .join()
                .expect("second consumer cancels cleanly");
            panic!("both correlated consumers did not enter empty waits");
        }
        for (id, marker) in [(RequestId::Number(202), 2), (RequestId::Number(101), 1)] {
            transport
                .send(
                    &Cx::for_testing(),
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        id,
                        serde_json::json!({"consumer": marker}),
                    )),
                )
                .expect("each correlated response is admitted");
        }

        let received = (0..2)
            .map(|_| result_receiver.recv_timeout(Duration::from_secs(1)))
            .collect::<Result<Vec<_>, _>>();
        if received.is_err() {
            first_cancel_cx.set_cancel_requested(true);
            second_cancel_cx.set_cancel_requested(true);
        }
        first_worker.join().expect("first consumer completes");
        second_worker.join().expect("second consumer completes");

        let mut observed = HashSet::new();
        for (expected_id, result) in received.expect("both consumers complete") {
            let response = result.expect("correlated receive succeeds");
            assert_eq!(response.id.as_ref(), Some(&expected_id));
            assert!(
                observed.insert(expected_id),
                "response delivered more than once"
            );
        }
        assert_eq!(observed.len(), 2);
        assert_eq!(transport.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_unmatched_pop_retains_response_and_byte_reservation() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let expected_id = RequestId::Number(303);
        transport
            .send(
                &Cx::for_testing(),
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    expected_id.clone(),
                    serde_json::json!({"retained": true}),
                )),
            )
            .expect("response is admitted");
        let retained_before = transport
            .response_mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retained_bytes;
        assert!(retained_before > 0);

        assert!(
            response_stream
                .pop_response(Some(&RequestId::Number(404)))
                .expect("unmatched pop is not terminal")
                .is_none()
        );
        assert_eq!(transport.pending_responses(), 1);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retained_bytes,
            retained_before
        );

        let response = response_stream
            .pop_response(Some(&expected_id))
            .expect("matching pop succeeds")
            .expect("matching response is still retained");
        assert_eq!(response.id, Some(expected_id));
        assert_eq!(transport.pending_responses(), 0);
        assert_eq!(
            transport
                .response_mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retained_bytes,
            0
        );
    }

    #[test]
    fn e2e_http_streaming_response_queue_is_fifo() {
        use fastmcp_protocol::RequestId;

        let mut transport = StreamableHttpTransport::new();
        let cx = Cx::for_testing();

        let first = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({"seq": 1})),
            error: None,
            id: Some(RequestId::Number(1)),
        };
        let second = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({"seq": 2})),
            error: None,
            id: Some(RequestId::Number(2)),
        };

        transport
            .send(&cx, &JsonRpcMessage::Response(first))
            .unwrap();
        transport
            .send(&cx, &JsonRpcMessage::Response(second))
            .unwrap();

        let first_out = transport
            .pop_response()
            .expect("response channel remains open")
            .expect("first response");
        let second_out = transport
            .pop_response()
            .expect("response channel remains open")
            .expect("second response");
        assert_eq!(first_out.id, Some(RequestId::Number(1)));
        assert_eq!(second_out.id, Some(RequestId::Number(2)));
    }

    #[test]
    fn e2e_http_streaming_rejects_server_to_client_requests() {
        let mut transport = StreamableHttpTransport::new();
        let cx = Cx::for_testing();
        let request = JsonRpcRequest::notification("notifications/message", None);

        let err = transport
            .send(&cx, &JsonRpcMessage::Request(request))
            .expect_err("streamable transport must reject server-to-client requests");

        assert!(matches!(err, TransportError::Io(_)));
    }

    #[test]
    fn e2e_http_streaming_response_queue_is_hard_bounded() {
        use fastmcp_protocol::RequestId;

        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let cx = Cx::for_testing();
        let first = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
            id: Some(RequestId::Number(1)),
        };
        let second = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
            id: Some(RequestId::Number(2)),
        };

        transport
            .send(&cx, &JsonRpcMessage::Response(first))
            .unwrap();

        let err = transport
            .send(&cx, &JsonRpcMessage::Response(second))
            .expect_err("a second queued response must exceed capacity one");
        assert!(matches!(
            err,
            TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert_eq!(transport.pending_responses(), 1);
    }

    #[test]
    fn e2e_http_streaming_request_queue_is_hard_bounded_and_fifo() {
        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let cx = Cx::for_testing();
        transport
            .push_request(&cx, JsonRpcRequest::new("first", None, 1i64))
            .unwrap();
        let error = transport
            .push_request(&cx, JsonRpcRequest::new("rejected", None, 2i64))
            .expect_err("a second queued request must exceed capacity one");
        assert!(matches!(
            error,
            TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        let JsonRpcMessage::Request(request) = transport.recv(&cx).unwrap() else {
            panic!("expected request");
        };
        assert_eq!(request.method, "first");
        assert_eq!(transport.pending_requests(), 0);
    }

    #[test]
    fn streamable_http_rejects_invalid_or_oversized_typed_messages_before_queueing() {
        let cx = Cx::for_testing();
        let mut transport =
            StreamableHttpTransport::with_capacity(2).expect("capacity two is valid");
        transport.codec.set_max_message_size(64);

        let oversized = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({"payload": "x".repeat(128)})),
            1_i64,
        );
        assert!(matches!(
            transport.push_request(&cx, oversized),
            Err(TransportError::Codec(CodecError::MessageTooLarge(_)))
        ));
        assert_eq!(transport.pending_requests(), 0);

        let invalid_response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        };
        assert!(matches!(
            transport.send(&cx, &JsonRpcMessage::Response(invalid_response)),
            Err(TransportError::Codec(CodecError::Json(_)))
        ));
        assert_eq!(transport.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_enforces_aggregate_byte_budgets_in_both_directions() {
        const TEST_BYTE_BUDGET: usize = 512;

        let cx = Cx::for_testing();
        let mut transport = StreamableHttpTransport::with_queue_limits(4, TEST_BYTE_BUDGET)
            .expect("test limits are valid");
        let (request_ingress, response_stream) = transport
            .split_handles()
            .expect("endpoints can be externalized once");
        let request = JsonRpcRequest::new(
            "budget/request",
            Some(serde_json::json!({"payload": "x".repeat(320)})),
            1_i64,
        );
        let request_bytes = transport
            .codec
            .encode_request(&request)
            .expect("request is encodable")
            .len();
        assert!(request_bytes <= TEST_BYTE_BUDGET);
        assert!(request_bytes * 2 > TEST_BYTE_BUDGET);

        request_ingress.push_request(&cx, request.clone()).unwrap();
        let request_error = request_ingress
            .push_request(&cx, request.clone())
            .expect_err("aggregate request bytes must be bounded");
        assert!(matches!(
            request_error,
            TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert!(matches!(
            transport.recv(&cx).unwrap(),
            JsonRpcMessage::Request(_)
        ));
        request_ingress
            .push_request(&cx, request)
            .expect("dequeue releases the request byte budget");

        let response = JsonRpcResponse::success(
            fastmcp_protocol::RequestId::Number(1),
            serde_json::json!({"payload": "x".repeat(320)}),
        );
        let response_bytes = transport
            .codec
            .encode_response(&response)
            .expect("response is encodable")
            .len();
        assert!(response_bytes <= TEST_BYTE_BUDGET);
        assert!(response_bytes * 2 > TEST_BYTE_BUDGET);

        transport
            .send(&cx, &JsonRpcMessage::Response(response.clone()))
            .unwrap();
        let response_error = transport
            .send(&cx, &JsonRpcMessage::Response(response.clone()))
            .expect_err("aggregate response bytes must be bounded");
        assert!(matches!(
            response_error,
            TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        response_stream
            .pop_response(Some(&fastmcp_protocol::RequestId::Number(1)))
            .expect("response queue remains open")
            .expect("one response was queued");
        transport
            .send(&cx, &JsonRpcMessage::Response(response))
            .expect("dequeue releases the response byte budget");
    }

    #[test]
    fn e2e_http_streaming_rejects_zero_capacity() {
        assert!(matches!(
            StreamableHttpTransport::with_capacity(0),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            StreamableHttpTransport::with_capacity(MAX_STREAMABLE_QUEUE_CAPACITY + 1),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn streamable_http_close_is_terminal_for_public_queue_operations() {
        let cx = Cx::for_testing();
        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let (request_ingress, response_stream) = transport
            .split_handles()
            .expect("endpoints can be externalized once");
        let response_id = fastmcp_protocol::RequestId::Number(1);
        transport
            .send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    response_id.clone(),
                    serde_json::Value::Null,
                )),
            )
            .unwrap();
        request_ingress
            .push_request(&cx, JsonRpcRequest::new("queued", None, 1_i64))
            .unwrap();
        assert!(transport.has_responses());
        assert_eq!(transport.pending_requests(), 1);
        assert_eq!(transport.pending_responses(), 1);

        transport.close(&cx).unwrap();

        assert!(transport.has_responses());
        assert_eq!(transport.pending_requests(), 0);
        assert_eq!(transport.pending_responses(), 1);
        assert!(request_ingress.is_closed());
        assert!(response_stream.is_closed());
        cx.set_cancel_requested(true);
        assert!(matches!(
            transport.push_request(&cx, JsonRpcRequest::new("after-close", None, 2_i64)),
            Err(TransportError::Closed)
        ));
        assert!(matches!(
            request_ingress
                .push_request(&cx, JsonRpcRequest::new("after-close-handle", None, 3_i64)),
            Err(TransportError::Closed)
        ));
        let drained = response_stream
            .pop_response(Some(&response_id))
            .expect("already-admitted response remains drainable")
            .expect("queued response is retained across graceful close");
        assert_eq!(drained.id, Some(response_id.clone()));
        assert_eq!(transport.pending_responses(), 0);
        assert!(matches!(
            response_stream.pop_response(Some(&response_id)),
            Err(TransportError::Closed)
        ));
        assert!(matches!(
            transport.send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    fastmcp_protocol::RequestId::Number(2),
                    serde_json::Value::Null,
                )),
            ),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn streamable_http_owner_drop_closes_external_handles() {
        let (request_ingress, response_stream) = {
            let mut transport = StreamableHttpTransport::new();
            transport
                .split_handles()
                .expect("endpoints can be externalized once")
        };

        assert!(request_ingress.is_closed());
        assert!(response_stream.is_closed());
        assert!(matches!(
            request_ingress.push_request(
                &Cx::for_testing(),
                JsonRpcRequest::new("after-owner-drop", None, 1_i64)
            ),
            Err(TransportError::Closed)
        ));
        assert!(matches!(
            response_stream.pop_response(Some(&fastmcp_protocol::RequestId::Number(1))),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn streamable_http_owner_drop_preserves_admitted_response_for_external_drain() {
        let response_id = fastmcp_protocol::RequestId::Number(77);
        let response_stream = {
            let mut transport = StreamableHttpTransport::new();
            let response_stream = transport
                .response_stream()
                .expect("response stream can be externalized once");
            transport
                .send(
                    &Cx::for_testing(),
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        response_id.clone(),
                        serde_json::json!({"drain": true}),
                    )),
                )
                .expect("response is admitted before owner drop");
            response_stream
        };

        let response = response_stream
            .pop_response(Some(&response_id))
            .expect("owner drop still permits an admitted response to drain")
            .expect("the admitted response remains present");
        assert_eq!(response.id, Some(response_id.clone()));
        assert!(matches!(
            response_stream.pop_response(Some(&response_id)),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn streamable_http_externalizes_each_endpoint_exactly_once() {
        let mut request_transport = StreamableHttpTransport::new();
        let request_ingress = request_transport
            .request_ingress()
            .expect("request ingress can be externalized once");
        assert!(request_transport.request_sender.is_none());
        assert!(matches!(
            request_transport.request_ingress(),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        drop(request_ingress);
        assert!(matches!(
            request_transport.recv(&Cx::for_testing()),
            Err(TransportError::Closed)
        ));

        let mut response_transport = StreamableHttpTransport::new();
        let response_stream = response_transport
            .response_stream()
            .expect("response stream can be externalized once");
        assert!(matches!(
            response_transport.response_stream(),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert!(matches!(
            response_transport.pop_response(),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        response_transport
            .send(
                &Cx::for_testing(),
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    fastmcp_protocol::RequestId::Number(1),
                    serde_json::Value::Null,
                )),
            )
            .expect("response is admitted while the external consumer lives");
        assert_eq!(response_transport.pending_responses(), 1);
        drop(response_stream);
        assert_eq!(response_transport.pending_responses(), 0);
        assert_eq!(
            response_transport
                .response_mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retained_bytes,
            0
        );
        assert!(matches!(
            response_transport.send(
                &Cx::for_testing(),
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    fastmcp_protocol::RequestId::Number(2),
                    serde_json::Value::Null,
                )),
            ),
            Err(TransportError::Closed)
        ));
        assert_eq!(response_transport.pending_responses(), 0);
    }

    #[test]
    fn streamable_http_response_pop_and_admission_are_nonblocking_under_contention() {
        let mut transport = StreamableHttpTransport::new();
        let response_stream = transport
            .response_stream()
            .expect("response stream can be externalized once");
        let mailbox = Arc::clone(&transport.response_mailbox);
        let mailbox_guard = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        assert!(matches!(
            response_stream.pop_response(Some(&fastmcp_protocol::RequestId::Number(1))),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert!(matches!(
            transport.send(
                &Cx::for_testing(),
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    fastmcp_protocol::RequestId::Number(1),
                    serde_json::Value::Null,
                )),
            ),
            Err(TransportError::Io(ref error))
                if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert_eq!(transport.pending_responses(), 0);
        drop(mailbox_guard);
    }

    #[test]
    fn streamable_http_observes_deadline_budget_and_masked_checkpoint_semantics() {
        let mut transport =
            StreamableHttpTransport::with_capacity(2).expect("capacity two is valid");
        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert!(matches!(
            transport.push_request(&deadline_cx, JsonRpcRequest::new("expired", None, 1_i64),),
            Err(TransportError::Timeout)
        ));
        assert_eq!(transport.pending_requests(), 0);

        let exhausted_cx =
            Cx::for_testing_with_budget(asupersync::Budget::new().with_poll_quota(0));
        assert!(matches!(
            transport.push_request(
                &exhausted_cx,
                JsonRpcRequest::new("budget-exhausted", None, 2_i64),
            ),
            Err(TransportError::Cancelled)
        ));

        let masked_deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        masked_deadline_cx.masked(|| {
            transport
                .push_request(
                    &masked_deadline_cx,
                    JsonRpcRequest::new("masked", None, 3_i64),
                )
                .expect("masking defers deadline enforcement at the checkpoint");
        });
        assert!(matches!(
            transport.recv(&Cx::for_testing()),
            Ok(JsonRpcMessage::Request(ref request)) if request.method == "masked"
        ));
        assert!(matches!(
            transport.push_request(
                &masked_deadline_cx,
                JsonRpcRequest::new("after-mask", None, 4_i64),
            ),
            Err(TransportError::Timeout)
        ));

        let mut response_transport = StreamableHttpTransport::new();
        let response_stream = response_transport
            .response_stream()
            .expect("response stream can be externalized once");
        let response_id = fastmcp_protocol::RequestId::Number(5);
        response_transport
            .send(
                &Cx::for_testing(),
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    response_id.clone(),
                    serde_json::Value::Null,
                )),
            )
            .expect("response is admitted");
        let cancelled_cx = Cx::for_testing();
        cancelled_cx.set_cancel_requested(true);
        let masked_response = cancelled_cx
            .masked(|| response_stream.recv_response(&cancelled_cx, Some(&response_id)));
        assert_eq!(
            masked_response.expect("masking defers cancellation").id,
            Some(response_id)
        );

        let mut empty_transport = StreamableHttpTransport::new();
        let empty_stream = empty_transport
            .response_stream()
            .expect("response stream can be externalized once");
        let expired_wait_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert!(matches!(
            empty_stream.recv_response(
                &expired_wait_cx,
                Some(&fastmcp_protocol::RequestId::Number(6)),
            ),
            Err(TransportError::Timeout)
        ));
    }

    #[test]
    fn streamable_http_transport_send_and_recv_use_checkpoint_not_raw_cancel_flag() {
        let mut send_transport = StreamableHttpTransport::new();
        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert!(matches!(
            send_transport.send(
                &deadline_cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    fastmcp_protocol::RequestId::Number(1),
                    serde_json::Value::Null,
                )),
            ),
            Err(TransportError::Timeout)
        ));
        assert_eq!(send_transport.pending_responses(), 0);

        let masked_cx = Cx::for_testing();
        masked_cx.set_cancel_requested(true);
        masked_cx.masked(|| {
            send_transport
                .send(
                    &masked_cx,
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        fastmcp_protocol::RequestId::Number(2),
                        serde_json::Value::Null,
                    )),
                )
                .expect("masking defers cancellation during send admission");
        });
        assert_eq!(
            send_transport
                .pop_response()
                .expect("direct response consumer remains open")
                .expect("masked send admitted one response")
                .id,
            Some(fastmcp_protocol::RequestId::Number(2))
        );

        let mut receive_transport = StreamableHttpTransport::new();
        let expired_receive_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert!(matches!(
            receive_transport.recv(&expired_receive_cx),
            Err(TransportError::Timeout)
        ));
    }

    #[test]
    fn streamable_http_recv_drains_admission_that_finishes_during_ingress_close() {
        let mut transport =
            StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
        let empty_polls = Arc::clone(&transport.request_empty_polls);
        let ingress = transport
            .request_ingress()
            .expect("request ingress can be externalized once");
        let request = JsonRpcRequest::new("close/drain", None, 909_i64);
        let serialized_bytes = ingress
            .codec
            .encode_request(&request)
            .expect("test request is encodable")
            .len();
        let sender = ingress.sender.clone();
        let retained_bytes = Arc::clone(&ingress.retained_bytes);
        let retained_bytes_observer = Arc::clone(&ingress.retained_bytes);
        let admissions_open = Arc::clone(&ingress.admissions_open);
        let active_admissions = Arc::clone(&ingress.active_admissions);
        let active_admissions_observer = Arc::clone(&ingress.active_admissions);
        let max_queued_bytes = ingress.max_queued_bytes;
        let (admitted_sender, admitted_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();

        let producer = std::thread::spawn(move || {
            let admission = begin_streamable_admission(&admissions_open, &active_admissions)
                .expect("producer enters before close");
            reserve_streamable_bytes(
                &retained_bytes,
                max_queued_bytes,
                serialized_bytes,
                &admissions_open,
                &Cx::for_testing(),
                "test request byte budget is full",
            )
            .expect("producer reserves bytes before close");
            admitted_sender
                .send(())
                .expect("test admission receiver remains available");
            release_receiver
                .recv()
                .expect("test releases the admitted producer");
            assert!(
                sender
                    .try_send(QueuedRequest {
                        message: request,
                        serialized_bytes,
                    })
                    .is_ok(),
                "the pre-close admission commits to the bounded queue"
            );
            drop(admission);
        });
        admitted_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("producer reaches the admitted pre-commit state");

        let close_open = Arc::clone(&ingress.admissions_open);
        let closer = std::thread::spawn(move || ingress.close());
        let close_deadline = Instant::now() + Duration::from_secs(1);
        while close_open.load(Ordering::SeqCst) {
            if Instant::now() >= close_deadline {
                release_sender
                    .send(())
                    .expect("release producer during test cleanup");
                producer.join().expect("producer cleanup succeeds");
                closer.join().expect("closer cleanup succeeds");
                panic!("ingress close did not seal the admission gate");
            }
            std::thread::yield_now();
        }

        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let receive_cx = Cx::for_testing();
        let cancel_receive_cx = receive_cx.clone();
        let receiver = std::thread::spawn(move || {
            result_sender
                .send(transport.recv(&receive_cx))
                .expect("test result receiver remains available");
        });
        if !wait_for_counter(&empty_polls, 1) {
            release_sender
                .send(())
                .expect("release producer during test cleanup");
            producer.join().expect("producer cleanup succeeds");
            closer.join().expect("closer cleanup succeeds");
            cancel_receive_cx.set_cancel_requested(true);
            receiver.join().expect("receiver cleanup succeeds");
            panic!("receiver reported closure instead of waiting for the active admission");
        }

        release_sender
            .send(())
            .expect("release the admitted producer");
        producer.join().expect("producer completes");
        closer.join().expect("ingress close completes");
        let received = match result_receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result.expect("pre-close admission remains receivable"),
            Err(error) => {
                cancel_receive_cx.set_cancel_requested(true);
                receiver.join().expect("receiver cleanup succeeds");
                panic!("receiver did not drain the admitted request: {error}");
            }
        };
        receiver.join().expect("receiver completes");
        assert!(matches!(
            received,
            JsonRpcMessage::Request(ref request) if request.method == "close/drain"
        ));
        assert_eq!(retained_bytes_observer.load(Ordering::Acquire), 0);
        assert_eq!(active_admissions_observer.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn streamable_http_request_close_race_preserves_accounting() {
        for sequence in 0..32_i64 {
            let mut transport =
                StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
            let ingress = transport
                .request_ingress()
                .expect("request ingress can be externalized once");
            let worker_ingress = ingress.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                worker_ingress.push_request(
                    &Cx::for_testing(),
                    JsonRpcRequest::new("race", None, sequence),
                )
            });

            barrier.wait();
            ingress.close();
            match worker.join().expect("request producer completes") {
                Ok(()) => assert!(matches!(
                    transport.recv(&Cx::for_testing()),
                    Ok(JsonRpcMessage::Request(_))
                )),
                Err(TransportError::Closed) => {
                    assert_eq!(transport.pending_requests(), 0);
                }
                Err(error) => panic!("unexpected request race result: {error}"),
            }
            assert_eq!(transport.pending_requests(), 0);
            assert_eq!(transport.request_retained_bytes.load(Ordering::Acquire), 0);
            assert_eq!(
                transport.request_active_admissions.load(Ordering::Acquire),
                0
            );
        }
    }

    #[test]
    fn streamable_http_response_close_race_preserves_accounting_and_drain() {
        for sequence in 0..32_i64 {
            let mut transport =
                StreamableHttpTransport::with_capacity(1).expect("capacity one is valid");
            let response_stream = transport
                .response_stream()
                .expect("response stream can be externalized once");
            let response_id = fastmcp_protocol::RequestId::Number(sequence);
            let worker_id = response_id.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                let result = transport.send(
                    &Cx::for_testing(),
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        worker_id,
                        serde_json::Value::Null,
                    )),
                );
                (transport, result)
            });

            barrier.wait();
            response_stream.close();
            let (transport, result) = worker.join().expect("response producer completes");
            match result {
                Ok(()) => {
                    let drained = response_stream
                        .pop_response(Some(&response_id))
                        .expect("pre-close admission remains drainable")
                        .expect("successful admission has exactly one response");
                    assert_eq!(drained.id, Some(response_id.clone()));
                }
                Err(TransportError::Closed) => {
                    assert_eq!(transport.pending_responses(), 0);
                }
                Err(error) => panic!("unexpected response race result: {error}"),
            }
            assert_eq!(transport.pending_responses(), 0);
            assert_eq!(
                transport
                    .response_mailbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retained_bytes,
                0
            );
            assert_eq!(
                transport.response_active_admissions.load(Ordering::Acquire),
                0
            );
            assert!(matches!(
                response_stream.pop_response(Some(&response_id)),
                Err(TransportError::Closed)
            ));
        }
    }

    #[test]
    fn e2e_http_session_lifecycle() {
        let store = SessionStore::new(Duration::from_millis(100));

        // Create session
        let id = store.create().unwrap();
        assert_eq!(store.count(), 1);

        // Get and modify session
        let mut session = store.get(&id).unwrap();
        session.set("user_id", serde_json::json!(42)).unwrap();
        store.update(session).unwrap();

        // Retrieve and verify
        let session = store.get(&id).unwrap();
        assert_eq!(session.get("user_id"), Some(&serde_json::json!(42)));

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(150));

        // Session should be expired
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn http_session_count_excludes_expired_sessions_without_prior_lookup() {
        let timeout = Duration::from_secs(60);
        let store = SessionStore::new(timeout);
        let id = store.create().unwrap();
        assert_eq!(store.count(), 1);

        store
            .sessions
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .last_activity = Instant::now() - timeout - Duration::from_secs(1);

        assert_eq!(store.count(), 0);
    }

    #[test]
    fn e2e_http_transport_cancellation() {
        use std::io::Cursor;

        let reader = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let mut transport = HttpTransport::new(reader, &mut output);

        // Send should respect cancellation
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: None,
        };
        let result = transport.send(&cx, &JsonRpcMessage::Response(response));
        assert!(matches!(result, Err(TransportError::Cancelled)));

        // Nothing should be written
        assert_eq!(output.len(), 0);
    }

    #[test]
    fn e2e_http_transport_close() {
        use std::io::Cursor;

        let reader = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let cx = Cx::for_testing();
        let mut transport = HttpTransport::new(reader, &mut output);

        // Close transport
        transport.close(&cx).unwrap();

        // Operations should fail after close
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: None,
        };
        let result = transport.send(&cx, &JsonRpcMessage::Response(response));
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    #[test]
    fn e2e_http_body_size_limit() {
        let config = HttpHandlerConfig {
            max_body_size: 100,
            ..Default::default()
        };
        let handler = HttpRequestHandler::with_config(config);

        // Body exceeding limit
        let large_body = vec![b'x'; 200];
        let request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
            .with_header("Content-Type", "application/json")
            .with_body(large_body);

        let result = handler.parse_request(&request);
        assert!(matches!(result, Err(HttpError::BodyTooLarge { .. })));
    }

    #[test]
    fn handler_body_limit_configures_the_strict_codec_boundary() {
        let configured_limit = 12 * 1024 * 1024;
        let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            max_body_size: configured_limit,
            ..Default::default()
        });

        assert_eq!(handler.codec.max_message_size(), configured_limit);
    }

    #[test]
    fn http_method_as_str_round_trips() {
        let methods = [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
            HttpMethod::Options,
            HttpMethod::Head,
            HttpMethod::Patch,
        ];
        for m in methods {
            let s = m.as_str();
            let parsed = HttpMethod::parse(s).unwrap();
            assert_eq!(parsed, m);
        }
    }

    #[test]
    fn http_status_boundary_cases() {
        assert!(HttpStatus(299).is_success());
        assert!(!HttpStatus(300).is_success());
        assert!(HttpStatus(499).is_client_error());
        assert!(!HttpStatus(500).is_client_error());
        assert!(HttpStatus(599).is_server_error());
        assert!(!HttpStatus(600).is_server_error());
    }

    #[test]
    fn http_request_content_type_and_authorization() {
        let req = HttpRequest::new(HttpMethod::Get, "/")
            .with_header("Content-Type", "text/plain")
            .with_header("Authorization", "Bearer token123");
        assert_eq!(req.content_type(), Some("text/plain"));
        assert_eq!(req.authorization(), Some("Bearer token123"));
    }

    #[test]
    fn http_request_debug_redacts_headers_body_path_and_query_values() {
        let canary = "HTTP-REQUEST-SECRET-CANARY";
        let req = HttpRequest::new(HttpMethod::Post, format!("/mcp/{canary}"))
            .with_header("Authorization", format!("Bearer {canary}"))
            .with_body(format!("{{\"secret\":\"{canary}\"}}"))
            .with_query("token", canary);

        let debug = format!("{req:?}");
        assert!(debug.contains("HttpRequest"));
        assert!(debug.contains("header_count: 1"));
        assert!(!debug.contains(canary));
        assert!(!debug.contains("Authorization"));
        assert!(!debug.contains("token"));
    }

    #[test]
    fn http_request_json_parse() {
        let body = serde_json::json!({"key": "value"});
        let req =
            HttpRequest::new(HttpMethod::Post, "/").with_body(serde_json::to_vec(&body).unwrap());
        let parsed: serde_json::Value = req.json().unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn http_response_convenience_constructors() {
        let bad = HttpResponse::bad_request();
        assert_eq!(bad.status, HttpStatus::BAD_REQUEST);
        let err = HttpResponse::internal_error();
        assert_eq!(err.status, HttpStatus::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn http_handler_config_defaults() {
        let config = HttpHandlerConfig::default();
        assert_eq!(config.base_path, "/mcp/v1");
        assert!(!config.allow_cors);
        assert_eq!(config.cors_origins.len(), 0);
        assert_eq!(config.max_body_size, 10 * 1024 * 1024);
    }

    #[test]
    fn http_handler_config_accessor() {
        let handler = HttpRequestHandler::new();
        assert_eq!(handler.config().base_path, "/mcp/v1");
    }

    #[test]
    fn http_error_display_all_variants() {
        let cases: Vec<(HttpError, &str)> = vec![
            (HttpError::InvalidMethod("X".into()), "invalid HTTP method"),
            (
                HttpError::InvalidRequestLine("bad".into()),
                "invalid HTTP request line",
            ),
            (
                HttpError::InvalidHeader("bad".into()),
                "invalid HTTP header",
            ),
            (
                HttpError::InvalidContentType("text/xml".into()),
                "invalid content type",
            ),
            (
                HttpError::InvalidPath("/wrong".into()),
                "invalid MCP endpoint path",
            ),
            (
                HttpError::OriginNotAllowed("https://denied.example".into()),
                "origin is not allowed",
            ),
            (
                HttpError::HeadersTooLarge { size: 100, max: 50 },
                "headers too large: 100 > 50",
            ),
            (
                HttpError::BodyTooLarge {
                    size: 200,
                    max: 100,
                },
                "body too large: 200 > 100",
            ),
            (
                HttpError::UnsupportedTransferEncoding("gzip".into()),
                "unsupported transfer encoding",
            ),
            (HttpError::Timeout, "request timeout"),
            (HttpError::Closed, "connection closed"),
        ];
        for (err, expected) in cases {
            assert!(
                err.to_string().contains(expected),
                "expected '{}' in '{}'",
                expected,
                err
            );
        }
    }

    #[test]
    fn http_error_from_codec_error() {
        let codec_err = CodecError::MessageTooLarge(999);
        let http_err: HttpError = codec_err.into();
        assert!(matches!(http_err, HttpError::CodecError(_)));
        assert!(http_err.to_string().contains("codec error"));
    }

    #[test]
    fn http_error_from_transport_error() {
        let transport_err = TransportError::Closed;
        let http_err: HttpError = transport_err.into();
        assert!(matches!(http_err, HttpError::Transport(_)));
        assert!(http_err.to_string().contains("transport error"));
    }

    #[test]
    fn http_transport_send_rejects_request_messages() {
        use std::io::Cursor;

        let reader = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let cx = Cx::for_testing();
        let mut transport = HttpTransport::new(reader, &mut output);

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = transport.send(&cx, &JsonRpcMessage::Request(request));
        assert!(result.is_err());
    }

    #[test]
    fn session_store_cleanup_removes_expired() {
        let store = SessionStore::new(Duration::from_millis(50));
        let _id1 = store.create().unwrap();
        let _id2 = store.create().unwrap();
        assert_eq!(store.count(), 2);

        std::thread::sleep(Duration::from_millis(100));
        store.cleanup();
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn handle_options_cors_disabled() {
        let config = HttpHandlerConfig {
            allow_cors: false,
            ..Default::default()
        };
        let handler = HttpRequestHandler::with_config(config);
        let request = HttpRequest::new(HttpMethod::Options, "/mcp/v1");
        let response = handler.handle_options(&request);
        assert_eq!(response.status, HttpStatus::METHOD_NOT_ALLOWED);
    }

    #[cfg(feature = "legacy-2024-11-05")]
    fn dual_era_modern_sse_request(id: i64) -> HttpRequest {
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "weather",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            })),
            id,
        );
        HttpRequest::new(HttpMethod::Post, "/mcp")
            .with_header("content-type", "application/json")
            .with_header("accept", "text/event-stream")
            .with_header("MCP-Protocol-Version", "2026-07-28")
            .with_header("Mcp-Method", "tools/call")
            .with_header("Mcp-Name", "weather")
            .with_body(serde_json::to_vec(&request).expect("modern request serializes"))
    }

    #[cfg(feature = "legacy-2024-11-05")]
    fn dual_era_endpoint() -> DualEraHttpEndpoint {
        let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: "/mcp".to_string(),
            ..HttpHandlerConfig::default()
        });
        let config =
            DualEraHttpEndpointConfig::new("/legacy/sse", "/legacy/messages", "http://legacy.test");
        DualEraHttpEndpoint::new(handler, config).expect("dual-era endpoint configuration is valid")
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_refuses_a_tls_legacy_origin_and_says_why() {
        let endpoint = |origin: &str| {
            DualEraHttpEndpoint::new(
                HttpRequestHandler::new(),
                DualEraHttpEndpointConfig::new("/legacy/sse", "/legacy/messages", origin),
            )
        };
        assert!(endpoint("http://legacy.test").is_ok());
        // Near-identical negative: only the scheme differs.
        match endpoint("https://legacy.test").err() {
            Some(DualEraHttpEndpointError::InvalidConfiguration(message)) => {
                assert_eq!(message, LEGACY_ORIGIN_REQUIRES_PLAIN_HTTP);
            }
            other => panic!("an https legacy origin must be refused: {other:?}"),
        }
    }

    #[cfg(feature = "legacy-2024-11-05")]
    fn dual_era_legacy_initialize_request(id: i64) -> JsonRpcRequest {
        JsonRpcRequest::new(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "legacy-http-client",
                    "version": "1.0.0"
                }
            })),
            id,
        )
    }

    fn task_lifecycle_http_request(method: &str, mcp_name: Option<&str>) -> HttpRequest {
        let mut parameters = serde_json::json!({
            "taskId": "task-73",
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {
                    "extensions": {"io.modelcontextprotocol/tasks": {}}
                }
            }
        });
        if method == "tasks/update" {
            parameters
                .as_object_mut()
                .expect("task parameters remain an object")
                .insert("inputResponses".to_owned(), serde_json::json!({}));
        }
        let json_rpc = JsonRpcRequest::new(method, Some(parameters), 73_i64);
        let mut request = HttpRequest::new(HttpMethod::Post, "/mcp")
            .with_header("content-type", "application/json")
            .with_header("accept", "application/json")
            .with_header("MCP-Protocol-Version", "2026-07-28")
            .with_header("Mcp-Method", method);
        if let Some(mcp_name) = mcp_name {
            request = request.with_header("Mcp-Name", mcp_name);
        }
        request.with_body(serde_json::to_vec(&json_rpc).expect("task request serializes"))
    }

    fn task_lifecycle_handler() -> HttpRequestHandler {
        HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: "/mcp".to_owned(),
            ..HttpHandlerConfig::default()
        })
    }

    #[test]
    fn modern_transport_admits_task_lifecycle_task_id_mcp_name_mirrors() {
        let handler = task_lifecycle_handler();
        for method in ["tasks/get", "tasks/update", "tasks/cancel"] {
            let admitted = handler
                .admit_modern_request(&task_lifecycle_http_request(method, Some("task-73")))
                .expect("matching Tasks taskId/Mcp-Name must admit before dispatch");
            assert_eq!(admitted.request().method, method);
        }
    }

    #[test]
    fn modern_transport_rejects_task_lifecycle_mismatched_mcp_name_before_dispatch() {
        let error = task_lifecycle_handler()
            .admit_modern_request(&task_lifecycle_http_request(
                "tasks/get",
                Some("task-other"),
            ))
            .expect_err("changing only Mcp-Name must reject before Tasks dispatch");
        assert!(matches!(error, HttpError::ProtocolAdmission(_)));
    }

    #[test]
    fn modern_transport_rejects_task_lifecycle_missing_mcp_name_before_dispatch() {
        let error = task_lifecycle_handler()
            .admit_modern_request(&task_lifecycle_http_request("tasks/get", None))
            .expect_err("removing only Mcp-Name must reject before Tasks dispatch");
        assert!(matches!(error, HttpError::ProtocolAdmission(_)));
    }

    fn modern_http_accept_request(accept: Option<&str>) -> HttpRequest {
        let request = HttpRequest::new(HttpMethod::Post, "/mcp")
            .with_header("content-type", "application/json")
            .with_header("mcp-protocol-version", "2026-07-28")
            .with_header("mcp-method", "server/discover")
            .with_body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 271,
                    "method": "server/discover",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    },
                }))
                .expect("modern discovery request serializes"),
            );
        match accept {
            Some(accept) => request.with_header("accept", accept),
            None => request,
        }
    }

    #[test]
    fn modern_http_handler_accept_preferences_positive() {
        let handler = task_lifecycle_handler();
        for (accept, expected) in [
            (None, HttpResponseRepresentation::Json),
            (Some("*/*"), HttpResponseRepresentation::Json),
            (Some("application/*"), HttpResponseRepresentation::Json),
            (Some("text/*"), HttpResponseRepresentation::Sse),
            (
                Some("application/json;q=0, */*;q=1"),
                HttpResponseRepresentation::Sse,
            ),
            (
                Some("*/*;q=1, application/json;q=0"),
                HttpResponseRepresentation::Sse,
            ),
            (
                Some("text/event-stream;q=0, */*;q=1"),
                HttpResponseRepresentation::Json,
            ),
            (
                Some("application/json;q=0.1, text/event-stream;q=0.9"),
                HttpResponseRepresentation::Sse,
            ),
            (
                Some("application/json;q=0.7, text/event-stream;q=0.7"),
                HttpResponseRepresentation::Json,
            ),
            (
                Some("application/json, APPLICATION/JSON;q=0, text/event-stream"),
                HttpResponseRepresentation::Sse,
            ),
        ] {
            let request = modern_http_accept_request(accept);
            let original_body = request.body.clone();
            let admitted = handler
                .admit_modern_request(&request)
                .expect("valid HTTP preferences admit through the public handler");
            assert_eq!(admitted.response_representation(), expected, "{accept:?}");
            assert_eq!(admitted.request().id, Some(271_i64.into()));
            assert_eq!(admitted.request().method, "server/discover");
            assert_eq!(request.body, original_body);
        }
    }

    #[test]
    fn modern_http_handler_accept_preferences_planted_negative() {
        let handler = task_lifecycle_handler();
        for accept in [
            "*/*;q=1, application/json;q=0, text/event-stream;q=0",
            "application/json;q=NaN, text/event-stream",
            "application/json;q=1;Q=0, text/event-stream",
            "application/json;q=0.0001, text/event-stream",
            "application/json;q=1.0000, text/event-stream",
            "application/json;q=\"0.5\", text/event-stream",
            "application/json;q =1, text/event-stream",
            "*/json, text/event-stream",
            "application/json;profile=\"x, text/event-stream;q=1\"",
            "application/json;profile=\"unterminated, text/event-stream",
            "",
        ] {
            let request = modern_http_accept_request(Some(accept));
            let original_body = request.body.clone();
            assert!(matches!(
                handler.admit_modern_request(&request),
                Err(HttpError::NotAcceptable)
            ));
            assert_eq!(request.body, original_body);
        }
        let admitted = handler
            .admit_modern_request(&modern_http_accept_request(Some("application/json")))
            .expect("refused preferences do not affect a later request");
        assert_eq!(
            admitted.response_representation(),
            HttpResponseRepresentation::Json,
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_modern_accept_preferences_select_body_and_reject_before_enqueue() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let accepted = modern_http_accept_request(Some("application/json;q=0, */*;q=1"));
        let rejected = accepted.clone().with_header(
            "accept",
            "application/json;q=0, text/event-stream;q=0, */*;q=1",
        );
        assert!(matches!(
            session.handle(&cx, rejected),
            Err(DualEraHttpEndpointError::Http(HttpError::NotAcceptable))
        ));
        assert_eq!(session.modern_transport.pending_requests(), 0);
        assert_eq!(session.modern_responses.live_request_bodies().unwrap(), 0);

        let response = session
            .handle(&cx, accepted)
            .expect("an exact JSON exclusion leaves the wildcard SSE offer");
        let DualEraHttpEndpointResponse::ModernSse(response) = response else {
            panic!("the public transport endpoint must not select explicitly excluded JSON");
        };
        assert_eq!(response.response().status, HttpStatus::OK);
        assert_eq!(
            response
                .response()
                .headers
                .get("content-type")
                .map(String::as_str),
            Some("text/event-stream"),
        );
        assert_eq!(session.modern_transport.pending_requests(), 1);
        assert_eq!(
            session.recv_modern_request(&cx).unwrap().id,
            Some(271_i64.into()),
        );
        assert_eq!(session.modern_transport.pending_requests(), 0);
        response
            .sender()
            .send_response(
                &cx,
                JsonRpcResponse::success(
                    271_i64.into(),
                    serde_json::json!({"resultType": "complete"}),
                ),
            )
            .expect("the selected SSE body accepts its terminal result");
        let event = response.recv_event(&cx).expect("terminal result is emitted");
        assert!(matches!(
            Codec::new().decode_complete_message(event.data.as_bytes()),
            Ok(JsonRpcMessage::Response(message)) if message.id == Some(271_i64.into())
        ));
        assert!(response.is_finished());
        assert!(matches!(
            response.pop_event(),
            Err(DualEraHttpEndpointError::Transport(TransportError::Closed))
        ));
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_modern_h1_sse_delivers_every_request_owned_event_before_terminal() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();

        let response = session
            .handle(&cx, dual_era_modern_sse_request(171))
            .expect("a real modern H1 SSE request is admitted");
        let DualEraHttpEndpointResponse::ModernSse(response) = response else {
            panic!("Accept: text/event-stream creates a request-owned modern SSE body");
        };
        assert_eq!(
            session
                .recv_modern_request(&cx)
                .expect("admitted H1 request reaches the modern dispatch side")
                .method,
            "tools/call"
        );

        let sender = response.sender();
        sender
            .send_notification(
                &cx,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 1})),
                ),
            )
            .expect("the first request-owned notification is admitted");
        sender
            .send_notification(
                &cx,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 2})),
                ),
            )
            .expect("the second request-owned notification is admitted");
        sender
            .send_response(
                &cx,
                JsonRpcResponse::success(RequestId::Number(171), serde_json::json!({"ok": true})),
            )
            .expect("the terminal response is admitted after both notifications");

        let first = response
            .pop_event()
            .expect("the first event frames")
            .expect("the first event is queued");
        let second = response
            .pop_event()
            .expect("the second event frames")
            .expect("the second event is queued");
        let terminal = response
            .pop_event()
            .expect("the terminal event frames")
            .expect("the terminal event is queued");
        assert!(matches!(
            codec
                .decode_complete_message(first.data.as_bytes())
                .expect("first SSE event remains JSON-RPC"),
            JsonRpcMessage::Request(notification)
                if notification.method == "notifications/progress"
                    && notification.params == Some(serde_json::json!({"progress": 1}))
        ));
        assert!(matches!(
            codec
                .decode_complete_message(second.data.as_bytes())
                .expect("second SSE event remains JSON-RPC"),
            JsonRpcMessage::Request(notification)
                if notification.method == "notifications/progress"
                    && notification.params == Some(serde_json::json!({"progress": 2}))
        ));
        assert!(matches!(
            codec
                .decode_complete_message(terminal.data.as_bytes())
                .expect("terminal SSE event remains JSON-RPC"),
            JsonRpcMessage::Response(message) if message.id == Some(RequestId::Number(171))
        ));
        assert!(response.is_finished());
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_modern_h1_sse_disconnect_cancels_and_rejects_the_later_effect() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();

        let response = session
            .handle(&cx, dual_era_modern_sse_request(172))
            .expect("the otherwise identical modern H1 SSE request is admitted");
        let DualEraHttpEndpointResponse::ModernSse(response) = response else {
            panic!("Accept: text/event-stream creates a request-owned modern SSE body");
        };
        assert_eq!(
            session
                .recv_modern_request(&cx)
                .expect("admitted H1 request reaches the modern dispatch side")
                .method,
            "tools/call"
        );

        let sender = response.sender();
        sender
            .send_notification(
                &cx,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 1})),
                ),
            )
            .expect("the first request-owned notification is admitted");

        // Planted forbidden dimension: the peer body closes before the
        // otherwise identical second notification and terminal response.
        drop(response);

        assert!(sender.request_cancellation().is_cancel_requested());
        assert!(matches!(
            sender.send_notification(
                &cx,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 2})),
                ),
            ),
            Err(TransportError::Cancelled)
        ));
        assert!(matches!(
            sender.send_response(
                &cx,
                JsonRpcResponse::success(RequestId::Number(172), serde_json::json!({"ok": true})),
            ),
            Err(TransportError::Cancelled)
        ));
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_composes_fresh_legacy_sse_with_modern_request_bodies() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();

        session
            .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(41),
                serde_json::json!({"phase": "before-stream"}),
            )))
            .expect("a message without a live stream is not retained");

        let legacy_get = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut legacy_get) = legacy_get else {
            panic!("legacy GET returns a live SSE response body");
        };
        assert_eq!(legacy_get.response().status, HttpStatus::OK);
        assert_eq!(
            legacy_get.response().headers.get("content-type"),
            Some(&"text/event-stream".to_string())
        );
        assert_eq!(
            legacy_get.response().headers.get("cache-control"),
            Some(&"no-cache".to_string())
        );
        assert_eq!(
            legacy_get.response().headers.get("connection"),
            Some(&"keep-alive".to_string())
        );
        assert_eq!(
            legacy_get.response().headers.get("x-accel-buffering"),
            Some(&"no".to_string())
        );

        let endpoint_event = legacy_get
            .recv_event(&cx)
            .expect("fresh legacy endpoint event is available");
        assert_eq!(endpoint_event.data, session.legacy_message_endpoint());
        assert!(endpoint_event.id.is_none());

        session
            .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(42),
                serde_json::json!({"phase": "live"}),
            )))
            .expect("a message is delivered to the live legacy stream");
        let live_event = legacy_get
            .recv_event(&cx)
            .expect("the live legacy event is available");
        assert!(live_event.id.is_none());
        assert!(matches!(
            codec
                .decode_complete_message(live_event.data.as_bytes())
                .expect("live legacy data remains JSON-RPC"),
            JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(42))
        ));

        let legacy_request =
            JsonRpcRequest::new("ping", Some(serde_json::json!({"value": 1})), 77_i64);
        let legacy_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&legacy_request)
                    .expect("legacy request serializes"),
            );
        let accepted_post = session
            .handle(&cx, legacy_post.clone())
            .expect("advertised legacy POST is admitted");
        let DualEraHttpEndpointResponse::Immediate(accepted_post) = accepted_post else {
            panic!("legacy POST has a complete HTTP acceptance response");
        };
        assert_eq!(accepted_post.status, HttpStatus::ACCEPTED);
        assert_eq!(
            session
                .take_legacy_request()
                .expect("legacy POST reaches only the legacy request queue")
                .method,
            "ping"
        );

        // Planted forbidden dimension: the same advertised POST follows SSE
        // body closure, so it must fail before it can repopulate the queue.
        drop(legacy_get);
        let rejected_post = session
            .handle(&cx, legacy_post)
            .expect("a closed legacy SSE lifecycle becomes an HTTP rejection");
        let DualEraHttpEndpointResponse::Immediate(rejected_post) = rejected_post else {
            panic!("a post after the SSE body closes cannot create a response stream");
        };
        assert_eq!(rejected_post.status, HttpStatus::SERVICE_UNAVAILABLE);
        assert!(session.take_legacy_request().is_none());

        let modern_sse = session
            .handle(&cx, dual_era_modern_sse_request(91))
            .expect("modern request with matching directional headers is admitted");
        let DualEraHttpEndpointResponse::ModernSse(modern_sse) = modern_sse else {
            panic!("modern Accept selection creates a request-scoped SSE response body");
        };
        assert_eq!(
            modern_sse.response().headers.get("content-type"),
            Some(&"text/event-stream".to_string())
        );
        assert_eq!(
            modern_sse.response().headers.get("x-accel-buffering"),
            Some(&"no".to_string())
        );
        assert_eq!(
            session
                .recv_modern_request(&cx)
                .expect("only the modern route reaches the modern transport")
                .method,
            "tools/call"
        );
        let cancellation = modern_sse.cancellation();
        session
            .send_modern_sse_notification(
                &cx,
                &cancellation,
                JsonRpcRequest::notification(
                    "notifications/progress",
                    Some(serde_json::json!({"progress": 50})),
                ),
            )
            .expect("the request-owned modern notification is committed before the response");
        session
            .send_modern_sse_response(
                &cx,
                &cancellation,
                JsonRpcResponse::success(RequestId::Number(91), serde_json::json!({"ok": true})),
            )
            .expect("the bound modern SSE response is committed through its guard");
        let notification_event = modern_sse
            .recv_event(&cx)
            .expect("the request-owned notification renders as the first modern SSE event");
        assert!(notification_event.id.is_none());
        assert!(matches!(
            codec
                .decode_complete_message(notification_event.data.as_bytes())
                .expect("modern SSE notification data remains JSON-RPC"),
            JsonRpcMessage::Request(notification)
                if notification.is_notification()
                    && notification.method == "notifications/progress"
                    && notification.params == Some(serde_json::json!({"progress": 50}))
        ));
        let modern_event = modern_sse
            .recv_event(&cx)
            .expect("the bound modern response renders as an SSE event");
        assert!(modern_event.id.is_none());
        assert!(matches!(
            codec
                .decode_complete_message(modern_event.data.as_bytes())
                .expect("modern SSE data remains JSON-RPC"),
            JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(91))
        ));

        let mut modern_json_request = dual_era_modern_sse_request(93);
        modern_json_request
            .headers
            .insert("accept".to_string(), "application/json".to_string());
        let modern_json = session
            .handle(&cx, modern_json_request)
            .expect("modern JSON request with matching directional headers is admitted");
        let DualEraHttpEndpointResponse::ModernJson(modern_json) = modern_json else {
            panic!("modern JSON Accept selection creates a finite JSON response handle");
        };
        assert_eq!(
            session
                .recv_modern_request(&cx)
                .expect("modern JSON request reaches the modern transport")
                .method,
            "tools/call"
        );
        session
            .send_modern_json_response(
                &cx,
                JsonRpcResponse::success(RequestId::Number(93), serde_json::json!({"ok": true})),
            )
            .expect("modern JSON response is committed");
        let modern_json = modern_json
            .try_response()
            .expect("modern JSON response rendering succeeds")
            .expect("the matching modern JSON response is available");
        assert_eq!(modern_json.status, HttpStatus::OK);
        assert_eq!(
            modern_json.headers.get("content-type"),
            Some(&"application/json".to_string())
        );
        assert!(matches!(
            codec
                .decode_complete_message(&modern_json.body)
                .expect("modern JSON body remains JSON-RPC"),
            JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(93))
        ));

        let abandoned = session
            .handle(&cx, dual_era_modern_sse_request(92))
            .expect("second modern SSE request is admitted before cleanup");
        let DualEraHttpEndpointResponse::ModernSse(abandoned) = abandoned else {
            panic!("second modern request has its own response body");
        };
        let abandoned_cancellation = abandoned.cancellation();
        session.close();
        assert!(session.is_closed());
        assert!(abandoned_cancellation.is_cancelled());
        assert!(session.take_legacy_request().is_none());
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_ignores_last_event_id_and_starts_a_fresh_legacy_stream() {
        let cx = Cx::for_testing();
        let codec = Codec::new();

        let fresh_reconnect = |last_event_id: Option<&str>| {
            let endpoint = dual_era_endpoint();
            let mut session = endpoint.open_session().expect("endpoint opens a session");

            let first_get = session
                .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
                .expect("baseline legacy GET is admitted");
            let DualEraHttpEndpointResponse::LegacySse(mut first_get) = first_get else {
                panic!("baseline legacy GET creates a live SSE response body");
            };
            let first_endpoint = first_get
                .recv_event(&cx)
                .expect("baseline stream begins with its endpoint event");
            assert_eq!(first_endpoint.data, session.legacy_message_endpoint());
            assert!(first_endpoint.id.is_none());
            let stale_lifecycle = session.legacy_lifecycle();

            let stale_request = dual_era_legacy_initialize_request(181);
            let stale_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
                .with_header("content-type", "application/json")
                .with_query("session_id", session.session_id())
                .with_body(
                    codec
                        .encode_request(&stale_request)
                        .expect("old-generation initialize serializes"),
                );
            let stale_post = session
                .handle(&cx, stale_post)
                .expect("old-generation initialize is admitted while its stream is live");
            assert!(matches!(
                stale_post,
                DualEraHttpEndpointResponse::Immediate(response)
                    if response.status == HttpStatus::ACCEPTED
            ));
            assert_eq!(session.legacy_requests.len(), 1);

            session
                .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(81),
                    serde_json::json!({"phase": "closed-stream"}),
                )))
                .expect("the baseline stream accepts one live message");
            drop(first_get);

            let session_id_before = session.session_id().to_owned();
            let endpoint_before = session.legacy_message_endpoint().to_owned();
            let generation_before = session.legacy_stream_generation;
            let pending_before = session.legacy_live_pending.load(Ordering::Acquire);
            assert_eq!(pending_before, 0);

            let reconnect = match last_event_id {
                Some(last_event_id) => HttpRequest::new(HttpMethod::Get, "/legacy/sse")
                    .with_header("last-event-id", last_event_id),
                None => HttpRequest::new(HttpMethod::Get, "/legacy/sse"),
            };
            let resumed_get = session
                .handle(&cx, reconnect)
                .expect("the primed reconnect opens a fresh legacy GET");
            let DualEraHttpEndpointResponse::LegacySse(mut resumed_get) = resumed_get else {
                panic!("the primed reconnect creates a fresh live SSE response body");
            };
            assert_ne!(session.session_id(), session_id_before);
            assert_ne!(session.legacy_message_endpoint(), endpoint_before);
            assert!(
                stale_lifecycle.commit_if_live(|| ()).is_none(),
                "the closed generation cannot regain mutation authority"
            );
            assert!(session.legacy_lifecycle().commit_if_live(|| ()).is_some());
            assert!(session.legacy_stream_generation > generation_before);
            assert!(session.legacy_requests.is_empty());
            assert!(
                session.take_legacy_request().is_none(),
                "the old-generation initialize cannot enter fresh dispatch"
            );
            assert_eq!(
                session.legacy_live_pending.load(Ordering::Acquire),
                pending_before
            );

            let fresh_endpoint = resumed_get
                .recv_event(&cx)
                .expect("fresh stream begins with its endpoint instead of a replay");
            assert_eq!(fresh_endpoint.data, session.legacy_message_endpoint());
            assert!(fresh_endpoint.id.is_none());

            let fresh_request = dual_era_legacy_initialize_request(182);
            let stale_capability_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
                .with_header("content-type", "application/json")
                .with_query("session_id", session_id_before)
                .with_body(
                    codec
                        .encode_request(&fresh_request)
                        .expect("fresh initialize serializes for stale-capability probe"),
                );
            let stale_capability_post = session
                .handle(&cx, stale_capability_post)
                .expect("old POST capability becomes an HTTP rejection");
            assert!(matches!(
                stale_capability_post,
                DualEraHttpEndpointResponse::Immediate(response)
                    if response.status == HttpStatus::NOT_FOUND
            ));
            assert!(session.legacy_requests.is_empty());

            let fresh_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
                .with_header("content-type", "application/json")
                .with_query("session_id", session.session_id())
                .with_body(
                    codec
                        .encode_request(&fresh_request)
                        .expect("fresh-generation initialize serializes"),
                );
            let fresh_post = session
                .handle(&cx, fresh_post)
                .expect("fresh-generation initialize is admitted by the new stream");
            assert!(matches!(
                fresh_post,
                DualEraHttpEndpointResponse::Immediate(response)
                    if response.status == HttpStatus::ACCEPTED
            ));
            assert_eq!(
                session
                    .take_legacy_request()
                    .expect("fresh-generation initialize reaches dispatch")
                    .id,
                Some(RequestId::Number(182))
            );

            session
                .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(82),
                    serde_json::json!({"phase": "fresh-stream"}),
                )))
                .expect("the fresh stream accepts a new live message");
            let fresh_event = resumed_get
                .recv_event(&cx)
                .expect("only the new live message arrives after the endpoint event");
            assert!(fresh_event.id.is_none());
            let JsonRpcMessage::Response(response) = codec
                .decode_complete_message(fresh_event.data.as_bytes())
                .expect("fresh legacy data remains JSON-RPC")
            else {
                panic!("fresh legacy event must be the new response");
            };
            assert_eq!(response.id, Some(RequestId::Number(82)));

            (fresh_endpoint.id, fresh_event.id, response.result)
        };

        let control = fresh_reconnect(None);
        let with_last_event_id = fresh_reconnect(Some("legacy-cursor-that-cannot-replay"));
        assert_eq!(
            with_last_event_id, control,
            "Last-Event-ID must produce the same fresh-only stream as the equivalently primed control"
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_streams_legacy_response_after_advertised_post() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();

        let legacy_sse = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut legacy_sse) = legacy_sse else {
            panic!("legacy GET creates its live SSE response body");
        };
        assert_eq!(legacy_sse.response().status, HttpStatus::OK);
        assert_eq!(
            legacy_sse
                .recv_event(&cx)
                .expect("the advertised legacy POST endpoint is first")
                .data,
            session.legacy_message_endpoint()
        );

        let request = JsonRpcRequest::new("ping", Some(serde_json::json!({"value": 7})), 71_i64);
        let post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&request)
                    .expect("legacy request serializes"),
            );
        let post = session
            .handle(&cx, post)
            .expect("the advertised legacy POST is admitted");
        let DualEraHttpEndpointResponse::Immediate(post) = post else {
            panic!("legacy POST returns its HTTP acceptance response");
        };
        assert_eq!(post.status, HttpStatus::ACCEPTED);
        assert_eq!(
            session
                .take_legacy_request()
                .expect("the POST reached legacy application dispatch")
                .id,
            Some(RequestId::Number(71))
        );

        session
            .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(71),
                serde_json::json!({"pong": true}),
            )))
            .expect("the dispatch response is sent to the live stream");
        let response_event = legacy_sse
            .recv_event(&cx)
            .expect("the open legacy stream receives the response after POST");
        assert!(response_event.id.is_none());
        assert!(matches!(
            codec
                .decode_complete_message(response_event.data.as_bytes())
                .expect("live legacy response stays JSON-RPC"),
            JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(71))
        ));
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_publishes_committed_notification_without_reentering_lifecycle_guard() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();
        let lifecycle = session.legacy_lifecycle();

        let legacy_sse = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut legacy_sse) = legacy_sse else {
            panic!("legacy GET creates its live SSE response body");
        };
        assert_eq!(
            legacy_sse
                .recv_event(&cx)
                .expect("the advertised legacy POST endpoint is first")
                .data,
            session.legacy_message_endpoint()
        );

        let published = lifecycle
            .commit_if_live(|| {
                session.publish_legacy_message_committed(&JsonRpcMessage::Request(
                    JsonRpcRequest::notification(
                        "notifications/tools/list_changed",
                        Some(serde_json::json!({})),
                    ),
                ))
            })
            .expect("a live generation admits one committed catalog notification");
        published.expect("committed publish must not re-lock the generation guard");

        let changed = legacy_sse
            .recv_event(&cx)
            .expect("the live stream receives the committed catalog notification");
        assert!(changed.id.is_none());
        assert!(matches!(
            codec
                .decode_complete_message(changed.data.as_bytes())
                .expect("committed catalog notification stays JSON-RPC"),
            JsonRpcMessage::Request(request)
                if request.method == "notifications/tools/list_changed" && request.id.is_none()
        ));

        drop(legacy_sse);
        assert!(
            lifecycle
                .commit_if_live(|| {
                    session.publish_legacy_message_committed(&JsonRpcMessage::Request(
                        JsonRpcRequest::notification(
                            "notifications/tools/list_changed",
                            Some(serde_json::json!({})),
                        ),
                    ))
                })
                .is_none(),
            "closing only the live body must drop committed-publish authority"
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_legacy_post_admits_exact_reverse_response_but_rejects_request_members() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();

        let stream = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut stream) = stream else {
            panic!("legacy GET creates the liveness gate required for POST");
        };
        let _endpoint = stream
            .recv_event(&cx)
            .expect("the live stream advertises its exact POST endpoint");

        let response = JsonRpcResponse::success(
            RequestId::Number(76),
            serde_json::json!({"legacy": true, "_meta": {"com.example/application": true}}),
        );
        let accepted = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_response(&response)
                    .expect("exact legacy response serializes"),
            );
        let mut rejected = accepted.clone();
        let mut planted: serde_json::Value =
            serde_json::from_slice(&accepted.body).expect("baseline response body is JSON");
        planted["params"] = serde_json::json!({});
        rejected.body = serde_json::to_vec(&planted).expect("planted response encodes");

        let rejected = session
            .handle(&cx, rejected)
            .expect("response shape rejection is an HTTP response");
        let DualEraHttpEndpointResponse::Immediate(rejected) = rejected else {
            panic!("rejected response cannot create a response body");
        };
        assert_eq!(rejected.status, HttpStatus::BAD_REQUEST);
        assert!(session.take_legacy_response().is_none());

        let accepted = session
            .handle(&cx, accepted)
            .expect("otherwise identical exact response is admitted");
        let DualEraHttpEndpointResponse::Immediate(accepted) = accepted else {
            panic!("exact reverse response has an immediate acceptance response");
        };
        assert_eq!(accepted.status, HttpStatus::ACCEPTED);
        assert_eq!(
            session.take_legacy_response(),
            Some(response),
            "only the request-only params member differs from the admitted reverse response"
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_legacy_stream_cancellation_revokes_post_authority_before_reconnect() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let cancelled_cx = Cx::for_testing();
        let codec = Codec::new();

        let stream = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut stream) = stream else {
            panic!("legacy GET creates a live stream");
        };
        let _endpoint = stream
            .recv_event(&cx)
            .expect("the live stream advertises its exact POST endpoint");
        let stale_lifecycle = session.legacy_lifecycle();
        let request = JsonRpcRequest::new("ping", None, 77_i64);
        let post = |session: &DualEraHttpSession| {
            HttpRequest::new(HttpMethod::Post, "/legacy/messages")
                .with_header("content-type", "application/json")
                .with_query("session_id", session.session_id())
                .with_body(
                    codec
                        .encode_request(&request)
                        .expect("exact legacy request serializes"),
                )
        };

        cancelled_cx.set_cancel_requested(true);
        assert!(matches!(
            stream.try_recv_event(&cancelled_cx),
            Err(DualEraHttpEndpointError::Transport(
                TransportError::Cancelled
            ))
        ));
        assert!(
            !stale_lifecycle.is_live(),
            "a cancelled stream must revoke its post-side capability"
        );
        let rejected = session
            .handle(&cx, post(&session))
            .expect("a dead-stream POST has an HTTP response");
        assert!(matches!(
            rejected,
            DualEraHttpEndpointResponse::Immediate(response)
                if response.status == HttpStatus::SERVICE_UNAVAILABLE
        ));
        assert!(session.take_legacy_request().is_none());

        let fresh = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("the cancelled generation permits a fresh reconnect");
        let DualEraHttpEndpointResponse::LegacySse(mut fresh) = fresh else {
            panic!("reconnect creates a fresh stream");
        };
        let _endpoint = fresh
            .recv_event(&cx)
            .expect("fresh stream advertises a fresh POST authority");
        assert!(
            !stale_lifecycle.is_live(),
            "the old generation remains dead after a fresh reconnect"
        );
        let admitted = session
            .handle(&cx, post(&session))
            .expect("otherwise identical POST is admitted by the fresh stream");
        assert!(matches!(
            admitted,
            DualEraHttpEndpointResponse::Immediate(response)
                if response.status == HttpStatus::ACCEPTED
        ));
        assert_eq!(
            session
                .take_legacy_request()
                .expect("fresh stream receives the identical request")
                .id,
            Some(RequestId::Number(77))
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_legacy_post_admits_only_exact_client_ingress_before_enqueue() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();

        let legacy_sse = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut legacy_sse) = legacy_sse else {
            panic!("legacy GET creates a live SSE response body");
        };
        let _endpoint = legacy_sse
            .recv_event(&cx)
            .expect("the client receives the advertised POST endpoint first");

        let accepted_request = JsonRpcRequest::new(
            "ping",
            Some(serde_json::json!({
                "_meta": {"com.example/application": true}
            })),
            72_i64,
        );
        let accepted_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&accepted_request)
                    .expect("client-direction legacy request serializes"),
            );
        let mut final_metadata_request = accepted_request.clone();
        final_metadata_request
            .params
            .as_mut()
            .expect("ping has metadata params")["_meta"]["io.modelcontextprotocol/protocolVersion"] =
            serde_json::json!("2026-07-28");
        let final_metadata_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&final_metadata_request)
                    .expect("cross-era metadata remains JSON-RPC"),
            );
        let server_only_request = JsonRpcRequest::new("roots/list", None, 72_i64);
        let rejected_post = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&server_only_request)
                    .expect("server-direction legacy request remains JSON-RPC"),
            );

        let final_metadata_post = session
            .handle(&cx, final_metadata_post)
            .expect("final-era metadata becomes an HTTP rejection");
        let DualEraHttpEndpointResponse::Immediate(final_metadata_post) = final_metadata_post
        else {
            panic!("cross-era metadata cannot create a streaming response");
        };
        assert_eq!(final_metadata_post.status, HttpStatus::BAD_REQUEST);
        assert!(session.take_legacy_request().is_none());

        let rejected_post = session
            .handle(&cx, rejected_post)
            .expect("server-direction legacy POST becomes an HTTP rejection");
        let DualEraHttpEndpointResponse::Immediate(rejected_post) = rejected_post else {
            panic!("rejected legacy POST cannot create a streaming response");
        };
        assert_eq!(rejected_post.status, HttpStatus::BAD_REQUEST);
        assert!(session.take_legacy_request().is_none());

        let accepted_post = session
            .handle(&cx, accepted_post)
            .expect("client-direction legacy POST is admitted");
        let DualEraHttpEndpointResponse::Immediate(accepted_post) = accepted_post else {
            panic!("accepted legacy POST returns an immediate response");
        };
        assert_eq!(accepted_post.status, HttpStatus::ACCEPTED);
        assert_eq!(
            session
                .take_legacy_request()
                .expect("the rejected POST left the legacy queue unchanged")
                .id,
            Some(RequestId::Number(72))
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_routes_shared_modern_post_and_legacy_get_by_method() {
        let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: "/bridge".to_string(),
            ..HttpHandlerConfig::default()
        });
        let config =
            DualEraHttpEndpointConfig::new("/bridge", "/legacy/messages", "http://legacy.test");
        let endpoint =
            DualEraHttpEndpoint::new(handler, config).expect("shared GET/POST target is valid");
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();

        let legacy_get = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/bridge"))
            .expect("shared GET target selects legacy SSE");
        let DualEraHttpEndpointResponse::LegacySse(legacy_get) = legacy_get else {
            panic!("shared GET target must not enter final modern admission");
        };
        assert_eq!(legacy_get.response().status, HttpStatus::OK);

        let mut modern_post = dual_era_modern_sse_request(78);
        modern_post.path = "/bridge".to_owned();
        let modern_post = session
            .handle(&cx, modern_post)
            .expect("shared POST target selects final modern admission");
        assert!(matches!(
            modern_post,
            DualEraHttpEndpointResponse::ModernSse(_)
        ));

        let rejected = session
            .handle(&cx, HttpRequest::new(HttpMethod::Put, "/bridge"))
            .expect("a shared-target wrong method is an HTTP rejection");
        let DualEraHttpEndpointResponse::Immediate(rejected) = rejected else {
            panic!("wrong shared-target method cannot create a stream");
        };
        assert_eq!(rejected.status, HttpStatus::METHOD_NOT_ALLOWED);
        assert_eq!(
            rejected.headers.get("allow"),
            Some(&"GET, POST".to_string())
        );
        assert!(session.take_legacy_request().is_none());
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_recovers_live_capacity_after_queue_rejection() {
        let handler = HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: "/mcp".to_string(),
            ..HttpHandlerConfig::default()
        });
        let mut config =
            DualEraHttpEndpointConfig::new("/legacy/sse", "/legacy/messages", "http://legacy.test");
        config.legacy_request_capacity = 1;
        let endpoint = DualEraHttpEndpoint::new(handler, config)
            .expect("capacity-one live stream configuration is valid");
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let legacy_sse = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(mut legacy_sse) = legacy_sse else {
            panic!("legacy GET creates a live SSE response body");
        };
        let _endpoint = legacy_sse
            .recv_event(&cx)
            .expect("endpoint event is available before live messages");

        session
            .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(85),
                serde_json::json!({"queued": true}),
            )))
            .expect("the first live message fills the capacity-one queue");
        let rejected = session
            .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(86),
                serde_json::json!({"queued": false}),
            )))
            .expect_err("the second live message is rejected before enqueue");
        assert!(matches!(
            &rejected,
            DualEraHttpEndpointError::Transport(TransportError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && error.to_string().contains("legacy SSE live queue is full")
        ));

        let first_event = legacy_sse
            .recv_event(&cx)
            .expect("the first queued message reaches the live stream");
        assert!(first_event.id.is_none());

        session
            .publish_legacy_message(&JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(86),
                serde_json::json!({"queued": false}),
            )))
            .expect("consuming the first message releases live capacity");
        let recovered_event = legacy_sse
            .recv_event(&cx)
            .expect("the retried message reaches the live stream");
        assert!(recovered_event.id.is_none());
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_rejects_only_wrong_session_legacy_post_without_mutation() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();
        let legacy_sse = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted before an advertised POST");
        let DualEraHttpEndpointResponse::LegacySse(_legacy_sse) = legacy_sse else {
            panic!("legacy GET creates the live SSE lifecycle required for POST");
        };
        let request = JsonRpcRequest::new("ping", Some(serde_json::json!({"value": 9})), 73_i64);
        let accepted = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&request)
                    .expect("legacy request serializes"),
            );
        let mut wrong_session = accepted.clone();
        wrong_session
            .query
            .insert("session_id".to_string(), "other-session".to_string());
        assert_eq!(wrong_session.method, accepted.method);
        assert_eq!(wrong_session.path, accepted.path);
        assert_eq!(wrong_session.headers, accepted.headers);
        assert_eq!(wrong_session.body, accepted.body);
        assert_eq!(wrong_session.query.len(), accepted.query.len());
        assert_ne!(wrong_session.query, accepted.query);

        let rejected = session
            .handle(&cx, wrong_session)
            .expect("wrong-session legacy POST becomes an HTTP rejection");
        let DualEraHttpEndpointResponse::Immediate(rejected) = rejected else {
            panic!("wrong-session POST cannot create a streaming response");
        };
        assert_eq!(rejected.status, HttpStatus::NOT_FOUND);
        assert!(session.take_legacy_request().is_none());

        let accepted = session
            .handle(&cx, accepted)
            .expect("the otherwise identical correct-session POST is admitted");
        let DualEraHttpEndpointResponse::Immediate(accepted) = accepted else {
            panic!("correct-session POST has an immediate acceptance response");
        };
        assert_eq!(accepted.status, HttpStatus::ACCEPTED);
        assert_eq!(
            session
                .take_legacy_request()
                .expect("wrong-session rejection left the queue empty")
                .id,
            Some(RequestId::Number(73))
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_legacy_post_admits_identity_but_rejects_gzip_without_queue_mutation() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let codec = Codec::new();
        let legacy_sse = session
            .handle(&cx, HttpRequest::new(HttpMethod::Get, "/legacy/sse"))
            .expect("legacy SSE GET is admitted before an advertised POST");
        let DualEraHttpEndpointResponse::LegacySse(_legacy_sse) = legacy_sse else {
            panic!("legacy GET creates the live SSE lifecycle required for POST");
        };
        let request = JsonRpcRequest::new("ping", Some(serde_json::json!({"value": 11})), 74_i64);
        let identity = HttpRequest::new(HttpMethod::Post, "/legacy/messages")
            .with_header("content-type", "application/json")
            .with_header("content-encoding", "identity")
            .with_query("session_id", session.session_id())
            .with_body(
                codec
                    .encode_request(&request)
                    .expect("legacy request serializes"),
            );
        let mut gzip = identity.clone();
        // Planted forbidden dimension: only the content coding changes.
        gzip.headers
            .insert("content-encoding".to_owned(), "gzip".to_owned());

        let rejected = session
            .handle(&cx, gzip)
            .expect("unsupported legacy content coding is an HTTP rejection");
        let DualEraHttpEndpointResponse::Immediate(rejected) = rejected else {
            panic!("coded legacy POST cannot allocate a response stream");
        };
        assert_eq!(rejected.status, HttpStatus::BAD_REQUEST);
        assert!(session.take_legacy_request().is_none());

        let admitted = session
            .handle(&cx, identity)
            .expect("the otherwise identical identity-coded POST is admitted");
        let DualEraHttpEndpointResponse::Immediate(admitted) = admitted else {
            panic!("identity-coded legacy POST has an immediate acceptance response");
        };
        assert_eq!(admitted.status, HttpStatus::ACCEPTED);
        assert_eq!(
            session
                .take_legacy_request()
                .expect("rejected coding left the legacy request queue unchanged")
                .id,
            Some(RequestId::Number(74))
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_legacy_get_rejects_body_and_modern_bindings_before_lifecycle_mutation() {
        let baseline = HttpRequest::new(HttpMethod::Get, "/legacy/sse");
        let mut nonempty_body = baseline.clone();
        nonempty_body.body = b"{}".to_vec();
        let rejected_requests = [
            ("body", nonempty_body),
            (
                "protocol version",
                baseline
                    .clone()
                    .with_header("mcp-protocol-version", "2026-07-28"),
            ),
            (
                "method binding",
                baseline.clone().with_header("mcp-method", "tools/call"),
            ),
            (
                "name binding",
                baseline.clone().with_header("mcp-name", "weather"),
            ),
            (
                "modern session",
                baseline
                    .clone()
                    .with_header("mcp-session-id", "final-session"),
            ),
        ];

        for (dimension, rejected_request) in rejected_requests {
            let endpoint = dual_era_endpoint();
            let mut session = endpoint.open_session().expect("endpoint opens a session");
            let cx = Cx::for_testing();
            let before = (
                session.session_id().to_owned(),
                session.legacy_message_endpoint().to_owned(),
                session.legacy_stream_generation,
                session.legacy_live_active.load(Ordering::Acquire),
                session.legacy_live_pending.load(Ordering::Acquire),
                session.legacy_live_sender.is_some(),
                session.legacy_requests.len(),
            );

            let rejected = session
                .handle(&cx, rejected_request)
                .expect("invalid legacy GET becomes an HTTP rejection");
            let DualEraHttpEndpointResponse::Immediate(rejected) = rejected else {
                panic!("invalid legacy GET cannot create a live response body: {dimension}");
            };
            assert_eq!(rejected.status, HttpStatus::BAD_REQUEST, "{dimension}");
            assert_eq!(
                (
                    session.session_id().to_owned(),
                    session.legacy_message_endpoint().to_owned(),
                    session.legacy_stream_generation,
                    session.legacy_live_active.load(Ordering::Acquire),
                    session.legacy_live_pending.load(Ordering::Acquire),
                    session.legacy_live_sender.is_some(),
                    session.legacy_requests.len(),
                ),
                before,
                "legacy GET rejection must preserve all lifecycle and queue state: {dimension}",
            );

            let admitted = session
                .handle(&cx, baseline.clone())
                .expect("the otherwise identical empty unbound GET is admitted");
            assert!(
                matches!(admitted, DualEraHttpEndpointResponse::LegacySse(_)),
                "baseline legacy GET must open the stream: {dimension}"
            );
        }
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn dual_era_endpoint_rejects_only_the_wrong_legacy_sse_method_without_mutation() {
        let endpoint = dual_era_endpoint();
        let mut session = endpoint.open_session().expect("endpoint opens a session");
        let cx = Cx::for_testing();
        let allowed = HttpRequest::new(HttpMethod::Get, "/legacy/sse");
        let mut rejected = allowed.clone();
        rejected.method = HttpMethod::Post;

        let allowed = session
            .handle(&cx, allowed)
            .expect("the baseline legacy SSE GET is admitted");
        let DualEraHttpEndpointResponse::LegacySse(allowed) = allowed else {
            panic!("baseline legacy SSE route returns a live response body");
        };
        assert_eq!(allowed.response().status, HttpStatus::OK);

        let rejected = session
            .handle(&cx, rejected)
            .expect("method rejection is an HTTP response rather than a queue mutation");
        let DualEraHttpEndpointResponse::Immediate(rejected) = rejected else {
            panic!("rejected legacy SSE method returns an immediate response");
        };
        assert_eq!(rejected.status, HttpStatus::METHOD_NOT_ALLOWED);
        assert_eq!(rejected.headers.get("allow"), Some(&"GET".to_string()));
        assert!(session.take_legacy_request().is_none());
    }

    #[test]
    fn modern_http_sse_collector_incrementally_delivers_notifications_then_terminal() {
        let request_id = RequestId::Number(4_201);
        let notification = JsonRpcRequest::notification(
            "notifications/progress",
            Some(serde_json::json!({"progress": 50})),
        );
        let response = JsonRpcResponse::success(
            request_id.clone(),
            serde_json::json!({"result": "complete"}),
        );
        let notification_json = serde_json::to_string(&notification).expect("notification encodes");
        let notification_bytes = notification_json.as_bytes().to_vec();
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let body = format!(
            "event: ignored\r\ndata: {notification_json}\r\n\r\ndata: {response_json}\r\n\r\n"
        );
        let split = body.len() / 2;
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");
        let cx = Cx::for_testing();
        let mut notifications = Vec::new();

        collector
            .push(&cx, &body.as_bytes()[..split], |notification| {
                notifications.push(
                    serde_json::to_vec(&notification).expect("delivered notification encodes"),
                );
                Ok(())
            })
            .expect("a partial HTTP chunk does not synthesize an SSE event");
        collector
            .push(&cx, &body.as_bytes()[split..], |notification| {
                notifications.push(
                    serde_json::to_vec(&notification).expect("delivered notification encodes"),
                );
                Ok(())
            })
            .expect("the remaining chunk admits its notification and terminal response");

        assert_eq!(notifications, vec![notification_bytes]);
        assert_eq!(
            collector
                .finish(&cx)
                .expect("EOF returns the one correlated terminal response"),
            response
        );
    }

    #[test]
    fn modern_http_sse_collector_finish_returns_one_complete_terminal_response() {
        let request_id = RequestId::Number(4_208);
        let response = JsonRpcResponse::success(request_id.clone(), serde_json::json!(true));
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id, limits).expect("valid request ID");
        let cx = Cx::for_testing();

        collector
            .push(&cx, format!("data: {response_json}\n\n").as_bytes(), |_| {
                Ok(())
            })
            .expect("complete terminal event is admitted before EOF");
        assert_eq!(
            collector
                .finish(&cx)
                .expect("uncancelled EOF releases the terminal response"),
            response
        );
        assert_collector_is_closed(&mut collector, &cx);
    }

    #[test]
    fn modern_http_sse_collector_finish_admission_preserves_raw_terminal_result() {
        let request_id = RequestId::Number(4_210);
        // The typed `Value` normalizes the exponent, but final-result decoding
        // needs the source form exactly as received on the SSE wire.
        let raw_result =
            r#"{"resultType":"complete","opaque":{"decimal":1.20e+4,"order":{"z":1,"a":2}}}"#;
        let body = format!("data: {{\"jsonrpc\":\"2.0\",\"id\":4210,\"result\":{raw_result}}}\n\n");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");
        let cx = Cx::for_testing();

        collector
            .push(&cx, body.as_bytes(), |_| Ok(()))
            .expect("correlated terminal response is admitted");
        let admission = collector
            .finish_admission(&cx)
            .expect("EOF returns the admitted terminal response");

        assert_eq!(admission.response().id, Some(request_id));
        assert_eq!(admission.raw_result(), Some(raw_result));
    }

    #[test]
    fn modern_http_sse_collector_correlates_mathematical_terminal_ids_and_preserves_source() {
        let request_id = RequestId::Number(2);
        let raw_result =
            r#"{"resultType":"complete","opaque":{"decimal":1.20e+4,"order":{"z":1,"a":2}}}"#;
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let cx = Cx::for_testing();

        for terminal_id in ["2.0", "2e0"] {
            let body = format!(
                "data: {{\"jsonrpc\":\"2.0\",\"id\":{terminal_id},\"result\":{raw_result}}}\n\n"
            );
            let mut collector =
                ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");

            collector
                .push(&cx, body.as_bytes(), |_| Ok(()))
                .expect("mathematically correlated terminal response is admitted");
            let admission = collector
                .finish_admission(&cx)
                .expect("EOF returns the admitted terminal response");

            assert!(
                admission
                    .response()
                    .id
                    .as_ref()
                    .is_some_and(|actual| actual.correlates_with(&request_id))
            );
            assert_eq!(admission.raw_result(), Some(raw_result));
        }
    }

    #[test]
    fn modern_http_sse_collector_mismatched_mathematical_terminal_id_closes_without_reuse() {
        let request_id = RequestId::Number(2);
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let cx = Cx::for_testing();
        let raw_result =
            r#"{"resultType":"complete","opaque":{"decimal":1.20e+4,"order":{"z":1,"a":2}}}"#;
        // This is byte-for-byte the correlated `2.0` terminal payload from
        // the positive proof above except for its one forbidden dimension.
        let mismatched =
            format!("data: {{\"jsonrpc\":\"2.0\",\"id\":3.0,\"result\":{raw_result}}}\n\n");
        let mut collector =
            ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");

        assert!(matches!(
            collector.push(&cx, mismatched.as_bytes(), |_| Ok(())),
            Err(ModernHttpSseCollectorError::TerminalResponseIdMismatch {
                expected,
                actual: Some(RequestId::Integer(actual)),
            }) if expected == request_id && actual == "3.0"
        ));
        assert_collector_is_closed(&mut collector, &cx);

        let body = format!("data: {{\"jsonrpc\":\"2.0\",\"id\":2e0,\"result\":{raw_result}}}\n\n");
        let mut fresh_collector = ModernHttpSseCollector::new(request_id, limits)
            .expect("a separate collector is unaffected by the closed one");
        fresh_collector
            .push(&cx, body.as_bytes(), |_| Ok(()))
            .expect("fresh collector admits its own mathematically correlated terminal");
        assert_eq!(
            fresh_collector
                .finish_admission(&cx)
                .expect("fresh collector returns only its own terminal")
                .raw_result(),
            Some(raw_result)
        );
    }

    #[test]
    fn modern_http_sse_collector_absent_terminal_id_closes_without_reuse() {
        let request_id = RequestId::Number(2);
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let cx = Cx::for_testing();
        let raw_result =
            r#"{"resultType":"complete","opaque":{"decimal":1.20e+4,"order":{"z":1,"a":2}}}"#;
        // JSON-RPC success requires an ID, so use the one valid response form
        // whose ID may be absent: an uncorrelated error. This reaches the
        // collector's correlation gate instead of failing typed decoding.
        let absent = "data: {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}\n\n";
        let mut collector =
            ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");

        assert!(matches!(
            collector.push(&cx, absent.as_bytes(), |_| Ok(())),
            Err(ModernHttpSseCollectorError::TerminalResponseIdMismatch {
                expected,
                actual: None,
            }) if expected == request_id
        ));
        assert_collector_is_closed(&mut collector, &cx);

        let body = format!("data: {{\"jsonrpc\":\"2.0\",\"id\":2e0,\"result\":{raw_result}}}\n\n");
        let mut fresh_collector = ModernHttpSseCollector::new(request_id, limits)
            .expect("a separate collector is unaffected by the closed one");
        fresh_collector
            .push(&cx, body.as_bytes(), |_| Ok(()))
            .expect("fresh collector admits its own mathematically correlated terminal");
        assert_eq!(
            fresh_collector
                .finish_admission(&cx)
                .expect("fresh collector returns only its own terminal")
                .raw_result(),
            Some(raw_result)
        );
    }

    #[test]
    fn modern_http_sse_collector_finish_admission_does_not_fabricate_raw_result_for_error() {
        let request_id = RequestId::Number(4_210);
        // This differs from the preserved-result case only in the terminal
        // envelope: an error response has no `result` member to preserve.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":4210,\"error\":{\"code\":-32000,\"message\":\"failure\"}}\n\n";
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");
        let cx = Cx::for_testing();

        collector
            .push(&cx, body.as_bytes(), |_| Ok(()))
            .expect("correlated error response is admitted");
        let admission = collector
            .finish_admission(&cx)
            .expect("EOF returns the admitted error response");

        assert_eq!(admission.response().id, Some(request_id));
        assert!(admission.response().error.is_some());
        assert_eq!(admission.response().result, None);
        assert_eq!(admission.raw_result(), None);
    }

    #[test]
    fn modern_http_sse_collector_finish_cancellation_cannot_be_reused() {
        let request_id = RequestId::Number(4_209);
        let response = JsonRpcResponse::success(request_id.clone(), serde_json::json!(true));
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id, limits).expect("valid request ID");
        let fresh_cx = Cx::for_testing();
        let cancelled_cx = Cx::for_testing();

        collector
            .push(
                &fresh_cx,
                format!("data: {response_json}\n\n").as_bytes(),
                |_| Ok(()),
            )
            .expect("terminal response is retained before cancelled EOF");
        cancelled_cx.set_cancel_requested(true);
        assert!(matches!(
            collector.finish(&cancelled_cx),
            Err(ModernHttpSseCollectorError::Cancelled)
        ));
        assert_collector_is_closed(&mut collector, &fresh_cx);
    }

    #[test]
    fn modern_http_sse_collector_rejects_only_mismatched_terminal_id() {
        let request_id = RequestId::Number(4_202);
        let response = JsonRpcResponse::success(
            RequestId::Number(4_203),
            serde_json::json!({"result": "complete"}),
        );
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let body = format!("data: {response_json}\n\n");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id.clone(), limits).expect("valid request ID");
        let cx = Cx::for_testing();

        assert!(matches!(
            collector.push(&cx, body.as_bytes(), |_| Ok(())),
            Err(ModernHttpSseCollectorError::TerminalResponseIdMismatch {
                expected,
                actual: Some(RequestId::Number(4_203)),
            }) if expected == request_id
        ));
        assert_collector_is_closed(&mut collector, &cx);
    }

    #[test]
    fn modern_http_sse_collector_rejects_only_trailing_incomplete_bytes_after_terminal() {
        let request_id = RequestId::Number(4_204);
        let response = JsonRpcResponse::success(request_id.clone(), serde_json::json!(true));
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let complete_body = format!("data: {response_json}\n\n");
        let body = format!("{complete_body}data: {{");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id, limits).expect("valid request ID");
        let cx = Cx::for_testing();

        collector
            .push(&cx, body.as_bytes(), |_| Ok(()))
            .expect("the terminal is complete before the trailing partial SSE line");
        assert!(matches!(
            collector.finish(&cx),
            Err(ModernHttpSseCollectorError::EndOfStream {
                framing: ModernSseEndOfStream {
                    discarded_pending_event: false,
                    discarded_partial_line: true,
                },
            })
        ));
        assert_collector_is_closed(&mut collector, &cx);
    }

    #[test]
    fn modern_http_sse_collector_poisoned_by_codec_error_cannot_release_prior_terminal() {
        let request_id = RequestId::Number(4_205);
        let response = JsonRpcResponse::success(request_id.clone(), serde_json::json!(true));
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let body = format!("data: {response_json}\n\ndata: not-json\n\n");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id, limits).expect("valid request ID");
        let cx = Cx::for_testing();

        assert!(matches!(
            collector.push(&cx, body.as_bytes(), |_| Ok(())),
            Err(ModernHttpSseCollectorError::Codec(_))
        ));
        assert_collector_is_closed(&mut collector, &cx);
    }

    #[test]
    fn modern_http_sse_collector_poisoned_by_sse_framing_error_stays_closed() {
        let limits = ModernSseLimits::new(8, 4_096, 8).expect("nonzero SSE limits");
        let mut collector = ModernHttpSseCollector::new(RequestId::Number(4_206), limits)
            .expect("valid request ID");
        let cx = Cx::for_testing();

        assert!(matches!(
            collector.push(&cx, b"data: too-long\n", |_| Ok(())),
            Err(ModernHttpSseCollectorError::Sse(
                ModernSseParseError::LineTooLong { .. }
            ))
        ));
        assert_collector_is_closed(&mut collector, &cx);
    }

    #[test]
    fn modern_http_sse_collector_poisoned_by_notification_delivery_error_stays_closed() {
        let notification = JsonRpcRequest::notification(
            "notifications/progress",
            Some(serde_json::json!({"progress": 50})),
        );
        let notification_json = serde_json::to_string(&notification).expect("notification encodes");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector = ModernHttpSseCollector::new(RequestId::Number(4_206), limits)
            .expect("valid request ID");
        let cx = Cx::for_testing();

        assert!(matches!(
            collector.push(
                &cx,
                format!("data: {notification_json}\n\n").as_bytes(),
                |_| { Err(TransportError::Closed) }
            ),
            Err(ModernHttpSseCollectorError::NotificationDelivery(
                TransportError::Closed
            ))
        ));
        assert_collector_is_closed(&mut collector, &cx);
    }

    #[test]
    fn modern_http_sse_collector_poisoned_by_mid_chunk_cancellation_stays_closed() {
        let request_id = RequestId::Number(4_207);
        let notification = JsonRpcRequest::notification("notifications/progress", None);
        let response = JsonRpcResponse::success(request_id.clone(), serde_json::json!(true));
        let notification_json = serde_json::to_string(&notification).expect("notification encodes");
        let response_json = serde_json::to_string(&response).expect("response encodes");
        let body = format!("data: {notification_json}\n\ndata: {response_json}\n\n");
        let limits = ModernSseLimits::new(4_096, 4_096, 8).expect("nonzero SSE limits");
        let mut collector =
            ModernHttpSseCollector::new(request_id, limits).expect("valid request ID");
        let cancelled_cx = Cx::for_testing();
        let fresh_cx = Cx::for_testing();

        assert!(matches!(
            collector.push(&cancelled_cx, body.as_bytes(), |_| {
                cancelled_cx.set_cancel_requested(true);
                Ok(())
            }),
            Err(ModernHttpSseCollectorError::Cancelled)
        ));
        assert_collector_is_closed(&mut collector, &fresh_cx);
    }

    fn assert_collector_is_closed(collector: &mut ModernHttpSseCollector, cx: &Cx) {
        assert!(matches!(
            collector.push(cx, b"", |_| Ok(())),
            Err(ModernHttpSseCollectorError::Closed)
        ));
        assert!(matches!(
            collector.finish(cx),
            Err(ModernHttpSseCollectorError::Closed)
        ));
    }
}
