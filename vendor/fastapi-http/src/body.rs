//! HTTP request body handling.
//!
//! This module provides body reading with support for:
//! - Content-Length based reading with size limits
//! - Chunked transfer encoding parsing
//! - Streaming API for large bodies
//! - Integration points for async I/O
//!
//! # Body Size Limits
//!
//! By default, bodies are limited to 1MB. This prevents denial-of-service
//! attacks via large payloads. The limit is configurable per-request.
//!
//! # Streaming
//!
//! Large bodies can be read incrementally via the body reader API,
//! which supports async I/O integration with checkpoints.
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::body::{BodyConfig, ContentLengthReader};
//!
//! let config = BodyConfig::default().with_max_size(1024 * 1024);
//! let mut reader = ContentLengthReader::new(body_bytes, 100, &config)?;
//!
//! let body = reader.read_all()?;
//! ```

use crate::parser::{BodyLength, ParseError};

/// Default maximum body size (1MB).
pub const DEFAULT_MAX_BODY_SIZE: usize = 1024 * 1024;

/// Configuration for body reading.
#[derive(Debug, Clone)]
pub struct BodyConfig {
    /// Maximum body size in bytes.
    max_size: usize,
    /// Initial buffer capacity for streaming.
    initial_capacity: usize,
}

impl Default for BodyConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_BODY_SIZE,
            initial_capacity: 4096,
        }
    }
}

impl BodyConfig {
    /// Create a new body configuration with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum body size.
    #[must_use]
    pub fn with_max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Set the initial buffer capacity.
    #[must_use]
    pub fn with_initial_capacity(mut self, capacity: usize) -> Self {
        self.initial_capacity = capacity;
        self
    }

    /// Returns the maximum body size.
    #[must_use]
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Returns the initial buffer capacity.
    #[must_use]
    pub fn initial_capacity(&self) -> usize {
        self.initial_capacity
    }
}

/// Error types for body reading.
#[derive(Debug)]
pub enum BodyError {
    /// Body exceeds maximum allowed size.
    TooLarge {
        /// The declared or actual size.
        size: usize,
        /// The maximum allowed size.
        max: usize,
    },
    /// Invalid chunked encoding.
    InvalidChunkedEncoding {
        /// Description of the error.
        detail: &'static str,
    },
    /// Incomplete body (need more data).
    Incomplete {
        /// Bytes received so far.
        received: usize,
        /// Expected total size (if known).
        expected: Option<usize>,
    },
    /// Unexpected end of input.
    UnexpectedEof,
    /// Parse error from underlying parser.
    Parse(ParseError),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { size, max } => {
                write!(f, "body too large: {size} bytes exceeds limit of {max}")
            }
            Self::InvalidChunkedEncoding { detail } => {
                write!(f, "invalid chunked encoding: {detail}")
            }
            Self::Incomplete { received, expected } => {
                if let Some(exp) = expected {
                    write!(f, "incomplete body: received {received} of {exp} bytes")
                } else {
                    write!(f, "incomplete body: received {received} bytes")
                }
            }
            Self::UnexpectedEof => write!(f, "unexpected end of body"),
            Self::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

impl std::error::Error for BodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ParseError> for BodyError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

// ============================================================================
// Content-Length Body Reading
// ============================================================================

/// Reads a body with a known Content-Length.
///
/// This reader validates that exactly `length` bytes are provided and
/// enforces the configured size limit.
#[derive(Debug)]
pub struct ContentLengthReader<'a> {
    buffer: &'a [u8],
    length: usize,
    position: usize,
    // Stored for potential future use (streaming chunk size configuration)
    #[allow(dead_code)]
    config: BodyConfig,
}

impl<'a> ContentLengthReader<'a> {
    /// Create a new Content-Length reader.
    ///
    /// # Arguments
    ///
    /// * `buffer` - The buffer containing body bytes
    /// * `length` - The Content-Length value
    /// * `config` - Body reading configuration
    ///
    /// # Errors
    ///
    /// Returns `BodyError::TooLarge` if `length` exceeds the configured maximum.
    pub fn new(buffer: &'a [u8], length: usize, config: &BodyConfig) -> Result<Self, BodyError> {
        // Check size limit before reading
        if length > config.max_size {
            return Err(BodyError::TooLarge {
                size: length,
                max: config.max_size,
            });
        }

        Ok(Self {
            buffer,
            length,
            position: 0,
            config: config.clone(),
        })
    }

    /// Returns the expected body length.
    #[must_use]
    pub fn length(&self) -> usize {
        self.length
    }

    /// Returns the number of bytes remaining.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.length.saturating_sub(self.position)
    }

    /// Returns true if all bytes have been read.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.position >= self.length
    }

    /// Read up to `max_bytes` into the provided buffer.
    ///
    /// Returns the number of bytes read.
    pub fn read(&mut self, dest: &mut [u8]) -> Result<usize, BodyError> {
        if self.is_complete() {
            return Ok(0);
        }

        let available = self.buffer.len().saturating_sub(self.position);
        let to_read = dest.len().min(self.remaining()).min(available);

        if to_read == 0 && !self.is_complete() {
            return Err(BodyError::Incomplete {
                received: self.position,
                expected: Some(self.length),
            });
        }

        dest[..to_read].copy_from_slice(&self.buffer[self.position..self.position + to_read]);
        self.position += to_read;

        Ok(to_read)
    }

    /// Read all remaining body bytes.
    ///
    /// # Errors
    ///
    /// Returns `BodyError::Incomplete` if the buffer doesn't contain enough data.
    pub fn read_all(&mut self) -> Result<Vec<u8>, BodyError> {
        if self.buffer.len() < self.length {
            return Err(BodyError::Incomplete {
                received: self.buffer.len(),
                expected: Some(self.length),
            });
        }

        let body = self.buffer[..self.length].to_vec();
        self.position = self.length;
        Ok(body)
    }

    /// Read all remaining body bytes as a borrowed slice.
    ///
    /// This is zero-copy when the entire body is already in the buffer.
    ///
    /// # Errors
    ///
    /// Returns `BodyError::Incomplete` if the buffer doesn't contain enough data.
    pub fn read_all_borrowed(&self) -> Result<&'a [u8], BodyError> {
        if self.buffer.len() < self.length {
            return Err(BodyError::Incomplete {
                received: self.buffer.len(),
                expected: Some(self.length),
            });
        }

        Ok(&self.buffer[..self.length])
    }
}

// ============================================================================
// Chunked Transfer Encoding
// ============================================================================

