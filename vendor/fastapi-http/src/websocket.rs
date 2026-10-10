//! WebSocket protocol implementation (RFC 6455).
//!
//! This module provides a complete WebSocket implementation built on asupersync's
//! I/O primitives, with no external dependencies for SHA-1 or base64.
//!
//! # Architecture
//!
//! The implementation is layered:
//!
//! 1. **Handshake** — HTTP upgrade negotiation (101 Switching Protocols)
//! 2. **Frame codec** — Binary frame parsing/encoding per RFC 6455 §5
//! 3. **WebSocket** — High-level API matching FastAPI/Starlette semantics
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::websocket::{WebSocket, Message};
//!
//! async fn handler(mut ws: WebSocket) {
//!     ws.accept(None).await.unwrap();
//!     loop {
//!         match ws.receive().await {
//!             Ok(Message::Text(text)) => {
//!                 ws.send_text(&text).await.unwrap();
//!             }
//!             Ok(Message::Close(_, _)) | Err(_) => break,
//!             Ok(Message::Binary(_)) => {}
//!             Ok(Message::Ping(_) | Message::Pong(_)) => unreachable!(),
//!         }
//!     }
//! }
//! ```

use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::TcpStream;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::Poll;

// ============================================================================
// SHA-1 (RFC 3174) — minimal, safe implementation for WebSocket handshake
// ============================================================================

/// SHA-1 digest (20 bytes / 160 bits).
///
/// Standard SHA-1 variable names (a-e, h0-h4, w[]) follow RFC 3174 exactly.
#[allow(clippy::many_single_char_names)]
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    // Pre-processing: pad message
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) block
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (idx, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[idx * 4],
                block[idx * 4 + 1],
                block[idx * 4 + 2],
                block[idx * 4 + 3],
            ]);
        }
        for idx in 16..80 {
            w[idx] = (w[idx - 3] ^ w[idx - 8] ^ w[idx - 14] ^ w[idx - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);

        #[allow(clippy::needless_range_loop)]
        for idx in 0..80 {
            let (f, k) = match idx {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1_u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC_u32),
                _ => (b ^ c ^ d, 0xCA62_C1D6_u32),
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[idx]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut result = [0u8; 20];
    result[0..4].copy_from_slice(&h0.to_be_bytes());
    result[4..8].copy_from_slice(&h1.to_be_bytes());
    result[8..12].copy_from_slice(&h2.to_be_bytes());
    result[12..16].copy_from_slice(&h3.to_be_bytes());
    result[16..20].copy_from_slice(&h4.to_be_bytes());
    result
}

// ============================================================================
// Base64 encoding — minimal implementation for handshake accept key
// ============================================================================

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64-encode bytes to a string.
fn base64_encode(data: &[u8]) -> String {
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(BASE64_CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(BASE64_CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(BASE64_CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(BASE64_CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

// ============================================================================
// WebSocket constants (RFC 6455)
// ============================================================================

/// The WebSocket GUID used in the handshake (RFC 6455 §4.2.2).
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Default maximum frame payload size (16 MiB).
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Default maximum message size (64 MiB, for multi-frame messages).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

// ============================================================================
// Types
// ============================================================================

/// WebSocket frame opcode (RFC 6455 §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// Continuation frame (0x0).
    Continuation,
    /// Text frame (0x1) — payload must be valid UTF-8.
    Text,
    /// Binary frame (0x2).
    Binary,
    /// Connection close (0x8).
    Close,
    /// Ping (0x9).
    Ping,
    /// Pong (0xA).
    Pong,
}

impl Opcode {
    /// Parse an opcode from the low 4 bits of the first frame byte.
    fn from_u8(value: u8) -> Result<Self, WebSocketError> {
        match value & 0x0F {
            0x0 => Ok(Self::Continuation),
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x8 => Ok(Self::Close),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            other => Err(WebSocketError::Protocol(format!(
                "unknown opcode: 0x{other:X}"
            ))),
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Continuation => 0x0,
            Self::Text => 0x1,
            Self::Binary => 0x2,
            Self::Close => 0x8,
            Self::Ping => 0x9,
            Self::Pong => 0xA,
        }
    }

    /// Returns true for control frames (close, ping, pong).
    fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

/// WebSocket close status code (RFC 6455 §7.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    /// Normal closure (1000).
    Normal,
    /// Endpoint going away (1001).
    GoingAway,
    /// Protocol error (1002).
    ProtocolError,
    /// Unsupported data type (1003).
    UnsupportedData,
    /// No status code present (1005) — must not be sent in a frame.
    NoStatusReceived,
    /// Abnormal closure (1006) — must not be sent in a frame.
    AbnormalClosure,
    /// Invalid payload data (1007).
    InvalidPayload,
    /// Policy violation (1008).
    PolicyViolation,
    /// Message too big (1009).
    MessageTooBig,
    /// Missing expected extension (1010).
    MandatoryExtension,
    /// Internal server error (1011).
    InternalError,
    /// Service restart (1012).
    ServiceRestart,
    /// Try again later (1013).
    TryAgainLater,
    /// Bad gateway (1014).
    BadGateway,
    /// Application-defined or registered code in the 3000-4999 range.
    Application(u16),
}

impl CloseCode {
    /// Convert to the 2-byte wire representation.
    pub fn to_u16(self) -> u16 {
        match self {
            Self::Normal => 1000,
            Self::GoingAway => 1001,
            Self::ProtocolError => 1002,
            Self::UnsupportedData => 1003,
            Self::NoStatusReceived => 1005,
            Self::AbnormalClosure => 1006,
            Self::InvalidPayload => 1007,
            Self::PolicyViolation => 1008,
            Self::MessageTooBig => 1009,
            Self::MandatoryExtension => 1010,
            Self::InternalError => 1011,
            Self::ServiceRestart => 1012,
            Self::TryAgainLater => 1013,
            Self::BadGateway => 1014,
            Self::Application(code) => code,
        }
    }

    /// Parse from a 2-byte wire value.
    pub fn from_u16(code: u16) -> Self {
        match code {
            1000 => Self::Normal,
            1001 => Self::GoingAway,
            1002 => Self::ProtocolError,
            1003 => Self::UnsupportedData,
            1005 => Self::NoStatusReceived,
            1006 => Self::AbnormalClosure,
            1007 => Self::InvalidPayload,
            1008 => Self::PolicyViolation,
            1009 => Self::MessageTooBig,
            1010 => Self::MandatoryExtension,
            1011 => Self::InternalError,
            1012 => Self::ServiceRestart,
            1013 => Self::TryAgainLater,
            1014 => Self::BadGateway,
            3000..=4999 => Self::Application(code),
            _ => Self::ProtocolError,
        }
    }
}

impl std::fmt::Display for CloseCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_u16())
    }
}

/// A WebSocket message (assembled from one or more frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// UTF-8 text message.
    Text(String),
    /// Binary message.
    Binary(Vec<u8>),
    /// Ping with optional payload (max 125 bytes).
    Ping(Vec<u8>),
    /// Pong with optional payload (max 125 bytes).
    Pong(Vec<u8>),
    /// Close with optional code and reason.
    Close(Option<CloseCode>, Option<String>),
}

/// A raw WebSocket frame.
#[derive(Debug, Clone)]
struct Frame {
    fin: bool,
    opcode: Opcode,
    payload: Vec<u8>,
}

/// WebSocket error type.
#[derive(Debug)]
pub enum WebSocketError {
    /// I/O error on the underlying stream.
    Io(io::Error),
    /// Protocol violation.
    Protocol(String),
    /// Connection closed.
    ConnectionClosed,
    /// Frame or message exceeds configured size limit.
    MessageTooLarge { size: usize, limit: usize },
    /// Invalid UTF-8 in a text message.
    InvalidUtf8,
    /// Handshake failed.
    HandshakeFailed(String),
}

impl std::fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "WebSocket I/O error: {e}"),
            Self::Protocol(msg) => write!(f, "WebSocket protocol error: {msg}"),
            Self::ConnectionClosed => write!(f, "WebSocket connection closed"),
            Self::MessageTooLarge { size, limit } => {
                write!(
                    f,
                    "WebSocket message too large: {size} bytes (limit: {limit})"
                )
            }
            Self::InvalidUtf8 => write!(f, "WebSocket: invalid UTF-8 in text message"),
            Self::HandshakeFailed(msg) => write!(f, "WebSocket handshake failed: {msg}"),
        }
    }
}

