//! WebSocket transport for MCP.
//!
//! This module provides WebSocket-based transport for bidirectional MCP
//! communication. Unlike SSE (server-push only), WebSocket allows both
//! client and server to send messages at any time.
//!
//! # Wire Format
//!
//! MCP over WebSocket uses:
//! - Text frames for JSON-RPC messages (one message per frame)
//! - Standard JSON-RPC request/response format
//! - Optional ping/pong for keep-alive
//!
//! # Architecture
//!
//! [`AsyncWsClientTransport::connect`](crate::websocket::AsyncWsClientTransport::connect)
//! establishes a native `ws://` connection and performs the HTTP Upgrade handshake.
//! [`connect_wss`](crate::websocket::AsyncWsClientTransport::connect_wss) does
//! the same over a caller-supplied or WebPKI-rooted asupersync TLS connector.
//! [`WebSocketUpgradeAdmission`](crate::websocket::WebSocketUpgradeAdmission)
//! validates one bounded server Upgrade request, while
//! [`WebSocketListener`](crate::websocket::WebSocketListener) returns a bounded
//! parsed request for the caller's route and authentication decision before
//! it can complete that admission.
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_transport::websocket::AsyncWsClientTransport;
//!
//! let mut transport = AsyncWsClientTransport::connect(&cx, "ws://127.0.0.1:9000/mcp").await?;
//!
//! // Receive a message
//! let msg = transport.recv(&cx).await?;
//!
//! // Send a response
//! transport.send(&cx, &response).await?;
//! ```
//!
//! # Cancellation behavior
//!
//! The prior synchronous caller-provided `std::io` implementation is retained
//! only as a focused test fixture. It is not part of the public transport
//! because an arbitrary blocking reader cannot be interrupted safely.
//!
//! [`AsyncWsServerTransport`](crate::websocket::AsyncWsServerTransport) and
//! [`AsyncWsClientTransport`](crate::websocket::AsyncWsClientTransport) are the
//! cancellation-safe API for owned asupersync socket I/O. The client uses
//! asupersync's native WebSocket implementation; the server adapter owns its
//! bounded RFC 6455 framing over the upgraded byte stream. Both poll the owned
//! socket through the supplied [`Cx`], so cancellation preempts an idle read.

#[cfg(test)]
use std::io::{BufReader, Read, Write};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    task::Poll,
};
use std::{net::SocketAddr, pin::Pin};

use asupersync::{
    Cx,
    bytes::{Bytes, BytesMut},
    channel::oneshot,
    codec::{Decoder, Encoder},
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{
        TcpListener, TcpStream,
        websocket::{
            ClientHandshake, CloseCode, CloseReason, Frame, FrameCodec, HandshakeError,
            HttpRequest as NativeHttpRequest, HttpResponse as NativeHttpResponse,
            Message as NativeWsMessage, Opcode, ServerHandshake, WebSocket, WebSocketConfig,
            WebSocketRead, WebSocketWrite, WsConnectError, WsError, WsUrl,
        },
    },
    sync::{Mutex, OwnedMutexGuard},
    tls::{TlsConnector, TlsStream},
};
#[cfg(test)]
use fastmcp_core::{WebSocketMask, draw_websocket_mask};

#[cfg(test)]
use crate::{ClientTransportRecvHalf, Transport, TransportRecvHalf, TransportSendHalf};
use crate::{Codec, ReceivedTransportFrame, TransportError};
use fastmcp_protocol::JsonRpcMessage;
#[cfg(test)]
use fastmcp_protocol::{JsonRpcRequest, JsonRpcResponse};

fn websocket_checkpoint(cx: &Cx) -> Result<(), TransportError> {
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

fn native_websocket_error(cx: &Cx, error: WsError) -> TransportError {
    match error {
        WsError::Io(error)
            if error.kind() == std::io::ErrorKind::Interrupted && cx.is_cancel_requested() =>
        {
            websocket_checkpoint(cx)
                .err()
                .unwrap_or(TransportError::Cancelled)
        }
        WsError::Io(error) => TransportError::Io(error),
        error => websocket_invalid_data(error.to_string()),
    }
}

fn native_websocket_close_reason(error: &WsError) -> CloseReason {
    let code = error.as_close_code();
    if code.is_sendable() {
        CloseReason::new(code, None)
    } else {
        CloseReason::empty()
    }
}

fn websocket_connect_error(cx: &Cx, error: WsConnectError) -> TransportError {
    match error {
        WsConnectError::Io(error) => {
            if error.kind() == std::io::ErrorKind::Interrupted && cx.is_cancel_requested() {
                websocket_checkpoint(cx)
                    .err()
                    .unwrap_or(TransportError::Cancelled)
            } else {
                TransportError::Io(error)
            }
        }
        WsConnectError::Cancelled => websocket_checkpoint(cx)
            .err()
            .unwrap_or(TransportError::Cancelled),
        WsConnectError::InvalidUrl(error) | WsConnectError::Handshake(error) => {
            websocket_handshake_error(error)
        }
        WsConnectError::TlsRequired => websocket_invalid_data(
            "native ws connector received wss://; use connect_wss or connect_wss_with_connector",
        ),
        WsConnectError::Protocol(error) => websocket_invalid_data(error.to_string()),
    }
}

fn websocket_handshake_error(error: HandshakeError) -> TransportError {
    websocket_invalid_data(error.to_string())
}

fn websocket_tls_error(error: impl std::fmt::Display) -> TransportError {
    websocket_invalid_data(format!("WebSocket TLS handshake failed: {error}"))
}

fn websocket_lock_error(cx: &Cx, error: impl std::fmt::Display) -> TransportError {
    if cx.is_cancel_requested() {
        return websocket_checkpoint(cx)
            .err()
            .unwrap_or(TransportError::Cancelled);
    }
    websocket_invalid_data(format!("WebSocket writer coordination failed: {error}"))
}

/// Runs one owned establishment phase under a child of the caller's context.
///
/// `oneshot::Receiver::recv(cx)` registers the caller context's cancellation
/// waker. That makes a quiet TCP, TLS, or Upgrade wait interruptible even when
/// its I/O future has no readiness event to wake it. The phase itself runs in a
/// region-owned child context, so cancellation also wakes that task to observe
/// its checkpoint and drop its partially established socket or TLS stream.
async fn await_with_caller_cx<F, Fut, T>(cx: &Cx, phase: F) -> Result<T, TransportError>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    websocket_checkpoint(cx)?;
    let (sender, mut receiver) = oneshot::channel();
    let phase_task = cx
        .spawn(move |phase_cx| async move {
            let mut future = Box::pin(phase(phase_cx.clone()));
            let output = std::future::poll_fn(|task_cx| {
                if phase_cx.checkpoint().is_err() {
                    return Poll::Ready(None);
                }
                future.as_mut().poll(task_cx).map(Some)
            })
            .await;
            if let Some(output) = output {
                let _ = sender.send(&phase_cx, output);
            }
        })
        .map_err(|error| {
            websocket_invalid_data(format!(
                "WebSocket establishment task could not start: {error}"
            ))
        })?;

    match receiver.recv(cx).await {
        Ok(output) => Ok(output),
        Err(oneshot::RecvError::Cancelled) => {
            phase_task.abort();
            Err(websocket_checkpoint(cx)
                .err()
                .unwrap_or(TransportError::Cancelled))
        }
        Err(oneshot::RecvError::Closed) => {
            phase_task.abort();
            Err(websocket_checkpoint(cx).err().unwrap_or_else(|| {
                websocket_invalid_data("WebSocket establishment phase ended without a result")
            }))
        }
        Err(oneshot::RecvError::PolledAfterCompletion) => Err(websocket_invalid_data(
            "WebSocket establishment result was polled after completion",
        )),
    }
}

/// Maximum WebSocket frame and assembled-message size accepted by FastMCP.
pub const FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

fn websocket_invalid_data(message: impl Into<String>) -> TransportError {
    TransportError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

// A maximum-sized frame uses the 64-bit extended-length form. Client input
// carries the 4-byte mask while server output does not.
const FASTMCP_WEBSOCKET_MAX_CLIENT_FRAME_ENVELOPE_SIZE: usize = 14;
const FASTMCP_WEBSOCKET_MAX_SERVER_FRAME_ENVELOPE_SIZE: usize = 10;
const FASTMCP_WEBSOCKET_READ_CHUNK_SIZE: usize = 4096;
const FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE: usize =
    FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE + FASTMCP_WEBSOCKET_MAX_CLIENT_FRAME_ENVELOPE_SIZE;
const FASTMCP_WEBSOCKET_MAX_PENDING_WRITE_BYTES: usize =
    FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE + FASTMCP_WEBSOCKET_MAX_SERVER_FRAME_ENVELOPE_SIZE;

/// Maximum HTTP request or response header block admitted during WebSocket Upgrade.
///
/// This is intentionally separate from the JSON-RPC and RFC 6455 frame bounds:
/// an attacker must not consume a full message allocation before it has passed
/// the HTTP Upgrade gate.
pub const FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE: usize = 16 * 1024;

fn fastmcp_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE)
        .max_message_size(FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE)
        .max_pending_write_bytes(
            FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE + FASTMCP_WEBSOCKET_MAX_CLIENT_FRAME_ENVELOPE_SIZE,
        )
}

fn encode_native_websocket_message(
    codec: &mut Codec,
    message: &JsonRpcMessage,
) -> Result<NativeWsMessage, TransportError> {
    let bytes = match message {
        JsonRpcMessage::Request(request) => codec.encode_request(request)?,
        JsonRpcMessage::Response(response) => codec.encode_response(response)?,
    };
    let text = String::from_utf8(bytes).map_err(|error| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid UTF-8 in message: {error}"),
        ))
    })?;

    Ok(NativeWsMessage::text(text.trim_end().to_owned()))
}

fn decode_native_websocket_message(
    message: Option<NativeWsMessage>,
) -> Result<ReceivedTransportFrame, TransportError> {
    match message {
        // `String` owns the exact RFC 6455 text payload. Converting it back
        // into bytes here does not parse, normalize, or reserialize JSON, so
        // result member order and number lexemes remain peer-authored source.
        Some(NativeWsMessage::Text(text)) => ReceivedTransportFrame::admit(text.into_bytes()),
        Some(NativeWsMessage::Binary(_)) => Err(websocket_invalid_data(
            "Binary WebSocket messages are not supported by MCP",
        )),
        Some(NativeWsMessage::Close(_)) | None => Err(TransportError::Closed),
        // asupersync consumes ping/pong control frames internally before
        // returning from recv; reaching either branch is therefore a protocol
        // boundary violation rather than an application message.
        Some(NativeWsMessage::Ping(_) | NativeWsMessage::Pong(_)) => Err(websocket_invalid_data(
            "Unexpected WebSocket control message after native handling",
        )),
    }
}

/// Cancellation-safe client-side WebSocket transport over owned asupersync I/O.
///
/// Construct it from an already-upgraded owned asupersync socket. A pending
/// [`Self::recv`] is interrupted when `cx` is cancelled, without requiring
/// peer traffic.
///
/// It intentionally does not implement the synchronous transport traits:
/// satisfying them would require blocking an async receive or creating a
/// runtime, either of which would lose the caller-owned `Cx` cancellation
/// semantics this type provides. Use [`Self::into_split`] for its native
/// asynchronous one-reader / independent-writer driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebSocketTerminalState {
    Open,
    Closing,
    Closed,
    Failed,
}

impl WebSocketTerminalState {
    const OPEN: u8 = 0;
    const CLOSING: u8 = 1;
    const CLOSED: u8 = 2;
    const FAILED: u8 = 3;

    const fn as_u8(self) -> u8 {
        match self {
            Self::Open => Self::OPEN,
            Self::Closing => Self::CLOSING,
            Self::Closed => Self::CLOSED,
            Self::Failed => Self::FAILED,
        }
    }

    const fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }

    fn load(terminal: &AtomicU8) -> Self {
        match terminal.load(Ordering::Acquire) {
            Self::OPEN => Self::Open,
            Self::CLOSING => Self::Closing,
            Self::CLOSED => Self::Closed,
            Self::FAILED => Self::Failed,
            _ => Self::Failed,
        }
    }
}

fn terminal_unavailable(terminal: &AtomicU8) -> bool {
    !WebSocketTerminalState::load(terminal).is_open()
}

/// Writes an elected close frame while holding the one shared writer lock.
///
/// Normal close election occurs only after the lock is acquired. Therefore a
/// caller cancelled while *waiting* for a competing write leaves the shared
/// state open and may safely retry. Once the frame write starts, either a
/// successful frame commits `Closed`, or an error commits `Failed`; neither
/// path can be misreported as an already-successful later close.
async fn close_split_connection<IO>(
    cx: &Cx,
    writer: &Arc<Mutex<WebSocketWrite<IO>>>,
    terminal: &Arc<AtomicU8>,
    reason: CloseReason,
) -> Result<(), TransportError>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    websocket_checkpoint(cx)?;
    let mut writer = OwnedMutexGuard::lock(Arc::clone(writer), cx)
        .await
        .map_err(|error| websocket_lock_error(cx, error))?;

    match WebSocketTerminalState::load(terminal) {
        WebSocketTerminalState::Closed => return Ok(()),
        WebSocketTerminalState::Failed | WebSocketTerminalState::Closing => {
            return Err(TransportError::Closed);
        }
        WebSocketTerminalState::Open => {}
    }
    terminal.store(WebSocketTerminalState::Closing.as_u8(), Ordering::Release);

    match writer.send(cx, NativeWsMessage::Close(Some(reason))).await {
        Ok(()) => {
            terminal.store(WebSocketTerminalState::Closed.as_u8(), Ordering::Release);
            Ok(())
        }
        Err(error) => {
            terminal.store(WebSocketTerminalState::Failed.as_u8(), Ordering::Release);
            Err(native_websocket_error(cx, error))
        }
    }
}

/// Makes a protocol failure terminal before attempting its structured Close.
///
/// Unlike a caller-requested close, an invalid peer frame must prevent later
/// application writes even if cancellation interrupts the best-effort reply.
async fn terminate_split_connection<IO>(
    cx: &Cx,
    writer: &Arc<Mutex<WebSocketWrite<IO>>>,
    terminal: &Arc<AtomicU8>,
    reason: CloseReason,
) where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    if terminal
        .compare_exchange(
            WebSocketTerminalState::Open.as_u8(),
            WebSocketTerminalState::Closing.as_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }

    let result = async {
        let mut writer = OwnedMutexGuard::lock(Arc::clone(writer), cx)
            .await
            .map_err(|error| websocket_lock_error(cx, error))?;
        writer
            .send(cx, NativeWsMessage::Close(Some(reason)))
            .await
            .map_err(|error| native_websocket_error(cx, error))
    }
    .await;
    terminal.store(
        if result.is_ok() {
            WebSocketTerminalState::Closed.as_u8()
        } else {
            WebSocketTerminalState::Failed.as_u8()
        },
        Ordering::Release,
    );
}

pub struct AsyncWsClientTransport<IO> {
    websocket: WebSocket<IO>,
    codec: Codec,
    terminal: WebSocketTerminalState,
}

impl<IO> AsyncWsClientTransport<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    /// Creates a cancellation-safe client transport from an upgraded owned I/O stream.
    #[must_use]
    pub fn from_upgraded(io: IO) -> Self {
        Self {
            websocket: WebSocket::from_upgraded(io, fastmcp_websocket_config()),
            codec: Codec::new(),
            terminal: WebSocketTerminalState::Open,
        }
    }

    /// Sends a JSON-RPC message through the owned native WebSocket.
    pub async fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if !self.terminal.is_open() {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;
        let message = encode_native_websocket_message(&mut self.codec, message)?;
        match self.websocket.send(cx, message).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let close_reason = native_websocket_close_reason(&error);
                let error = native_websocket_error(cx, error);
                Err(self.terminate(cx, close_reason, error).await)
            }
        }
    }

    /// Receives a JSON-RPC message, interrupting an idle owned-socket read on cancellation.
    pub async fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.recv_with_source(cx)
            .await
            .map(ReceivedTransportFrame::into_message)
    }

    /// Receives a JSON-RPC message with its exact bounded source document.
    ///
    /// This is the client ingress API for consumers that must preserve a
    /// response `result` object's peer-supplied member order and JSON-number
    /// lexemes. The WebSocket text payload is admitted directly, without a
    /// typed-message reserialization step.
    pub async fn recv_with_source(
        &mut self,
        cx: &Cx,
    ) -> Result<ReceivedTransportFrame, TransportError> {
        if !self.terminal.is_open() {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;
        let message = match self.websocket.recv(cx).await {
            Ok(message) => message,
            Err(error) => {
                let close_reason = native_websocket_close_reason(&error);
                let error = native_websocket_error(cx, error);
                return Err(self.terminate(cx, close_reason, error).await);
            }
        };
        self.decode_or_terminate(cx, message).await
    }

    /// Closes the native WebSocket and commits its terminal outcome.
    pub async fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        match self.terminal {
            WebSocketTerminalState::Closed => return Ok(()),
            WebSocketTerminalState::Closing | WebSocketTerminalState::Failed => {
                return Err(TransportError::Closed);
            }
            WebSocketTerminalState::Open => {}
        }
        websocket_checkpoint(cx)?;
        self.terminal = WebSocketTerminalState::Closing;
        match self.websocket.close(cx, CloseReason::normal()).await {
            Ok(()) => {
                self.terminal = WebSocketTerminalState::Closed;
                Ok(())
            }
            Err(error) => {
                self.terminal = WebSocketTerminalState::Failed;
                Err(native_websocket_error(cx, error))
            }
        }
    }

    async fn decode_or_terminate(
        &mut self,
        cx: &Cx,
        message: Option<NativeWsMessage>,
    ) -> Result<ReceivedTransportFrame, TransportError> {
        if matches!(message, Some(NativeWsMessage::Close(_)) | None) {
            self.terminal = WebSocketTerminalState::Closed;
            return Err(TransportError::Closed);
        }

        let close_code = match &message {
            Some(NativeWsMessage::Text(_)) => CloseCode::InvalidPayload,
            Some(NativeWsMessage::Binary(_)) => CloseCode::Unsupported,
            Some(NativeWsMessage::Ping(_) | NativeWsMessage::Pong(_)) => CloseCode::ProtocolError,
            Some(NativeWsMessage::Close(_)) | None => unreachable!("handled above"),
        };
        match decode_native_websocket_message(message) {
            Ok(message) => Ok(message),
            Err(error) => Err(self
                .terminate(cx, CloseReason::new(close_code, None), error)
                .await),
        }
    }

    async fn terminate(
        &mut self,
        cx: &Cx,
        close_reason: CloseReason,
        error: TransportError,
    ) -> TransportError {
        self.terminal = WebSocketTerminalState::Failed;
        let _ = self.websocket.close(cx, close_reason).await;
        error
    }

    /// Splits this established connection into one source-preserving ingress
    /// driver and one caller-context egress driver.
    ///
    /// The receive half is intentionally not `Clone`: an embedding has one
    /// explicit reader to route correlated responses, notifications, and
    /// cancellation. The send half serializes writes through asupersync's
    /// cancel-aware mutex while allowing it to progress independently of an
    /// idle receive. Both halves share terminal state; a protocol failure or
    /// structured close makes subsequent operations fail closed.
    #[must_use]
    pub fn into_split(self) -> (AsyncWsClientRecvHalf<IO>, AsyncWsClientSendHalf<IO>) {
        let (reader, writer) = self.websocket.split();
        let writer = Arc::new(Mutex::with_name("websocket-client-write", writer));
        let terminal = Arc::new(AtomicU8::new(self.terminal.as_u8()));
        (
            AsyncWsClientRecvHalf {
                reader,
                writer: Arc::clone(&writer),
                terminal: Arc::clone(&terminal),
            },
            AsyncWsClientSendHalf {
                writer,
                codec: self.codec,
                terminal,
            },
        )
    }
}

/// The sole asynchronous ingress driver for a split WebSocket client.
///
/// Keep this value with the connection's multiplexer. Its mutable receive API
/// prevents two independent tasks from consuming and misrouting peer frames.
pub struct AsyncWsClientRecvHalf<IO> {
    reader: WebSocketRead<IO>,
    writer: Arc<Mutex<WebSocketWrite<IO>>>,
    terminal: Arc<AtomicU8>,
}

impl<IO> AsyncWsClientRecvHalf<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    /// Receives one JSON-RPC message and discards its retained wire source.
    pub async fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.recv_with_source(cx)
            .await
            .map(ReceivedTransportFrame::into_message)
    }

    /// Receives one JSON-RPC message with its exact bounded JSON text payload.
    ///
    /// This is the client-facing ingress primitive for result decoders that
    /// must retain peer-authored member ordering and JSON number lexemes.
    pub async fn recv_with_source(
        &mut self,
        cx: &Cx,
    ) -> Result<ReceivedTransportFrame, TransportError> {
        if terminal_unavailable(&self.terminal) {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;
        let message = match self.reader.recv(cx).await {
            Ok(message) => message,
            Err(error) => {
                let close_reason = native_websocket_close_reason(&error);
                let error = native_websocket_error(cx, error);
                return Err(self.terminate(cx, close_reason, error).await);
            }
        };

        if matches!(message, Some(NativeWsMessage::Close(_)) | None) {
            self.terminal
                .store(WebSocketTerminalState::Closed.as_u8(), Ordering::Release);
            return Err(TransportError::Closed);
        }

        let close_code = match &message {
            Some(NativeWsMessage::Text(_)) => CloseCode::InvalidPayload,
            Some(NativeWsMessage::Binary(_)) => CloseCode::Unsupported,
            Some(NativeWsMessage::Ping(_) | NativeWsMessage::Pong(_)) => CloseCode::ProtocolError,
            Some(NativeWsMessage::Close(_)) | None => unreachable!("handled above"),
        };
        match decode_native_websocket_message(message) {
            Ok(message) => Ok(message),
            Err(error) => Err(self
                .terminate(cx, CloseReason::new(close_code, None), error)
                .await),
        }
    }

    /// Closes the shared connection through the paired write half.
    pub async fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        self.close_with_reason(cx, CloseReason::normal()).await
    }

    async fn close_with_reason(
        &mut self,
        cx: &Cx,
        reason: CloseReason,
    ) -> Result<(), TransportError> {
        close_split_connection(cx, &self.writer, &self.terminal, reason).await
    }

    async fn terminate(
        &mut self,
        cx: &Cx,
        close_reason: CloseReason,
        error: TransportError,
    ) -> TransportError {
        terminate_split_connection(cx, &self.writer, &self.terminal, close_reason).await;
        error
    }
}