/// State machine for chunked encoding parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkedState {
    /// Expecting chunk size line.
    ChunkSize,
    /// Reading chunk data.
    ChunkData { remaining: usize },
    /// Expecting CRLF after chunk data.
    ChunkDataEnd,
    /// Reading trailers (after final chunk).
    Trailers,
    /// Complete.
    Complete,
}

/// Parses chunked transfer encoding.
///
/// Chunked encoding format:
/// ```text
/// chunk-size CRLF
/// chunk-data CRLF
/// ...
/// 0 CRLF
/// [trailers] CRLF
/// ```
#[derive(Debug)]
pub struct ChunkedReader<'a> {
    buffer: &'a [u8],
    position: usize,
    state: ChunkedState,
    total_size: usize,
    config: BodyConfig,
}

impl<'a> ChunkedReader<'a> {
    /// Create a new chunked reader.
    ///
    /// # Arguments
    ///
    /// * `buffer` - The buffer containing chunked body data
    /// * `config` - Body reading configuration
    #[must_use]
    pub fn new(buffer: &'a [u8], config: &BodyConfig) -> Self {
        Self {
            buffer,
            position: 0,
            state: ChunkedState::ChunkSize,
            total_size: 0,
            config: config.clone(),
        }
    }

    /// Returns true if parsing is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.state == ChunkedState::Complete
    }

    /// Returns the total decoded body size so far.
    #[must_use]
    pub fn total_size(&self) -> usize {
        self.total_size
    }

    /// Parse the chunk size line and return the size.
    fn parse_chunk_size(&self) -> Result<(usize, usize), BodyError> {
        let remaining = &self.buffer[self.position..];

        // Find CRLF
        let line_end =
            remaining
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or(BodyError::Incomplete {
                    received: self.position,
                    expected: None,
                })?;

        let size_line = &remaining[..line_end];

        // Parse hex size (ignore chunk extensions after semicolon)
        let size_str = if let Some(semi) = size_line.iter().position(|&b| b == b';') {
            &size_line[..semi]
        } else {
            size_line
        };

        let size_str =
            std::str::from_utf8(size_str).map_err(|_| BodyError::InvalidChunkedEncoding {
                detail: "invalid UTF-8 in chunk size",
            })?;

        let size = usize::from_str_radix(size_str.trim(), 16).map_err(|_| {
            BodyError::InvalidChunkedEncoding {
                detail: "invalid hex chunk size",
            }
        })?;

        // Reject unreasonably large chunk sizes early to prevent DoS.
        // Individual chunks over 16MB are almost certainly attacks.
        const MAX_SINGLE_CHUNK: usize = 16 * 1024 * 1024;
        if size > MAX_SINGLE_CHUNK {
            return Err(BodyError::InvalidChunkedEncoding {
                detail: "chunk size exceeds 16MB limit",
            });
        }

        // bytes_consumed = size_line + CRLF
        Ok((size, line_end + 2))
    }

    /// Decode all chunks into a single buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The total size exceeds the configured limit
    /// - The chunked encoding is malformed
    /// - The buffer is incomplete
    pub fn decode_all(&mut self) -> Result<Vec<u8>, BodyError> {
        let mut output = Vec::with_capacity(self.config.initial_capacity);

        loop {
            match self.state {
                ChunkedState::ChunkSize => {
                    let (size, consumed) = self.parse_chunk_size()?;
                    self.position += consumed;

                    // Check size limit
                    let new_total = self.total_size.saturating_add(size);
                    if new_total > self.config.max_size {
                        return Err(BodyError::TooLarge {
                            size: new_total,
                            max: self.config.max_size,
                        });
                    }

                    if size == 0 {
                        // Final chunk - transition to trailers
                        self.state = ChunkedState::Trailers;
                    } else {
                        self.state = ChunkedState::ChunkData { remaining: size };
                    }
                }
                ChunkedState::ChunkData { remaining } => {
                    let available = self.buffer.len().saturating_sub(self.position);
                    if available < remaining {
                        return Err(BodyError::Incomplete {
                            received: self.total_size + (remaining - available),
                            expected: None,
                        });
                    }

                    // Copy chunk data
                    let chunk_data = &self.buffer[self.position..self.position + remaining];
                    output.extend_from_slice(chunk_data);
                    self.position += remaining;
                    self.total_size += remaining;

                    self.state = ChunkedState::ChunkDataEnd;
                }
                ChunkedState::ChunkDataEnd => {
                    // Expect CRLF
                    let remaining = &self.buffer[self.position..];
                    if remaining.len() < 2 {
                        return Err(BodyError::Incomplete {
                            received: self.total_size,
                            expected: None,
                        });
                    }

                    if &remaining[..2] != b"\r\n" {
                        return Err(BodyError::InvalidChunkedEncoding {
                            detail: "expected CRLF after chunk data",
                        });
                    }

                    self.position += 2;
                    self.state = ChunkedState::ChunkSize;
                }
                ChunkedState::Trailers => {
                    // Skip trailers until empty line
                    let remaining = &self.buffer[self.position..];

                    // Look for CRLF (empty line) or trailer headers
                    if remaining.starts_with(b"\r\n") {
                        self.position += 2;
                        self.state = ChunkedState::Complete;
                    } else {
                        // Find end of trailer line
                        let line_end = remaining.windows(2).position(|w| w == b"\r\n");
                        match line_end {
                            Some(pos) => {
                                // Skip this trailer
                                self.position += pos + 2;
                                // Stay in Trailers state to handle more trailers or final CRLF
                            }
                            None => {
                                return Err(BodyError::Incomplete {
                                    received: self.total_size,
                                    expected: None,
                                });
                            }
                        }
                    }
                }
                ChunkedState::Complete => {
                    break;
                }
            }
        }

        Ok(output)
    }

    /// Returns the number of bytes consumed from the raw buffer.
    #[must_use]
    pub fn bytes_consumed(&self) -> usize {
        self.position
    }
}

// ============================================================================
// Body Parsing from Headers
// ============================================================================

/// Parse a request body from a buffer given the body length indicator.
///
/// This is the main entry point for body parsing. It dispatches to the
/// appropriate reader based on Content-Length or Transfer-Encoding.
///
/// # Arguments
///
/// * `buffer` - The buffer containing the body (after headers)
/// * `body_length` - The body length indicator from header parsing
/// * `config` - Body reading configuration
///
/// # Returns
///
/// Returns the parsed body bytes, or `None` if no body is expected.
///
/// # Errors
///
/// Returns an error if:
/// - The body exceeds the configured size limit
/// - The chunked encoding is malformed
/// - The buffer is incomplete
pub fn parse_body(
    buffer: &[u8],
    body_length: BodyLength,
    config: &BodyConfig,
) -> Result<Option<Vec<u8>>, BodyError> {
    let (body, _) = parse_body_with_consumed(buffer, body_length, config)?;
    Ok(body)
}