impl std::error::Error for WebSocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for WebSocketError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Configuration for WebSocket connections.
#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    /// Maximum frame payload size in bytes.
    pub max_frame_size: usize,
    /// Maximum message size in bytes (for multi-frame messages).
    pub max_message_size: usize,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }
}

// ============================================================================
// Handshake
// ============================================================================

/// Compute the `Sec-WebSocket-Accept` value from the client's
/// `Sec-WebSocket-Key` header (RFC 6455 §4.2.2 step 4).
pub fn accept_key(client_key: &str) -> String {
    let mut input = String::with_capacity(client_key.len() + WS_GUID.len());
    input.push_str(client_key.trim());
    input.push_str(WS_GUID);
    base64_encode(&sha1(input.as_bytes()))
}

/// Validate that an HTTP request is a valid WebSocket upgrade request.
///
/// Checks (RFC 6455 §4.2.1):
/// - Method is GET
/// - `Upgrade: websocket` header present (case-insensitive)
/// - `Connection: upgrade` header present (case-insensitive)
/// - `Sec-WebSocket-Key` header present and non-empty
/// - `Sec-WebSocket-Version: 13` header present
///
/// Returns the `Sec-WebSocket-Key` value on success.
pub fn validate_upgrade_request(
    method: &str,
    headers: &[(String, Vec<u8>)],
) -> Result<String, WebSocketError> {
    // Must be GET
    if !method.eq_ignore_ascii_case("GET") {
        return Err(WebSocketError::HandshakeFailed(
            "WebSocket upgrade requires GET method".into(),
        ));
    }

    let find_header = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
    };

    // Check Upgrade header
    let upgrade = find_header("upgrade")
        .ok_or_else(|| WebSocketError::HandshakeFailed("missing Upgrade header".into()))?;
    if !upgrade
        .split(',')
        .any(|v| v.trim().eq_ignore_ascii_case("websocket"))
    {
        return Err(WebSocketError::HandshakeFailed(
            "Upgrade header must contain 'websocket'".into(),
        ));
    }

    // Check Connection header
    let connection = find_header("connection")
        .ok_or_else(|| WebSocketError::HandshakeFailed("missing Connection header".into()))?;
    if !connection
        .split(',')
        .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
    {
        return Err(WebSocketError::HandshakeFailed(
            "Connection header must contain 'upgrade'".into(),
        ));
    }

    // Check Sec-WebSocket-Key
    let key = find_header("sec-websocket-key").ok_or_else(|| {
        WebSocketError::HandshakeFailed("missing Sec-WebSocket-Key header".into())
    })?;
    let key = key.trim();
    if key.is_empty() {
        return Err(WebSocketError::HandshakeFailed(
            "Sec-WebSocket-Key must not be empty".into(),
        ));
    }
    if fastapi_core::websocket_accept_from_key(key).is_err() {
        return Err(WebSocketError::HandshakeFailed(
            "invalid Sec-WebSocket-Key (must be valid base64 with 16 decoded bytes)".into(),
        ));
    }

    // Check Sec-WebSocket-Version
    let version = find_header("sec-websocket-version").ok_or_else(|| {
        WebSocketError::HandshakeFailed("missing Sec-WebSocket-Version header".into())
    })?;
    if version.trim() != "13" {
        return Err(WebSocketError::HandshakeFailed(format!(
            "unsupported WebSocket version: {version} (expected 13)"
        )));
    }

    Ok(key.to_string())
}