/// Independent asynchronous egress for a split WebSocket client.
///
/// `send` and `close` both require the caller's `Cx`; lock waits and socket
/// writes therefore observe cancellation without coupling egress to the one
/// connection reader.
pub struct AsyncWsClientSendHalf<IO> {
    writer: Arc<Mutex<WebSocketWrite<IO>>>,
    codec: Codec,
    terminal: Arc<AtomicU8>,
}

impl<IO> AsyncWsClientSendHalf<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    /// Sends one JSON-RPC text message through the split connection.
    pub async fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if terminal_unavailable(&self.terminal) {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;
        let message = encode_native_websocket_message(&mut self.codec, message)?;
        let mut writer = OwnedMutexGuard::lock(Arc::clone(&self.writer), cx)
            .await
            .map_err(|error| websocket_lock_error(cx, error))?;
        // A peer close or the paired receive half may have become terminal
        // while this sender was parked on the writer lock. Recheck under the
        // lock so no data frame can overtake that terminal transition.
        if terminal_unavailable(&self.terminal) {
            return Err(TransportError::Closed);
        }
        let send_result = writer.send(cx, message).await;
        drop(writer);
        match send_result {
            Ok(()) => Ok(()),
            Err(error) => {
                let close_reason = native_websocket_close_reason(&error);
                let error = native_websocket_error(cx, error);
                Err(self.terminate(cx, close_reason, error).await)
            }
        }
    }

    /// Initiates a normal RFC 6455 close under the caller's context.
    pub async fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        self.close_with_reason(cx, CloseReason::normal()).await
    }

    async fn close_with_reason(
        &mut self,
        cx: &Cx,
        reason: CloseReason,
    ) -> Result<(), TransportError> {
        close_split_connection(cx, &self.writer, &self.terminal, reason).await
    }

    async fn terminate(
        &mut self,
        cx: &Cx,
        close_reason: CloseReason,
        error: TransportError,
    ) -> TransportError {
        terminate_split_connection(cx, &self.writer, &self.terminal, close_reason).await;
        error
    }
}

impl AsyncWsClientTransport<TcpStream> {
    /// Connects to a `ws://` endpoint and completes the RFC 6455 handshake.
    ///
    /// The connection, HTTP Upgrade, framing, masking, and close handshake are
    /// all owned by asupersync. `wss://` is deliberately rejected here so TLS
    /// authentication cannot be bypassed accidentally; use
    /// [`connect_wss`](AsyncWsClientTransport::connect_wss) or
    /// [`connect_wss_with_connector`](AsyncWsClientTransport::connect_wss_with_connector)
    /// for that scheme.
    pub async fn connect(cx: &Cx, url: &str) -> Result<Self, TransportError> {
        let parsed = WsUrl::parse(url).map_err(websocket_handshake_error)?;
        if parsed.tls {
            return Err(websocket_invalid_data(
                "wss:// requires AsyncWsClientTransport::connect_wss",
            ));
        }
        websocket_checkpoint(cx)?;
        let url = url.to_owned();
        let websocket = await_with_caller_cx(cx, move |phase_cx| async move {
            WebSocket::connect_with_config(&phase_cx, &url, fastmcp_websocket_config()).await
        })
        .await?
        .map_err(|error| websocket_connect_error(cx, error))?;
        Ok(Self {
            websocket,
            codec: Codec::new(),
            terminal: WebSocketTerminalState::Open,
        })
    }
}

impl AsyncWsClientTransport<TlsStream<TcpStream>> {
    /// Connects to a `wss://` endpoint using the built-in WebPKI root store.
    ///
    /// This uses asupersync's Rustls implementation with SNI and HTTP/1.1
    /// ALPN advertisement. Applications with private roots, pinning, or client
    /// certificates should use
    /// [`connect_wss_with_connector`](Self::connect_wss_with_connector) instead.
    pub async fn connect_wss(cx: &Cx, url: &str) -> Result<Self, TransportError> {
        let connector = TlsConnector::builder()
            .with_webpki_roots()
            .alpn_protocols(vec![b"http/1.1".to_vec()])
            .build()
            .map_err(websocket_tls_error)?;
        Self::connect_wss_with_connector(cx, url, &connector).await
    }

    /// Connects to a `wss://` endpoint using an explicit asupersync TLS policy.
    ///
    /// # Cancellation boundary
    ///
    /// TCP establishment, the TLS handshake, and HTTP Upgrade are each driven
    /// in a child of `cx`; a native cancel-aware result wait registers the
    /// caller's cancellation waker. Cancellation therefore interrupts a quiet
    /// phase, drops the in-flight socket or TLS stream, and returns a typed
    /// cancellation error rather than retaining a retryable half-connection.
    pub async fn connect_wss_with_connector(
        cx: &Cx,
        url: &str,
        connector: &TlsConnector,
    ) -> Result<Self, TransportError> {
        let parsed = WsUrl::parse(url).map_err(websocket_handshake_error)?;
        if !parsed.tls {
            return Err(websocket_invalid_data(
                "connect_wss_with_connector accepts only wss:// URLs",
            ));
        }
        websocket_checkpoint(cx)?;
        let address = websocket_socket_address(&parsed);
        let tcp = await_with_caller_cx(cx, move |_phase_cx| TcpStream::connect(address))
            .await?
            .map_err(TransportError::Io)?;
        tcp.set_nodelay(true).map_err(TransportError::Io)?;
        websocket_checkpoint(cx)?;
        let connector = connector.clone();
        let domain = parsed.host.clone();
        let tls = await_with_caller_cx(cx, move |_phase_cx| async move {
            connector.connect(&domain, tcp).await
        })
        .await?
        .map_err(websocket_tls_error)?;
        websocket_checkpoint(cx)?;
        client_upgrade(cx, url, tls).await
    }
}

fn websocket_socket_address(url: &WsUrl) -> String {
    if url.host.contains(':') {
        format!("[{}]:{}", url.host, url.port)
    } else {
        format!("{}:{}", url.host, url.port)
    }
}

async fn client_upgrade<IO>(
    cx: &Cx,
    url: &str,
    io: IO,
) -> Result<AsyncWsClientTransport<IO>, TransportError>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let url = url.to_owned();
    await_with_caller_cx(cx, move |phase_cx| async move {
        client_upgrade_phase(&phase_cx, &url, io).await
    })
    .await?
}

async fn client_upgrade_phase<IO>(
    cx: &Cx,
    url: &str,
    mut io: IO,
) -> Result<AsyncWsClientTransport<IO>, TransportError>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let handshake = ClientHandshake::new(url, cx.entropy()).map_err(websocket_handshake_error)?;
    write_all_with_checkpoint(cx, &mut io, &handshake.request_bytes()).await?;
    let response_bytes = read_http_headers_with_checkpoint(cx, &mut io).await?;
    let response = NativeHttpResponse::parse(&response_bytes).map_err(websocket_handshake_error)?;
    handshake
        .validate_response(&response)
        .map_err(websocket_handshake_error)?;
    websocket_checkpoint(cx)?;
    Ok(AsyncWsClientTransport::from_upgraded(io))
}

async fn write_all_with_checkpoint<IO>(
    cx: &Cx,
    io: &mut IO,
    bytes: &[u8],
) -> Result<(), TransportError>
where
    IO: AsyncWrite + Unpin,
{
    let mut written = 0;
    while written < bytes.len() {
        let count = std::future::poll_fn(|task_cx| {
            websocket_checkpoint(cx).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::Interrupted, error.to_string())
            })?;
            Pin::new(&mut *io).poll_write(task_cx, &bytes[written..])
        })
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::Interrupted && cx.is_cancel_requested() {
                websocket_checkpoint(cx)
                    .err()
                    .unwrap_or(TransportError::Cancelled)
            } else {
                TransportError::Io(error)
            }
        })?;
        if count == 0 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "WebSocket HTTP Upgrade write returned zero",
            )));
        }
        written = written.checked_add(count).ok_or_else(|| {
            websocket_invalid_data("WebSocket HTTP Upgrade write length overflow")
        })?;
    }
    std::future::poll_fn(|task_cx| {
        websocket_checkpoint(cx).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::Interrupted, error.to_string())
        })?;
        Pin::new(&mut *io).poll_flush(task_cx)
    })
    .await
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::Interrupted && cx.is_cancel_requested() {
            websocket_checkpoint(cx)
                .err()
                .unwrap_or(TransportError::Cancelled)
        } else {
            TransportError::Io(error)
        }
    })
}

async fn read_http_headers_with_checkpoint<IO>(
    cx: &Cx,
    io: &mut IO,
) -> Result<Vec<u8>, TransportError>
where
    IO: AsyncRead + Unpin,
{
    let mut headers = Vec::with_capacity(1024);
    loop {
        websocket_checkpoint(cx)?;
        if let Some(end) = websocket_http_header_end(&headers) {
            return Ok(headers[..end].to_vec());
        }
        if headers.len() >= FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE {
            return Err(websocket_invalid_data(
                "WebSocket HTTP Upgrade headers exceed 16 KiB",
            ));
        }

        // Read one byte at a time during the Upgrade boundary so bytes from
        // the first WebSocket frame are never lost between the HTTP and RFC
        // 6455 decoders. This happens once per connection, not per message.
        let mut byte = [0_u8; 1];
        let count = std::future::poll_fn(|task_cx| {
            websocket_checkpoint(cx).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::Interrupted, error.to_string())
            })?;
            let mut read_buf = ReadBuf::new(&mut byte);
            match Pin::new(&mut *io).poll_read(task_cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::Interrupted && cx.is_cancel_requested() {
                websocket_checkpoint(cx)
                    .err()
                    .unwrap_or(TransportError::Cancelled)
            } else {
                TransportError::Io(error)
            }
        })?;
        if count == 0 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF before WebSocket HTTP Upgrade headers completed",
            )));
        }
        headers.push(byte[0]);
    }
}

fn websocket_http_header_end(bytes: &[u8]) -> Option<usize> {
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4);
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2);
    match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(crlf.min(lf)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

fn parse_websocket_upgrade_request(
    request_bytes: &[u8],
) -> Result<(NativeHttpRequest, Box<[u8]>), TransportError> {
    let maximum_request_bytes = FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE
        .checked_add(FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE)
        .ok_or_else(|| websocket_invalid_data("WebSocket Upgrade request limit overflow"))?;
    if request_bytes.len() > maximum_request_bytes {
        return Err(websocket_invalid_data(
            "WebSocket Upgrade request exceeds its bounded header and pre-read limits",
        ));
    }

    // Scan only the header allotment. The old implementation searched the
    // whole caller-provided slice before checking a limit, allowing an
    // arbitrary oversized input to consume CPU before it was rejected.
    let scan_len = request_bytes
        .len()
        .min(FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE);
    let header_end = websocket_http_header_end(&request_bytes[..scan_len]).ok_or_else(|| {
        if request_bytes.len() > FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE {
            websocket_invalid_data("WebSocket HTTP Upgrade headers exceed 16 KiB")
        } else {
            websocket_invalid_data(
                "WebSocket HTTP Upgrade request is missing its header terminator",
            )
        }
    })?;
    let initial_websocket_bytes = &request_bytes[header_end..];
    if initial_websocket_bytes.len() > FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE {
        return Err(websocket_invalid_data(
            "WebSocket bytes pipelined after Upgrade exceed the bounded read buffer",
        ));
    }
    let request = NativeHttpRequest::parse(&request_bytes[..header_end])
        .map_err(websocket_handshake_error)?;
    Ok((request, initial_websocket_bytes.into()))
}

/// Validated HTTP Upgrade response plus any pre-read WebSocket bytes.
///
/// Construct this with [`Self::admit`] at the HTTP routing boundary, after a
/// route and authentication policy has selected WebSocket service. Calling
/// [`Self::complete`] commits exactly one 101 response and transfers only the
/// bounded bytes after that response into the RFC 6455 decoder.
pub struct WebSocketUpgradeAdmission {
    response: Box<[u8]>,
    initial_websocket_bytes: Box<[u8]>,
}

impl WebSocketUpgradeAdmission {
    /// Validates one bounded RFC 6455 HTTP Upgrade request.
    pub fn admit(request_bytes: &[u8]) -> Result<Self, TransportError> {
        let (request, initial_websocket_bytes) = parse_websocket_upgrade_request(request_bytes)?;
        Self::from_parsed_request(&request, initial_websocket_bytes)
    }

    fn from_parsed_request(
        request: &NativeHttpRequest,
        initial_websocket_bytes: Box<[u8]>,
    ) -> Result<Self, TransportError> {
        let response = ServerHandshake::new()
            .accept(request)
            .map_err(websocket_handshake_error)?
            .response_bytes();
        Ok(Self {
            response: response.into_boxed_slice(),
            initial_websocket_bytes,
        })
    }

    /// Writes the 101 response and returns the admitted server transport.
    pub async fn complete<IO>(
        self,
        cx: &Cx,
        mut io: IO,
    ) -> Result<AsyncWsServerTransport<IO>, TransportError>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        write_all_with_checkpoint(cx, &mut io, &self.response).await?;
        AsyncWsServerTransport::from_upgraded_with_initial_bytes(io, self.initial_websocket_bytes)
    }
}

/// A bounded parsed HTTP Upgrade request awaiting caller authorization.
///
/// A [`WebSocketListener`] produces this value before it writes a 101 response.
/// Inspect [`Self::request`] (and apply route, header, origin, or authentication
/// policy) before calling [`Self::into_admission`]. Dropping this value rejects
/// the peer without an Upgrade response; [`Self::into_stream`] is available
/// when the caller needs to write a specific HTTP rejection.
pub struct WebSocketUpgradeRequest<IO> {
    request: NativeHttpRequest,
    initial_websocket_bytes: Box<[u8]>,
    io: IO,
}

impl<IO> WebSocketUpgradeRequest<IO> {
    /// Returns the bounded, parsed HTTP Upgrade request selected by the listener.
    #[must_use]
    pub const fn request(&self) -> &NativeHttpRequest {
        &self.request
    }

    /// Returns the request path for route admission.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.request.path
    }

    /// Transfers the connection into a validated, but not yet committed, Upgrade.
    pub fn into_admission(self) -> Result<AdmittedWebSocketUpgrade<IO>, TransportError> {
        let admission = WebSocketUpgradeAdmission::from_parsed_request(
            &self.request,
            self.initial_websocket_bytes,
        )?;
        Ok(AdmittedWebSocketUpgrade {
            admission,
            io: self.io,
        })
    }

    /// Returns the owned stream so the caller can write its own rejection.
    #[must_use]
    pub fn into_stream(self) -> IO {
        self.io
    }
}

/// A caller-authorized Upgrade that can commit exactly one 101 response.
pub struct AdmittedWebSocketUpgrade<IO> {
    admission: WebSocketUpgradeAdmission,
    io: IO,
}

impl<IO> AdmittedWebSocketUpgrade<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    /// Writes the 101 response and returns the admitted server transport.
    pub async fn complete(self, cx: &Cx) -> Result<AsyncWsServerTransport<IO>, TransportError> {
        self.admission.complete(cx, self.io).await
    }
}

/// A native TCP listener that reads one bounded HTTP Upgrade per connection.
pub struct WebSocketListener {
    listener: TcpListener,
}

impl WebSocketListener {
    /// Binds a WebSocket TCP listener without creating an async runtime.
    pub async fn bind<A: std::net::ToSocketAddrs + Send + 'static>(
        address: A,
    ) -> Result<Self, TransportError> {
        TcpListener::bind(address)
            .await
            .map(|listener| Self { listener })
            .map_err(TransportError::Io)
    }

    /// Returns the bound socket address.
    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.listener.local_addr().map_err(TransportError::Io)
    }

    /// Accepts TCP and returns one bounded parsed Upgrade request.
    ///
    /// This method deliberately does not write a 101 response. Callers must
    /// inspect the request and explicitly authorize it with
    /// [`WebSocketUpgradeRequest::into_admission`] before completion.
    pub async fn accept(
        &self,
        cx: &Cx,
    ) -> Result<WebSocketUpgradeRequest<TcpStream>, TransportError> {
        websocket_checkpoint(cx)?;
        let (mut stream, _) = std::future::poll_fn(|task_cx| {
            websocket_checkpoint(cx).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::Interrupted, error.to_string())
            })?;
            self.listener.poll_accept(task_cx)
        })
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::Interrupted && cx.is_cancel_requested() {
                websocket_checkpoint(cx)
                    .err()
                    .unwrap_or(TransportError::Cancelled)
            } else {
                TransportError::Io(error)
            }
        })?;
        let request_bytes = read_http_headers_with_checkpoint(cx, &mut stream).await?;
        let (request, initial_websocket_bytes) = parse_websocket_upgrade_request(&request_bytes)?;
        Ok(WebSocketUpgradeRequest {
            request,
            initial_websocket_bytes,
            io: stream,
        })
    }
}

/// Cancellation-safe server-side WebSocket transport over owned asupersync I/O.
///
/// [`Self::accept`] completes a validated HTTP Upgrade request. The adapter
/// then owns RFC 6455 server-role framing: client input must be masked, server
/// output is unmasked, and frame, assembled-message, read-buffer, and
/// pending-write limits are all enforced by this type.
///
/// It intentionally does not implement the synchronous or split transport
/// traits for the same reason as [`AsyncWsClientTransport`].
pub struct AsyncWsServerTransport<IO> {
    io: IO,
    codec: Codec,
    frame_decoder: FrameCodec,
    frame_encoder: FrameCodec,
    read_buf: BytesMut,
    write_buf: BytesMut,
    fragment_buffer: Vec<u8>,
    fragmented_text: bool,
    closed: bool,
}