/// Parse a request body and return both the decoded body and bytes consumed.
///
/// This is useful for incremental parsing to determine request boundaries.
///
/// # Errors
///
/// Returns an error if:
/// - The body exceeds the configured size limit
/// - The chunked encoding is malformed
/// - The buffer is incomplete
pub fn parse_body_with_consumed(
    buffer: &[u8],
    body_length: BodyLength,
    config: &BodyConfig,
) -> Result<(Option<Vec<u8>>, usize), BodyError> {
    match body_length {
        BodyLength::None => Ok((None, 0)),
        BodyLength::ContentLength(len) => {
            if len == 0 {
                return Ok((Some(Vec::new()), 0));
            }
            let mut reader = ContentLengthReader::new(buffer, len, config)?;
            let body = reader.read_all()?;
            Ok((Some(body), len))
        }
        BodyLength::Chunked => {
            let mut reader = ChunkedReader::new(buffer, config);
            let body = reader.decode_all()?;
            Ok((Some(body), reader.bytes_consumed()))
        }
        BodyLength::Conflicting => Err(BodyError::InvalidChunkedEncoding {
            detail: "conflicting body length indicators",
        }),
    }
}

/// Validates body size against Content-Length header before reading.
///
/// Call this early (before buffering) to reject oversized requests.
///
/// # Arguments
///
/// * `content_length` - The Content-Length header value
/// * `config` - Body reading configuration
///
/// # Errors
///
/// Returns `BodyError::TooLarge` if the content length exceeds the limit.
pub fn validate_content_length(
    content_length: usize,
    config: &BodyConfig,
) -> Result<(), BodyError> {
    if content_length > config.max_size {
        return Err(BodyError::TooLarge {
            size: content_length,
            max: config.max_size,
        });
    }
    Ok(())
}

// ============================================================================
// Async Streaming Body Readers
// ============================================================================

use asupersync::io::AsyncRead;
use asupersync::stream::Stream;
use fastapi_core::RequestBodyStreamError;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Default threshold for enabling streaming (64KB).
///
/// Bodies larger than this will be streamed rather than buffered entirely.
pub const DEFAULT_STREAMING_THRESHOLD: usize = 64 * 1024;

/// Configuration for async body streaming.
#[derive(Debug, Clone)]
pub struct StreamingBodyConfig {
    /// Threshold above which bodies are streamed.
    pub streaming_threshold: usize,
    /// Size of each read chunk.
    pub chunk_size: usize,
    /// Maximum body size (enforced during streaming).
    pub max_size: usize,
}

impl Default for StreamingBodyConfig {
    fn default() -> Self {
        Self {
            streaming_threshold: DEFAULT_STREAMING_THRESHOLD,
            chunk_size: 8 * 1024, // 8KB chunks
            max_size: DEFAULT_MAX_BODY_SIZE,
        }
    }
}

impl StreamingBodyConfig {
    /// Create a new streaming config with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the streaming threshold.
    #[must_use]
    pub fn with_streaming_threshold(mut self, threshold: usize) -> Self {
        self.streaming_threshold = threshold;
        self
    }

    /// Set the chunk size for reads.
    ///
    /// Note: For network efficiency, values below 1KB are allowed but not recommended
    /// for production use.
    #[must_use]
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1); // Minimum 1 byte (for testing)
        self
    }

    /// Set the maximum body size.
    #[must_use]
    pub fn with_max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Returns true if the given content length should be streamed.
    #[must_use]
    pub fn should_stream(&self, content_length: usize) -> bool {
        content_length > self.streaming_threshold
    }
}

/// An async stream that reads a Content-Length body in chunks.
///
/// This stream first yields any buffered data from the parser, then
/// continues reading from the underlying async reader until the
/// expected length is reached.
///
/// # Memory Efficiency
///
/// Only one chunk is buffered at a time, making this suitable for
/// streaming large request bodies without excessive memory usage.
pub struct AsyncContentLengthStream<R> {
    /// Optional reader for more data (None after initial buffer exhausted and no reader).
    reader: Option<R>,
    /// Initial buffer from parser.
    initial_buffer: Vec<u8>,
    /// Position in initial buffer.
    initial_position: usize,
    /// Expected total size from Content-Length.
    expected_size: usize,
    /// Bytes read so far.
    bytes_read: usize,
    /// Chunk size for reads.
    chunk_size: usize,
    /// Maximum allowed size.
    max_size: usize,
    /// Read buffer (reused across reads).
    read_buffer: Vec<u8>,
    /// Whether the stream is complete.
    complete: bool,
    /// Whether an error occurred.
    error: bool,
}

impl<R> AsyncContentLengthStream<R>
where
    R: AsyncRead + Unpin + Send + Sync + 'static,
{
    /// Create a new Content-Length stream.
    ///
    /// # Arguments
    ///
    /// * `initial_buffer` - Any bytes already buffered by the parser
    /// * `reader` - The async reader for remaining bytes
    /// * `content_length` - Expected total body size
    /// * `config` - Streaming configuration
    pub fn new(
        initial_buffer: Vec<u8>,
        reader: R,
        content_length: usize,
        config: &StreamingBodyConfig,
    ) -> Self {
        Self {
            reader: Some(reader),
            initial_buffer,
            initial_position: 0,
            expected_size: content_length,
            bytes_read: 0,
            chunk_size: config.chunk_size,
            max_size: config.max_size,
            read_buffer: vec![0u8; config.chunk_size],
            complete: false,
            error: false,
        }
    }

    /// Create a Content-Length stream with default config.
    pub fn with_defaults(initial_buffer: Vec<u8>, reader: R, content_length: usize) -> Self {
        Self::new(
            initial_buffer,
            reader,
            content_length,
            &StreamingBodyConfig::default(),
        )
    }

    /// Returns the expected total size.
    #[must_use]
    pub fn expected_size(&self) -> usize {
        self.expected_size
    }

    /// Returns the number of bytes read so far.
    #[must_use]
    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }

    /// Returns the remaining bytes to read.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.expected_size.saturating_sub(self.bytes_read)
    }

    /// Returns true if the stream is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    fn initial_remaining(&self) -> usize {
        self.initial_buffer
            .len()
            .saturating_sub(self.initial_position)
    }
}