fn is_valid_subprotocol_token(value: &str) -> bool {
    // RFC 6455 references HTTP token syntax: 1*<any CHAR except CTLs or separators>.
    // Restrict to visible ASCII token chars and reject separators/whitespace.
    const SEPARATORS: &str = "()<>@,;:\\\"/[]?={} \t";
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii() && (0x21..=0x7E).contains(&b) && !SEPARATORS.contains(b as char))
}

/// Build the HTTP 101 Switching Protocols response bytes for a WebSocket upgrade.
///
/// If `subprotocol` is provided, includes `Sec-WebSocket-Protocol` in the response.
pub fn build_accept_response(
    client_key: &str,
    subprotocol: Option<&str>,
) -> Result<Vec<u8>, WebSocketError> {
    let accept = accept_key(client_key);
    let mut response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(proto) = subprotocol {
        if !is_valid_subprotocol_token(proto) {
            return Err(WebSocketError::HandshakeFailed(
                "invalid Sec-WebSocket-Protocol token".into(),
            ));
        }
        response.push_str(&format!("Sec-WebSocket-Protocol: {proto}\r\n"));
    }
    response.push_str("\r\n");
    Ok(response.into_bytes())
}

// ============================================================================
// Frame codec
// ============================================================================

/// Read a single WebSocket frame from the stream.
///
/// Handles variable-length payload encoding, masking (client-to-server),
/// and validates control frame constraints (max 125 bytes, must be FIN).
async fn read_frame(
    stream: &mut TcpStream,
    config: &WebSocketConfig,
) -> Result<Frame, WebSocketError> {
    // Read first 2 bytes: FIN/opcode + mask/payload-len
    let mut header = [0u8; 2];
    read_exact(stream, &mut header).await?;

    let fin = (header[0] & 0x80) != 0;
    let rsv = (header[0] >> 4) & 0x07;
    if rsv != 0 {
        return Err(WebSocketError::Protocol(
            "reserved bits must be 0 (no extensions negotiated)".into(),
        ));
    }

    let opcode = Opcode::from_u8(header[0])?;
    let masked = (header[1] & 0x80) != 0;
    let payload_len_byte = header[1] & 0x7F;

    if !masked {
        return Err(WebSocketError::Protocol(
            "client-to-server frames must be masked".into(),
        ));
    }

    // Determine actual payload length
    let payload_len: usize = match payload_len_byte {
        0..=125 => payload_len_byte as usize,
        126 => {
            let mut len_bytes = [0u8; 2];
            read_exact(stream, &mut len_bytes).await?;
            u16::from_be_bytes(len_bytes) as usize
        }
        _ => {
            // 127 — 8-byte length
            let mut len_bytes = [0u8; 8];
            read_exact(stream, &mut len_bytes).await?;
            let len = u64::from_be_bytes(len_bytes);
            // RFC 6455 §5.2: most significant bit MUST be 0
            if (len >> 63) != 0 {
                return Err(WebSocketError::Protocol(
                    "64-bit frame length has most significant bit set".into(),
                ));
            }
            // Check for overflow and excessive size
            if len > usize::MAX as u64 {
                return Err(WebSocketError::MessageTooLarge {
                    size: usize::MAX,
                    limit: config.max_frame_size,
                });
            }
            len as usize
        }
    };

    // Validate control frame constraints (RFC 6455 §5.5)
    if opcode.is_control() {
        if !fin {
            return Err(WebSocketError::Protocol(
                "control frames must not be fragmented".into(),
            ));
        }
        if payload_len > 125 {
            return Err(WebSocketError::Protocol(
                "control frame payload must not exceed 125 bytes".into(),
            ));
        }
    }

    // Check frame size limit
    if payload_len > config.max_frame_size {
        return Err(WebSocketError::MessageTooLarge {
            size: payload_len,
            limit: config.max_frame_size,
        });
    }

    // Read masking key (if present)
    let mask_key = if masked {
        let mut key = [0u8; 4];
        read_exact(stream, &mut key).await?;
        Some(key)
    } else {
        None
    };

    // Read payload
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        read_exact(stream, &mut payload).await?;
    }

    // Unmask payload (XOR with mask key)
    if let Some(key) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[i % 4];
        }
    }

    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// Write a single WebSocket frame to the stream.