impl<IO> AsyncWsServerTransport<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    /// Validates and completes an HTTP Upgrade over an owned asupersync stream.
    ///
    /// Route and authentication policy remain outside this primitive: call it
    /// only after the enclosing HTTP boundary has selected the WebSocket
    /// endpoint. TCP servers should use [`WebSocketListener`], inspect its
    /// [`WebSocketUpgradeRequest`], and then explicitly authorize completion.
    pub async fn accept(cx: &Cx, request_bytes: &[u8], io: IO) -> Result<Self, TransportError> {
        WebSocketUpgradeAdmission::admit(request_bytes)?
            .complete(cx, io)
            .await
    }

    /// Creates a cancellation-safe server transport from an already-upgraded I/O stream.
    ///
    /// This is useful only when another trusted in-process HTTP boundary has
    /// already performed admission. New TCP servers should prefer
    /// [`Self::accept`] or [`WebSocketListener`].
    #[must_use]
    pub fn from_upgraded(io: IO) -> Self {
        // This constructor has no pre-read bytes, so the bounded initializer
        // cannot fail.
        Self::from_upgraded_with_initial_bytes(io, Box::new([]))
            .expect("empty WebSocket initial buffer is always within bounds")
    }

    fn from_upgraded_with_initial_bytes(
        io: IO,
        initial_websocket_bytes: Box<[u8]>,
    ) -> Result<Self, TransportError> {
        if initial_websocket_bytes.len() > FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE {
            return Err(websocket_invalid_data(
                "WebSocket initial read buffer exceeds its configured bound",
            ));
        }
        let mut read_buf = BytesMut::with_capacity(
            FASTMCP_WEBSOCKET_READ_CHUNK_SIZE.max(initial_websocket_bytes.len()),
        );
        read_buf.extend_from_slice(&initial_websocket_bytes);
        Ok(Self {
            io,
            codec: Codec::new(),
            frame_decoder: FrameCodec::server()
                .max_payload_size(FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE),
            frame_encoder: FrameCodec::server()
                .max_payload_size(FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE),
            // `read_more` caps its logical length before asking `BytesMut` to
            // grow it; the admission path additionally bounds pipelined bytes.
            read_buf,
            write_buf: BytesMut::new(),
            fragment_buffer: Vec::new(),
            fragmented_text: false,
            closed: false,
        })
    }

    /// Sends a JSON-RPC message in one unmasked server-role text frame.
    pub async fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;
        let frame = encode_server_websocket_message(&mut self.codec, message)?;
        if let Err(error) = self.write_frame(cx, frame).await {
            return Err(self.terminate(cx, CloseCode::InternalError, error).await);
        }
        Ok(())
    }

    /// Receives one JSON-RPC text message and intentionally discards its raw source.
    ///
    /// Call [`Self::recv_with_source`] when a peer response's exact `result`
    /// bytes are required by the consumer.
    pub async fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.recv_with_source(cx)
            .await
            .map(ReceivedTransportFrame::into_message)
    }

    /// Receives one JSON-RPC text message with its exact admitted source.
    ///
    /// This preserves response `result` member order and number lexemes across
    /// the WebSocket framing boundary while the typed [`Self::recv`] adapter
    /// remains available to consumers that do not need that source.
    pub async fn recv_with_source(
        &mut self,
        cx: &Cx,
    ) -> Result<ReceivedTransportFrame, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;

        loop {
            let frame = match self.read_frame(cx).await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.closed = true;
                    self.clear_fragments();
                    return Err(TransportError::Closed);
                }
                Err((close_code, error)) => {
                    let close_code = if cx.is_cancel_requested() {
                        CloseCode::GoingAway
                    } else {
                        close_code
                    };
                    return Err(self.terminate(cx, close_code, error).await);
                }
            };

            match frame.opcode {
                Opcode::Text => {
                    if self.fragmented_text {
                        return Err(self
                            .terminate(
                                cx,
                                CloseCode::ProtocolError,
                                websocket_invalid_data(
                                    "Received Text frame while inside fragmented message",
                                ),
                            )
                            .await);
                    }
                    if frame.fin {
                        return self.decode_frame_or_terminate(cx, &frame.payload).await;
                    }
                    if let Err(error) = self.append_fragment(&frame.payload) {
                        return Err(self.terminate(cx, CloseCode::MessageTooBig, error).await);
                    }
                    self.fragmented_text = true;
                }
                Opcode::Continuation => {
                    if !self.fragmented_text {
                        return Err(self
                            .terminate(
                                cx,
                                CloseCode::ProtocolError,
                                websocket_invalid_data(
                                    "Received Continuation frame without fragmented message",
                                ),
                            )
                            .await);
                    }
                    if let Err(error) = self.append_fragment(&frame.payload) {
                        return Err(self.terminate(cx, CloseCode::MessageTooBig, error).await);
                    }
                    if frame.fin {
                        self.fragmented_text = false;
                        let payload = std::mem::take(&mut self.fragment_buffer);
                        return self.decode_frame_or_terminate(cx, &payload).await;
                    }
                }
                Opcode::Binary => {
                    return Err(self
                        .terminate(
                            cx,
                            CloseCode::Unsupported,
                            websocket_invalid_data(
                                "Binary WebSocket messages are not supported by MCP",
                            ),
                        )
                        .await);
                }
                Opcode::Ping => {
                    if let Err(error) = self.write_frame(cx, Frame::pong(frame.payload)).await {
                        return Err(self.terminate(cx, CloseCode::InternalError, error).await);
                    }
                }
                Opcode::Pong => {}
                Opcode::Close => {
                    self.closed = true;
                    self.clear_fragments();
                    // A peer may use a code that is valid to receive but not
                    // valid to send (for example, a future registered code).
                    // Rebuild the reply through `CloseReason` so the server
                    // role never emits an invalid Close frame.
                    let response = server_close_response(frame.payload);
                    let _ = self.write_frame(cx, response).await;
                    return Err(TransportError::Closed);
                }
            }
        }
    }

    /// Sends a normal Close frame and permanently latches this transport closed.
    pub async fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.clear_fragments();
        self.write_frame(cx, Frame::close(Some(u16::from(CloseCode::Normal)), None))
            .await
    }

    async fn decode_frame_or_terminate(
        &mut self,
        cx: &Cx,
        payload: &[u8],
    ) -> Result<ReceivedTransportFrame, TransportError> {
        match ReceivedTransportFrame::admit(payload.to_vec().into_boxed_slice()) {
            Ok(frame) => Ok(frame),
            Err(error) => Err(self.terminate(cx, CloseCode::InvalidPayload, error).await),
        }
    }

    async fn terminate(
        &mut self,
        cx: &Cx,
        close_code: CloseCode,
        error: TransportError,
    ) -> TransportError {
        self.closed = true;
        self.clear_fragments();
        if close_code.is_sendable() {
            let _ = self
                .write_frame(cx, Frame::close(Some(u16::from(close_code)), None))
                .await;
        }
        error
    }

    fn clear_fragments(&mut self) {
        self.fragment_buffer.clear();
        self.fragmented_text = false;
    }

    fn append_fragment(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        let next_len = self
            .fragment_buffer
            .len()
            .checked_add(payload.len())
            .ok_or_else(|| websocket_invalid_data("WebSocket message length overflow"))?;
        if next_len > FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE {
            return Err(websocket_invalid_data(format!(
                "WebSocket message too large: {next_len} bytes"
            )));
        }
        self.fragment_buffer
            .try_reserve_exact(payload.len())
            .map_err(|error| {
                websocket_invalid_data(format!("WebSocket fragment allocation failed: {error}"))
            })?;
        self.fragment_buffer.extend_from_slice(payload);
        Ok(())
    }

    async fn read_frame(&mut self, cx: &Cx) -> Result<Option<Frame>, (CloseCode, TransportError)> {
        loop {
            websocket_checkpoint(cx).map_err(|error| (CloseCode::GoingAway, error))?;
            match self.frame_decoder.decode(&mut self.read_buf) {
                Ok(Some(frame)) => return Ok(Some(frame)),
                Ok(None) => {}
                Err(error) => {
                    let close_code = error.as_close_code();
                    return Err((close_code, websocket_invalid_data(error.to_string())));
                }
            }

            let read = match self.read_more(cx).await {
                Ok(read) => read,
                Err(error @ TransportError::Io(_)) => {
                    return Err((CloseCode::Abnormal, error));
                }
                Err(error) => return Err((CloseCode::ProtocolError, error)),
            };
            if read == 0 {
                return Ok(None);
            }
        }
    }

    async fn read_more(&mut self, cx: &Cx) -> Result<usize, TransportError> {
        let remaining = FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE
            .checked_sub(self.read_buf.len())
            .ok_or_else(|| websocket_invalid_data("WebSocket read buffer limit exceeded"))?;
        if remaining == 0 {
            return Err(websocket_invalid_data(
                "WebSocket read buffer limit exceeded",
            ));
        }

        let mut temporary = [0_u8; FASTMCP_WEBSOCKET_READ_CHUNK_SIZE];
        let limit = temporary.len().min(remaining);
        let read = std::future::poll_fn(|task_cx| {
            if let Err(error) = websocket_checkpoint(cx) {
                return Poll::Ready(Err(error));
            }
            let mut read_buf = ReadBuf::new(&mut temporary[..limit]);
            match Pin::new(&mut self.io).poll_read(task_cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
                // Native I/O can observe cancellation after our checkpoint.
                // Preserve actual I/O failures, but classify its Interrupted
                // cancellation exactly as the client transport does.
                Poll::Ready(Err(error)) => {
                    Poll::Ready(Err(native_websocket_error(cx, WsError::Io(error))))
                }
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        if read > 0 {
            self.read_buf.reserve(read);
            self.read_buf.extend_from_slice(&temporary[..read]);
        }
        Ok(read)
    }

    async fn write_frame(&mut self, cx: &Cx, frame: Frame) -> Result<(), TransportError> {
        let payload_len = frame.payload.len();
        if payload_len > FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE {
            return Err(websocket_invalid_data(format!(
                "WebSocket frame too large: {payload_len} bytes"
            )));
        }
        let envelope = if payload_len < 126 {
            2
        } else if payload_len < 65_536 {
            4
        } else {
            FASTMCP_WEBSOCKET_MAX_SERVER_FRAME_ENVELOPE_SIZE
        };
        let encoded_len = payload_len
            .checked_add(envelope)
            .ok_or_else(|| websocket_invalid_data("WebSocket frame length overflow"))?;
        if encoded_len > FASTMCP_WEBSOCKET_MAX_PENDING_WRITE_BYTES {
            return Err(websocket_invalid_data(
                "WebSocket pending-write limit exceeded",
            ));
        }

        self.flush_write_buf(cx).await?;
        // The exact checked length prevents the temporary encoding allocation
        // from exceeding the configured pending-write bound.
        let mut encoded = BytesMut::with_capacity(encoded_len);
        self.frame_encoder
            .encode(frame, &mut encoded)
            .map_err(|error| websocket_invalid_data(error.to_string()))?;
        if encoded.len() > FASTMCP_WEBSOCKET_MAX_PENDING_WRITE_BYTES {
            return Err(websocket_invalid_data(
                "WebSocket pending-write limit exceeded",
            ));
        }
        self.write_encoded(cx, &mut encoded).await
    }

    async fn write_encoded(
        &mut self,
        cx: &Cx,
        encoded: &mut BytesMut,
    ) -> Result<(), TransportError> {
        let written = std::future::poll_fn(|task_cx| {
            if let Err(error) = websocket_checkpoint(cx) {
                return Poll::Ready(Err(error));
            }
            Pin::new(&mut self.io)
                .poll_write(task_cx, encoded)
                .map(|result| result.map_err(TransportError::Io))
        })
        .await?;
        if written == 0 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "WebSocket write returned zero",
            )));
        }
        let _ = encoded.split_to(written);
        if !encoded.is_empty() {
            // Move, rather than copy, the unwritten tail into the retained
            // write buffer so a slow peer never doubles the bounded frame.
            self.write_buf = std::mem::take(encoded);
        }
        self.flush_write_buf(cx).await
    }

    async fn flush_write_buf(&mut self, cx: &Cx) -> Result<(), TransportError> {
        while !self.write_buf.is_empty() {
            let written = std::future::poll_fn(|task_cx| {
                if let Err(error) = websocket_checkpoint(cx) {
                    return Poll::Ready(Err(error));
                }
                Pin::new(&mut self.io)
                    .poll_write(task_cx, &self.write_buf)
                    .map(|result| result.map_err(TransportError::Io))
            })
            .await?;
            if written == 0 {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "WebSocket write returned zero",
                )));
            }
            let _ = self.write_buf.split_to(written);
        }
        std::future::poll_fn(|task_cx| {
            if let Err(error) = websocket_checkpoint(cx) {
                return Poll::Ready(Err(error));
            }
            Pin::new(&mut self.io)
                .poll_flush(task_cx)
                .map_err(TransportError::Io)
        })
        .await
    }
}

fn encode_server_websocket_message(
    codec: &mut Codec,
    message: &JsonRpcMessage,
) -> Result<Frame, TransportError> {
    let mut bytes = match message {
        JsonRpcMessage::Request(request) => codec.encode_request(request)?,
        JsonRpcMessage::Response(response) => codec.encode_response(response)?,
    };
    // `Codec` emits one trailing newline for its NDJSON users. RFC 6455 text
    // framing already supplies the message boundary, so remove precisely that
    // delimiter while retaining ownership of the bounded encode allocation.
    if bytes.last() == Some(&b'\n') {
        let _ = bytes.pop();
    }
    Ok(Frame::text(Bytes::from(bytes)))
}

fn server_close_response(payload: Bytes) -> Frame {
    CloseReason::parse(&payload)
        .map_or_else(|_| Frame::close(None, None), |reason| reason.to_frame())
}

/// WebSocket frame types.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsFrameType {
    /// Continuation frame (for fragmented messages).
    Continuation,
    /// Text frame containing UTF-8 data (used for JSON-RPC).
    Text,
    /// Binary frame.
    Binary,
    /// Close frame.
    Close,
    /// Ping frame (keep-alive).
    Ping,
    /// Pong frame (keep-alive response).
    Pong,
}

#[cfg(test)]
impl WsFrameType {
    /// Returns the opcode for this frame type.
    fn opcode(&self) -> u8 {
        match self {
            WsFrameType::Continuation => 0x00,
            WsFrameType::Text => 0x01,
            WsFrameType::Binary => 0x02,
            WsFrameType::Close => 0x08,
            WsFrameType::Ping => 0x09,
            WsFrameType::Pong => 0x0A,
        }
    }

    /// Parses a frame type from an opcode.
    fn from_opcode(opcode: u8) -> Option<Self> {
        match opcode {
            0x00 => Some(WsFrameType::Continuation),
            0x01 => Some(WsFrameType::Text),
            0x02 => Some(WsFrameType::Binary),
            0x08 => Some(WsFrameType::Close),
            0x09 => Some(WsFrameType::Ping),
            0x0A => Some(WsFrameType::Pong),
            _ => None,
        }
    }
}

/// A WebSocket frame.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct WsFrame {
    /// Frame type.
    pub frame_type: WsFrameType,
    /// Frame payload.
    pub payload: Vec<u8>,
    /// Whether this is the final frame in a message.
    pub fin: bool,
}

#[cfg(test)]
impl WsFrame {
    /// Creates a new text frame with the given payload.
    #[must_use]
    pub fn text(payload: impl Into<String>) -> Self {
        Self {
            frame_type: WsFrameType::Text,
            payload: payload.into().into_bytes(),
            fin: true,
        }
    }

    /// Creates a new close frame.
    #[must_use]
    pub fn close() -> Self {
        Self {
            frame_type: WsFrameType::Close,
            payload: Vec::new(),
            fin: true,
        }
    }

    /// Creates a new ping frame.
    #[must_use]
    pub fn ping(payload: Vec<u8>) -> Self {
        Self {
            frame_type: WsFrameType::Ping,
            payload,
            fin: true,
        }
    }

    /// Creates a new pong frame.
    #[must_use]
    pub fn pong(payload: Vec<u8>) -> Self {
        Self {
            frame_type: WsFrameType::Pong,
            payload,
            fin: true,
        }
    }

    /// Returns the payload as a UTF-8 string if this is a text frame.
    pub fn as_text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.payload)
    }
}

#[cfg(test)]
fn validate_close_payload(payload: &[u8]) -> Result<(), TransportError> {
    if payload.len() == 1 {
        return Err(websocket_invalid_data(
            "WebSocket close payload cannot contain a one-byte status code",
        ));
    }
    if payload.len() < 2 {
        return Ok(());
    }

    let status = u16::from_be_bytes([payload[0], payload[1]]);
    let status_is_allowed = matches!(status, 1000..=1003 | 1007..=1014 | 3000..=4999);
    if !status_is_allowed {
        return Err(websocket_invalid_data(format!(
            "Invalid WebSocket close status code: {status}"
        )));
    }
    std::str::from_utf8(&payload[2..])
        .map_err(|_| websocket_invalid_data("WebSocket close reason must be valid UTF-8"))?;
    Ok(())
}

#[cfg(test)]
fn validate_outbound_frame(frame: &WsFrame) -> Result<(), TransportError> {
    if matches!(
        frame.frame_type,
        WsFrameType::Close | WsFrameType::Ping | WsFrameType::Pong
    ) {
        if !frame.fin {
            return Err(websocket_invalid_data(
                "Fragmented WebSocket control frames are not allowed",
            ));
        }
        if frame.payload.len() > 125 {
            return Err(websocket_invalid_data(
                "WebSocket control frame payload exceeds 125 bytes",
            ));
        }
    }
    if frame.frame_type == WsFrameType::Close {
        validate_close_payload(&frame.payload)?;
    }
    // A complete text message must be valid UTF-8 even when callers bypass
    // WsFrame::text and populate the public fields directly. Fragmented text
    // is validated by the stateful transport once its final continuation is
    // assembled; a standalone frame writer cannot validate it in isolation.
    if frame.frame_type == WsFrameType::Text && frame.fin {
        std::str::from_utf8(&frame.payload)
            .map_err(|_| websocket_invalid_data("WebSocket text payload must be valid UTF-8"))?;
    }
    Ok(())
}

#[cfg(test)]
fn invalid_utf8_close_frame() -> WsFrame {
    WsFrame {
        frame_type: WsFrameType::Close,
        payload: 1007_u16.to_be_bytes().to_vec(),
        fin: true,
    }
}

/// Local endpoint role used to enforce RFC 6455 mask direction.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointRole {
    /// A server endpoint receives masked frames from a client.
    Server,
    /// A client endpoint receives unmasked frames from a server.
    Client,
}

#[cfg(test)]
impl EndpointRole {
    fn validate_peer_mask(self, masked: bool) -> Result<(), TransportError> {
        let violation = match (self, masked) {
            (Self::Server, false) => {
                Some("Client-to-server WebSocket frames MUST be masked per RFC 6455")
            }
            (Self::Client, true) => {
                Some("Server-to-client WebSocket frames MUST NOT be masked per RFC 6455")
            }
            _ => None,
        };

        if let Some(message) = violation {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            )));
        }

        Ok(())
    }
}

/// WebSocket frame reader.
///
/// Reads WebSocket frames from an underlying byte stream.
/// Handles frame parsing according to RFC 6455.
#[cfg(test)]
pub struct WsReader<R> {
    reader: BufReader<R>,
    max_frame_size: usize,
    endpoint_role: EndpointRole,
}

#[cfg(test)]
impl<R: Read> WsReader<R> {
    /// Creates a new WebSocket reader for server-side use.
    ///
    /// Per RFC 6455, servers MUST reject unmasked frames from clients.
    pub fn new(reader: R) -> Self {
        Self::with_role(reader, EndpointRole::Server)
    }

    /// Creates a new WebSocket reader for client-side use.
    ///
    /// Per RFC 6455, clients MUST reject masked frames from servers.
    pub fn new_client(reader: R) -> Self {
        Self::with_role(reader, EndpointRole::Client)
    }

    /// Creates a new WebSocket reader for an explicit endpoint role.
    fn with_role(reader: R, endpoint_role: EndpointRole) -> Self {
        Self {
            reader: BufReader::new(reader),
            max_frame_size: 10 * 1024 * 1024,
            endpoint_role,
        }
    }

    /// Reads the next WebSocket frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame is malformed or I/O fails.
    pub fn read_frame(&mut self) -> Result<WsFrame, TransportError> {
        // Read first two bytes (header)
        let mut header = [0u8; 2];
        self.reader.read_exact(&mut header)?;

        let fin = (header[0] & 0x80) != 0;
        let rsv = header[0] & 0x70;
        let opcode = header[0] & 0x0F;
        let masked = (header[1] & 0x80) != 0;
        let payload_len_code = header[1] & 0x7F;
        let mut payload_len = u64::from(payload_len_code);

        if rsv != 0 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WebSocket RSV bits set but no extensions are supported",
            )));
        }

        // RFC 6455 Section 5.1 makes mask direction endpoint-specific for every
        // frame, including control and continuation frames.
        self.endpoint_role.validate_peer_mask(masked)?;

        // The opcode is fully known from the first header byte. Rejecting it
        // before extended lengths and payload allocation prevents malformed
        // reserved frames from forcing a maximum-sized read first.
        let frame_type = WsFrameType::from_opcode(opcode)
            .ok_or_else(|| websocket_invalid_data(format!("Unknown WebSocket opcode: {opcode}")))?;

        // Extended payload length
        if payload_len_code == 126 {
            let mut ext = [0u8; 2];
            self.reader.read_exact(&mut ext)?;
            payload_len = u16::from_be_bytes(ext) as u64;
            if payload_len < 126 {
                return Err(websocket_invalid_data(
                    "Non-minimal WebSocket payload-length encoding",
                ));
            }
        } else if payload_len_code == 127 {
            let mut ext = [0u8; 8];
            self.reader.read_exact(&mut ext)?;
            if ext[0] & 0x80 != 0 {
                return Err(websocket_invalid_data(
                    "WebSocket payload length exceeds the RFC 6455 63-bit range",
                ));
            }
            payload_len = u64::from_be_bytes(ext);
            if u16::try_from(payload_len).is_ok() {
                return Err(websocket_invalid_data(
                    "Non-minimal WebSocket payload-length encoding",
                ));
            }
        }

        let is_control = matches!(opcode, 0x08..=0x0A);
        if is_control && !fin {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Fragmented control frames are not allowed",
            )));
        }
        if is_control && payload_len > 125 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Control frame payload too large",
            )));
        }

        let max_frame_size = self.max_frame_size as u64;
        if payload_len > max_frame_size {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WebSocket frame too large: {payload_len} bytes"),
            )));
        }
        if payload_len > usize::MAX as u64 {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WebSocket frame length exceeds platform limits",
            )));
        }

        // Read masking key if present (client -> server frames are masked)
        let mask_key = if masked {
            let mut key = [0u8; 4];
            self.reader.read_exact(&mut key)?;
            Some(key)
        } else {
            None
        };

        // Read payload
        let mut payload = vec![0u8; payload_len as usize];
        self.reader.read_exact(&mut payload)?;

        // Unmask if necessary
        if let Some(key) = mask_key {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[i % 4];
            }
        }

        if frame_type == WsFrameType::Close {
            validate_close_payload(&payload)?;
        }

        Ok(WsFrame {
            frame_type,
            payload,
            fin,
        })
    }
}

/// WebSocket frame writer.
///
/// Writes WebSocket frames to an underlying byte stream.
/// Server frames are unmasked per RFC 6455.
#[cfg(test)]
pub struct WsWriter<W> {
    writer: W,
}

#[cfg(test)]
impl<W: Write> WsWriter<W> {
    /// Creates a new WebSocket writer.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Writes a WebSocket frame.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O fails.
    pub fn write_frame(&mut self, frame: &WsFrame) -> Result<(), TransportError> {
        validate_outbound_frame(frame)?;

        // First byte: FIN + opcode
        let byte1 = if frame.fin { 0x80 } else { 0x00 } | frame.frame_type.opcode();

        // Second byte: mask bit (0 for server) + payload length
        let payload_len = frame.payload.len();

        if payload_len < 126 {
            self.writer.write_all(&[byte1, payload_len as u8])?;
        } else if payload_len < 65536 {
            self.writer.write_all(&[byte1, 126])?;
            self.writer.write_all(&(payload_len as u16).to_be_bytes())?;
        } else {
            self.writer.write_all(&[byte1, 127])?;
            self.writer.write_all(&(payload_len as u64).to_be_bytes())?;
        }

        // Write payload (unmasked for server -> client)
        self.writer.write_all(&frame.payload)?;
        self.writer.flush()?;

        Ok(())
    }
}

/// WebSocket transport for MCP.
///
/// Provides bidirectional message passing over WebSocket.
/// Messages are JSON-RPC encoded as text frames.
///
/// # Example
///
/// ```ignore
/// let transport = WsTransport::new(tcp_read, tcp_write);
///
/// // Receive a message
/// match transport.recv(&cx)? {
///     JsonRpcMessage::Request(req) => {
///         // Handle request and send response
///         let response = handle_request(req);
///         transport.send(&cx, &JsonRpcMessage::Response(response))?;
///     }
///     _ => {}
/// }
/// ```
#[cfg(test)]
pub struct WsTransport<R, W> {
    reader: WsReader<R>,
    writer: WsWriter<W>,
    codec: Codec,
    fragment_buffer: Vec<u8>,
    fragmented_text: bool,
    max_message_size: usize,
    closed: bool,
}