impl<R> Stream for AsyncContentLengthStream<R>
where
    R: AsyncRead + Unpin + Send + Sync + 'static,
{
    type Item = Result<Vec<u8>, RequestBodyStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Check if complete or error
        if self.complete || self.error {
            return Poll::Ready(None);
        }

        // Check size limit
        if self.bytes_read > self.max_size {
            self.error = true;
            let bytes_read = self.bytes_read;
            let max_size = self.max_size;
            return Poll::Ready(Some(Err(RequestBodyStreamError::TooLarge {
                received: bytes_read,
                max: max_size,
            })));
        }

        // Check if we've read all expected bytes
        if self.bytes_read >= self.expected_size {
            self.complete = true;
            return Poll::Ready(None);
        }

        let remaining_for_body = self.expected_size.saturating_sub(self.bytes_read);
        let remaining_budget = self.max_size.saturating_sub(self.bytes_read);
        if remaining_for_body > 0 && remaining_budget == 0 {
            self.error = true;
            return Poll::Ready(Some(Err(RequestBodyStreamError::TooLarge {
                received: self.bytes_read.saturating_add(1),
                max: self.max_size,
            })));
        }

        // First, try to yield from initial buffer
        let initial_remaining = self.initial_remaining();
        if initial_remaining > 0 {
            let chunk_size = self
                .chunk_size
                .min(initial_remaining)
                .min(remaining_for_body)
                .min(remaining_budget);

            if chunk_size > 0 {
                let start = self.initial_position;
                let chunk = self.initial_buffer[start..start + chunk_size].to_vec();
                self.initial_position += chunk_size;
                self.bytes_read += chunk_size;
                return Poll::Ready(Some(Ok(chunk)));
            }
        }

        // Initial buffer exhausted, read from reader
        let remaining = self.expected_size.saturating_sub(self.bytes_read);
        let to_read = self.chunk_size.min(remaining).min(remaining_budget);

        if to_read == 0 {
            self.complete = true;
            return Poll::Ready(None);
        }

        // Ensure buffer is sized appropriately
        if self.read_buffer.len() < to_read {
            self.read_buffer.resize(to_read, 0);
        }

        // Take reader temporarily to avoid borrow conflicts
        let mut reader = match self.reader.take() {
            Some(r) => r,
            None => {
                self.error = true;
                return Poll::Ready(Some(Err(RequestBodyStreamError::ConnectionClosed)));
            }
        };

        // Perform the read and extract result before modifying self
        let read_result = {
            let mut read_buf = asupersync::io::ReadBuf::new(&mut self.read_buffer[..to_read]);
            match Pin::new(&mut reader).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    let n = read_buf.filled().len();
                    let chunk = read_buf.filled().to_vec();
                    Poll::Ready(Ok((n, chunk)))
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        };

        match read_result {
            Poll::Ready(Ok((n, chunk))) => {
                if n == 0 {
                    // EOF before expected bytes - incomplete body
                    self.error = true;
                    return Poll::Ready(Some(Err(RequestBodyStreamError::ConnectionClosed)));
                }

                self.bytes_read += n;

                // Put reader back
                self.reader = Some(reader);

                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Err(e)) => {
                self.error = true;
                Poll::Ready(Some(Err(RequestBodyStreamError::Io(e.to_string()))))
            }
            Poll::Pending => {
                // Put reader back before returning Pending
                self.reader = Some(reader);
                Poll::Pending
            }
        }
    }
}

/// Parsing state for chunked encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsyncChunkedState {
    /// Parsing chunk size line.
    ChunkSize,
    /// Reading chunk data.
    ChunkData { remaining: usize },
    /// Expecting CRLF after chunk data.
    ChunkDataEnd,
    /// Reading trailers (after final chunk).
    Trailers,
    /// Complete.
    Complete,
    /// Error.
    Error,
}

/// An async stream that reads a chunked-encoded body.
///
/// This stream parses chunked transfer encoding on the fly,
/// yielding decoded chunks as they become available.
///
/// # Chunked Encoding Format
///
/// ```text
/// chunk-size CRLF
/// chunk-data CRLF
/// ...
/// 0 CRLF
/// [trailers] CRLF
/// ```
pub struct AsyncChunkedStream<R> {
    /// Reader for more data (used when buffer is exhausted).
    #[allow(dead_code)]
    reader: Option<R>,
    /// Parsing state.
    state: AsyncChunkedState,
    /// Total decoded bytes so far.
    bytes_decoded: usize,
    /// Maximum allowed size.
    max_size: usize,
    /// Chunk size for reads.
    chunk_size: usize,
    /// Read buffer (used when socket reads are needed).
    #[allow(dead_code)]
    read_buffer: Vec<u8>,
    /// Buffer for initial data from parser + any data read from socket.
    buffer: Vec<u8>,
    /// Position in buffer.
    position: usize,
}