///
/// Server-to-client frames are NOT masked per RFC 6455 §5.1.
async fn write_frame(
    stream: &mut TcpStream,
    fin: bool,
    opcode: Opcode,
    payload: &[u8],
) -> Result<(), WebSocketError> {
    let mut header = Vec::with_capacity(10);

    // First byte: FIN + opcode
    let first_byte = if fin { 0x80 } else { 0x00 } | opcode.to_u8();
    header.push(first_byte);

    // Second byte: mask=0 + payload length
    let len = payload.len();
    if len < 126 {
        header.push(len as u8);
    } else if len <= 0xFFFF {
        header.push(126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }

    // Write header + payload
    ws_write_all(stream, &header).await?;
    if !payload.is_empty() {
        ws_write_all(stream, payload).await?;
    }
    ws_flush(stream).await?;

    Ok(())
}

/// Assemble a complete message from potentially fragmented frames.
///
/// Handles continuation frames and interleaved control frames.
async fn read_message(
    stream: &mut TcpStream,
    config: &WebSocketConfig,
) -> Result<Message, WebSocketError> {
    let mut message_opcode: Option<Opcode> = None;
    let mut message_data: Vec<u8> = Vec::new();

    loop {
        let frame = read_frame(stream, config).await?;

        // Handle control frames (can be interleaved with data frames)
        if frame.opcode.is_control() {
            match frame.opcode {
                Opcode::Close => {
                    let (code, reason) = parse_close_payload(&frame.payload)?;
                    return Ok(Message::Close(code, reason));
                }
                Opcode::Ping => {
                    write_frame(stream, true, Opcode::Pong, &frame.payload).await?;
                    continue;
                }
                Opcode::Pong => continue,
                _ => unreachable!(),
            }
        }

        // Data frame handling
        match frame.opcode {
            Opcode::Continuation if message_opcode.is_none() => {
                return Err(WebSocketError::Protocol(
                    "continuation frame without initial frame".into(),
                ));
            }
            Opcode::Continuation => {}
            Opcode::Text | Opcode::Binary => {
                if message_opcode.is_some() {
                    return Err(WebSocketError::Protocol(
                        "new data frame while previous message is incomplete".into(),
                    ));
                }
                message_opcode = Some(frame.opcode);
            }
            _ => {}
        }

        // Check message size limit
        let new_size = message_data.len() + frame.payload.len();
        if new_size > config.max_message_size {
            return Err(WebSocketError::MessageTooLarge {
                size: new_size,
                limit: config.max_message_size,
            });
        }

        message_data.extend_from_slice(&frame.payload);

        if frame.fin {
            break;
        }
    }

    let opcode = message_opcode
        .ok_or_else(|| WebSocketError::Protocol("empty message (no data frames)".into()))?;

    match opcode {
        Opcode::Text => {
            let text = String::from_utf8(message_data).map_err(|_| WebSocketError::InvalidUtf8)?;
            Ok(Message::Text(text))
        }
        Opcode::Binary => Ok(Message::Binary(message_data)),
        _ => unreachable!(),
    }
}

/// Parse a close frame payload into (code, reason).
fn parse_close_payload(
    payload: &[u8],
) -> Result<(Option<CloseCode>, Option<String>), WebSocketError> {
    if payload.len() < 2 {
        if payload.is_empty() {
            return Ok((None, None));
        }
        return Err(WebSocketError::Protocol(
            "close frame payload must be empty or at least 2 bytes".into(),
        ));
    }
    let code_raw = u16::from_be_bytes([payload[0], payload[1]]);
    if !is_valid_close_code(code_raw) {
        return Err(WebSocketError::Protocol(format!(
            "invalid close code in close frame: {code_raw}"
        )));
    }
    let code = CloseCode::from_u16(code_raw);
    let reason = if payload.len() > 2 {
        Some(
            std::str::from_utf8(&payload[2..])
                .map_err(|_| WebSocketError::Protocol("close reason must be valid UTF-8".into()))?
                .to_string(),
        )
    } else {
        None
    };
    Ok((Some(code), reason))
}

/// Build a close frame payload from code and reason.
fn build_close_payload(code: CloseCode, reason: Option<&str>) -> Result<Vec<u8>, WebSocketError> {
    if !is_valid_close_code(code.to_u16()) {
        return Err(WebSocketError::Protocol(format!(
            "invalid close code for close frame: {}",
            code.to_u16()
        )));
    }
    let mut payload = Vec::with_capacity(2 + reason.map_or(0, str::len));
    payload.extend_from_slice(&code.to_u16().to_be_bytes());
    if let Some(reason_str) = reason {
        // Truncate reason to fit in 125 bytes total
        let max_reason = 123; // 125 - 2 bytes for code
        let mut end = reason_str.len().min(max_reason);
        while end > 0 && !reason_str.is_char_boundary(end) {
            end -= 1;
        }
        payload.extend_from_slice(&reason_str.as_bytes()[..end]);
    }
    Ok(payload)
}

fn is_valid_close_code(code: u16) -> bool {
    matches!(
        code,
        1000 | 1001 | 1002 | 1003 | 1007 | 1008 | 1009 | 1010 | 1011 | 1012 | 1013 | 1014 | 3000
            ..=4999
    )
}

// ============================================================================
// WebSocket — high-level API
// ============================================================================

/// Connection state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsState {
    /// Upgrade request received but not yet accepted.
    Pending,
    /// Handshake complete, frames can be sent/received.
    Open,
    /// Close frame sent, waiting for peer's close response.
    CloseSent,
    /// Connection fully closed.
    Closed,
}