#[cfg(test)]
impl<R: Read, W: Write> WsTransport<R, W> {
    /// Creates a new WebSocket transport.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: WsReader::new(reader),
            writer: WsWriter::new(writer),
            codec: Codec::new(),
            fragment_buffer: Vec::new(),
            fragmented_text: false,
            max_message_size: 10 * 1024 * 1024,
            closed: false,
        }
    }

    /// Separates server-side WebSocket ingress from egress.
    ///
    /// The returned halves retain the server wire direction: incoming frames
    /// must be masked and outgoing frames are unmasked. The receive half holds
    /// a shared control-frame writer solely for RFC 6455 ping/pong and close
    /// handshakes; application messages remain exclusively owned by the send
    /// half.
    #[must_use]
    pub fn into_split(self) -> (WsServerRecvHalf<R, W>, WsServerSendHalf<W>) {
        let Self {
            reader,
            writer,
            codec,
            fragment_buffer,
            fragmented_text,
            max_message_size,
            closed,
        } = self;
        let writer = SharedWsWriter::new(writer, closed);
        let mut send_codec = Codec::new();
        send_codec.set_max_message_size(codec.max_message_size());

        (
            WsServerRecvHalf {
                reader,
                writer: writer.clone(),
                codec,
                fragment_buffer,
                fragmented_text,
                max_message_size,
                closed,
            },
            WsServerSendHalf {
                writer,
                codec: send_codec,
            },
        )
    }

    /// Sends a JSON-RPC message over the WebSocket.
    ///
    /// # Cancel-Safety
    ///
    /// Checks for cancellation before sending.
    ///
    /// # Errors
    ///
    /// Returns an error if cancelled, the connection is closed, or I/O fails.
    pub fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        // Check cancellation.
        websocket_checkpoint(cx)?;

        // Encode message
        let bytes = match message {
            JsonRpcMessage::Request(req) => self.codec.encode_request(req)?,
            JsonRpcMessage::Response(resp) => self.codec.encode_response(resp)?,
        };

        // Convert to string (strip trailing newline from NDJSON format)
        let text = String::from_utf8(bytes).map_err(|e| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid UTF-8 in message: {e}"),
            ))
        })?;
        let text = text.trim_end();

        // Send as text frame
        let frame = WsFrame::text(text);
        if let Err(error) = self.writer.write_frame(&frame) {
            self.closed = true;
            self.fragment_buffer.clear();
            self.fragmented_text = false;
            return Err(error);
        }

        Ok(())
    }

    /// Receives the next JSON-RPC message from the WebSocket.
    ///
    /// Handles control frames (ping/pong) automatically.
    /// Handles message fragmentation (Continuation frames).
    ///
    /// # Cancel-Safety
    ///
    /// Checks for cancellation before blocking.
    ///
    /// # Errors
    ///
    /// Returns an error if cancelled, the connection is closed, or parsing fails.
    pub fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        loop {
            if self.closed {
                return Err(TransportError::Closed);
            }
            // Check cancellation.
            websocket_checkpoint(cx)?;

            // Read next frame
            let frame = match self.reader.read_frame() {
                Ok(frame) => frame,
                Err(error) => {
                    self.closed = true;
                    self.fragment_buffer.clear();
                    self.fragmented_text = false;
                    return Err(error);
                }
            };

            match frame.frame_type {
                WsFrameType::Text => {
                    if self.fragmented_text {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        self.closed = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Received Text frame while inside fragmented message",
                        )));
                    }

                    if frame.fin {
                        // Complete message in single frame
                        return self.decode_message(frame.payload);
                    }

                    // Start of fragmented message
                    self.fragmented_text = true;
                    let next_len = self
                        .fragment_buffer
                        .len()
                        .saturating_add(frame.payload.len());
                    if next_len > self.max_message_size {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        self.closed = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Fragmented message exceeds size limit",
                        )));
                    }
                    self.fragment_buffer.extend(frame.payload);
                    continue;
                }
                WsFrameType::Continuation => {
                    if !self.fragmented_text {
                        self.closed = true;
                        self.fragment_buffer.clear();
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Received Continuation frame without start frame",
                        )));
                    }

                    let next_len = self
                        .fragment_buffer
                        .len()
                        .saturating_add(frame.payload.len());
                    if next_len > self.max_message_size {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        self.closed = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Fragmented message exceeds size limit",
                        )));
                    }
                    self.fragment_buffer.extend(frame.payload);

                    if frame.fin {
                        // End of fragmented message
                        let payload = std::mem::take(&mut self.fragment_buffer);
                        self.fragmented_text = false;
                        return self.decode_message(payload);
                    }

                    // More fragments to come
                    continue;
                }
                WsFrameType::Binary => {
                    if self.fragmented_text {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                    }
                    self.closed = true;
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Binary WebSocket messages are not supported by MCP",
                    )));
                }
                WsFrameType::Close => {
                    self.closed = true;
                    self.fragment_buffer.clear();
                    self.fragmented_text = false;
                    let close_reply = WsFrame {
                        frame_type: WsFrameType::Close,
                        payload: frame.payload,
                        fin: true,
                    };
                    self.writer.write_frame(&close_reply)?;
                    return Err(TransportError::Closed);
                }
                WsFrameType::Ping => {
                    // Auto-respond with pong
                    let pong = WsFrame::pong(frame.payload);
                    if let Err(error) = self.writer.write_frame(&pong) {
                        self.closed = true;
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        return Err(error);
                    }
                    continue;
                }
                WsFrameType::Pong => {
                    // Ignore pong frames
                    continue;
                }
            }
        }
    }

    /// Decodes a payload into a JSON-RPC message.
    fn decode_message(&mut self, payload: Vec<u8>) -> Result<JsonRpcMessage, TransportError> {
        if let Err(error) = std::str::from_utf8(&payload) {
            self.closed = true;
            self.fragment_buffer.clear();
            self.fragmented_text = false;
            // RFC 6455 assigns 1007 to inconsistent text data. The close write
            // is best-effort: the primary error still explains why the peer
            // connection became terminal.
            let _close_result = self.writer.write_frame(&invalid_utf8_close_frame());
            return Err(websocket_invalid_data(format!(
                "Invalid UTF-8 in WebSocket text message: {error}"
            )));
        }
        Ok(self.codec.decode_complete_message(&payload)?)
    }

    /// Sends a close frame and shuts down the connection.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O fails.
    pub fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.fragment_buffer.clear();
        self.fragmented_text = false;
        let frame = WsFrame::close();
        self.writer.write_frame(&frame)?;
        Ok(())
    }

    /// Sends a request through this transport.
    ///
    /// Convenience method that wraps a request in a message.
    pub fn send_request(
        &mut self,
        cx: &Cx,
        request: &JsonRpcRequest,
    ) -> Result<(), TransportError> {
        self.send(cx, &JsonRpcMessage::Request(request.clone()))
    }

    /// Sends a response through this transport.
    ///
    /// Convenience method that wraps a response in a message.
    pub fn send_response(
        &mut self,
        cx: &Cx,
        response: &JsonRpcResponse,
    ) -> Result<(), TransportError> {
        self.send(cx, &JsonRpcMessage::Response(response.clone()))
    }

    /// Sends a ping frame.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O fails.
    pub fn ping(&mut self) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let frame = WsFrame::ping(Vec::new());
        if let Err(error) = self.writer.write_frame(&frame) {
            self.closed = true;
            self.fragment_buffer.clear();
            self.fragmented_text = false;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
impl<R: Read, W: Write> Transport for WsTransport<R, W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        WsTransport::send(self, cx, message)
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        WsTransport::recv(self, cx)
    }

    fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        WsTransport::close(self, cx)
    }
}

/// Client-side WebSocket mask generation.
///
/// Clients must mask frames per RFC 6455. This struct provides
/// frame writing with proper masking using cryptographically secure
/// random mask keys.
#[cfg(test)]
pub struct WsClientWriter<W> {
    writer: W,
}

#[cfg(test)]
fn map_mask_draw_error<E>(error: E) -> TransportError
where
    E: std::error::Error + Send + Sync + 'static,
{
    TransportError::Io(std::io::Error::other(error))
}

#[cfg(test)]
impl<W: Write> WsClientWriter<W> {
    /// Creates a new client WebSocket writer.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Generates a cryptographically secure mask key.
    ///
    /// RFC 6455 Section 5.3: The masking key MUST be unpredictable.
    fn generate_mask() -> Result<[u8; 4], TransportError> {
        draw_websocket_mask()
            .map(WebSocketMask::into_bytes)
            .map_err(map_mask_draw_error)
    }

    /// Writes a WebSocket frame with client masking.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O fails.
    pub fn write_frame(&mut self, frame: &WsFrame) -> Result<(), TransportError> {
        self.write_frame_with_mask_draw(frame, Self::generate_mask)
    }

    fn write_frame_with_mask_draw<F>(
        &mut self,
        frame: &WsFrame,
        draw_mask: F,
    ) -> Result<(), TransportError>
    where
        F: FnOnce() -> Result<[u8; 4], TransportError>,
    {
        validate_outbound_frame(frame)?;

        // Draw before emitting any header bytes. RNG failure is terminal and
        // cannot leave a partial frame on the stream.
        let mask = draw_mask()?;

        // First byte: FIN + opcode
        let byte1 = if frame.fin { 0x80 } else { 0x00 } | frame.frame_type.opcode();

        // Second byte: mask bit (1 for client) + payload length
        let payload_len = frame.payload.len();
        let mask_bit = 0x80u8;

        if payload_len < 126 {
            self.writer
                .write_all(&[byte1, mask_bit | payload_len as u8])?;
        } else if payload_len < 65536 {
            self.writer.write_all(&[byte1, mask_bit | 126])?;
            self.writer.write_all(&(payload_len as u16).to_be_bytes())?;
        } else {
            self.writer.write_all(&[byte1, mask_bit | 127])?;
            self.writer.write_all(&(payload_len as u64).to_be_bytes())?;
        }

        // Write mask key (cryptographically random per RFC 6455)
        self.writer.write_all(&mask)?;

        // Write masked payload
        let masked: Vec<u8> = frame
            .payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        self.writer.write_all(&masked)?;
        self.writer.flush()?;

        Ok(())
    }
}

/// Client-side WebSocket transport.
///
/// Similar to `WsTransport` but masks outgoing frames as required
/// for client-to-server communication per RFC 6455.
#[cfg(test)]
pub struct WsClientTransport<R, W> {
    reader: WsReader<R>,
    writer: WsClientWriter<W>,
    codec: Codec,
    fragment_buffer: Vec<u8>,
    fragmented_text: bool,
    max_message_size: usize,
    closed: bool,
}

#[cfg(test)]
impl<R: Read, W: Write> WsClientTransport<R, W> {
    /// Creates a new client WebSocket transport.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            // Client requires unmasked frames from the server.
            reader: WsReader::new_client(reader),
            writer: WsClientWriter::new(writer),
            codec: Codec::new(),
            fragment_buffer: Vec::new(),
            fragmented_text: false,
            max_message_size: 10 * 1024 * 1024,
            closed: false,
        }
    }

    /// Separates client-side WebSocket ingress from egress.
    ///
    /// The returned halves retain the client wire direction: incoming frames
    /// must be unmasked and outgoing frames are masked. The receive half holds
    /// a shared control-frame writer solely for RFC 6455 ping/pong and close
    /// handshakes; application messages remain exclusively owned by the send
    /// half.
    #[must_use]
    pub fn into_split(self) -> (WsClientRecvHalf<R, W>, WsClientSendHalf<W>) {
        let Self {
            reader,
            writer,
            codec,
            fragment_buffer,
            fragmented_text,
            max_message_size,
            closed,
        } = self;
        let writer = SharedWsWriter::new(writer, closed);
        let mut send_codec = Codec::new();
        send_codec.set_max_message_size(codec.max_message_size());

        (
            WsClientRecvHalf {
                reader,
                writer: writer.clone(),
                codec,
                fragment_buffer,
                fragmented_text,
                max_message_size,
                closed,
            },
            WsClientSendHalf {
                writer,
                codec: send_codec,
            },
        )
    }

    /// Sends a JSON-RPC message over the WebSocket.
    ///
    /// The frame will be masked as required for clients.
    ///
    /// # Cancel-Safety
    ///
    /// Checks for cancellation before sending.
    ///
    /// # Errors
    ///
    /// Returns an error if cancelled, the connection is closed, or I/O fails.
    pub fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        // Check cancellation.
        websocket_checkpoint(cx)?;

        // Encode message
        let bytes = match message {
            JsonRpcMessage::Request(req) => self.codec.encode_request(req)?,
            JsonRpcMessage::Response(resp) => self.codec.encode_response(resp)?,
        };

        // Convert to string (strip trailing newline from NDJSON format)
        let text = String::from_utf8(bytes).map_err(|e| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid UTF-8 in message: {e}"),
            ))
        })?;
        let text = text.trim_end();

        // Send as text frame (masked)
        let frame = WsFrame::text(text);
        if let Err(error) = self.writer.write_frame(&frame) {
            self.closed = true;
            self.fragment_buffer.clear();
            self.fragmented_text = false;
            return Err(error);
        }

        Ok(())
    }

    /// Receives the next JSON-RPC message from the WebSocket.
    ///
    /// Handles control frames (ping/pong) automatically.
    ///
    /// # Cancel-Safety
    ///
    /// Checks for cancellation before blocking.
    ///
    /// # Errors
    ///
    /// Returns an error if cancelled, the connection is closed, or parsing fails.
    pub fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        loop {
            if self.closed {
                return Err(TransportError::Closed);
            }
            // Check cancellation.
            websocket_checkpoint(cx)?;

            // Read next frame
            let frame = match self.reader.read_frame() {
                Ok(frame) => frame,
                Err(error) => {
                    self.closed = true;
                    self.fragment_buffer.clear();
                    self.fragmented_text = false;
                    return Err(error);
                }
            };

            match frame.frame_type {
                WsFrameType::Text => {
                    if self.fragmented_text {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        self.closed = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Received Text frame while inside fragmented message",
                        )));
                    }

                    if frame.fin {
                        // Complete message in single frame
                        return self.decode_message(frame.payload);
                    }

                    // Start of fragmented message
                    self.fragmented_text = true;
                    let next_len = self
                        .fragment_buffer
                        .len()
                        .saturating_add(frame.payload.len());
                    if next_len > self.max_message_size {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        self.closed = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Fragmented message exceeds size limit",
                        )));
                    }
                    self.fragment_buffer.extend(frame.payload);
                    continue;
                }
                WsFrameType::Continuation => {
                    if !self.fragmented_text {
                        self.closed = true;
                        self.fragment_buffer.clear();
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Received Continuation frame without start frame",
                        )));
                    }

                    let next_len = self
                        .fragment_buffer
                        .len()
                        .saturating_add(frame.payload.len());
                    if next_len > self.max_message_size {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        self.closed = true;
                        return Err(TransportError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Fragmented message exceeds size limit",
                        )));
                    }
                    self.fragment_buffer.extend(frame.payload);

                    if frame.fin {
                        // End of fragmented message
                        let payload = std::mem::take(&mut self.fragment_buffer);
                        self.fragmented_text = false;
                        return self.decode_message(payload);
                    }

                    // More fragments to come
                    continue;
                }
                WsFrameType::Binary => {
                    if self.fragmented_text {
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                    }
                    self.closed = true;
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Binary WebSocket messages are not supported by MCP",
                    )));
                }
                WsFrameType::Close => {
                    self.closed = true;
                    self.fragment_buffer.clear();
                    self.fragmented_text = false;
                    let close_reply = WsFrame {
                        frame_type: WsFrameType::Close,
                        payload: frame.payload,
                        fin: true,
                    };
                    self.writer.write_frame(&close_reply)?;
                    return Err(TransportError::Closed);
                }
                WsFrameType::Ping => {
                    // Respond with pong (masked)
                    let pong = WsFrame::pong(frame.payload);
                    if let Err(error) = self.writer.write_frame(&pong) {
                        self.closed = true;
                        self.fragment_buffer.clear();
                        self.fragmented_text = false;
                        return Err(error);
                    }
                    continue;
                }
                WsFrameType::Pong => {
                    continue;
                }
            }
        }
    }

    /// Decodes a payload into a JSON-RPC message.
    fn decode_message(&mut self, payload: Vec<u8>) -> Result<JsonRpcMessage, TransportError> {
        if let Err(error) = std::str::from_utf8(&payload) {
            self.closed = true;
            self.fragment_buffer.clear();
            self.fragmented_text = false;
            let _close_result = self.writer.write_frame(&invalid_utf8_close_frame());
            return Err(websocket_invalid_data(format!(
                "Invalid UTF-8 in WebSocket text message: {error}"
            )));
        }
        Ok(self.codec.decode_complete_message(&payload)?)
    }

    /// Sends a close frame.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O fails.
    pub fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.fragment_buffer.clear();
        self.fragmented_text = false;
        let frame = WsFrame::close();
        self.writer.write_frame(&frame)?;
        Ok(())
    }
}

#[cfg(test)]
impl<R: Read, W: Write> Transport for WsClientTransport<R, W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        WsClientTransport::send(self, cx, message)
    }

    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        WsClientTransport::recv(self, cx)
    }

    fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
        WsClientTransport::close(self, cx)
    }
}

#[cfg(test)]
trait WsFrameSink {
    fn write_ws_frame(&mut self, frame: &WsFrame) -> Result<(), TransportError>;
}

#[cfg(test)]
impl<W: Write> WsFrameSink for WsWriter<W> {
    fn write_ws_frame(&mut self, frame: &WsFrame) -> Result<(), TransportError> {
        self.write_frame(frame)
    }
}

#[cfg(test)]
impl<W: Write> WsFrameSink for WsClientWriter<W> {
    fn write_ws_frame(&mut self, frame: &WsFrame) -> Result<(), TransportError> {
        self.write_frame(frame)
    }
}

#[cfg(test)]
struct SplitWsWriter<F> {
    writer: F,
    closed: bool,
}

#[cfg(test)]
struct SharedWsWriter<F> {
    inner: Arc<std::sync::Mutex<SplitWsWriter<F>>>,
    terminal: Arc<AtomicBool>,
}

#[cfg(test)]
impl<F> Clone for SharedWsWriter<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            terminal: Arc::clone(&self.terminal),
        }
    }
}

#[cfg(test)]
impl<F> SharedWsWriter<F>
where
    F: WsFrameSink,
{
    fn new(writer: F, closed: bool) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(SplitWsWriter { writer, closed })),
            terminal: Arc::new(AtomicBool::new(closed)),
        }
    }

    fn with_writer<T>(
        &self,
        operation: impl FnOnce(&mut SplitWsWriter<F>) -> Result<T, TransportError>,
    ) -> Result<T, TransportError> {
        let mut writer = self.inner.lock().map_err(|_| {
            self.terminal.store(true, Ordering::Release);
            TransportError::Io(std::io::Error::other(
                "WebSocket split writer lock poisoned",
            ))
        })?;
        operation(&mut writer)
    }

    fn is_closed(&self) -> bool {
        self.terminal.load(Ordering::Acquire)
    }

    fn send_frame(&self, frame: &WsFrame) -> Result<(), TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }
        self.with_writer(|writer| {
            if writer.closed {
                return Err(TransportError::Closed);
            }
            if let Err(error) = writer.writer.write_ws_frame(frame) {
                writer.closed = true;
                self.terminal.store(true, Ordering::Release);
                return Err(error);
            }
            Ok(())
        })
    }

    fn close_with_frame(&self, frame: WsFrame) -> Result<(), TransportError> {
        self.terminal.store(true, Ordering::Release);
        self.with_writer(|writer| {
            if writer.closed {
                return Ok(());
            }
            writer.closed = true;
            writer.writer.write_ws_frame(&frame)
        })
    }

    fn terminate(&self) {
        self.terminal.store(true, Ordering::Release);
        if let Ok(mut writer) = self.inner.lock() {
            writer.closed = true;
        }
    }
}

#[cfg(test)]
fn send_split_message<F>(
    writer: &SharedWsWriter<F>,
    codec: &Codec,
    cx: &Cx,
    message: &JsonRpcMessage,
) -> Result<(), TransportError>
where
    F: WsFrameSink,
{
    if writer.is_closed() {
        return Err(TransportError::Closed);
    }
    websocket_checkpoint(cx)?;

    let bytes = match message {
        JsonRpcMessage::Request(request) => codec.encode_request(request)?,
        JsonRpcMessage::Response(response) => codec.encode_response(response)?,
    };
    let text = String::from_utf8(bytes)
        .map_err(|error| websocket_invalid_data(format!("Invalid UTF-8 in message: {error}")))?;
    writer.send_frame(&WsFrame::text(text.trim_end()))
}

#[cfg(test)]
fn terminate_split_receive<F>(
    writer: &SharedWsWriter<F>,
    fragment_buffer: &mut Vec<u8>,
    fragmented_text: &mut bool,
    closed: &mut bool,
) where
    F: WsFrameSink,
{
    *closed = true;
    fragment_buffer.clear();
    *fragmented_text = false;
    writer.terminate();
}

#[cfg(test)]
fn recv_split_message<R, F>(
    reader: &mut WsReader<R>,
    writer: &SharedWsWriter<F>,
    codec: &Codec,
    fragment_buffer: &mut Vec<u8>,
    fragmented_text: &mut bool,
    max_message_size: usize,
    closed: &mut bool,
    cx: &Cx,
) -> Result<JsonRpcMessage, TransportError>
where
    R: Read,
    F: WsFrameSink,
{
    recv_split_message_with_source(
        reader,
        writer,
        codec,
        fragment_buffer,
        fragmented_text,
        max_message_size,
        closed,
        cx,
    )
    .map(ReceivedTransportFrame::into_message)
}