impl<R> AsyncChunkedStream<R>
where
    R: AsyncRead + Unpin + Send + Sync + 'static,
{
    /// Create a new chunked stream.
    ///
    /// # Arguments
    ///
    /// * `initial_buffer` - Any bytes already buffered by the parser
    /// * `reader` - The async reader for remaining bytes
    /// * `config` - Streaming configuration
    ///
    /// # Panics
    ///
    /// Panics if `initial_buffer` exceeds `config.max_size`. Use `try_new` for
    /// fallible construction.
    pub fn new(initial_buffer: Vec<u8>, reader: R, config: &StreamingBodyConfig) -> Self {
        assert!(
            initial_buffer.len() <= config.max_size,
            "initial buffer size {} exceeds max size {}",
            initial_buffer.len(),
            config.max_size
        );
        Self {
            reader: Some(reader),
            state: AsyncChunkedState::ChunkSize,
            bytes_decoded: 0,
            max_size: config.max_size,
            chunk_size: config.chunk_size,
            read_buffer: vec![0u8; config.chunk_size],
            buffer: initial_buffer,
            position: 0,
        }
    }

    /// Try to create a new chunked stream, returning error if initial buffer is too large.
    ///
    /// # Errors
    ///
    /// Returns error if `initial_buffer.len()` exceeds `config.max_size`.
    pub fn try_new(
        initial_buffer: Vec<u8>,
        reader: R,
        config: &StreamingBodyConfig,
    ) -> Result<Self, RequestBodyStreamError> {
        if initial_buffer.len() > config.max_size {
            return Err(RequestBodyStreamError::Io(format!(
                "initial buffer size {} exceeds max size {}",
                initial_buffer.len(),
                config.max_size
            )));
        }
        Ok(Self {
            reader: Some(reader),
            state: AsyncChunkedState::ChunkSize,
            bytes_decoded: 0,
            max_size: config.max_size,
            chunk_size: config.chunk_size,
            read_buffer: vec![0u8; config.chunk_size],
            buffer: initial_buffer,
            position: 0,
        })
    }

    /// Create a chunked stream with default config.
    pub fn with_defaults(initial_buffer: Vec<u8>, reader: R) -> Self {
        Self::new(initial_buffer, reader, &StreamingBodyConfig::default())
    }

    /// Returns the total decoded bytes so far.
    #[must_use]
    pub fn bytes_decoded(&self) -> usize {
        self.bytes_decoded
    }

    /// Returns true if the stream is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.state == AsyncChunkedState::Complete
    }

    /// Get remaining buffer bytes.
    fn buffer_remaining(&self) -> &[u8] {
        &self.buffer[self.position..]
    }

    /// Consume bytes from buffer.
    fn consume(&mut self, n: usize) {
        self.position += n;
    }

    fn compact_buffer_if_needed(&mut self) {
        if self.position == 0 {
            return;
        }
        if self.position >= self.buffer.len() {
            self.buffer.clear();
            self.position = 0;
            return;
        }

        // Avoid unbounded growth: once we've consumed enough, shift the unread tail down.
        let should_compact = self.position > 8 * 1024 || self.position > (self.buffer.len() / 2);
        if should_compact {
            self.buffer.drain(..self.position);
            self.position = 0;
        }
    }

    fn poll_read_more_sized(
        &mut self,
        cx: &mut Context<'_>,
        max_read: usize,
    ) -> Poll<Result<usize, RequestBodyStreamError>> {
        self.compact_buffer_if_needed();

        let max_read = max_read.min(self.read_buffer.len());
        if max_read == 0 {
            self.state = AsyncChunkedState::Error;
            return Poll::Ready(Err(RequestBodyStreamError::Io(
                "invalid read buffer size".to_string(),
            )));
        }

        let mut reader = match self.reader.take() {
            Some(r) => r,
            None => {
                self.state = AsyncChunkedState::Error;
                return Poll::Ready(Err(RequestBodyStreamError::ConnectionClosed));
            }
        };

        let read_result = {
            let mut read_buf = asupersync::io::ReadBuf::new(&mut self.read_buffer[..max_read]);
            match Pin::new(&mut reader).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    let filled = read_buf.filled();
                    Poll::Ready(Ok(filled.len()))
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        };

        match read_result {
            Poll::Ready(Ok(n)) => {
                if n == 0 {
                    self.state = AsyncChunkedState::Error;
                    self.reader = Some(reader);
                    return Poll::Ready(Err(RequestBodyStreamError::ConnectionClosed));
                }
                self.buffer.extend_from_slice(&self.read_buffer[..n]);
                self.reader = Some(reader);
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => {
                self.state = AsyncChunkedState::Error;
                self.reader = Some(reader);
                Poll::Ready(Err(RequestBodyStreamError::Io(e.to_string())))
            }
            Poll::Pending => {
                self.reader = Some(reader);
                Poll::Pending
            }
        }
    }
}