/// A WebSocket connection.
///
/// Wraps a `TcpStream` that has been upgraded from HTTP. Provides a
/// high-level API matching FastAPI/Starlette semantics.
///
/// # Lifecycle
///
/// 1. Created by the server after detecting an upgrade request
/// 2. Handler calls [`accept()`](WebSocket::accept) to complete the handshake
/// 3. Handler sends/receives messages
/// 4. Handler calls [`close()`](WebSocket::close) or the peer closes
///
/// # Example
///
/// ```ignore
/// async fn chat(mut ws: WebSocket) {
///     ws.accept(None).await.unwrap();
///     while let Ok(msg) = ws.receive().await {
///         match msg {
///             Message::Text(text) => {
///                 ws.send_text(&format!("echo: {text}")).await.unwrap();
///             }
///             Message::Binary(_data) => {}
///             // receive() auto-replies to ping and does not surface pong.
///             Message::Ping(_) | Message::Pong(_) => unreachable!(),
///             Message::Close(_, _) => break,
///         }
///     }
///     ws.close(CloseCode::Normal, None).await.ok();
/// }
/// ```
pub struct WebSocket {
    stream: TcpStream,
    state: WsState,
    client_key: String,
    config: WebSocketConfig,
}

impl WebSocket {
    /// Create a new WebSocket from an upgraded TCP stream.
    ///
    /// The `client_key` is the `Sec-WebSocket-Key` header value from the
    /// upgrade request.
    pub fn new(stream: TcpStream, client_key: String) -> Self {
        Self {
            stream,
            state: WsState::Pending,
            client_key,
            config: WebSocketConfig::default(),
        }
    }

    /// Create a new WebSocket with custom configuration.
    pub fn with_config(stream: TcpStream, client_key: String, config: WebSocketConfig) -> Self {
        Self {
            stream,
            state: WsState::Pending,
            client_key,
            config,
        }
    }

    /// Complete the WebSocket handshake by sending the 101 response.
    ///
    /// Optionally specify a subprotocol to include in the response.
    ///
    /// # Errors
    ///
    /// Returns an error if the handshake has already been completed or
    /// if writing the response fails.
    pub async fn accept(&mut self, subprotocol: Option<&str>) -> Result<(), WebSocketError> {
        if self.state != WsState::Pending {
            return Err(WebSocketError::Protocol(
                "accept() called on non-pending WebSocket".into(),
            ));
        }

        let response_bytes = build_accept_response(&self.client_key, subprotocol)?;
        ws_write_all(&mut self.stream, &response_bytes).await?;
        ws_flush(&mut self.stream).await?;
        self.state = WsState::Open;
        Ok(())
    }