#[cfg(test)]
fn recv_split_message_with_source<R, F>(
    reader: &mut WsReader<R>,
    writer: &SharedWsWriter<F>,
    codec: &Codec,
    fragment_buffer: &mut Vec<u8>,
    fragmented_text: &mut bool,
    max_message_size: usize,
    closed: &mut bool,
    cx: &Cx,
) -> Result<ReceivedTransportFrame, TransportError>
where
    R: Read,
    F: WsFrameSink,
{
    loop {
        if *closed || writer.is_closed() {
            return Err(TransportError::Closed);
        }
        websocket_checkpoint(cx)?;

        let frame = match reader.read_frame() {
            Ok(frame) => frame,
            Err(error) => {
                terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                return Err(error);
            }
        };

        match frame.frame_type {
            WsFrameType::Text => {
                if *fragmented_text {
                    terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                    return Err(websocket_invalid_data(
                        "Received Text frame while inside fragmented message",
                    ));
                }
                if frame.fin {
                    let source = frame.payload.into_boxed_slice();
                    if let Err(error) = std::str::from_utf8(&source) {
                        *closed = true;
                        fragment_buffer.clear();
                        *fragmented_text = false;
                        let _close_result = writer.close_with_frame(invalid_utf8_close_frame());
                        return Err(websocket_invalid_data(format!(
                            "Invalid UTF-8 in WebSocket text message: {error}"
                        )));
                    }
                    codec
                        .decode_complete_message(&source)
                        .map_err(TransportError::Codec)?;
                    return ReceivedTransportFrame::admit(source);
                }

                *fragmented_text = true;
                let next_len = fragment_buffer.len().saturating_add(frame.payload.len());
                if next_len > max_message_size {
                    terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                    return Err(websocket_invalid_data(
                        "Fragmented message exceeds size limit",
                    ));
                }
                fragment_buffer.extend(frame.payload);
            }
            WsFrameType::Continuation => {
                if !*fragmented_text {
                    terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                    return Err(websocket_invalid_data(
                        "Received Continuation frame without start frame",
                    ));
                }

                let next_len = fragment_buffer.len().saturating_add(frame.payload.len());
                if next_len > max_message_size {
                    terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                    return Err(websocket_invalid_data(
                        "Fragmented message exceeds size limit",
                    ));
                }
                fragment_buffer.extend(frame.payload);
                if frame.fin {
                    let source = std::mem::take(fragment_buffer).into_boxed_slice();
                    *fragmented_text = false;
                    if let Err(error) = std::str::from_utf8(&source) {
                        *closed = true;
                        fragment_buffer.clear();
                        *fragmented_text = false;
                        let _close_result = writer.close_with_frame(invalid_utf8_close_frame());
                        return Err(websocket_invalid_data(format!(
                            "Invalid UTF-8 in WebSocket text message: {error}"
                        )));
                    }
                    codec
                        .decode_complete_message(&source)
                        .map_err(TransportError::Codec)?;
                    return ReceivedTransportFrame::admit(source);
                }
            }
            WsFrameType::Binary => {
                terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                return Err(websocket_invalid_data(
                    "Binary WebSocket messages are not supported by MCP",
                ));
            }
            WsFrameType::Close => {
                *closed = true;
                fragment_buffer.clear();
                *fragmented_text = false;
                writer.close_with_frame(WsFrame {
                    frame_type: WsFrameType::Close,
                    payload: frame.payload,
                    fin: true,
                })?;
                return Err(TransportError::Closed);
            }
            WsFrameType::Ping => {
                if let Err(error) = writer.send_frame(&WsFrame::pong(frame.payload)) {
                    terminate_split_receive(writer, fragment_buffer, fragmented_text, closed);
                    return Err(error);
                }
            }
            WsFrameType::Pong => {}
        }
    }
}

/// Independently owned server-side WebSocket ingress.
#[cfg(test)]
pub struct WsServerRecvHalf<R, W> {
    reader: WsReader<R>,
    writer: SharedWsWriter<WsWriter<W>>,
    codec: Codec,
    fragment_buffer: Vec<u8>,
    fragmented_text: bool,
    max_message_size: usize,
    closed: bool,
}

#[cfg(test)]
impl<R: Read, W: Write> TransportRecvHalf for WsServerRecvHalf<R, W> {
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        recv_split_message(
            &mut self.reader,
            &self.writer,
            &self.codec,
            &mut self.fragment_buffer,
            &mut self.fragmented_text,
            self.max_message_size,
            &mut self.closed,
            cx,
        )
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        self.fragment_buffer.clear();
        self.fragmented_text = false;
        self.writer.close_with_frame(WsFrame::close())
    }
}

/// Independently owned server-side WebSocket egress.
#[cfg(test)]
pub struct WsServerSendHalf<W> {
    writer: SharedWsWriter<WsWriter<W>>,
    codec: Codec,
}

#[cfg(test)]
impl<W: Write + Send> TransportSendHalf for WsServerSendHalf<W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        send_split_message(&self.writer, &self.codec, cx, message)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.writer.close_with_frame(WsFrame::close())
    }
}

/// Independently owned client-side WebSocket ingress.
#[cfg(test)]
pub struct WsClientRecvHalf<R, W> {
    reader: WsReader<R>,
    writer: SharedWsWriter<WsClientWriter<W>>,
    codec: Codec,
    fragment_buffer: Vec<u8>,
    fragmented_text: bool,
    max_message_size: usize,
    closed: bool,
}

#[cfg(test)]
impl<R: Read, W: Write> TransportRecvHalf for WsClientRecvHalf<R, W> {
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        recv_split_message(
            &mut self.reader,
            &self.writer,
            &self.codec,
            &mut self.fragment_buffer,
            &mut self.fragmented_text,
            self.max_message_size,
            &mut self.closed,
            cx,
        )
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.closed = true;
        self.fragment_buffer.clear();
        self.fragmented_text = false;
        self.writer.close_with_frame(WsFrame::close())
    }
}

#[cfg(test)]
impl<R: Read + Send, W: Write + Send> ClientTransportRecvHalf for WsClientRecvHalf<R, W> {
    fn recv_with_source(&mut self, cx: &Cx) -> Result<ReceivedTransportFrame, TransportError> {
        recv_split_message_with_source(
            &mut self.reader,
            &self.writer,
            &self.codec,
            &mut self.fragment_buffer,
            &mut self.fragmented_text,
            self.max_message_size,
            &mut self.closed,
            cx,
        )
    }
}

/// Independently owned client-side WebSocket egress.
#[cfg(test)]
pub struct WsClientSendHalf<W> {
    writer: SharedWsWriter<WsClientWriter<W>>,
    codec: Codec,
}

#[cfg(test)]
impl<W: Write + Send> TransportSendHalf for WsClientSendHalf<W> {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        send_split_message(&self.writer, &self.codec, cx, message)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.writer.close_with_frame(WsFrame::close())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodecError;
    use asupersync::io::{AsyncReadExt, AsyncWriteExt, ReadBuf};
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::test_utils::run_test;
    use fastmcp_protocol::RequestId;
    use std::io::{self, Cursor, Read, Write};
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::thread;

    fn spawn_public_ws_peer(
        status_line: &'static str,
        source: Option<&'static [u8]>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback WebSocket peer");
        let address = listener
            .local_addr()
            .expect("read loopback WebSocket peer address");
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept WebSocket client");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("bound peer handshake read");
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while websocket_http_header_end(&request).is_none() {
                stream
                    .read_exact(&mut byte)
                    .expect("read WebSocket Upgrade request byte");
                request.push(byte[0]);
                assert!(
                    request.len() <= FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE,
                    "client Upgrade request must stay bounded"
                );
            }
            let request = NativeHttpRequest::parse(&request).expect("parse client Upgrade request");
            if status_line == "HTTP/1.1 101 Switching Protocols" {
                let response = ServerHandshake::new()
                    .accept(&request)
                    .expect("admit client Upgrade request")
                    .response_bytes();
                stream
                    .write_all(&response)
                    .expect("write valid WebSocket Upgrade response");
                if let Some(source) = source {
                    let mut frame = BytesMut::new();
                    FrameCodec::server()
                        .encode(Frame::text(Bytes::copy_from_slice(source)), &mut frame)
                        .expect("encode loopback WebSocket text frame");
                    stream
                        .write_all(&frame)
                        .expect("write loopback WebSocket text frame");
                }
            } else {
                stream
                    .write_all(format!("{status_line}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .expect("write forbidden Upgrade response");
            }
        });
        (format!("ws://{address}/mcp"), peer)
    }

    #[test]
    fn public_upgrade_admission_accepts_one_bounded_switching_request() {
        let request = b"GET /mcp HTTP/1.1\r\nHost: example.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let admission = WebSocketUpgradeAdmission::admit(request)
            .expect("public server Upgrade admission must accept an RFC 6455 request");
        assert!(
            admission
                .response
                .starts_with(b"HTTP/1.1 101 Switching Protocols\r\n")
        );
        assert!(admission.initial_websocket_bytes.is_empty());
    }