impl<R> Stream for AsyncChunkedStream<R>
where
    R: AsyncRead + Unpin + Send + Sync + 'static,
{
    type Item = Result<Vec<u8>, RequestBodyStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Check if complete or error
        if self.state == AsyncChunkedState::Complete || self.state == AsyncChunkedState::Error {
            return Poll::Ready(None);
        }

        loop {
            match self.state {
                AsyncChunkedState::ChunkSize => {
                    // Try to find chunk size line in buffer
                    let remaining = self.buffer_remaining();
                    if let Some(crlf_pos) = remaining.windows(2).position(|w| w == b"\r\n") {
                        // Parse chunk size
                        let size_line = &remaining[..crlf_pos];

                        // Parse hex size (ignore extensions after semicolon)
                        let size_str = if let Some(semi) = size_line.iter().position(|&b| b == b';')
                        {
                            &size_line[..semi]
                        } else {
                            size_line
                        };

                        let size_str = match std::str::from_utf8(size_str) {
                            Ok(s) => s.trim(),
                            Err(_) => {
                                self.state = AsyncChunkedState::Error;
                                return Poll::Ready(Some(Err(RequestBodyStreamError::Io(
                                    "invalid UTF-8 in chunk size".to_string(),
                                ))));
                            }
                        };

                        let chunk_size = match usize::from_str_radix(size_str, 16) {
                            Ok(s) => s,
                            Err(_) => {
                                self.state = AsyncChunkedState::Error;
                                return Poll::Ready(Some(Err(RequestBodyStreamError::Io(
                                    "invalid hex chunk size".to_string(),
                                ))));
                            }
                        };

                        // Enforce max size before consuming/streaming this chunk.
                        if chunk_size > 0
                            && self.bytes_decoded.saturating_add(chunk_size) > self.max_size
                        {
                            self.state = AsyncChunkedState::Error;
                            let bytes_decoded = self.bytes_decoded;
                            let max_size = self.max_size;
                            return Poll::Ready(Some(Err(RequestBodyStreamError::TooLarge {
                                received: bytes_decoded,
                                max: max_size,
                            })));
                        }

                        // Reject unreasonably large chunk sizes early
                        const MAX_SINGLE_CHUNK: usize = 16 * 1024 * 1024;
                        if chunk_size > MAX_SINGLE_CHUNK {
                            self.state = AsyncChunkedState::Error;
                            return Poll::Ready(Some(Err(RequestBodyStreamError::Io(
                                "chunk size exceeds 16MB limit".to_string(),
                            ))));
                        }

                        self.consume(crlf_pos + 2);

                        if chunk_size == 0 {
                            // Final chunk - transition to trailers (includes required CRLF)
                            self.state = AsyncChunkedState::Trailers;
                            continue;
                        }

                        self.state = AsyncChunkedState::ChunkData {
                            remaining: chunk_size,
                        };
                        continue;
                    }

                    // Need more data from the reader (socket).
                    //
                    // Defensive cap: reject absurdly long chunk size lines without CRLF.
                    const MAX_CHUNK_SIZE_LINE: usize = 1024;
                    if remaining.len() > MAX_CHUNK_SIZE_LINE {
                        self.state = AsyncChunkedState::Error;
                        return Poll::Ready(Some(Err(RequestBodyStreamError::Io(
                            "chunk size line too long".to_string(),
                        ))));
                    }

                    // Read minimally to avoid consuming bytes beyond request boundaries
                    // on keep-alive connections.
                    match self.poll_read_more_sized(cx, 1) {
                        Poll::Ready(Ok(_n)) => {}
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                AsyncChunkedState::ChunkData { remaining } => {
                    // Ensure we never yield bytes beyond max_size.
                    if remaining > 0 && self.bytes_decoded >= self.max_size {
                        self.state = AsyncChunkedState::Error;
                        let bytes_decoded = self.bytes_decoded;
                        let max_size = self.max_size;
                        return Poll::Ready(Some(Err(RequestBodyStreamError::TooLarge {
                            received: bytes_decoded,
                            max: max_size,
                        })));
                    }

                    // Read chunk data from buffer
                    let buffer_remaining = self.buffer_remaining();
                    let to_read = remaining.min(buffer_remaining.len()).min(self.chunk_size);

                    if to_read > 0 {
                        let chunk = buffer_remaining[..to_read].to_vec();
                        self.consume(to_read);
                        self.bytes_decoded += to_read;

                        let new_remaining = remaining - to_read;
                        if new_remaining == 0 {
                            self.state = AsyncChunkedState::ChunkDataEnd;
                        } else {
                            self.state = AsyncChunkedState::ChunkData {
                                remaining: new_remaining,
                            };
                        }

                        return Poll::Ready(Some(Ok(chunk)));
                    }

                    // Need more data from the reader (socket).
                    // Read at most the remaining bytes in this chunk to avoid read-ahead.
                    let want = remaining.min(self.chunk_size).max(1);
                    match self.poll_read_more_sized(cx, want) {
                        Poll::Ready(Ok(_n)) => {}
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                AsyncChunkedState::ChunkDataEnd => {
                    // Expect CRLF
                    let remaining = self.buffer_remaining();
                    if remaining.len() >= 2 {
                        if &remaining[..2] == b"\r\n" {
                            self.consume(2);
                            self.state = AsyncChunkedState::ChunkSize;
                            continue;
                        }
                        self.state = AsyncChunkedState::Error;
                        return Poll::Ready(Some(Err(RequestBodyStreamError::Io(
                            "expected CRLF after chunk data".to_string(),
                        ))));
                    }

                    // Need more data from the reader (socket).
                    match self.poll_read_more_sized(cx, 1) {
                        Poll::Ready(Ok(_n)) => {}
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                AsyncChunkedState::Trailers => {
                    // Skip trailers until empty line (CRLF). Trailers are not exposed to the app.
                    //
                    // Read minimally to avoid swallowing bytes that belong to the next request
                    // on keep-alive connections.
                    let remaining = self.buffer_remaining();

                    if remaining.len() >= 2 && &remaining[..2] == b"\r\n" {
                        self.consume(2);
                        self.state = AsyncChunkedState::Complete;
                        return Poll::Ready(None);
                    }

                    // Defensive cap: trailer lines must be reasonably bounded.
                    const MAX_TRAILER_LINE: usize = 8 * 1024;
                    if remaining.len() > MAX_TRAILER_LINE {
                        self.state = AsyncChunkedState::Error;
                        return Poll::Ready(Some(Err(RequestBodyStreamError::Io(
                            "trailer line too long".to_string(),
                        ))));
                    }

                    if let Some(crlf_pos) = remaining.windows(2).position(|w| w == b"\r\n") {
                        // Skip one trailer line (header) and continue.
                        self.consume(crlf_pos + 2);
                        continue;
                    }

                    match self.poll_read_more_sized(cx, 1) {
                        Poll::Ready(Ok(_n)) => {}
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                AsyncChunkedState::Complete | AsyncChunkedState::Error => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

/// Create a streaming body from a Content-Length body.
///
/// Returns a `fastapi_core::Body::Stream` that yields chunks from the given
/// initial buffer and async reader.
///
/// # Arguments
///
/// * `initial_buffer` - Any bytes already buffered by the parser
/// * `reader` - The async reader for remaining bytes
/// * `content_length` - Expected total body size
/// * `config` - Streaming configuration
pub fn create_content_length_stream<R>(
    initial_buffer: Vec<u8>,
    reader: R,
    content_length: usize,
    config: &StreamingBodyConfig,
) -> fastapi_core::Body
where
    R: AsyncRead + Unpin + Send + Sync + 'static,
{
    let stream = AsyncContentLengthStream::new(initial_buffer, reader, content_length, config);
    fastapi_core::Body::streaming_with_size(stream, content_length)
}

/// Create a streaming body from a chunked transfer-encoded body.
///
/// Returns a `fastapi_core::Body::Stream` that yields decoded chunks.
///
/// # Arguments
///
/// * `initial_buffer` - Any bytes already buffered by the parser
/// * `reader` - The async reader for remaining bytes
/// * `config` - Streaming configuration
pub fn create_chunked_stream<R>(
    initial_buffer: Vec<u8>,
    reader: R,
    config: &StreamingBodyConfig,
) -> fastapi_core::Body
where
    R: AsyncRead + Unpin + Send + Sync + 'static,
{
    let stream = AsyncChunkedStream::new(initial_buffer, reader, config);
    fastapi_core::Body::streaming(stream)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // BodyConfig Tests
    // ========================================================================

    #[test]
    fn body_config_defaults() {
        let config = BodyConfig::default();
        assert_eq!(config.max_size(), DEFAULT_MAX_BODY_SIZE);
        assert_eq!(config.initial_capacity(), 4096);
    }

    #[test]
    fn body_config_custom() {
        let config = BodyConfig::new()
            .with_max_size(2048)
            .with_initial_capacity(1024);
        assert_eq!(config.max_size(), 2048);
        assert_eq!(config.initial_capacity(), 1024);
    }

    // ========================================================================
    // Content-Length Reader Tests
    // ========================================================================

    #[test]
    fn content_length_basic() {
        let body = b"Hello, World!";
        let config = BodyConfig::default();
        let mut reader = ContentLengthReader::new(body, body.len(), &config).unwrap();

        assert_eq!(reader.length(), 13);
        assert_eq!(reader.remaining(), 13);
        assert!(!reader.is_complete());

        let result = reader.read_all().unwrap();
        assert_eq!(result, b"Hello, World!");
        assert!(reader.is_complete());
    }

    #[test]
    fn content_length_zero() {
        let body = b"";
        let config = BodyConfig::default();
        let mut reader = ContentLengthReader::new(body, 0, &config).unwrap();

        assert_eq!(reader.length(), 0);
        assert!(reader.is_complete());

        let result = reader.read_all().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn content_length_too_large() {
        let body = b"small";
        let config = BodyConfig::new().with_max_size(3);
        let result = ContentLengthReader::new(body, 100, &config);

        assert!(matches!(
            result,
            Err(BodyError::TooLarge { size: 100, max: 3 })
        ));
    }

    #[test]
    fn content_length_incomplete() {
        let body = b"Hello";
        let config = BodyConfig::default();
        let mut reader = ContentLengthReader::new(body, 10, &config).unwrap();

        let result = reader.read_all();
        assert!(matches!(
            result,
            Err(BodyError::Incomplete {
                received: 5,
                expected: Some(10)
            })
        ));
    }

    #[test]
    fn content_length_borrowed() {
        let body = b"Hello, World!";
        let config = BodyConfig::default();
        let reader = ContentLengthReader::new(body, body.len(), &config).unwrap();

        let borrowed = reader.read_all_borrowed().unwrap();
        assert_eq!(borrowed, body);
        // Verify it's the same memory location (zero-copy)
        assert_eq!(borrowed.as_ptr(), body.as_ptr());
    }

    #[test]
    fn content_length_incremental_read() {
        let body = b"Hello, World!";
        let config = BodyConfig::default();
        let mut reader = ContentLengthReader::new(body, body.len(), &config).unwrap();

        let mut buf = [0u8; 5];

        // First read
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"Hello");
        assert_eq!(reader.remaining(), 8);

        // Second read
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b", Wor");
        assert_eq!(reader.remaining(), 3);

        // Third read
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], b"ld!");
        assert!(reader.is_complete());

        // No more data
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    // ========================================================================
    // Chunked Encoding Tests
    // ========================================================================

    #[test]
    fn chunked_single_chunk() {
        let body = b"5\r\nHello\r\n0\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all().unwrap();
        assert_eq!(result, b"Hello");
        assert!(reader.is_complete());
    }

    #[test]
    fn chunked_multiple_chunks() {
        let body = b"5\r\nHello\r\n7\r\n, World\r\n1\r\n!\r\n0\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all().unwrap();
        assert_eq!(result, b"Hello, World!");
        assert!(reader.is_complete());
    }

    #[test]
    fn chunked_empty() {
        let body = b"0\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all().unwrap();
        assert!(result.is_empty());
        assert!(reader.is_complete());
    }

    #[test]
    fn chunked_with_extension() {
        // Chunk extensions should be ignored
        let body = b"5;ext=value\r\nHello\r\n0\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all().unwrap();
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn chunked_with_trailers() {
        let body = b"5\r\nHello\r\n0\r\nTrailer: value\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all().unwrap();
        assert_eq!(result, b"Hello");
        assert!(reader.is_complete());
    }

    #[test]
    fn chunked_hex_sizes() {
        // Test various hex chunk sizes
        let body = b"a\r\n0123456789\r\nF\r\n0123456789ABCDE\r\n0\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all().unwrap();
        assert_eq!(result.len(), 10 + 15); // a=10, F=15
    }

    #[test]
    fn chunked_too_large() {
        let body = b"10\r\n0123456789ABCDEF\r\n0\r\n\r\n"; // 16 bytes
        let config = BodyConfig::new().with_max_size(10);
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all();
        assert!(matches!(
            result,
            Err(BodyError::TooLarge { size: 16, max: 10 })
        ));
    }

    #[test]
    fn chunked_invalid_size() {
        let body = b"xyz\r\nHello\r\n0\r\n\r\n";
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all();
        assert!(matches!(
            result,
            Err(BodyError::InvalidChunkedEncoding { detail: _ })
        ));
    }

    #[test]
    fn chunked_missing_crlf() {
        let body = b"5\r\nHelloX0\r\n\r\n"; // Missing CRLF after data
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all();
        assert!(matches!(
            result,
            Err(BodyError::InvalidChunkedEncoding {
                detail: "expected CRLF after chunk data"
            })
        ));
    }

    #[test]
    fn chunked_incomplete() {
        let body = b"5\r\nHel"; // Incomplete chunk data
        let config = BodyConfig::default();
        let mut reader = ChunkedReader::new(body, &config);

        let result = reader.decode_all();
        assert!(matches!(result, Err(BodyError::Incomplete { .. })));
    }

    // ========================================================================
    // parse_body Tests
    // ========================================================================

    #[test]
    fn parse_body_none() {
        let config = BodyConfig::default();
        let result = parse_body(b"ignored", BodyLength::None, &config).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_body_content_length() {
        let config = BodyConfig::default();
        let result = parse_body(b"Hello, World!", BodyLength::ContentLength(13), &config).unwrap();
        assert_eq!(result.unwrap(), b"Hello, World!");
    }

    #[test]
    fn parse_body_content_length_zero() {
        let config = BodyConfig::default();
        let result = parse_body(b"", BodyLength::ContentLength(0), &config).unwrap();
        assert_eq!(result.unwrap(), b"");
    }

    #[test]
    fn parse_body_chunked() {
        let config = BodyConfig::default();
        let result = parse_body(b"5\r\nHello\r\n0\r\n\r\n", BodyLength::Chunked, &config).unwrap();
        assert_eq!(result.unwrap(), b"Hello");
    }

    #[test]
    fn parse_body_with_consumed_content_length() {
        let config = BodyConfig::default();
        let (body, consumed) =
            parse_body_with_consumed(b"Hello, World!", BodyLength::ContentLength(13), &config)
                .unwrap();
        assert_eq!(body.unwrap(), b"Hello, World!");
        assert_eq!(consumed, 13);
    }

    #[test]
    fn parse_body_with_consumed_chunked() {
        let config = BodyConfig::default();
        let (body, consumed) =
            parse_body_with_consumed(b"5\r\nHello\r\n0\r\n\r\n", BodyLength::Chunked, &config)
                .unwrap();
        assert_eq!(body.unwrap(), b"Hello");
        assert_eq!(consumed, 15);
    }

    // ========================================================================
    // validate_content_length Tests
    // ========================================================================

    #[test]
    fn validate_content_length_ok() {
        let config = BodyConfig::new().with_max_size(1000);
        assert!(validate_content_length(500, &config).is_ok());
        assert!(validate_content_length(1000, &config).is_ok());
    }

    #[test]
    fn validate_content_length_too_large() {
        let config = BodyConfig::new().with_max_size(1000);
        let result = validate_content_length(1001, &config);
        assert!(matches!(
            result,
            Err(BodyError::TooLarge {
                size: 1001,
                max: 1000
            })
        ));
    }

    // ========================================================================
    // BodyError Tests
    // ========================================================================

    #[test]
    fn body_error_display() {
        let err = BodyError::TooLarge {
            size: 2000,
            max: 1000,
        };
        assert_eq!(
            format!("{err}"),
            "body too large: 2000 bytes exceeds limit of 1000"
        );

        let err = BodyError::InvalidChunkedEncoding {
            detail: "bad format",
        };
        assert_eq!(format!("{err}"), "invalid chunked encoding: bad format");

        let err = BodyError::Incomplete {
            received: 50,
            expected: Some(100),
        };
        assert_eq!(
            format!("{err}"),
            "incomplete body: received 50 of 100 bytes"
        );

        let err = BodyError::UnexpectedEof;
        assert_eq!(format!("{err}"), "unexpected end of body");
    }

    // ========================================================================
    // StreamingBodyConfig Tests
    // ========================================================================

    #[test]
    fn streaming_body_config_defaults() {
        let config = StreamingBodyConfig::default();
        assert_eq!(config.streaming_threshold, DEFAULT_STREAMING_THRESHOLD);
        assert_eq!(config.chunk_size, 8 * 1024);
        assert_eq!(config.max_size, DEFAULT_MAX_BODY_SIZE);
    }

    #[test]
    fn streaming_body_config_custom() {
        let config = StreamingBodyConfig::new()
            .with_streaming_threshold(1024)
            .with_chunk_size(4096)
            .with_max_size(10_000);
        assert_eq!(config.streaming_threshold, 1024);
        assert_eq!(config.chunk_size, 4096);
        assert_eq!(config.max_size, 10_000);
    }

    #[test]
    fn streaming_body_config_minimum_chunk_size() {
        let config = StreamingBodyConfig::new().with_chunk_size(0);
        // Should be clamped to minimum of 1 byte
        assert_eq!(config.chunk_size, 1);
    }

    #[test]
    fn streaming_body_config_should_stream() {
        let config = StreamingBodyConfig::new().with_streaming_threshold(1000);
        assert!(!config.should_stream(500));
        assert!(!config.should_stream(1000));
        assert!(config.should_stream(1001));
        assert!(config.should_stream(10000));
    }

    // ========================================================================
    // AsyncContentLengthStream Tests
    // ========================================================================

    #[test]
    fn async_content_length_stream_from_buffer() {
        use std::task::Waker;

        fn noop_waker() -> Waker {
            Waker::noop().clone()
        }

        // Create a mock reader that won't be used (buffer is complete)
        struct EmptyReader;
        impl AsyncRead for EmptyReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut asupersync::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let buffer = b"Hello, World!".to_vec();
        let config = StreamingBodyConfig::new().with_chunk_size(5);
        let mut stream = AsyncContentLengthStream::new(buffer, EmptyReader, 13, &config);

        assert_eq!(stream.expected_size(), 13);
        assert_eq!(stream.bytes_read(), 0);
        assert_eq!(stream.remaining(), 13);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // First chunk: "Hello"
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Ok(chunk))) => {
                assert_eq!(chunk, b"Hello");
            }
            _ => panic!("expected chunk"),
        }
        assert_eq!(stream.bytes_read(), 5);

        // Second chunk: ", Wor"
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Ok(chunk))) => {
                assert_eq!(chunk, b", Wor");
            }
            _ => panic!("expected chunk"),
        }

        // Third chunk: "ld!"
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Ok(chunk))) => {
                assert_eq!(chunk, b"ld!");
            }
            _ => panic!("expected chunk"),
        }

        // End of stream
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        assert!(matches!(result, Poll::Ready(None)));
        assert!(stream.is_complete());
    }

    #[test]
    fn async_content_length_stream_enforces_max_size() {
        use std::io::Cursor;
        use std::task::Waker;

        fn noop_waker() -> Waker {
            Waker::noop().clone()
        }

        let initial = b"123456".to_vec();
        let reader = Cursor::new(b"abcdef".to_vec());
        let config = StreamingBodyConfig::new()
            .with_chunk_size(8)
            .with_max_size(10);
        let mut stream = AsyncContentLengthStream::new(initial, reader, 12, &config);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // First 6 bytes come from initial buffer.
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Ok(chunk))) => assert_eq!(chunk, b"123456"),
            _ => panic!("expected initial chunk"),
        }

        // Stream can still emit bytes up to max_size.
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Ok(chunk))) => assert_eq!(chunk, b"abcd"),
            _ => panic!("expected bounded reader chunk"),
        }

        // Next poll must fail because body is larger than max_size.
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Err(RequestBodyStreamError::TooLarge { received, max }))) => {
                assert_eq!(received, 11);
                assert_eq!(max, 10);
            }
            _ => panic!("expected TooLarge error, got {:?}", result),
        }
    }

    // ========================================================================
    // AsyncChunkedStream Tests
    // ========================================================================

    #[test]
    fn async_chunked_stream_simple() {
        use std::task::Waker;

        fn noop_waker() -> Waker {
            Waker::noop().clone()
        }

        struct EmptyReader;
        impl AsyncRead for EmptyReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut asupersync::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        // Complete chunked body in buffer: "Hello" in chunked encoding
        let buffer = b"5\r\nHello\r\n0\r\n\r\n".to_vec();
        let config = StreamingBodyConfig::new().with_chunk_size(1024);
        let mut stream = AsyncChunkedStream::new(buffer, EmptyReader, &config);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // First chunk: "Hello"
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        match result {
            Poll::Ready(Some(Ok(chunk))) => {
                assert_eq!(chunk, b"Hello");
            }
            _ => panic!("expected chunk, got {:?}", result),
        }

        // Note: Need to poll again to process CRLF and next chunk size
        // The implementation returns Pending after transition, then processes on next poll
        let result = Pin::new(&mut stream).poll_next(&mut cx);
        // Should be complete (0\r\n\r\n)
        assert!(matches!(result, Poll::Ready(None)));
        assert!(stream.is_complete());
    }

    #[test]
    fn async_chunked_stream_multiple_chunks() {
        use std::task::Waker;

        fn noop_waker() -> Waker {
            Waker::noop().clone()
        }

        struct EmptyReader;
        impl AsyncRead for EmptyReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut asupersync::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        // "Hello, World!" in chunked encoding
        let buffer = b"5\r\nHello\r\n8\r\n, World!\r\n0\r\n\r\n".to_vec();
        let config = StreamingBodyConfig::new();
        let mut stream = AsyncChunkedStream::new(buffer, EmptyReader, &config);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Collect all chunks
        let mut collected = Vec::new();
        loop {
            match Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(Ok(chunk))) => collected.extend_from_slice(&chunk),
                Poll::Ready(Some(Err(e))) => panic!("unexpected error: {e}"),
                Poll::Ready(None) => break,
                Poll::Pending => {} // Continue processing
            }
        }

        assert_eq!(collected, b"Hello, World!");
    }
}