    /// Receive the next message from the client.
    ///
    /// Automatically responds to ping frames with pong. Returns text,
    /// binary, and close messages to the caller.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection is closed or a protocol
    /// violation occurs.
    pub async fn receive(&mut self) -> Result<Message, WebSocketError> {
        self.ensure_can_receive()?;
        let msg = read_message(&mut self.stream, &self.config).await?;
        match msg {
            Message::Close(code, reason) => {
                // If we haven't sent close yet, echo it back
                if self.state == WsState::Open {
                    let payload = match code {
                        Some(close_code) => build_close_payload(close_code, reason.as_deref())?,
                        None => Vec::new(),
                    };
                    write_frame(&mut self.stream, true, Opcode::Close, &payload)
                        .await
                        .ok(); // Best-effort
                }
                self.state = WsState::Closed;
                Ok(Message::Close(code, reason))
            }
            _ => Ok(msg),
        }
    }

    /// Send a text message.
    pub async fn send_text(&mut self, text: &str) -> Result<(), WebSocketError> {
        self.ensure_open()?;
        write_frame(&mut self.stream, true, Opcode::Text, text.as_bytes()).await
    }

    /// Send a binary message.
    pub async fn send_bytes(&mut self, data: &[u8]) -> Result<(), WebSocketError> {
        self.ensure_open()?;
        write_frame(&mut self.stream, true, Opcode::Binary, data).await
    }

    /// Receive a text message.
    ///
    /// Skips pong messages, auto-responds to pings. Returns an error
    /// if a binary or close message is received.
    pub async fn receive_text(&mut self) -> Result<String, WebSocketError> {
        match self.receive().await? {
            Message::Text(text) => Ok(text),
            Message::Close(code, reason) => Err(WebSocketError::Protocol(format!(
                "expected text, got close (code={code:?}, reason={reason:?})"
            ))),
            other => Err(WebSocketError::Protocol(format!(
                "expected text message, got {other:?}"
            ))),
        }
    }

    /// Receive a binary message.
    ///
    /// Skips pong messages, auto-responds to pings. Returns an error
    /// if a text or close message is received.
    pub async fn receive_bytes(&mut self) -> Result<Vec<u8>, WebSocketError> {
        match self.receive().await? {
            Message::Binary(data) => Ok(data),
            Message::Close(code, reason) => Err(WebSocketError::Protocol(format!(
                "expected binary, got close (code={code:?}, reason={reason:?})"
            ))),
            other => Err(WebSocketError::Protocol(format!(
                "expected binary message, got {other:?}"
            ))),
        }
    }

    /// Send a ping frame with optional payload.
    pub async fn ping(&mut self, data: &[u8]) -> Result<(), WebSocketError> {
        self.ensure_open()?;
        if data.len() > 125 {
            return Err(WebSocketError::Protocol(
                "ping payload must not exceed 125 bytes".into(),
            ));
        }
        write_frame(&mut self.stream, true, Opcode::Ping, data).await
    }

    /// Send a pong frame with optional payload.
    pub async fn pong(&mut self, data: &[u8]) -> Result<(), WebSocketError> {
        self.ensure_open()?;
        if data.len() > 125 {
            return Err(WebSocketError::Protocol(
                "pong payload must not exceed 125 bytes".into(),
            ));
        }
        write_frame(&mut self.stream, true, Opcode::Pong, data).await
    }

    /// Initiate a close handshake.
    ///
    /// Sends a close frame and transitions to `CloseSent`. The peer should
    /// respond with its own close frame.
    pub async fn close(
        &mut self,
        code: CloseCode,
        reason: Option<&str>,
    ) -> Result<(), WebSocketError> {
        if self.state == WsState::Closed || self.state == WsState::CloseSent {
            return Ok(());
        }
        if self.state == WsState::Pending {
            self.state = WsState::Closed;
            return Ok(());
        }

        let payload = build_close_payload(code, reason)?;
        write_frame(&mut self.stream, true, Opcode::Close, &payload).await?;
        self.state = WsState::CloseSent;
        Ok(())
    }

    /// Returns `true` if the connection is open and can send/receive messages.
    pub fn is_open(&self) -> bool {
        self.state == WsState::Open
    }

    /// Returns the current connection state.
    pub fn state(&self) -> &'static str {
        match self.state {
            WsState::Pending => "pending",
            WsState::Open => "open",
            WsState::CloseSent => "close_sent",
            WsState::Closed => "closed",
        }
    }

    fn ensure_open(&self) -> Result<(), WebSocketError> {
        match self.state {
            WsState::Open => Ok(()),
            WsState::Pending => Err(WebSocketError::Protocol(
                "must call accept() before sending/receiving".into(),
            )),
            WsState::CloseSent | WsState::Closed => Err(WebSocketError::ConnectionClosed),
        }
    }

    fn ensure_can_receive(&self) -> Result<(), WebSocketError> {
        match self.state {
            WsState::Open | WsState::CloseSent => Ok(()),
            WsState::Pending => Err(WebSocketError::Protocol(
                "must call accept() before sending/receiving".into(),
            )),
            WsState::Closed => Err(WebSocketError::ConnectionClosed),
        }
    }
}

// ============================================================================
// I/O helpers (using asupersync primitives)
// ============================================================================