    #[test]
    fn public_upgrade_admission_rejects_near_identical_request_without_upgrade_token() {
        let request = b"GET /mcp HTTP/1.1\r\nHost: example.test\r\nUpgrade: websocket\r\nConnection: keep-alive\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let error = match WebSocketUpgradeAdmission::admit(request) {
            Ok(_) => panic!("only the missing Connection Upgrade token must reject the request"),
            Err(error) => error,
        };
        assert!(
            matches!(error, TransportError::Io(ref source) if source.kind() == io::ErrorKind::InvalidData),
            "forbidden non-Upgrade request must not produce a server transport: {error:?}"
        );
    }

    #[test]
    fn public_upgrade_admission_rejects_oversized_input_before_header_scan() {
        let mut request = b"GET /mcp HTTP/1.1\r\nHost: example.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n".to_vec();
        request.resize(
            FASTMCP_WEBSOCKET_MAX_HTTP_HEADER_SIZE + FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE + 1,
            b'x',
        );

        let error = match WebSocketUpgradeAdmission::admit(&request) {
            Ok(_) => panic!("a request beyond both bounded allotments must reject before parsing"),
            Err(error) => error,
        };
        assert!(
            matches!(error, TransportError::Io(ref source) if source.kind() == io::ErrorKind::InvalidData),
            "oversized Upgrade input must not expose an admission: {error:?}"
        );
    }

    #[test]
    fn public_listener_authorizes_configured_path_before_101() {
        let (address_sender, address_receiver) = std::sync::mpsc::sync_channel(1);
        let peer = thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build public WebSocket listener runtime");
            runtime.block_on(async {
                let cx = Cx::current().expect("runtime installs a listener context");
                let listener = WebSocketListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind public WebSocket listener");
                address_sender
                    .send(listener.local_addr().expect("read listener address"))
                    .expect("publish listener address");
                let pending = listener
                    .accept(&cx)
                    .await
                    .expect("read one bounded public Upgrade request");
                assert_eq!(pending.path(), "/mcp");
                let _transport = pending
                    .into_admission()
                    .expect("authorize configured path")
                    .complete(&cx)
                    .await
                    .expect("commit one authorized Upgrade");
            });
        });
        let address = address_receiver
            .recv()
            .expect("receive public listener address");
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build public WebSocket client runtime");
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs a client context");
            AsyncWsClientTransport::<TcpStream>::connect(&cx, &format!("ws://{address}/mcp"))
                .await
                .expect("authorized public listener must complete a 101");
        });
        peer.join().expect("public listener peer completes");
    }

    #[test]
    fn public_listener_rejects_near_identical_wrong_path_before_101() {
        let (address_sender, address_receiver) = std::sync::mpsc::sync_channel(1);
        let peer = thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build public WebSocket listener runtime");
            runtime.block_on(async {
                let cx = Cx::current().expect("runtime installs a listener context");
                let listener = WebSocketListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind public WebSocket listener");
                address_sender
                    .send(listener.local_addr().expect("read listener address"))
                    .expect("publish listener address");
                let pending = listener
                    .accept(&cx)
                    .await
                    .expect("read one bounded public Upgrade request");
                assert_eq!(pending.path(), "/forbidden");
                let mut stream = pending.into_stream();
                stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .expect("write caller-owned route rejection");
            });
        });
        let address = address_receiver
            .recv()
            .expect("receive public listener address");
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build public WebSocket client runtime");
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs a client context");
            let error = match AsyncWsClientTransport::<TcpStream>::connect(
                &cx,
                &format!("ws://{address}/forbidden"),
            )
            .await
            {
                Ok(_) => panic!("the wrong route must not receive a 101"),
                Err(error) => error,
            };
            assert!(
                matches!(error, TransportError::Io(ref source) if source.kind() == io::ErrorKind::InvalidData),
                "wrong-path listener rejection must fail before transport exposure: {error:?}"
            );
        });
        peer.join()
            .expect("forbidden public listener peer completes");
    }

    #[test]
    fn public_ws_client_handshake_preserves_exact_result_source() {
        const SOURCE: &[u8] = br#"{"jsonrpc":"2.0","id":71,"result":{"zeta":1.20e+4,"alpha":{"second":2,"first":1}}}"#;
        let (url, peer) = spawn_public_ws_peer("HTTP/1.1 101 Switching Protocols", Some(SOURCE));
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build public WebSocket client runtime");
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs a caller context");
            let mut client = AsyncWsClientTransport::<TcpStream>::connect(&cx, &url)
                .await
                .expect("public ws:// connection and Upgrade must succeed");
            let received = client
                .recv_with_source(&cx)
                .await
                .expect("public client must admit the server result");
            assert_eq!(received.source(), SOURCE);
            assert!(matches!(received.message(), JsonRpcMessage::Response(_)));
        });
        peer.join().expect("loopback WebSocket peer completes");
    }

    #[test]
    fn public_ws_client_split_preserves_exact_result_source_for_one_reader() {
        const SOURCE: &[u8] = br#"{"jsonrpc":"2.0","id":72,"result":{"zeta":1.20e+4,"alpha":{"second":2,"first":1}}}"#;
        let (url, peer) = spawn_public_ws_peer("HTTP/1.1 101 Switching Protocols", Some(SOURCE));
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build split WebSocket client runtime");
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs a caller context");
            let client = AsyncWsClientTransport::<TcpStream>::connect(&cx, &url)
                .await
                .expect("public ws:// split connection and Upgrade must succeed");
            let (mut receiver, _sender) = client.into_split();
            let received = receiver
                .recv_with_source(&cx)
                .await
                .expect("the single public split reader must admit the server result");
            assert_eq!(received.source(), SOURCE);
            assert!(matches!(received.message(), JsonRpcMessage::Response(_)));
        });
        peer.join()
            .expect("loopback split WebSocket peer completes");
    }

    #[test]
    fn public_ws_client_rejects_near_identical_non_switching_response() {
        let (url, peer) = spawn_public_ws_peer("HTTP/1.1 200 Switching Protocols", None);
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build public WebSocket client runtime");
        runtime.block_on(async {
            let cx = Cx::current().expect("runtime installs a caller context");
            let error = match AsyncWsClientTransport::<TcpStream>::connect(&cx, &url).await {
                Ok(_) => panic!("only the forbidden 101-to-200 status difference must reject Upgrade"),
                Err(error) => error,
            };
            assert!(
                matches!(error, TransportError::Io(ref source) if source.kind() == io::ErrorKind::InvalidData),
                "non-switching HTTP response must fail before a transport is exposed: {error:?}"
            );
        });
        peer.join()
            .expect("loopback forbidden WebSocket peer completes");
    }

    fn virtual_socket_pair() -> (
        asupersync::net::tcp::VirtualTcpStream,
        asupersync::net::tcp::VirtualTcpStream,
    ) {
        let client_addr: SocketAddr = "127.0.0.1:41001".parse().expect("client address");
        let server_addr: SocketAddr = "127.0.0.1:41002".parse().expect("server address");
        asupersync::net::tcp::VirtualTcpStream::pair(client_addr, server_addr)
    }

    struct ReadNotifyingIo {
        inner: asupersync::net::tcp::VirtualTcpStream,
        read_started: Arc<AtomicBool>,
    }

    impl ReadNotifyingIo {
        fn new(
            inner: asupersync::net::tcp::VirtualTcpStream,
            read_started: Arc<AtomicBool>,
        ) -> Self {
            Self {
                inner,
                read_started,
            }
        }
    }

    impl AsyncRead for ReadNotifyingIo {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.read_started.store(true, Ordering::Release);
            Pin::new(&mut this.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for ReadNotifyingIo {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    struct WriteCountingIo {
        writes: Arc<AtomicUsize>,
    }

    impl WriteCountingIo {
        fn new(writes: Arc<AtomicUsize>) -> Self {
            Self { writes }
        }
    }

    impl AsyncRead for WriteCountingIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for WriteCountingIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().writes.fetch_add(1, Ordering::AcqRel);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct GatedWriteIo {
        write_started: Arc<AtomicBool>,
    }

    impl GatedWriteIo {
        fn new(write_started: Arc<AtomicBool>) -> Self {
            Self { write_started }
        }
    }

    impl AsyncRead for GatedWriteIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for GatedWriteIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().write_started.store(true, Ordering::Release);
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// An injected establishment phase with no peer readiness. Its second
    /// poll records an observable mutation, so cancellation must make the
    /// production phase wrapper drop it before that mutation can occur.
    struct QuietEstablishmentPhase {
        started: Arc<AtomicBool>,
        post_cancel_progress: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    impl QuietEstablishmentPhase {
        fn new(
            started: Arc<AtomicBool>,
            post_cancel_progress: Arc<AtomicUsize>,
            dropped: Arc<AtomicBool>,
        ) -> Self {
            Self {
                started,
                post_cancel_progress,
                dropped,
            }
        }
    }

    impl Future for QuietEstablishmentPhase {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.started.swap(true, Ordering::AcqRel) {
                self.post_cancel_progress.fetch_add(1, Ordering::AcqRel);
            }
            Poll::Pending
        }
    }

    impl Drop for QuietEstablishmentPhase {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct ReadFailingIo {
        write_attempted: Arc<AtomicBool>,
        error_kind: io::ErrorKind,
        cancel_on_read: Option<Cx>,
    }

    impl ReadFailingIo {
        fn new(write_attempted: Arc<AtomicBool>) -> Self {
            Self {
                write_attempted,
                error_kind: io::ErrorKind::ConnectionReset,
                cancel_on_read: None,
            }
        }
    }

    impl AsyncRead for ReadFailingIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if let Some(cx) = &self.cancel_on_read {
                cx.set_cancel_requested(true);
            }
            Poll::Ready(Err(io::Error::new(
                self.error_kind,
                "peer reset during WebSocket receive",
            )))
        }
    }

    impl AsyncWrite for ReadFailingIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut()
                .write_attempted
                .store(true, Ordering::Release);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut()
                .write_attempted
                .store(true, Ordering::Release);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut()
                .write_attempted
                .store(true, Ordering::Release);
            Poll::Ready(Ok(()))
        }
    }

    async fn wait_for_idle_read(read_started: &AtomicBool) {
        while !read_started.load(Ordering::Acquire) {
            asupersync::runtime::yield_now().await;
        }
    }

    async fn wait_for_phase_drop(dropped: &AtomicBool) {
        while !dropped.load(Ordering::Acquire) {
            asupersync::runtime::yield_now().await;
        }
    }

    async fn assert_quiet_establishment_phase_cancellation(cx: &Cx, phase_name: &'static str) {
        let started = Arc::new(AtomicBool::new(false));
        let post_cancel_progress = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut caller = cx
            .spawn({
                let started = Arc::clone(&started);
                let post_cancel_progress = Arc::clone(&post_cancel_progress);
                let dropped = Arc::clone(&dropped);
                move |caller_cx| async move {
                    await_with_caller_cx(&caller_cx, move |_phase_cx| {
                        QuietEstablishmentPhase::new(started, post_cancel_progress, dropped)
                    })
                    .await
                }
            })
            .expect("spawn caller-owned quiet establishment phase");

        wait_for_idle_read(&started).await;
        assert!(
            !caller.is_finished(),
            "quiet {phase_name} phase must remain pending without peer progress"
        );
        assert_eq!(
            post_cancel_progress.load(Ordering::Acquire),
            0,
            "quiet {phase_name} phase must not mutate before caller cancellation"
        );

        caller.abort();
        assert!(matches!(
            caller.join(cx).await,
            Ok(Err(TransportError::Cancelled))
        ));
        wait_for_phase_drop(&dropped).await;
        assert!(
            dropped.load(Ordering::Acquire),
            "cancelled {phase_name} child phase must settle and drop"
        );
        assert_eq!(
            post_cancel_progress.load(Ordering::Acquire),
            0,
            "cancelled {phase_name} must not make a later connection or Upgrade mutation"
        );
    }

    #[test]
    fn async_websocket_native_configuration_uses_fastmcp_10_mib_limits() {
        let config = fastmcp_websocket_config();
        assert_eq!(config.max_frame_size, FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE);
        assert_eq!(config.max_message_size, FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE);
        assert_eq!(
            config.max_pending_write_bytes,
            FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE + FASTMCP_WEBSOCKET_MAX_CLIENT_FRAME_ENVELOPE_SIZE,
        );
        assert_eq!(FASTMCP_WEBSOCKET_MAX_SERVER_FRAME_ENVELOPE_SIZE, 10);
        assert_eq!(FASTMCP_WEBSOCKET_MAX_CLIENT_FRAME_ENVELOPE_SIZE, 14);
        assert_eq!(
            FASTMCP_WEBSOCKET_MAX_PENDING_WRITE_BYTES,
            FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE + 10,
        );
        assert_eq!(
            FASTMCP_WEBSOCKET_MAX_READ_BUFFER_SIZE,
            FASTMCP_WEBSOCKET_MAX_MESSAGE_SIZE + 14,
        );
    }

    #[test]
    fn xport_01_experimental_profile_requires_a_caller_upgraded_byte_stream() {
        let _: fn(
            asupersync::net::tcp::VirtualTcpStream,
        ) -> AsyncWsServerTransport<asupersync::net::tcp::VirtualTcpStream> =
            AsyncWsServerTransport::from_upgraded;
    }

    #[test]
    fn async_server_close_reply_never_echoes_an_unsendable_peer_code() {
        // RFC 6455 permits receiving unassigned codes in this range, but the
        // same code cannot be emitted in a Close reply. The response must be
        // an unmasked server frame with an empty close payload instead.
        let response = server_close_response(Bytes::copy_from_slice(&2000_u16.to_be_bytes()));
        assert_eq!(response.opcode, Opcode::Close);
        assert!(!response.masked);
        assert!(response.payload.is_empty());

        let mut codec = FrameCodec::server();
        let mut wire = BytesMut::new();
        codec
            .encode(response, &mut wire)
            .expect("the sanitized server close reply must encode");
        assert_eq!(wire.as_ref(), &[0x88, 0x00]);
    }

    #[test]
    fn test_frame_type_opcode_roundtrip() {
        for frame_type in [
            WsFrameType::Text,
            WsFrameType::Binary,
            WsFrameType::Close,
            WsFrameType::Ping,
            WsFrameType::Pong,
        ] {
            let opcode = frame_type.opcode();
            let parsed = WsFrameType::from_opcode(opcode);
            assert_eq!(parsed, Some(frame_type));
        }
    }

    #[test]
    fn test_frame_text() {
        let frame = WsFrame::text("hello");
        assert_eq!(frame.frame_type, WsFrameType::Text);
        assert_eq!(frame.as_text().unwrap(), "hello");
        assert!(frame.fin);
    }

    #[test]
    fn test_frame_close() {
        let frame = WsFrame::close();
        assert_eq!(frame.frame_type, WsFrameType::Close);
        assert_eq!(frame.payload.len(), 0);
        assert!(frame.fin);
    }

    #[test]
    fn test_frame_ping_pong() {
        let ping = WsFrame::ping(vec![1, 2, 3]);
        assert_eq!(ping.frame_type, WsFrameType::Ping);
        assert_eq!(ping.payload, vec![1, 2, 3]);

        let pong = WsFrame::pong(vec![1, 2, 3]);
        assert_eq!(pong.frame_type, WsFrameType::Pong);
        assert_eq!(pong.payload, vec![1, 2, 3]);
    }

    #[test]
    fn test_write_read_small_frame() {
        let mut buffer = Vec::new();

        // Write frame (server-side, unmasked)
        {
            let mut writer = WsWriter::new(&mut buffer);
            let frame = WsFrame::text("hello");
            writer.write_frame(&frame).unwrap();
        }

        // Read frame back (client-side, accepts unmasked)
        let mut reader = WsReader::new_client(Cursor::new(buffer));
        let frame = reader.read_frame().unwrap();

        assert_eq!(frame.frame_type, WsFrameType::Text);
        assert_eq!(frame.as_text().unwrap(), "hello");
        assert!(frame.fin);
    }

    #[test]
    fn test_write_read_medium_frame() {
        // 200 bytes - uses extended length (126)
        let payload = "x".repeat(200);
        let mut buffer = Vec::new();

        {
            let mut writer = WsWriter::new(&mut buffer);
            let frame = WsFrame::text(&payload);
            writer.write_frame(&frame).unwrap();
        }

        // Client-side reader accepts unmasked frames
        let mut reader = WsReader::new_client(Cursor::new(buffer));
        let frame = reader.read_frame().unwrap();

        assert_eq!(frame.as_text().unwrap(), payload);
    }

    #[test]
    fn test_write_read_large_frame() {
        // 70000 bytes - uses extended length (127)
        let payload = "x".repeat(70000);
        let mut buffer = Vec::new();

        {
            let mut writer = WsWriter::new(&mut buffer);
            let frame = WsFrame::text(&payload);
            writer.write_frame(&frame).unwrap();
        }

        // Client-side reader accepts unmasked frames
        let mut reader = WsReader::new_client(Cursor::new(buffer));
        let frame = reader.read_frame().unwrap();

        assert_eq!(frame.as_text().unwrap(), payload);
    }

    #[test]
    fn test_client_writer_masks_frames() {
        let mut buffer = Vec::new();

        {
            let mut writer = WsClientWriter::new(&mut buffer);
            let frame = WsFrame::text("hi");
            writer.write_frame(&frame).unwrap();
        }

        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer[0], 0x81);
        assert_ne!(buffer[1] & 0x80, 0, "Mask bit should be set for client");
        assert_eq!(buffer[6], b'h' ^ buffer[2]);
        assert_eq!(buffer[7], b'i' ^ buffer[3]);

        let mut reader = WsReader::new(Cursor::new(buffer));
        assert_eq!(reader.read_frame().unwrap().as_text().unwrap(), "hi");
    }

    #[test]
    fn client_writer_applies_injected_mask_exactly_once() {
        let mut buffer = Vec::new();
        let draw_calls = std::cell::Cell::new(0);

        {
            let mut writer = WsClientWriter::new(&mut buffer);
            writer
                .write_frame_with_mask_draw(&WsFrame::text("hi"), || {
                    draw_calls.set(draw_calls.get() + 1);
                    Ok([0x12, 0x34, 0x56, 0x78])
                })
                .unwrap();
        }

        assert_eq!(draw_calls.get(), 1);
        assert_eq!(buffer, vec![0x81, 0x82, 0x12, 0x34, 0x56, 0x78, 0x7a, 0x5d]);
    }

    #[test]
    fn client_writer_mask_failure_precedes_all_output() {
        #[derive(Debug)]
        struct ForcedMaskDrawError;

        impl std::fmt::Display for ForcedMaskDrawError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("forced WebSocket mask draw failure")
            }
        }

        impl std::error::Error for ForcedMaskDrawError {}

        let mut buffer = Vec::new();
        let draw_calls = std::cell::Cell::new(0);
        let result = {
            let mut writer = WsClientWriter::new(&mut buffer);
            writer.write_frame_with_mask_draw(&WsFrame::text("hi"), || {
                draw_calls.set(draw_calls.get() + 1);
                Err(map_mask_draw_error(ForcedMaskDrawError))
            })
        };

        let error = match result {
            Err(TransportError::Io(error)) => error,
            other => panic!("expected mask-draw I/O error, got {other:?}"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        // `io::Error::source()` reflects the wrapped error's own source chain,
        // which is empty here. `get_ref()` is the contract for recovering the
        // directly wrapped typed error.
        assert!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<ForcedMaskDrawError>())
                .is_some()
        );
        assert_eq!(draw_calls.get(), 1);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn client_writer_draws_fresh_mask_for_every_frame_including_empty() {
        let mut buffer = Vec::new();
        let draw_calls = std::cell::Cell::new(0);

        {
            let mut writer = WsClientWriter::new(&mut buffer);
            writer
                .write_frame_with_mask_draw(&WsFrame::text("x"), || {
                    draw_calls.set(draw_calls.get() + 1);
                    Ok([0x01, 0x02, 0x03, 0x04])
                })
                .unwrap();
            writer
                .write_frame_with_mask_draw(&WsFrame::close(), || {
                    draw_calls.set(draw_calls.get() + 1);
                    Ok([0x05, 0x06, 0x07, 0x08])
                })
                .unwrap();
        }

        assert_eq!(draw_calls.get(), 2);
        assert_eq!(&buffer[..6], &[0x81, 0x81, 0x01, 0x02, 0x03, 0x04]);
        assert_eq!(buffer[6], b'x' ^ 0x01);
        assert_eq!(&buffer[7..], &[0x88, 0x80, 0x05, 0x06, 0x07, 0x08]);
    }

    #[test]
    fn test_read_masked_frame() {
        // Build a masked frame manually
        let payload = b"test";
        let mask = [0x12, 0x34, 0x56, 0x78];
        let masked_payload: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();

        let mut buffer = Vec::new();
        buffer.push(0x81); // FIN + Text opcode
        buffer.push(0x80 | payload.len() as u8); // Mask bit + length
        buffer.extend_from_slice(&mask);
        buffer.extend_from_slice(&masked_payload);

        let mut reader = WsReader::new(Cursor::new(buffer));
        let frame = reader.read_frame().unwrap();

        assert_eq!(frame.as_text().unwrap(), "test");
    }

    #[test]
    fn test_reader_rejects_oversized_frame() {
        // Build a masked frame for server-side testing
        let mask = [0x12, 0x34, 0x56, 0x78];
        let payload = b"hey";
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();

        let mut buffer = Vec::new();
        buffer.push(0x81); // FIN + Text opcode
        buffer.push(0x80 | 0x03); // Mask bit + 3-byte payload
        buffer.extend_from_slice(&mask);
        buffer.extend_from_slice(&masked);

        let mut reader = WsReader::new(Cursor::new(buffer));
        reader.max_frame_size = 2;

        let err = reader.read_frame().unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn test_reader_rejects_control_frame_over_125() {
        let mut buffer = Vec::new();
        buffer.push(0x89); // FIN + Ping opcode
        buffer.push(0x80 | 126); // Mask bit + Extended length (not allowed for control frames)
        buffer.extend_from_slice(&126u16.to_be_bytes());
        buffer.extend_from_slice(&[0, 0, 0, 0]); // Mask key

        let mut reader = WsReader::new(Cursor::new(buffer));
        let err = reader.read_frame().unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn test_reader_rejects_fragmented_control_frame() {
        let mut buffer = Vec::new();
        buffer.push(0x09); // FIN=0 + Ping opcode
        buffer.push(0x80); // Mask bit + Zero payload
        buffer.extend_from_slice(&[0, 0, 0, 0]); // Mask key

        let mut reader = WsReader::new(Cursor::new(buffer));
        let err = reader.read_frame().unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn reader_rejects_reserved_opcode_before_reading_its_payload() {
        // The mask bit is direction-correct, but the reserved opcode is known
        // from these two bytes. No mask key or payload bytes are supplied.
        let mut reader = WsReader::new(Cursor::new(vec![0x83, 0x80 | 127]));

        let error = reader.read_frame().unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
                    && source.to_string().contains("Unknown WebSocket opcode")
        ));
    }

    #[test]
    fn reader_rejects_non_minimal_extended_payload_lengths() {
        let mut sixteen_bit = vec![0x81, 0x80 | 126];
        sixteen_bit.extend_from_slice(&125_u16.to_be_bytes());
        let mut reader = WsReader::new(Cursor::new(sixteen_bit));
        let error = reader.read_frame().unwrap_err();
        assert!(error.to_string().contains("Non-minimal"));

        let mut sixty_four_bit = vec![0x81, 0x80 | 127];
        sixty_four_bit.extend_from_slice(&u64::from(u16::MAX).to_be_bytes());
        let mut reader = WsReader::new(Cursor::new(sixty_four_bit));
        let error = reader.read_frame().unwrap_err();
        assert!(error.to_string().contains("Non-minimal"));
    }

    #[test]
    fn reader_rejects_invalid_close_payloads() {
        let one_byte = build_masked_frame(0x08, true, &[0x03]);
        let mut reader = WsReader::new(Cursor::new(one_byte));
        assert!(
            reader
                .read_frame()
                .unwrap_err()
                .to_string()
                .contains("one-byte")
        );

        let reserved_status = build_masked_frame(0x08, true, &1005_u16.to_be_bytes());
        let mut reader = WsReader::new(Cursor::new(reserved_status));
        assert!(
            reader
                .read_frame()
                .unwrap_err()
                .to_string()
                .contains("status code")
        );

        let mut invalid_reason = 1000_u16.to_be_bytes().to_vec();
        invalid_reason.push(0xff);
        let invalid_reason = build_masked_frame(0x08, true, &invalid_reason);
        let mut reader = WsReader::new(Cursor::new(invalid_reason));
        assert!(
            reader
                .read_frame()
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }

    #[test]
    fn writers_reject_invalid_control_frames_before_emitting_bytes_or_drawing_a_mask() {
        let invalid = WsFrame {
            frame_type: WsFrameType::Ping,
            payload: vec![0; 126],
            fin: true,
        };
        let mut server_output = Vec::new();
        {
            let mut server_writer = WsWriter::new(&mut server_output);
            assert!(server_writer.write_frame(&invalid).is_err());
        }
        assert_eq!(server_output.len(), 0);

        let draw_calls = std::cell::Cell::new(0);
        let mut client_output = Vec::new();
        {
            let mut client_writer = WsClientWriter::new(&mut client_output);
            let error = client_writer.write_frame_with_mask_draw(&invalid, || {
                draw_calls.set(draw_calls.get() + 1);
                Ok([1, 2, 3, 4])
            });
            assert!(error.is_err());
        }
        assert_eq!(draw_calls.get(), 0);
        assert_eq!(client_output.len(), 0);
    }

    #[test]
    fn writers_reject_complete_invalid_utf8_text_before_emitting_bytes_or_drawing_a_mask() {
        let invalid = WsFrame {
            frame_type: WsFrameType::Text,
            payload: vec![0xff],
            fin: true,
        };

        let mut server_output = Vec::new();
        {
            let mut server_writer = WsWriter::new(&mut server_output);
            assert!(server_writer.write_frame(&invalid).is_err());
        }
        assert_eq!(server_output.len(), 0);

        let draw_calls = std::cell::Cell::new(0);
        let mut client_output = Vec::new();
        {
            let mut client_writer = WsClientWriter::new(&mut client_output);
            let error = client_writer.write_frame_with_mask_draw(&invalid, || {
                draw_calls.set(draw_calls.get() + 1);
                Ok([1, 2, 3, 4])
            });
            assert!(error.is_err());
        }
        assert_eq!(draw_calls.get(), 0);
        assert_eq!(client_output.len(), 0);
    }

    #[test]
    fn test_reader_rejects_rsv_bits() {
        let mut buffer = Vec::new();
        buffer.push(0xC1); // FIN + RSV1 + Text opcode
        buffer.push(0x80); // Mask bit set + Zero payload
        buffer.extend_from_slice(&[0, 0, 0, 0]); // Mask key

        let mut reader = WsReader::new(Cursor::new(buffer));
        let err = reader.read_frame().unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn test_server_rejects_unmasked_client_frames() {
        // RFC 6455 Section 5.1: Server MUST reject unmasked frames
        let mut buffer = Vec::new();
        buffer.push(0x81); // FIN + Text opcode
        buffer.push(0x05); // NO mask bit + 5-byte payload
        buffer.extend_from_slice(b"hello");

        // Server-side reader (requires masking)
        let mut reader = WsReader::new(Cursor::new(buffer));
        let err = reader.read_frame().unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn test_client_accepts_unmasked_server_frames() {
        // Clients receive unmasked frames from servers
        let mut buffer = Vec::new();
        buffer.push(0x81); // FIN + Text opcode
        buffer.push(0x05); // NO mask bit + 5-byte payload
        buffer.extend_from_slice(b"hello");

        // Client-side reader (does not require masking)
        let mut reader = WsReader::new_client(Cursor::new(buffer));
        let frame = reader.read_frame().unwrap();
        assert_eq!(frame.as_text().unwrap(), "hello");
    }

    #[test]
    fn test_client_reader_rejects_masked_server_frames_for_every_frame_class() {
        let cases: [(&str, u8, bool, &[u8]); 7] = [
            ("text", 0x01, true, b"text"),
            ("fragmented text", 0x01, false, b"fragment"),
            ("continuation", 0x00, true, b"continuation"),
            ("binary", 0x02, true, b"binary"),
            ("close", 0x08, true, b""),
            ("ping", 0x09, true, b"ping"),
            ("pong", 0x0A, true, b"pong"),
        ];

        for (name, opcode, fin, payload) in cases {
            let mut reader =
                WsReader::new_client(Cursor::new(build_masked_frame(opcode, fin, payload)));
            let error = match reader.read_frame() {
                Ok(frame) => panic!("client accepted masked {name} frame: {frame:?}"),
                Err(error) => error,
            };

            assert!(
                matches!(
                    error,
                    TransportError::Io(ref source)
                        if source.kind() == std::io::ErrorKind::InvalidData
                ),
                "masked {name} frame did not fail with InvalidData: {error:?}"
            );
        }
    }

    /// Helper to build a masked WebSocket frame for testing server-side code.
    ///
    fn build_masked_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mask = [0x12, 0x34, 0x56, 0x78];
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();

        let mut frame = Vec::new();
        let byte1 = if fin { 0x80 } else { 0x00 } | opcode;
        frame.push(byte1);

        // Mask bit + payload length (including extended length encodings).
        let payload_len = payload.len();
        if payload_len < 126 {
            frame.push(0x80 | payload_len as u8);
        } else if payload_len < 65536 {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload_len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload_len as u64).to_be_bytes());
        }

        frame.extend_from_slice(&mask);
        frame.extend_from_slice(&masked);
        frame
    }

    fn assert_masked_normal_close(frame: &[u8; 8]) {
        assert_eq!(frame[0], 0x88, "close must use the RFC 6455 close opcode");
        assert_eq!(
            frame[1], 0x82,
            "client normal close must mask its two-byte status code"
        );
        let status = [frame[6] ^ frame[2], frame[7] ^ frame[3]];
        assert_eq!(
            u16::from_be_bytes(status),
            u16::from(CloseCode::Normal),
            "normal close must carry status 1000"
        );
    }

    fn build_unmasked_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push((if fin { 0x80 } else { 0x00 }) | opcode);
        assert!(
            payload.len() < 126,
            "test helper only supports short frames"
        );
        frame.push(payload.len() as u8);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn client_transport_rejects_masked_server_data_frame() {
        let input =
            build_masked_frame(0x01, true, br#"{"jsonrpc":"2.0","method":"masked","id":1}"#);
        let mut transport = WsClientTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn client_transport_rejects_masked_server_fragment_before_buffering() {
        let input = build_masked_frame(0x01, false, b"fragment");
        let mut transport = WsClientTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert!(!transport.fragmented_text);
        assert_eq!(transport.fragment_buffer.len(), 0);
    }

    #[test]
    fn client_transport_rejects_masked_server_ping_without_replying() {
        let input = build_masked_frame(0x09, true, b"ping");
        let mut transport = WsClientTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert_eq!(transport.writer.writer.len(), 0);
    }

    #[test]
    fn server_accepts_fragmented_text_with_empty_first_fragment() {
        let payload = br#"{"jsonrpc":"2.0","method":"empty-first","id":1}"#;
        let mut input = build_masked_frame(0x01, false, b"");
        input.extend(build_masked_frame(0x00, true, payload));
        let mut transport = WsTransport::new(Cursor::new(input), Vec::new());

        let message = transport.recv(&Cx::for_testing()).unwrap();

        let JsonRpcMessage::Request(request) = message else {
            panic!("expected request");
        };
        assert_eq!(request.method, "empty-first");
    }

    #[test]
    fn client_accepts_fragmented_text_with_empty_first_fragment() {
        let payload = br#"{"jsonrpc":"2.0","method":"empty-first","id":1}"#;
        let mut input = build_unmasked_frame(0x01, false, b"");
        input.extend(build_unmasked_frame(0x00, true, payload));
        let mut transport = WsClientTransport::new(Cursor::new(input), Vec::new());

        let message = transport.recv(&Cx::for_testing()).unwrap();

        let JsonRpcMessage::Request(request) = message else {
            panic!("expected request");
        };
        assert_eq!(request.method, "empty-first");
    }

    #[test]
    fn websocket_client_split_ingress_preserves_fragmented_final_result_source() {
        let source = br#"{"jsonrpc":"2.0","id":52,"result":{"resultType":"complete","opaque":{"decimal":1.20e+4}}}"#;
        let split = source.len() / 2;
        let mut input = build_unmasked_frame(0x01, false, &source[..split]);
        input.extend(build_unmasked_frame(0x00, true, &source[split..]));
        let (mut recv_half, _send_half) =
            WsClientTransport::new(Cursor::new(input), Vec::new()).into_split();

        let received = recv_half
            .recv_with_source(&Cx::for_testing())
            .expect("fragmented client ingress retains one complete source document");

        assert_eq!(received.source(), source);
        let JsonRpcMessage::Response(response) = received.message() else {
            panic!("final source must accompany the typed response");
        };
        assert_eq!(response.id, Some(RequestId::Number(52)));
    }

    #[test]
    fn websocket_split_halves_preserve_full_duplex_server_and_client_directions() {
        let cx = Cx::for_testing();
        let server_request = br#"{"jsonrpc":"2.0","method":"server/split","id":7}"#;
        let (mut server_recv, mut server_send) = WsTransport::new(
            Cursor::new(build_masked_frame(0x01, true, server_request)),
            Vec::new(),
        )
        .into_split();

        let JsonRpcMessage::Request(request) = server_recv.recv(&cx).expect("server split receive")
        else {
            panic!("server split must preserve a client request");
        };
        assert_eq!(request.method, "server/split");
        server_send
            .send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(7),
                    serde_json::json!({"server": true}),
                )),
            )
            .expect("server split response");
        server_send.close(&cx).expect("server split close");

        let server_output = server_send
            .writer
            .inner
            .lock()
            .expect("server split writer lock")
            .writer
            .writer
            .clone();
        let mut client_reader = WsReader::new_client(Cursor::new(server_output));
        let response = client_reader
            .read_frame()
            .expect("unmasked server response");
        assert_eq!(response.frame_type, WsFrameType::Text);
        assert_eq!(
            client_reader.read_frame().expect("server close").frame_type,
            WsFrameType::Close
        );

        let client_response = br#"{"jsonrpc":"2.0","id":8,"result":{"client":true}}"#;
        let (mut client_recv, mut client_send) = WsClientTransport::new(
            Cursor::new(build_unmasked_frame(0x01, true, client_response)),
            Vec::new(),
        )
        .into_split();

        let JsonRpcMessage::Response(response) =
            client_recv.recv(&cx).expect("client split receive")
        else {
            panic!("client split must preserve a server response");
        };
        assert_eq!(response.id, Some(RequestId::Number(8)));
        client_send
            .send(
                &cx,
                &JsonRpcMessage::Request(JsonRpcRequest::new("client/split", None, 8_i64)),
            )
            .expect("client split request");
        client_send.close(&cx).expect("client split close");

        let client_output = client_send
            .writer
            .inner
            .lock()
            .expect("client split writer lock")
            .writer
            .writer
            .clone();
        let mut server_reader = WsReader::new(Cursor::new(client_output));
        let request = server_reader.read_frame().expect("masked client request");
        assert_eq!(request.frame_type, WsFrameType::Text);
        assert_eq!(
            server_reader.read_frame().expect("client close").frame_type,
            WsFrameType::Close
        );
    }

    #[test]
    fn websocket_split_preserves_server_and_client_codec_limits() {
        let mut server = WsTransport::new(Cursor::new(Vec::new()), Vec::new());
        server.codec.set_max_message_size(128);
        let (server_recv, server_send) = server.into_split();
        assert_eq!(server_recv.codec.max_message_size(), 128);
        assert_eq!(server_send.codec.max_message_size(), 128);

        let mut client = WsClientTransport::new(Cursor::new(Vec::new()), Vec::new());
        client.codec.set_max_message_size(256);
        let (client_recv, client_send) = client.into_split();
        assert_eq!(client_recv.codec.max_message_size(), 256);
        assert_eq!(client_send.codec.max_message_size(), 256);
    }

    #[test]
    fn websocket_split_cancelled_send_writes_no_frame() {
        let (_recv_half, mut send_half) =
            WsTransport::new(Cursor::new(Vec::new()), Vec::new()).into_split();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        assert!(matches!(
            send_half.send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(1),
                    serde_json::json!({"cancelled": false}),
                )),
            ),
            Err(TransportError::Cancelled)
        ));
        assert_eq!(
            send_half
                .writer
                .inner
                .lock()
                .expect("split writer lock")
                .writer
                .writer
                .len(),
            0
        );
    }

    #[test]
    fn websocket_split_close_makes_pending_ingress_terminal() {
        let request = br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let (mut recv_half, mut send_half) = WsTransport::new(
            Cursor::new(build_masked_frame(0x01, true, request)),
            Vec::new(),
        )
        .into_split();

        let cx = Cx::for_testing();
        send_half.close(&cx).expect("close split writer");
        assert!(matches!(
            recv_half.recv(&Cx::for_testing()),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn websocket_split_terminal_close_wins_over_cancelled_context() {
        let (mut recv_half, mut send_half) =
            WsTransport::new(Cursor::new(Vec::new()), Vec::new()).into_split();
        let cx = Cx::for_testing();
        send_half.close(&cx).expect("close split writer");
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        assert!(matches!(recv_half.recv(&cx), Err(TransportError::Closed)));
        assert!(matches!(
            send_half.send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(1),
                    serde_json::json!({"closed": true}),
                )),
            ),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn async_client_recv_cancellation_wakes_idle_owned_socket_read() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (client_socket, _idle_peer) = virtual_socket_pair();
            let read_started = Arc::new(AtomicBool::new(false));
            let client_socket = ReadNotifyingIo::new(client_socket, Arc::clone(&read_started));
            let mut receive = cx
                .spawn(move |task_cx| async move {
                    let mut transport = AsyncWsClientTransport::from_upgraded(client_socket);
                    transport.recv(&task_cx).await
                })
                .expect("spawn client receive task");

            wait_for_idle_read(&read_started).await;
            assert!(
                !receive.is_finished(),
                "idle client receive must be blocked"
            );

            // TaskHandle::abort wakes the runtime-owned cancellation waker. The
            // peer stays idle, so completion proves cancellation preempted the
            // owned socket read rather than being driven by inbound traffic.
            receive.abort();
            assert!(matches!(
                receive.join(&cx).await,
                Ok(Err(TransportError::Cancelled))
            ));
        });
    }

    #[test]
    fn async_client_recv_without_cancellation_remains_idle() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (client_socket, _idle_peer) = virtual_socket_pair();
            let read_started = Arc::new(AtomicBool::new(false));
            let client_socket = ReadNotifyingIo::new(client_socket, Arc::clone(&read_started));
            let mut receive = cx
                .spawn(move |task_cx| async move {
                    let mut transport = AsyncWsClientTransport::from_upgraded(client_socket);
                    transport.recv(&task_cx).await
                })
                .expect("spawn client receive task");

            wait_for_idle_read(&read_started).await;
            // Near-negative: the same idle socket remains pending before the
            // cancellation waker is requested.
            assert!(!receive.is_finished());

            receive.abort();
            let _ = receive.join(&cx).await;
        });
    }

    #[test]
    fn async_client_upgrade_cancellation_wakes_a_quiet_peer_read() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (client_socket, _quiet_peer) = virtual_socket_pair();
            let read_started = Arc::new(AtomicBool::new(false));
            let client_socket = ReadNotifyingIo::new(client_socket, Arc::clone(&read_started));
            let mut upgrade = cx
                .spawn(move |task_cx| async move {
                    client_upgrade(&task_cx, "ws://quiet-peer.test/mcp", client_socket).await
                })
                .expect("spawn quiet-peer Upgrade task");

            wait_for_idle_read(&read_started).await;
            assert!(
                !upgrade.is_finished(),
                "quiet peer must leave the HTTP Upgrade read parked before cancellation"
            );

            // The peer writes no HTTP response. Completion therefore proves
            // that the caller-Cx-aware phase wait registered a cancellation
            // waker rather than relying on socket readiness.
            upgrade.abort();
            assert!(matches!(
                upgrade.join(&cx).await,
                Ok(Err(TransportError::Cancelled))
            ));
        });
    }

    #[test]
    fn async_client_tcp_connect_cancellation_wakes_a_quiet_phase() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            // This injected quiet connector uses the same production
            // `await_with_caller_cx` seam as `TcpStream::connect`, while
            // making the no-readiness interleaving deterministic.
            assert_quiet_establishment_phase_cancellation(&cx, "TCP connect").await;
        });
    }

    #[test]
    fn async_client_tls_handshake_cancellation_wakes_a_quiet_phase() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            // This injected quiet handshake uses the same production
            // `await_with_caller_cx` seam as `TlsConnector::connect`, while
            // making the no-readiness interleaving deterministic.
            assert_quiet_establishment_phase_cancellation(&cx, "TLS handshake").await;
        });
    }

    #[test]
    fn async_server_recv_cancellation_wakes_idle_owned_socket_read() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (_idle_peer, server_socket) = virtual_socket_pair();
            let read_started = Arc::new(AtomicBool::new(false));
            let server_socket = ReadNotifyingIo::new(server_socket, Arc::clone(&read_started));
            let transport = AsyncWsServerTransport::from_upgraded(server_socket);
            let mut receive = cx
                .spawn(move |task_cx| async move {
                    let mut transport = transport;
                    transport.recv(&task_cx).await
                })
                .expect("spawn server receive task");

            wait_for_idle_read(&read_started).await;
            assert!(
                !receive.is_finished(),
                "idle server receive must be blocked"
            );

            receive.abort();
            assert!(matches!(
                receive.join(&cx).await,
                Ok(Err(TransportError::Cancelled))
            ));
        });
    }

    #[test]
    fn async_server_recv_without_cancellation_remains_idle() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (_idle_peer, server_socket) = virtual_socket_pair();
            let read_started = Arc::new(AtomicBool::new(false));
            let server_socket = ReadNotifyingIo::new(server_socket, Arc::clone(&read_started));
            let transport = AsyncWsServerTransport::from_upgraded(server_socket);
            let mut receive = cx
                .spawn(move |task_cx| async move {
                    let mut transport = transport;
                    transport.recv(&task_cx).await
                })
                .expect("spawn server receive task");

            wait_for_idle_read(&read_started).await;
            assert!(!receive.is_finished());

            receive.abort();
            let _ = receive.join(&cx).await;
        });
    }

    #[test]
    fn async_server_reassembles_masked_text_across_ping_and_replies_unmasked() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (mut peer, server_socket) = virtual_socket_pair();
            let mut inbound =
                build_masked_frame(0x01, false, b"{\"jsonrpc\":\"2.0\",\"method\":\"");
            inbound.extend(build_masked_frame(0x09, true, b"p"));
            inbound.extend(build_masked_frame(
                0x00,
                true,
                br#"server/fragment","id":17}"#,
            ));
            let mut peer_task = cx
                .spawn(move |_task_cx| async move {
                    peer.write_all(&inbound)
                        .await
                        .expect("write masked fragmented client message");
                    let mut pong = [0_u8; 3];
                    peer.read_exact(&mut pong)
                        .await
                        .expect("read unmasked server pong");
                    pong
                })
                .expect("spawn raw WebSocket peer");

            let mut transport = AsyncWsServerTransport::from_upgraded(server_socket);
            let JsonRpcMessage::Request(request) =
                transport.recv(&cx).await.expect("reassembled text message")
            else {
                panic!("expected fragmented client request");
            };
            assert_eq!(request.method, "server/fragment");

            let pong = peer_task.join(&cx).await.expect("join raw WebSocket peer");
            assert_eq!(pong, [0x8A, 0x01, b'p']);
        });
    }

    #[test]
    fn async_server_send_writes_one_unmasked_text_frame() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (mut peer, server_socket) = virtual_socket_pair();
            let mut peer_task = cx
                .spawn(move |_task_cx| async move {
                    let mut header = [0_u8; 2];
                    peer.read_exact(&mut header)
                        .await
                        .expect("read server frame header");
                    assert_eq!(header[0], 0x81, "server output must be a final text frame");
                    assert_eq!(header[1] & 0x80, 0, "server output must be unmasked");
                    let payload_len = usize::from(header[1] & 0x7F);
                    assert!(payload_len < 126, "test message must use a short frame");
                    let mut payload = vec![0_u8; payload_len];
                    peer.read_exact(&mut payload)
                        .await
                        .expect("read server text payload");
                    payload
                })
                .expect("spawn raw WebSocket peer");

            let mut transport = AsyncWsServerTransport::from_upgraded(server_socket);
            transport
                .send(
                    &cx,
                    &JsonRpcMessage::Response(JsonRpcResponse::success(
                        RequestId::Number(18),
                        serde_json::json!({"server": "raw-stream"}),
                    )),
                )
                .await
                .expect("send raw server text frame");

            let payload = peer_task.join(&cx).await.expect("join raw WebSocket peer");
            let JsonRpcMessage::Response(response) = Codec::new()
                .decode_complete_message(&payload)
                .expect("server text payload must be JSON-RPC")
            else {
                panic!("expected server response");
            };
            assert_eq!(response.id, Some(RequestId::Number(18)));
        });
    }

    #[test]
    fn async_client_split_close_uses_the_caller_context_and_latches_both_halves() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (client_socket, mut peer) = virtual_socket_pair();
            let mut peer_task = cx
                .spawn(move |_task_cx| async move {
                    let mut frame = [0_u8; 8];
                    peer.read_exact(&mut frame)
                        .await
                        .expect("read masked client close frame");
                    frame
                })
                .expect("spawn split close peer");

            let client = AsyncWsClientTransport::from_upgraded(client_socket);
            let (mut receiver, mut sender) = client.into_split();
            sender
                .close(&cx)
                .await
                .expect("caller-context split close must emit one close frame");
            let frame = peer_task.join(&cx).await.expect("join split close peer");
            assert_masked_normal_close(&frame);
            assert!(matches!(sender.close(&cx).await, Ok(())));
            assert!(matches!(
                receiver.recv_with_source(&cx).await,
                Err(TransportError::Closed)
            ));
        });
    }

    #[test]
    fn async_client_split_cancelled_close_waiting_for_writer_can_retry() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (client_socket, mut peer) = virtual_socket_pair();
            let client = AsyncWsClientTransport::from_upgraded(client_socket);
            let (mut receiver, mut sender) = client.into_split();
            let writer = Arc::clone(&sender.writer);
            let held_writer = OwnedMutexGuard::lock(Arc::clone(&writer), &cx)
                .await
                .expect("hold the split writer before close election");

            let close_cx = Cx::for_testing();
            let mut cancelled_close = Box::pin(sender.close(&close_cx));
            std::future::poll_fn(|task_cx| {
                assert!(
                    cancelled_close.as_mut().poll(task_cx).is_pending(),
                    "the held writer must keep the close future pending"
                );
                Poll::Ready(())
            })
            .await;
            assert_eq!(
                writer.waiters(),
                1,
                "the close operation must be parked on the held writer before cancellation"
            );
            close_cx.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel blocked close"),
            );
            assert!(matches!(
                cancelled_close.await,
                Err(TransportError::Cancelled)
            ));
            assert_eq!(writer.waiters(), 0, "cancellation must retire its waiter");
            assert_eq!(
                WebSocketTerminalState::load(&receiver.terminal),
                WebSocketTerminalState::Open,
                "cancellation before writer-lock acquisition must not elect close"
            );

            drop(held_writer);
            receiver
                .close(&cx)
                .await
                .expect("a close cancelled before election must be safely retryable");
            let mut close = [0_u8; 8];
            peer.read_exact(&mut close)
                .await
                .expect("the retry must write one masked Close frame");
            assert_masked_normal_close(&close);
        });
    }

    #[test]
    fn async_client_split_sender_rechecks_terminal_state_after_writer_wait() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let writes = Arc::new(AtomicUsize::new(0));
            let client =
                AsyncWsClientTransport::from_upgraded(WriteCountingIo::new(Arc::clone(&writes)));
            let (receiver, mut sender) = client.into_split();
            let writer = Arc::clone(&sender.writer);
            let held_writer = OwnedMutexGuard::lock(writer, &cx)
                .await
                .expect("hold writer until sender is queued");
            let message = JsonRpcMessage::Response(JsonRpcResponse::success(
                RequestId::Number(73),
                serde_json::json!({"must_not_write": true}),
            ));
            let mut send = cx
                .spawn(move |task_cx| async move { sender.send(&task_cx, &message).await })
                .expect("spawn sender blocked on writer lock");
            asupersync::runtime::yield_now().await;

            // Force the exact interleaving: the send observed Open before the
            // lock, then the paired connection became terminal before release.
            receiver
                .terminal
                .store(WebSocketTerminalState::Closed.as_u8(), Ordering::Release);
            drop(held_writer);
            assert!(matches!(
                send.join(&cx).await,
                Ok(Err(TransportError::Closed))
            ));
            assert_eq!(
                writes.load(Ordering::Acquire),
                0,
                "a sender parked on the writer lock must not overtake terminal close"
            );
        });
    }

    #[test]
    fn async_client_split_cancel_during_close_write_commits_terminal_failure() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let write_started = Arc::new(AtomicBool::new(false));
            let client = AsyncWsClientTransport::from_upgraded(GatedWriteIo::new(Arc::clone(
                &write_started,
            )));
            let (mut receiver, mut sender) = client.into_split();
            let mut close = cx
                .spawn(move |task_cx| async move { sender.close(&task_cx).await })
                .expect("spawn close with a gated frame write");

            wait_for_idle_read(&write_started).await;
            close.abort();
            assert!(matches!(
                close.join(&cx).await,
                Ok(Err(TransportError::Cancelled))
            ));
            assert_eq!(
                WebSocketTerminalState::load(&receiver.terminal),
                WebSocketTerminalState::Failed,
                "a cancellation after close-frame emission starts is terminal, not a false success"
            );
            assert!(matches!(
                receiver.close(&cx).await,
                Err(TransportError::Closed)
            ));
        });
    }

    #[test]
    fn async_server_recv_with_source_preserves_admitted_response_bytes() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (mut peer, server_socket) = virtual_socket_pair();
            let source = br#"{"jsonrpc":"2.0","id":19,"result":{"zeta":1.20e+4,"alpha":{"second":2,"first":1}}}"#;
            let inbound = build_masked_frame(0x01, true, source);
            let mut writer = cx
                .spawn(move |_task_cx| async move {
                    peer.write_all(&inbound)
                        .await
                        .expect("write source-preserving client response");
                })
                .expect("spawn source-preserving client response writer");

            let mut transport = AsyncWsServerTransport::from_upgraded(server_socket);
            let received = transport
                .recv_with_source(&cx)
                .await
                .expect("receive one source-preserving response");
            assert_eq!(received.source(), source);
            assert!(matches!(received.message(), JsonRpcMessage::Response(_)));

            writer
                .join(&cx)
                .await
                .expect("join source-preserving client response writer");
        });
    }

    #[test]
    fn async_server_read_boundary_preserves_cancellation_and_real_errors() {
        run_test(|| async {
            for (cancel, kind) in [
                (true, io::ErrorKind::Interrupted),
                (false, io::ErrorKind::Interrupted),
                (true, io::ErrorKind::InvalidData),
            ] {
                let cx = Cx::for_testing();
                let write_attempted = Arc::new(AtomicBool::new(false));
                let mut io = ReadFailingIo::new(Arc::clone(&write_attempted));
                io.error_kind = kind;
                io.cancel_on_read = cancel.then(|| cx.clone());
                let mut transport = AsyncWsServerTransport::from_upgraded(io);
                assert!(!cx.is_cancel_requested());
                let error = transport.recv(&cx).await.expect_err("read must fail");
                if cancel && kind == io::ErrorKind::Interrupted {
                    assert!(matches!(error, TransportError::Cancelled));
                } else {
                    assert!(matches!(
                        error,
                        TransportError::Io(ref source) if source.kind() == kind
                    ));
                }
                assert_eq!(cx.is_cancel_requested(), cancel);
                assert!(!write_attempted.load(Ordering::Acquire));
                assert!(transport.read_buf.is_empty());
                assert!(matches!(
                    transport.recv(&cx).await,
                    Err(TransportError::Closed)
                ));
            }
        });
    }

    #[test]
    fn async_server_abnormal_read_failure_latches_closed_without_close_frame() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let write_attempted = Arc::new(AtomicBool::new(false));
            let mut transport = AsyncWsServerTransport::from_upgraded(ReadFailingIo::new(
                Arc::clone(&write_attempted),
            ));

            let error = transport
                .recv(&cx)
                .await
                .expect_err("peer I/O reset must be returned to the caller");
            assert!(matches!(
                error,
                TransportError::Io(ref source)
                    if source.kind() == io::ErrorKind::ConnectionReset
            ));
            assert!(
                !write_attempted.load(Ordering::Acquire),
                "RFC 6455 abnormal closure (1006) must not synthesize a Close frame"
            );
            assert!(matches!(
                transport.recv(&cx).await,
                Err(TransportError::Closed)
            ));
        });
    }

    #[test]
    fn async_client_binary_message_sends_close_and_latches_terminal_state() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (client_socket, mut peer) = virtual_socket_pair();
            let mut close_reply = cx
                .spawn(move |_task_cx| async move {
                    peer.write_all(&[0x88, 0x02, 0x03, 0xE8])
                        .await
                        .expect("write unmasked server close reply");
                })
                .expect("spawn client close reply");
            let mut transport = AsyncWsClientTransport::from_upgraded(client_socket);

            let error = transport
                .decode_or_terminate(&cx, Some(NativeWsMessage::Binary(vec![0x01].into())))
                .await
                .expect_err("binary WebSocket message must be rejected");
            assert!(matches!(
                error,
                TransportError::Io(ref source) if source.kind() == std::io::ErrorKind::InvalidData
            ));
            assert!(matches!(
                transport.recv(&cx).await,
                Err(TransportError::Closed)
            ));
            close_reply
                .join(&cx)
                .await
                .expect("join client close reply");
        });
    }

    #[test]
    fn async_server_invalid_message_sends_close_and_latches_terminal_state() {
        run_test(|| async {
            let cx = Cx::current().expect("runtime root context");
            let (mut peer, server_socket) = virtual_socket_pair();
            let invalid_message = build_masked_frame(0x01, true, b"not json");
            let mut invalid_message_writer = cx
                .spawn(move |_task_cx| async move {
                    peer.write_all(&invalid_message)
                        .await
                        .expect("write masked invalid client message");
                })
                .expect("spawn invalid message writer");
            let mut transport = AsyncWsServerTransport::from_upgraded(server_socket);

            let error = transport
                .recv(&cx)
                .await
                .expect_err("invalid JSON-RPC text must be rejected");
            assert!(matches!(error, TransportError::Codec(_)));
            assert!(matches!(
                transport.recv(&cx).await,
                Err(TransportError::Closed)
            ));
            invalid_message_writer
                .join(&cx)
                .await
                .expect("join invalid message writer");
        });
    }

    #[test]
    fn websocket_server_split_rejects_unmasked_client_frame_without_control_write() {
        let request = br#"{"jsonrpc":"2.0","method":"server/split","id":7}"#;
        let (mut recv_half, mut send_half) = WsTransport::new(
            Cursor::new(build_unmasked_frame(0x01, true, request)),
            Vec::new(),
        )
        .into_split();

        let error = recv_half.recv(&Cx::for_testing()).unwrap_err();
        assert!(matches!(
            error,
            TransportError::Io(ref source) if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert!(matches!(
            send_half.send(
                &Cx::for_testing(),
                &JsonRpcMessage::Request(JsonRpcRequest::new("client/split", None, 8_i64)),
            ),
            Err(TransportError::Closed)
        ));
        assert_eq!(
            send_half
                .writer
                .inner
                .lock()
                .expect("server split writer lock")
                .writer
                .writer
                .len(),
            0
        );
    }

    #[test]
    fn server_rejects_invalid_utf8_text_with_close_1007_and_latches_closed() {
        let input = build_masked_frame(0x01, true, &[0xff]);
        let mut transport = WsTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
                    && source.to_string().contains("UTF-8")
        ));
        assert!(transport.closed);
        assert_eq!(transport.fragment_buffer.len(), 0);
        assert!(!transport.fragmented_text);
        assert_eq!(transport.writer.writer, [0x88, 0x02, 0x03, 0xef]);

        let cancelled = Cx::for_testing();
        cancelled.set_cancel_requested(true);
        assert!(matches!(
            transport.recv(&cancelled),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn client_rejects_invalid_utf8_text_with_masked_close_1007_and_latches_closed() {
        let input = build_unmasked_frame(0x01, true, &[0xff]);
        let mut transport = WsClientTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
                    && source.to_string().contains("UTF-8")
        ));
        assert!(transport.closed);
        assert_eq!(transport.fragment_buffer.len(), 0);
        assert!(!transport.fragmented_text);

        let mut close_reader = WsReader::new(Cursor::new(transport.writer.writer.as_slice()));
        let close = close_reader.read_frame().unwrap();
        assert_eq!(close.frame_type, WsFrameType::Close);
        assert_eq!(close.payload, 1007_u16.to_be_bytes());
    }

    #[test]
    fn fragmented_invalid_utf8_is_rejected_only_after_complete_message_assembly() {
        let mut input = build_masked_frame(0x01, false, &[0xf0]);
        input.extend(build_masked_frame(0x00, true, &[0x28]));
        let mut transport = WsTransport::new(Cursor::new(input), Vec::new());

        assert!(matches!(
            transport.recv(&Cx::for_testing()),
            Err(TransportError::Io(ref source))
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert!(transport.closed);
        assert_eq!(transport.fragment_buffer.len(), 0);
        assert!(!transport.fragmented_text);
        assert_eq!(transport.writer.writer, [0x88, 0x02, 0x03, 0xef]);
    }

    #[test]
    fn terminal_frame_read_error_clears_fragment_state() {
        let mut input = build_masked_frame(0x01, false, b"prefix");
        // Reserved opcode is a terminal framing error.
        input.extend(build_masked_frame(0x03, true, b"bad"));
        let mut transport = WsTransport::new(Cursor::new(input), Vec::new());

        assert!(matches!(
            transport.recv(&Cx::for_testing()),
            Err(TransportError::Io(_))
        ));
        assert!(transport.closed);
        assert_eq!(transport.fragment_buffer.len(), 0);
        assert!(!transport.fragmented_text);
    }

    #[test]
    fn test_fragmented_message_size_limit() {
        // Build masked frames (client -> server)
        let mut buffer = Vec::new();
        // Text frame start (FIN=0, opcode=Text)
        buffer.extend(build_masked_frame(0x01, false, b"hello"));
        // Continuation frame end (FIN=1, opcode=Continuation)
        buffer.extend(build_masked_frame(0x00, true, b"world"));

        let cx = Cx::for_testing();
        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);
        transport.max_message_size = 8;

        let err = transport.recv(&cx).unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn test_transport_rejects_escaped_duplicate_object_member() {
        let payload = br#"{"jsonrpc":"2.0","method":"first","m\u0065thod":"second","id":1}"#;
        let frame = build_masked_frame(0x01, true, payload);
        let writer = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(frame), writer);

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
    fn test_rejects_interleaved_binary_during_fragmentation() {
        // RFC 6455 Section 5.4: Data frames MUST NOT be interleaved
        // Build masked frames (client -> server)
        let mut buffer = Vec::new();
        // Text frame start (FIN=0, opcode=Text)
        buffer.extend(build_masked_frame(0x01, false, b"hello"));
        // Binary frame (interleaved - MUST be rejected)
        buffer.extend(build_masked_frame(0x02, true, b"bad"));

        let cx = Cx::for_testing();
        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);

        let err = transport.recv(&cx).unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn test_transport_roundtrip() {
        use fastmcp_protocol::RequestId;

        // Create a pipe using in-memory buffers
        // Simulating server -> client: server writes unmasked, client reads unmasked
        let mut write_buf = Vec::new();

        // Server writes a request (unmasked)
        {
            let cx = Cx::for_testing();
            let reader: &[u8] = &[];
            let mut transport = WsTransport::new(reader, &mut write_buf);

            let request = JsonRpcRequest {
                jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
                id: Some(RequestId::Number(1)),
                method: "test".to_string(),
                params: None,
            };

            transport.send_request(&cx, &request).unwrap();
        }

        // Client reads the request (accepts unmasked frames from server)
        {
            let cx = Cx::for_testing();
            let writer: Vec<u8> = Vec::new();
            let mut transport = WsClientTransport::new(Cursor::new(write_buf), writer);

            let msg = transport.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Request(_)),
                "Expected request"
            );
            if let JsonRpcMessage::Request(req) = msg {
                assert_eq!(req.method, "test");
                assert_eq!(req.id, Some(RequestId::Number(1)));
            }
        }
    }

    #[test]
    fn test_close_frame_returns_closed_error() {
        // Build a masked close frame (simulating client -> server)
        let mut buffer = Vec::new();
        buffer.push(0x88); // FIN + Close opcode
        buffer.push(0x80); // Masked, 0 payload length
        buffer.extend_from_slice(&[0u8; 4]); // Mask key (zeros work for empty payload)

        let cx = Cx::for_testing();
        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);

        let result = transport.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
        assert_eq!(transport.writer.writer, vec![0x88, 0x00]);

        let request = JsonRpcRequest::new("after-close", None, 1_i64);
        assert!(matches!(
            transport.send_request(&cx, &request),
            Err(TransportError::Closed)
        ));
        assert!(matches!(transport.recv(&cx), Err(TransportError::Closed)));
    }

    #[test]
    fn server_and_client_echo_valid_peer_close_payloads_before_latching_closed() {
        let mut payload = 1000_u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"done");

        let server_input = build_masked_frame(0x08, true, &payload);
        let mut server = WsTransport::new(Cursor::new(server_input), Vec::new());
        assert!(matches!(
            server.recv(&Cx::for_testing()),
            Err(TransportError::Closed)
        ));
        assert_eq!(server.writer.writer[0], 0x88);
        assert_eq!(usize::from(server.writer.writer[1]), payload.len());
        assert_eq!(&server.writer.writer[2..], payload);

        let client_input = build_unmasked_frame(0x08, true, &payload);
        let mut client = WsClientTransport::new(Cursor::new(client_input), Vec::new());
        assert!(matches!(
            client.recv(&Cx::for_testing()),
            Err(TransportError::Closed)
        ));
        let mut echoed = WsReader::new(Cursor::new(client.writer.writer.as_slice()));
        let echoed = echoed.read_frame().expect("client close reply is masked");
        assert_eq!(echoed.frame_type, WsFrameType::Close);
        assert_eq!(echoed.payload, payload);
    }

    #[test]
    fn test_ping_auto_pong() {
        // Build masked frames (simulating client -> server)
        let mut buffer = Vec::new();

        // Ping frame (masked, opcode 0x09)
        buffer.extend(build_masked_frame(0x09, true, b"ping"));

        // Text frame with JSON-RPC (masked, opcode 0x01)
        let text = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        buffer.extend(build_masked_frame(0x01, true, text.as_bytes()));

        let mut response_buf = Vec::new();

        let cx = Cx::for_testing();
        let mut transport = WsTransport::new(Cursor::new(buffer), &mut response_buf);

        // Should skip ping (auto-pong) and return the text message
        let msg = transport.recv(&cx).unwrap();
        assert!(
            matches!(msg, JsonRpcMessage::Request(_)),
            "Expected request"
        );
        if let JsonRpcMessage::Request(req) = msg {
            assert_eq!(req.method, "test");
        }

        // Check that pong was written
        assert_ne!(response_buf.len(), 0);
        assert_eq!(response_buf[0] & 0x0F, 0x0A); // Pong opcode
    }

    // =========================================================================
    // E2E WebSocket Tests (bd-2kv / bd-2gyv)
    // =========================================================================

    #[test]
    fn e2e_ws_bidirectional_message_flow() {
        use fastmcp_protocol::RequestId;

        // Simulate a full bidirectional message flow
        // Server-side processing of multiple requests and responses

        let mut request_buffer = Vec::new();

        // Build multiple masked requests (client -> server)
        let req1 = r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;
        let req2 = r#"{"jsonrpc":"2.0","method":"tools/list","id":2}"#;
        let req3 = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"test"},"id":3}"#;

        request_buffer.extend(build_masked_frame(0x01, true, req1.as_bytes()));
        request_buffer.extend(build_masked_frame(0x01, true, req2.as_bytes()));
        request_buffer.extend(build_masked_frame(0x01, true, req3.as_bytes()));

        let mut response_buffer = Vec::new();
        let cx = Cx::for_testing();

        {
            let mut transport = WsTransport::new(Cursor::new(request_buffer), &mut response_buffer);

            // Receive and process each request
            for expected_id in 1..=3 {
                let msg = transport.recv(&cx).unwrap();
                assert!(
                    matches!(msg, JsonRpcMessage::Request(_)),
                    "Expected request"
                );
                let JsonRpcMessage::Request(req) = msg else {
                    return;
                };

                assert_eq!(req.id, Some(RequestId::Number(expected_id)));

                // Send response
                let response = JsonRpcResponse {
                    jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
                    result: Some(serde_json::json!({"ok": true})),
                    error: None,
                    id: req.id,
                };
                transport.send_response(&cx, &response).unwrap();
            }
        }

        // Verify responses were written (unmasked for server -> client)
        assert_ne!(response_buffer.len(), 0);
        // Each response should be a separate text frame
        #[allow(clippy::naive_bytecount)]
        let frame_count = response_buffer
            .iter()
            .filter(|&&b| b == 0x81) // FIN + Text opcode
            .count();
        assert_eq!(frame_count, 3, "Expected 3 response frames");
    }

    #[test]
    fn e2e_ws_fragmented_message_assembly() {
        // Test receiving a fragmented JSON-RPC message
        let full_msg =
            r#"{"jsonrpc":"2.0","method":"test","params":{"data":"hello world"},"id":1}"#;
        let mid = full_msg.len() / 2;

        let mut buffer = Vec::new();
        // First fragment (FIN=0, opcode=Text)
        buffer.extend(build_masked_frame(0x01, false, &full_msg.as_bytes()[..mid]));
        // Continuation fragment (FIN=1, opcode=Continuation)
        buffer.extend(build_masked_frame(0x00, true, &full_msg.as_bytes()[mid..]));

        let cx = Cx::for_testing();
        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);

        let msg = transport.recv(&cx).unwrap();
        assert!(
            matches!(msg, JsonRpcMessage::Request(_)),
            "Expected request"
        );
        let JsonRpcMessage::Request(req) = msg else {
            return;
        };
        assert_eq!(req.method, "test");
        let params = req.params.unwrap();
        assert_eq!(params.get("data").unwrap(), "hello world");
    }

    #[test]
    fn e2e_ws_interleaved_ping_during_operation() {
        // Test that ping/pong doesn't disrupt normal message flow
        let mut buffer = Vec::new();

        // Message 1
        buffer.extend(build_masked_frame(
            0x01,
            true,
            r#"{"jsonrpc":"2.0","method":"msg1","id":1}"#.as_bytes(),
        ));
        // Ping (should be handled automatically)
        buffer.extend(build_masked_frame(0x09, true, b"keepalive"));
        // Message 2
        buffer.extend(build_masked_frame(
            0x01,
            true,
            r#"{"jsonrpc":"2.0","method":"msg2","id":2}"#.as_bytes(),
        ));
        // Another ping
        buffer.extend(build_masked_frame(0x09, true, b"alive"));
        // Message 3
        buffer.extend(build_masked_frame(
            0x01,
            true,
            r#"{"jsonrpc":"2.0","method":"msg3","id":3}"#.as_bytes(),
        ));

        let mut response_buffer = Vec::new();
        let cx = Cx::for_testing();
        let mut transport = WsTransport::new(Cursor::new(buffer), &mut response_buffer);

        // Should receive all 3 messages, with pings handled automatically
        for i in 1..=3 {
            let msg = transport.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Request(_)),
                "Expected request"
            );
            let JsonRpcMessage::Request(req) = msg else {
                return;
            };
            assert_eq!(req.method, format!("msg{i}"));
        }

        // Verify pongs were sent - the response buffer should contain pong frames
        // Pong frames have opcode 0x0A and FIN bit set (0x8A)
        // Just verify we have some response data (pongs are there)
        assert!(
            !response_buffer.is_empty(),
            "Expected pong responses to be written"
        );
    }

    #[test]
    fn e2e_ws_graceful_close() {
        // Test graceful close handshake
        let mut buffer = Vec::new();
        // Message followed by close
        buffer.extend(build_masked_frame(
            0x01,
            true,
            r#"{"jsonrpc":"2.0","method":"last","id":1}"#.as_bytes(),
        ));
        buffer.extend(build_masked_frame(0x08, true, &[])); // Close frame

        let mut response_buffer = Vec::new();
        let cx = Cx::for_testing();
        let mut transport = WsTransport::new(Cursor::new(buffer), &mut response_buffer);

        // Receive the message
        let msg = transport.recv(&cx).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Request(_)));

        // Next recv should return Closed
        let result = transport.recv(&cx);
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    #[test]
    fn e2e_ws_cancellation_respected() {
        let buffer = build_masked_frame(
            0x01,
            true,
            r#"{"jsonrpc":"2.0","method":"test","id":1}"#.as_bytes(),
        );

        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);

        // Recv should respect cancellation
        let result = transport.recv(&cx);
        assert!(matches!(result, Err(TransportError::Cancelled)));
    }

    #[test]
    fn e2e_ws_send_cancellation_respected() {
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let reader: &[u8] = &[];
        let mut writer = Vec::new();
        let mut transport = WsTransport::new(reader, &mut writer);

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = transport.send_request(&cx, &request);
        assert!(matches!(result, Err(TransportError::Cancelled)));

        // Nothing should be written
        assert_eq!(writer.len(), 0);
    }

    #[test]
    fn e2e_ws_unicode_in_messages() {
        // Test Unicode handling in WebSocket text frames
        let unicode_msg =
            r#"{"jsonrpc":"2.0","method":"test","params":{"text":"Hello 世界 👋 éèê"},"id":1}"#;
        let buffer = build_masked_frame(0x01, true, unicode_msg.as_bytes());

        let cx = Cx::for_testing();
        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);

        let msg = transport.recv(&cx).unwrap();
        assert!(
            matches!(msg, JsonRpcMessage::Request(_)),
            "Expected request"
        );
        let JsonRpcMessage::Request(req) = msg else {
            return;
        };
        let params = req.params.unwrap();
        let text = params.get("text").unwrap().as_str().unwrap();
        assert!(text.contains("世界"));
        assert!(text.contains("👋"));
        assert!(text.contains("éèê"));
    }

    #[test]
    fn e2e_ws_client_server_full_flow() {
        use fastmcp_protocol::RequestId;

        // Full client-server flow with proper masking

        // 1. Client sends masked request
        let mut client_to_server = Vec::new();
        {
            let mut writer = WsClientWriter::new(&mut client_to_server);
            let request = r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;
            writer.write_frame(&WsFrame::text(request)).unwrap();
        }

        // 2. Server receives and processes
        let mut server_response = Vec::new();
        {
            let cx = Cx::for_testing();
            let mut transport =
                WsTransport::new(Cursor::new(client_to_server.clone()), &mut server_response);

            let msg = transport.recv(&cx).unwrap();
            if let JsonRpcMessage::Request(req) = msg {
                assert_eq!(req.method, "initialize");

                // Send response (unmasked)
                let response = JsonRpcResponse {
                    jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
                    result: Some(serde_json::json!({"capabilities": {}})),
                    error: None,
                    id: Some(RequestId::Number(1)),
                };
                transport.send_response(&cx, &response).unwrap();
            }
        }

        // 3. Client receives response
        {
            let cx = Cx::for_testing();
            let mut transport =
                WsClientTransport::new(Cursor::new(server_response), Vec::<u8>::new());

            let msg = transport.recv(&cx).unwrap();
            assert!(
                matches!(msg, JsonRpcMessage::Response(_)),
                "Expected response"
            );
            let JsonRpcMessage::Response(resp) = msg else {
                return;
            };
            assert_eq!(resp.id, Some(RequestId::Number(1)));
            assert!(resp.result.is_some());
        }
    }

    #[test]
    fn ws_continuation_opcode_roundtrip() {
        let ft = WsFrameType::Continuation;
        assert_eq!(WsFrameType::from_opcode(ft.opcode()), Some(ft));
    }

    #[test]
    fn ws_unknown_opcode_returns_none() {
        assert_eq!(WsFrameType::from_opcode(0x03), None);
        assert_eq!(WsFrameType::from_opcode(0x0F), None);
    }

    #[test]
    fn ws_frame_as_text_non_utf8_returns_error() {
        let frame = WsFrame {
            frame_type: WsFrameType::Text,
            payload: vec![0xFF, 0xFE],
            fin: true,
        };
        assert!(frame.as_text().is_err());
    }

    #[test]
    fn ws_transport_close_sends_close_frame() {
        let reader: &[u8] = &[];
        let mut output = Vec::new();
        let mut transport = WsTransport::new(reader, &mut output);
        let cx = Cx::for_testing();
        transport.close(&cx).unwrap();
        transport.close(&cx).expect("close is idempotent");

        let cx = Cx::for_testing();
        let request = JsonRpcRequest::new("after-close", None, 1_i64);
        assert!(matches!(
            transport.send_request(&cx, &request),
            Err(TransportError::Closed)
        ));
        assert!(matches!(transport.ping(), Err(TransportError::Closed)));
        assert!(matches!(transport.recv(&cx), Err(TransportError::Closed)));

        // Close frame: FIN + opcode 0x08 = 0x88, payload length 0
        assert_eq!(output, [0x88, 0x00]);
    }

    #[test]
    fn ws_transport_ping_sends_ping_frame() {
        let reader: &[u8] = &[];
        let mut output = Vec::new();
        let mut transport = WsTransport::new(reader, &mut output);
        transport.ping().unwrap();

        // Ping frame: FIN + opcode 0x09 = 0x89, payload length 0
        assert!(output.len() >= 2);
        assert_eq!(output[0], 0x89);
        assert_eq!(output[1], 0x00);
    }

    #[test]
    fn ws_client_transport_send_cancelled() {
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let reader: &[u8] = &[];
        let mut writer = Vec::new();
        let mut transport = WsClientTransport::new(reader, &mut writer);

        let request = JsonRpcRequest::new("test", None, 1i64);
        let result = transport.send(&cx, &JsonRpcMessage::Request(request));
        assert!(matches!(result, Err(TransportError::Cancelled)));
        assert_eq!(writer.len(), 0);
    }

    #[test]
    fn ws_server_rejects_binary_message_outside_fragmentation() {
        let input = build_masked_frame(0x02, true, b"binary-data");
        let mut transport = WsTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn ws_client_rejects_binary_message_outside_fragmentation() {
        let input = build_unmasked_frame(0x02, true, b"binary-data");
        let mut transport = WsClientTransport::new(Cursor::new(input), Vec::new());

        let error = transport.recv(&Cx::for_testing()).unwrap_err();

        assert!(matches!(
            error,
            TransportError::Io(ref source)
                if source.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn ws_pong_frame_skipped() {
        // Pong frame followed by a text message
        let mut buffer = Vec::new();
        buffer.extend(build_masked_frame(0x0A, true, b"pong-payload"));
        buffer.extend(build_masked_frame(
            0x01,
            true,
            r#"{"jsonrpc":"2.0","method":"after_pong","id":2}"#.as_bytes(),
        ));

        let cx = Cx::for_testing();
        let writer: Vec<u8> = Vec::new();
        let mut transport = WsTransport::new(Cursor::new(buffer), writer);

        let msg = transport.recv(&cx).unwrap();
        let JsonRpcMessage::Request(req) = msg else {
            panic!("expected request");
        };
        assert_eq!(req.method, "after_pong");
    }

    #[test]
    fn blocking_websocket_fixture_mask_ownership_and_fallbacks_are_denied() {
        let blocking_fixture = include_str!("websocket.rs");

        let writer_impl_start = blocking_fixture
            .find("impl<W: Write> WsClientWriter<W> {")
            .expect("client writer implementation marker");
        let writer_impl_end = blocking_fixture[writer_impl_start..]
            .find("/// Client-side WebSocket transport.")
            .map(|offset| writer_impl_start + offset)
            .expect("client writer implementation end marker");
        let writer_impl = &blocking_fixture[writer_impl_start..writer_impl_end];
        assert_eq!(writer_impl.matches("draw_websocket_mask()").count(), 1);
        assert_eq!(
            writer_impl.matches(".map_err(map_mask_draw_error)").count(),
            1
        );
        assert!(!writer_impl.contains("getrandom::"));
        assert!(!writer_impl.contains("draw_security_identifier"));
        let mask_decl = writer_impl
            .lines()
            .find(|line| line.contains("fn generate_mask()"))
            .expect("mask helper declaration")
            .trim_start();
        let injected_decl = writer_impl
            .lines()
            .find(|line| line.contains("fn write_frame_with_mask_draw"))
            .expect("injected mask helper declaration")
            .trim_start();
        assert!(mask_decl.starts_with("fn generate_mask()"));
        assert!(injected_decl.starts_with("fn write_frame_with_mask_draw"));
        assert_eq!(
            writer_impl
                .matches("self.write_frame_with_mask_draw(frame, Self::generate_mask)")
                .count(),
            1
        );

        for fallback in [
            "SystemTime",
            "Instant",
            "process::",
            "thread::",
            "Atomic",
            "rand::",
        ] {
            assert!(
                !writer_impl.contains(fallback),
                "WebSocket mask fallback found: {fallback}"
            );
        }
    }
}