/// Read exactly `buf.len()` bytes from the stream.
async fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), WebSocketError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = ws_read(stream, &mut buf[offset..]).await?;
        if n == 0 {
            return Err(WebSocketError::ConnectionClosed);
        }
        offset += n;
    }
    Ok(())
}

/// Read some bytes from the stream.
async fn ws_read(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, WebSocketError> {
    poll_fn(|cx| {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut *stream).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(WebSocketError::Io(e))),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Write all bytes to the stream.
async fn ws_write_all(stream: &mut TcpStream, mut buf: &[u8]) -> Result<(), WebSocketError> {
    while !buf.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *stream).poll_write(cx, buf))
            .await
            .map_err(WebSocketError::Io)?;
        if n == 0 {
            return Err(WebSocketError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write to WebSocket stream",
            )));
        }
        buf = &buf[n..];
    }
    Ok(())
}

/// Flush the stream.
async fn ws_flush(stream: &mut TcpStream) -> Result<(), WebSocketError> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_flush(cx))
        .await
        .map_err(WebSocketError::Io)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha1_empty() {
        let result = sha1(b"");
        let expected: [u8; 20] = [
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_sha1_abc() {
        let result = sha1(b"abc");
        let expected: [u8; 20] = [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_accept_key() {
        // RFC 6455 §4.2.2 example
        let key = accept_key("dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(key, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn test_close_code_roundtrip() {
        let codes = [
            CloseCode::Normal,
            CloseCode::GoingAway,
            CloseCode::ProtocolError,
            CloseCode::UnsupportedData,
            CloseCode::InvalidPayload,
            CloseCode::PolicyViolation,
            CloseCode::MessageTooBig,
            CloseCode::MandatoryExtension,
            CloseCode::InternalError,
            CloseCode::ServiceRestart,
            CloseCode::TryAgainLater,
            CloseCode::BadGateway,
            CloseCode::Application(3000),
            CloseCode::Application(4000),
            CloseCode::Application(4999),
        ];
        for code in codes {
            assert_eq!(CloseCode::from_u16(code.to_u16()), code);
        }
    }

    #[test]
    fn test_opcode_roundtrip() {
        let opcodes = [
            Opcode::Continuation,
            Opcode::Text,
            Opcode::Binary,
            Opcode::Close,
            Opcode::Ping,
            Opcode::Pong,
        ];
        for op in opcodes {
            assert_eq!(Opcode::from_u8(op.to_u8()).unwrap(), op);
        }
    }

    #[test]
    fn test_opcode_unknown() {
        assert!(Opcode::from_u8(0x03).is_err());
        assert!(Opcode::from_u8(0x07).is_err());
    }

    #[test]
    fn test_opcode_is_control() {
        assert!(!Opcode::Continuation.is_control());
        assert!(!Opcode::Text.is_control());
        assert!(!Opcode::Binary.is_control());
        assert!(Opcode::Close.is_control());
        assert!(Opcode::Ping.is_control());
        assert!(Opcode::Pong.is_control());
    }

    #[test]
    fn test_build_accept_response_basic() {
        let resp = build_accept_response("dGhlIHNhbXBsZSBub25jZQ==", None)
            .expect("response build should succeed");
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(resp_str.contains("Upgrade: websocket\r\n"));
        assert!(resp_str.contains("Connection: Upgrade\r\n"));
        assert!(resp_str.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(resp_str.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_build_accept_response_with_subprotocol() {
        let resp = build_accept_response("dGhlIHNhbXBsZSBub25jZQ==", Some("graphql-ws"))
            .expect("response build should succeed");
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("Sec-WebSocket-Protocol: graphql-ws\r\n"));
    }

    #[test]
    fn test_build_accept_response_rejects_invalid_subprotocol_token() {
        let err =
            build_accept_response("dGhlIHNhbXBsZSBub25jZQ==", Some("graphql-ws\r\nX-Evil: 1"))
                .expect_err("invalid subprotocol token must fail");
        assert!(matches!(err, WebSocketError::HandshakeFailed(_)));
        assert!(
            err.to_string()
                .contains("invalid Sec-WebSocket-Protocol token")
        );
    }

    #[test]
    fn test_validate_upgrade_request_valid() {
        let headers = vec![
            ("Upgrade".into(), b"websocket".to_vec()),
            ("Connection".into(), b"upgrade".to_vec()),
            (
                "Sec-WebSocket-Key".into(),
                b"dGhlIHNhbXBsZSBub25jZQ==".to_vec(),
            ),
            ("Sec-WebSocket-Version".into(), b"13".to_vec()),
        ];
        let result = validate_upgrade_request("GET", &headers);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "dGhlIHNhbXBsZSBub25jZQ==");
    }

    #[test]
    fn test_validate_upgrade_request_wrong_method() {
        let headers = vec![
            ("Upgrade".into(), b"websocket".to_vec()),
            ("Connection".into(), b"upgrade".to_vec()),
            (
                "Sec-WebSocket-Key".into(),
                b"dGhlIHNhbXBsZSBub25jZQ==".to_vec(),
            ),
            ("Sec-WebSocket-Version".into(), b"13".to_vec()),
        ];
        assert!(validate_upgrade_request("POST", &headers).is_err());
    }

    #[test]
    fn test_validate_upgrade_request_missing_upgrade() {
        let headers = vec![
            ("Connection".into(), b"upgrade".to_vec()),
            (
                "Sec-WebSocket-Key".into(),
                b"dGhlIHNhbXBsZSBub25jZQ==".to_vec(),
            ),
            ("Sec-WebSocket-Version".into(), b"13".to_vec()),
        ];
        assert!(validate_upgrade_request("GET", &headers).is_err());
    }

    #[test]
    fn test_validate_upgrade_request_wrong_version() {
        let headers = vec![
            ("Upgrade".into(), b"websocket".to_vec()),
            ("Connection".into(), b"upgrade".to_vec()),
            (
                "Sec-WebSocket-Key".into(),
                b"dGhlIHNhbXBsZSBub25jZQ==".to_vec(),
            ),
            ("Sec-WebSocket-Version".into(), b"8".to_vec()),
        ];
        assert!(validate_upgrade_request("GET", &headers).is_err());
    }

    #[test]
    fn test_validate_upgrade_request_invalid_key_base64() {
        let headers = vec![
            ("Upgrade".into(), b"websocket".to_vec()),
            ("Connection".into(), b"upgrade".to_vec()),
            ("Sec-WebSocket-Key".into(), b"not-base64".to_vec()),
            ("Sec-WebSocket-Version".into(), b"13".to_vec()),
        ];
        assert!(validate_upgrade_request("GET", &headers).is_err());
    }

    #[test]
    fn test_validate_upgrade_request_invalid_key_length() {
        let headers = vec![
            ("Upgrade".into(), b"websocket".to_vec()),
            ("Connection".into(), b"upgrade".to_vec()),
            ("Sec-WebSocket-Key".into(), b"Zm9v".to_vec()),
            ("Sec-WebSocket-Version".into(), b"13".to_vec()),
        ];
        assert!(validate_upgrade_request("GET", &headers).is_err());
    }

    #[test]
    fn test_close_payload_roundtrip() {
        let payload = build_close_payload(CloseCode::Normal, Some("goodbye")).unwrap();
        let (code, reason) = parse_close_payload(&payload).unwrap();
        assert_eq!(code, Some(CloseCode::Normal));
        assert_eq!(reason, Some("goodbye".into()));
    }

    #[test]
    fn test_close_payload_no_reason() {
        let payload = build_close_payload(CloseCode::GoingAway, None).unwrap();
        let (code, reason) = parse_close_payload(&payload).unwrap();
        assert_eq!(code, Some(CloseCode::GoingAway));
        assert_eq!(reason, None);
    }

    #[test]
    fn test_close_payload_empty() {
        let (code, reason) = parse_close_payload(&[]).unwrap();
        assert_eq!(code, None);
        assert_eq!(reason, None);
    }

    #[test]
    fn test_close_payload_len_one_is_invalid() {
        let err = parse_close_payload(&[0x03]).expect_err("len=1 close payload must fail");
        assert!(matches!(err, WebSocketError::Protocol(_)));
    }

    #[test]
    fn test_close_payload_invalid_code_is_rejected() {
        let err = parse_close_payload(&[0x03, 0xEE]).expect_err("1006 must be rejected");
        assert!(matches!(err, WebSocketError::Protocol(_)));
    }

    #[test]
    fn test_build_close_payload_rejects_unsendable_code() {
        let err = build_close_payload(CloseCode::NoStatusReceived, None)
            .expect_err("1005 must not be sent");
        assert!(matches!(err, WebSocketError::Protocol(_)));
    }

    #[test]
    fn test_build_close_payload_truncates_on_utf8_boundary() {
        let reason = "é".repeat(100); // 200 bytes UTF-8.
        let payload = build_close_payload(CloseCode::Normal, Some(&reason)).unwrap();
        assert!(payload.len() <= 125);
        let parsed =
            std::str::from_utf8(&payload[2..]).expect("reason bytes must stay valid UTF-8");
        assert!(!parsed.is_empty());
    }

    #[test]
    fn test_message_equality() {
        assert_eq!(Message::Text("hello".into()), Message::Text("hello".into()));
        assert_eq!(
            Message::Binary(vec![1, 2, 3]),
            Message::Binary(vec![1, 2, 3])
        );
        assert_ne!(
            Message::Text("hello".into()),
            Message::Binary(b"hello".to_vec())
        );
    }

    #[test]
    fn test_websocket_config_default() {
        let config = WebSocketConfig::default();
        assert_eq!(config.max_frame_size, DEFAULT_MAX_FRAME_SIZE);
        assert_eq!(config.max_message_size, DEFAULT_MAX_MESSAGE_SIZE);
    }
}
