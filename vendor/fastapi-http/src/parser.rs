//! HTTP request parser.
//!
//! This module provides zero-copy HTTP/1.1 parsing for request lines and headers.
//!
//! # Zero-Copy Design
//!
//! The parser returns borrowed types that reference the original buffer,
//! avoiding allocations on the hot path. Types:
//!
//! - [`RequestLine`] - borrowed request line (method, path, version)
//! - [`HeadersIter`] - iterator over borrowed headers
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::parser::{Parser, RequestLine};
//!
//! let buffer = b"GET /items/123?q=test HTTP/1.1\r\nHost: example.com\r\n\r\n";
//! let request_line = RequestLine::parse(buffer)?;
//!
//! assert_eq!(request_line.method(), Method::Get);
//! assert_eq!(request_line.path(), "/items/123");
//! assert_eq!(request_line.query(), Some("q=test"));
//! assert_eq!(request_line.version(), "HTTP/1.1");
//! ```

use crate::body::{BodyConfig, BodyError, parse_body_with_consumed};
use fastapi_core::{Body, HttpVersion, Method, Request};
use std::borrow::Cow;

/// HTTP parsing error.
#[derive(Debug)]
pub enum ParseError {
    /// Invalid request line.
    InvalidRequestLine,
    /// Invalid HTTP method.
    InvalidMethod,
    /// Invalid header.
    InvalidHeader,
    /// Invalid header name (non-token characters).
    InvalidHeaderName,
    /// Invalid bytes in header value.
    InvalidHeaderBytes,
    /// Request line too long.
    RequestLineTooLong,
    /// Header line too long.
    HeaderLineTooLong,
    /// Too many headers.
    TooManyHeaders,
    /// Header block too large.
    HeadersTooLarge,
    /// Unsupported or invalid Transfer-Encoding.
    InvalidTransferEncoding,
    /// Ambiguous body length (e.g., both Transfer-Encoding and Content-Length).
    AmbiguousBodyLength,
    /// Request too large.
    TooLarge,
    /// Incomplete request (need more data).
    Incomplete,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequestLine => write!(f, "invalid request line"),
            Self::InvalidMethod => write!(f, "invalid HTTP method"),
            Self::InvalidHeader => write!(f, "invalid header"),
            Self::InvalidHeaderName => write!(f, "invalid header name"),
            Self::InvalidHeaderBytes => write!(f, "invalid header bytes"),
            Self::RequestLineTooLong => write!(f, "request line too long"),
            Self::HeaderLineTooLong => write!(f, "header line too long"),
            Self::TooManyHeaders => write!(f, "too many headers"),
            Self::HeadersTooLarge => write!(f, "headers too large"),
            Self::InvalidTransferEncoding => write!(f, "invalid transfer-encoding"),
            Self::AmbiguousBodyLength => write!(f, "ambiguous body length"),
            Self::TooLarge => write!(f, "request too large"),
            Self::Incomplete => write!(f, "incomplete request"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parsing limits for request line and headers.
#[derive(Debug, Clone)]
pub struct ParseLimits {
    /// Maximum total request size in bytes.
    pub max_request_size: usize,
    /// Maximum request line length in bytes.
    pub max_request_line_len: usize,
    /// Maximum number of headers.
    pub max_header_count: usize,
    /// Maximum length of a single header line.
    pub max_header_line_len: usize,
    /// Maximum total header block size (including CRLF terminator).
    pub max_headers_size: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_request_size: 1024 * 1024,  // 1MB
            max_request_line_len: 8 * 1024, // 8KB
            max_header_count: 100,
            max_header_line_len: 8 * 1024, // 8KB
            max_headers_size: 64 * 1024,   // 64KB
        }
    }
}

fn has_invalid_request_line_bytes(line: &[u8]) -> bool {
    line.iter().any(|&b| b == 0 || b == b'\r' || b == b'\n')
}

// ============================================================================
// Zero-Copy Request Line Parser
// ============================================================================

/// A zero-copy view of an HTTP request line.
///
/// This type borrows from the original buffer and performs no allocations.
/// It provides access to the parsed method, path, query string, and HTTP version.
///
/// # Zero-Allocation Guarantee
///
/// All methods return borrowed data (`&str` or `Method`). No heap allocations
/// are performed during parsing or access.
///
/// # Example
///
/// ```ignore
/// let buffer = b"GET /items/123?q=test HTTP/1.1\r\n";
/// let line = RequestLine::parse(buffer)?;
///
/// assert_eq!(line.method(), Method::Get);
/// assert_eq!(line.path(), "/items/123");
/// assert_eq!(line.query(), Some("q=test"));
/// assert_eq!(line.version(), "HTTP/1.1");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct RequestLine<'a> {
    method: Method,
    /// Full URI as it appeared on the request line (path + optional `?query`).
    ///
    /// This is a zero-copy slice into the request buffer.
    uri: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    version: &'a str,
}

impl<'a> RequestLine<'a> {
    /// Parse a request line from bytes.
    ///
    /// The buffer should contain just the request line, ending with `\r\n` or EOF.
    /// Example: `GET /path?query HTTP/1.1\r\n`
    ///
    /// # Errors
    ///
    /// Returns `ParseError::InvalidRequestLine` if the format is invalid.
    /// Returns `ParseError::InvalidMethod` if the HTTP method is not recognized.
    pub fn parse(buffer: &'a [u8]) -> Result<Self, ParseError> {
        // Find the end of the request line (before any \r\n)
        let line_end = buffer
            .windows(2)
            .position(|w| w == b"\r\n")
            .unwrap_or(buffer.len());

        let line = &buffer[..line_end];
        if has_invalid_request_line_bytes(line) {
            return Err(ParseError::InvalidRequestLine);
        }

        // Split by spaces: METHOD SP URI SP VERSION
        let mut parts = line.splitn(3, |&b| b == b' ');

        // Parse method
        let method_bytes = parts.next().ok_or(ParseError::InvalidRequestLine)?;
        let method = Method::from_bytes(method_bytes).ok_or(ParseError::InvalidMethod)?;

        // Parse URI (path + optional query)
        let uri_bytes = parts.next().ok_or(ParseError::InvalidRequestLine)?;
        if uri_bytes.is_empty() {
            return Err(ParseError::InvalidRequestLine);
        }
        let uri = std::str::from_utf8(uri_bytes).map_err(|_| ParseError::InvalidRequestLine)?;

        // Parse version
        let version_bytes = parts.next().ok_or(ParseError::InvalidRequestLine)?;
        let version =
            std::str::from_utf8(version_bytes).map_err(|_| ParseError::InvalidRequestLine)?;

        // Split path and query from URI
        let (path, query) = if let Some(q_pos) = uri.find('?') {
            (&uri[..q_pos], Some(&uri[q_pos + 1..]))
        } else {
            (uri, None)
        };

        Ok(Self {
            method,
            uri,
            path,
            query,
            version,
        })
    }

    /// Parse a request line from a buffer, returning bytes consumed.
    ///
    /// This is useful for incremental parsing where you need to know
    /// how much of the buffer was consumed.
    ///
    /// # Returns
    ///
    /// Returns `(RequestLine, bytes_consumed)` on success.
    /// `bytes_consumed` includes the trailing `\r\n` if present.
    pub fn parse_with_len(buffer: &'a [u8]) -> Result<(Self, usize), ParseError> {
        let line_end = buffer
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or(ParseError::Incomplete)?;

        let line = Self::parse(&buffer[..line_end])?;
        // +2 for the \r\n
        Ok((line, line_end + 2))
    }

    /// Returns the HTTP method.
    #[inline]
    #[must_use]
    pub fn method(&self) -> Method {
        self.method
    }

    /// Returns the request path (without query string).
    ///
    /// Example: For `GET /items/123?q=test HTTP/1.1`, returns `/items/123`.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// Returns the query string (without the leading `?`), if present.
    ///
    /// Example: For `GET /items?q=test HTTP/1.1`, returns `Some("q=test")`.
    #[inline]
    #[must_use]
    pub fn query(&self) -> Option<&'a str> {
        self.query
    }

    /// Returns the full URI (path + query string).
    ///
    /// Example: For `GET /items?q=test HTTP/1.1`, returns `/items?q=test`.
    #[must_use]
    pub fn uri(&self) -> &'a str {
        self.uri
    }

    /// Returns the HTTP version string.
    ///
    /// Example: For `GET /path HTTP/1.1`, returns `HTTP/1.1`.
    #[inline]
    #[must_use]
    pub fn version(&self) -> &'a str {
        self.version
    }

    /// Returns true if this is HTTP/1.1.
    #[inline]
    #[must_use]
    pub fn is_http11(&self) -> bool {
        self.version == "HTTP/1.1"
    }

    /// Returns true if this is HTTP/1.0.
    #[inline]
    #[must_use]
    pub fn is_http10(&self) -> bool {
        self.version == "HTTP/1.0"
    }
}

// ============================================================================
// Zero-Copy Header Parser
// ============================================================================

fn is_token_char(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`'
            | b'|' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

fn is_valid_header_name(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| is_token_char(b))
}

fn has_invalid_header_value_bytes(value: &[u8]) -> bool {
    value
        .iter()
        .any(|&b| b == 0 || b == 0x7f || (b < 0x20 && b != b'\t' && b != b' '))
}

fn has_invalid_header_line_bytes(line: &[u8]) -> bool {
    line.iter().any(|&b| b == 0)
}

/// A zero-copy view of a single HTTP header.
#[derive(Debug, Clone, Copy)]
pub struct Header<'a> {
    name: &'a str,
    name_bytes: &'a [u8],
    value: &'a [u8],
}

impl<'a> Header<'a> {
    /// Returns the header name (case-preserved from original).
    #[inline]
    #[must_use]
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// Returns the header name as raw bytes.
    #[inline]
    #[must_use]
    pub fn name_bytes(&self) -> &'a [u8] {
        self.name_bytes
    }

    /// Returns the header as a raw `(&[u8], &[u8])` pair.
    ///
    /// This is useful for zero-allocation header processing.
    #[inline]
    #[must_use]
    pub fn as_bytes_pair(&self) -> (&'a [u8], &'a [u8]) {
        (self.name_bytes, self.value)
    }

    /// Returns the header value as bytes.
    #[inline]
    #[must_use]
    pub fn value(&self) -> &'a [u8] {
        self.value
    }

    /// Returns the header value as a string, if valid UTF-8.
    #[must_use]
    pub fn value_str(&self) -> Option<&'a str> {
        std::str::from_utf8(self.value).ok()
    }

    /// Returns true if this header name matches (case-insensitive).
    #[must_use]
    pub fn name_eq_ignore_case(&self, other: &str) -> bool {
        self.name.eq_ignore_ascii_case(other)
    }

    /// Returns true if this is the Content-Length header.
    #[inline]
    #[must_use]
    pub fn is_content_length(&self) -> bool {
        self.name_eq_ignore_case("content-length")
    }

    /// Returns true if this is the Transfer-Encoding header.
    #[inline]
    #[must_use]
    pub fn is_transfer_encoding(&self) -> bool {
        self.name_eq_ignore_case("transfer-encoding")
    }

    /// Parses the value as Content-Length (usize).
    ///
    /// Returns `None` if this isn't Content-Length or value isn't a valid integer.
    #[must_use]
    pub fn as_content_length(&self) -> Option<usize> {
        if !self.is_content_length() {
            return None;
        }
        self.value_str()?.trim().parse().ok()
    }

    /// Returns true if Transfer-Encoding is exactly "chunked" (case-insensitive,
    /// ignoring optional whitespace). We do NOT accept compound codings like
    /// "gzip, chunked" since the server does not support non-chunked transfer
    /// codings; rejecting them prevents request smuggling via unsupported
    /// intermediate codings.
    #[must_use]
    pub fn is_chunked_encoding(&self) -> bool {
        if !self.is_transfer_encoding() {
            return false;
        }
        self.value_str()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("chunked"))
    }
}

/// Iterator over HTTP headers in a buffer.
///
/// Zero-copy: returns borrowed [`Header`] references into the original buffer.
pub struct HeadersIter<'a> {
    remaining: &'a [u8],
}

impl<'a> HeadersIter<'a> {
    /// Create a new headers iterator from a buffer.
    ///
    /// The buffer should start at the first header line (after the request line).
    #[must_use]
    pub fn new(buffer: &'a [u8]) -> Self {
        Self { remaining: buffer }
    }

    /// Parse a single header from the buffer.
    fn parse_header(line: &'a [u8]) -> Result<Header<'a>, ParseError> {
        if has_invalid_header_line_bytes(line) {
            return Err(ParseError::InvalidHeaderBytes);
        }

        let colon_pos = line
            .iter()
            .position(|&b| b == b':')
            .ok_or(ParseError::InvalidHeader)?;

        let name_bytes = &line[..colon_pos];
        if !is_valid_header_name(name_bytes) {
            return Err(ParseError::InvalidHeaderName);
        }
        let name = std::str::from_utf8(name_bytes).map_err(|_| ParseError::InvalidHeader)?;

        // Trim leading whitespace from value
        let value_start = line[colon_pos + 1..]
            .iter()
            .position(|&b| b != b' ' && b != b'\t')
            .map_or(colon_pos + 1, |p| colon_pos + 1 + p);

        let value = &line[value_start..];
        if has_invalid_header_value_bytes(value) {
            return Err(ParseError::InvalidHeaderBytes);
        }

        // Trim the name (but keep original bytes for raw access)
        let trimmed_name = name.trim();

        Ok(Header {
            name: trimmed_name,
            name_bytes,
            value,
        })
    }
}

impl<'a> Iterator for HeadersIter<'a> {
    type Item = Result<Header<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }

        // Find the end of this header line
        let line_end = self
            .remaining
            .windows(2)
            .position(|w| w == b"\r\n")
            .unwrap_or(self.remaining.len());

        // Empty line signals end of headers
        if line_end == 0 {
            self.remaining = &[];
            return None;
        }

        let line = &self.remaining[..line_end];

        // Advance past this line and its \r\n
        self.remaining = if line_end + 2 <= self.remaining.len() {
            &self.remaining[line_end + 2..]
        } else {
            &[]
        };

        Some(Self::parse_header(line))
    }
}

// ============================================================================
// Headers Collection with Common Helpers
// ============================================================================

/// Body length indicator from headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyLength {
    /// Content-Length header specifies exact byte count.
    ContentLength(usize),
    /// Transfer-Encoding: chunked.
    Chunked,
    /// No body expected (no Content-Length or Transfer-Encoding).
    None,
    /// Multiple conflicting length indicators.
    Conflicting,
}

/// Parses headers and extracts Content-Length / Transfer-Encoding.
///
/// This is a zero-copy parser that yields headers while tracking
/// the body length indicator.
///
/// # Example
///
/// ```ignore
/// let buffer = b"Content-Length: 42\r\nHost: example.com\r\n\r\n";
/// let headers = HeadersParser::parse(buffer)?;
///
/// assert_eq!(headers.body_length(), BodyLength::ContentLength(42));
/// assert_eq!(headers.content_length(), Some(42));
/// ```
pub struct HeadersParser<'a> {
    buffer: &'a [u8],
    bytes_consumed: usize,
    content_length: Option<usize>,
    is_chunked: bool,
}

impl<'a> HeadersParser<'a> {
    /// Parse all headers from a buffer.
    ///
    /// Returns the parser with pre-computed Content-Length and Transfer-Encoding.
    /// The buffer should start at the first header line (after request line).
    pub fn parse(buffer: &'a [u8]) -> Result<Self, ParseError> {
        Self::parse_with_limits(buffer, &ParseLimits::default())
    }

    /// Parse all headers from a buffer with limits.
    ///
    /// Enforces header count, line length, and total size limits. Also
    /// rejects ambiguous body length indicators (Transfer-Encoding + Content-Length).
    pub fn parse_with_limits(buffer: &'a [u8], limits: &ParseLimits) -> Result<Self, ParseError> {
        let header_end = buffer.windows(4).position(|w| w == b"\r\n\r\n");
        let header_block_len = header_end.map_or(buffer.len(), |pos| pos + 4);
        if header_block_len > limits.max_headers_size {
            return Err(ParseError::HeadersTooLarge);
        }

        let mut content_length = None;
        let mut saw_transfer_encoding = false;
        let mut is_chunked = false;
        let mut header_count = 0usize;

        let mut remaining = &buffer[..header_block_len];
        while !remaining.is_empty() {
            let line_end = remaining
                .windows(2)
                .position(|w| w == b"\r\n")
                .unwrap_or(remaining.len());

            if line_end == 0 {
                break;
            }

            if line_end > limits.max_header_line_len {
                return Err(ParseError::HeaderLineTooLong);
            }

            let line = &remaining[..line_end];
            if matches!(line.first(), Some(b' ' | b'\t')) {
                return Err(ParseError::InvalidHeader);
            }

            let header = HeadersIter::parse_header(line)?;
            header_count += 1;
            if header_count > limits.max_header_count {
                return Err(ParseError::TooManyHeaders);
            }

            if header.is_content_length() {
                let len = header
                    .as_content_length()
                    .ok_or(ParseError::InvalidHeader)?;
                if content_length.is_some() && content_length != Some(len) {
                    return Err(ParseError::InvalidHeader);
                }
                content_length = Some(len);
            }

            if header.is_transfer_encoding() {
                saw_transfer_encoding = true;
                if header.is_chunked_encoding() {
                    is_chunked = true;
                } else {
                    return Err(ParseError::InvalidTransferEncoding);
                }
            }

            remaining = if line_end + 2 <= remaining.len() {
                &remaining[line_end + 2..]
            } else {
                &[]
            };
        }

        if saw_transfer_encoding && content_length.is_some() {
            return Err(ParseError::AmbiguousBodyLength);
        }

        let bytes_consumed = header_block_len;

        Ok(Self {
            buffer,
            bytes_consumed,
            content_length,
            is_chunked,
        })
    }

    /// Returns the body length indicator.
    ///
    /// Per RFC 7230:
    /// - If Transfer-Encoding: chunked is present, use chunked decoding
    /// - Else if Content-Length is present, use that
    /// - Else no body
    #[must_use]
    pub fn body_length(&self) -> BodyLength {
        if self.is_chunked {
            if self.content_length.is_some() {
                return BodyLength::Conflicting;
            }
            return BodyLength::Chunked;
        }

        if let Some(len) = self.content_length {
            return BodyLength::ContentLength(len);
        }

        BodyLength::None
    }

    /// Returns the Content-Length value if present.
    #[must_use]
    pub fn content_length(&self) -> Option<usize> {
        self.content_length
    }

    /// Returns true if Transfer-Encoding: chunked.
    #[must_use]
    pub fn is_chunked(&self) -> bool {
        self.is_chunked
    }

    /// Returns the number of bytes consumed (including final \r\n\r\n).
    #[must_use]
    pub fn bytes_consumed(&self) -> usize {
        self.bytes_consumed
    }

    /// Returns an iterator over all headers.
    #[must_use]
    pub fn iter(&self) -> HeadersIter<'a> {
        HeadersIter::new(self.buffer)
    }

    /// Finds a header by name (case-insensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Header<'a>> {
        self.iter()
            .filter_map(Result::ok)
            .find(|h| h.name_eq_ignore_case(name))
    }

    /// Returns all headers matching a name (case-insensitive).
    pub fn get_all<'b>(&'b self, name: &'b str) -> impl Iterator<Item = Header<'a>> + 'b {
        self.iter()
            .filter_map(Result::ok)
            .filter(move |h| h.name_eq_ignore_case(name))
    }
}

// ============================================================================
// High-Level Parser (with owned Request for convenience)
// ============================================================================

/// Zero-copy HTTP request parser.
pub struct Parser {
    limits: ParseLimits,
}

impl Parser {
    /// Create a new parser with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: ParseLimits::default(),
        }
    }

    /// Set maximum request size.
    #[must_use]
    pub fn with_max_size(mut self, size: usize) -> Self {
        self.limits.max_request_size = size;
        self
    }

    /// Set all parsing limits.
    #[must_use]
    pub fn with_limits(mut self, limits: ParseLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Parse an HTTP request from bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the request is malformed.
    pub fn parse(&self, buffer: &[u8]) -> Result<Request, ParseError> {
        if buffer.len() > self.limits.max_request_size {
            return Err(ParseError::TooLarge);
        }

        // Find end of headers
        let header_end = find_header_end(buffer).ok_or(ParseError::Incomplete)?;

        let header_bytes = &buffer[..header_end];
        let body_bytes = &buffer[header_end + 4..]; // Skip \r\n\r\n

        // Parse request line
        let first_line_end = header_bytes
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or(ParseError::InvalidRequestLine)?;
        if first_line_end > self.limits.max_request_line_len {
            return Err(ParseError::RequestLineTooLong);
        }

        let request_line = &header_bytes[..first_line_end];
        let mut request = parse_request_line(request_line)?;

        let header_start = first_line_end + 2;
        let header_block_len = header_end + 4 - header_start;
        if header_block_len > self.limits.max_headers_size {
            return Err(ParseError::HeadersTooLarge);
        }

        // Parse headers
        let headers =
            HeadersParser::parse_with_limits(&buffer[header_start..header_end + 4], &self.limits)?;

        // Set headers (using optimized insert to avoid double allocation)
        for header in headers.iter() {
            let header = header?;
            request
                .headers_mut()
                .insert_from_slice(header.name(), header.value());
        }

        // Set body
        if !body_bytes.is_empty() {
            request.set_body(Body::Bytes(body_bytes.to_vec()));
        }

        Ok(request)
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Incremental Stateful Parser
// ============================================================================

/// Result of an incremental parse attempt.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ParseStatus {
    /// Parsing completed with a request and bytes consumed.
    Complete { request: Request, consumed: usize },
    /// More data is required to complete the request.
    Incomplete,
}

#[derive(Debug)]
enum ParseState {
    RequestLine,
    Headers {
        request: Request,
        header_start: usize,
    },
    Body {
        request: Request,
        body_length: BodyLength,
        body_start: usize,
    },
}

/// Incremental HTTP/1.1 parser that handles partial reads.
///
/// Feed bytes via [`feed`][Self::feed]. When a full request is available,
/// returns [`ParseStatus::Complete`]. On partial data, returns
/// [`ParseStatus::Incomplete`].
pub struct StatefulParser {
    limits: ParseLimits,
    body_config: BodyConfig,
    buffer: Vec<u8>,
    state: ParseState,
}

impl StatefulParser {
    /// Create a new stateful parser with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: ParseLimits::default(),
            body_config: BodyConfig::default(),
            buffer: Vec::new(),
            state: ParseState::RequestLine,
        }
    }

    /// Set maximum request size.
    #[must_use]
    pub fn with_max_size(mut self, size: usize) -> Self {
        self.limits.max_request_size = size;
        self
    }

    /// Set all parsing limits.
    #[must_use]
    pub fn with_limits(mut self, limits: ParseLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Set the body parsing configuration.
    #[must_use]
    pub fn with_body_config(mut self, config: BodyConfig) -> Self {
        self.body_config = config;
        self
    }

    /// Returns the current buffered byte count.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Take the currently buffered (unconsumed) bytes.
    ///
    /// This is primarily used for protocol upgrades (e.g., WebSocket) where the HTTP parser
    /// must hand off any already-read bytes to the next protocol layer.
    #[must_use]
    pub fn take_buffered(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffer)
    }

    /// Clear buffered data and reset state.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.state = ParseState::RequestLine;
    }

    /// Feed new bytes into the parser and attempt to parse a request.
    ///
    /// To parse subsequent requests in the buffer, call `feed` again with
    /// an empty slice after a successful parse.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<ParseStatus, ParseError> {
        if !bytes.is_empty() {
            self.buffer.extend_from_slice(bytes);
        }

        if self.buffer.len() > self.limits.max_request_size {
            return Err(ParseError::TooLarge);
        }

        loop {
            let state = std::mem::replace(&mut self.state, ParseState::RequestLine);
            match state {
                ParseState::RequestLine => match parse_request_line_with_len_limit(
                    &self.buffer,
                    self.limits.max_request_line_len,
                ) {
                    Ok((request, header_start)) => {
                        self.state = ParseState::Headers {
                            request,
                            header_start,
                        };
                    }
                    Err(ParseError::Incomplete) => {
                        if self.buffer.len() > self.limits.max_request_line_len {
                            self.state = ParseState::RequestLine;
                            return Err(ParseError::RequestLineTooLong);
                        }
                        self.state = ParseState::RequestLine;
                        return Ok(ParseStatus::Incomplete);
                    }
                    Err(err) => return Err(err),
                },
                ParseState::Headers {
                    mut request,
                    header_start,
                } => {
                    let header_end = match find_header_end_from(&self.buffer, header_start) {
                        Some(pos) => pos,
                        None => {
                            if self.buffer.len().saturating_sub(header_start)
                                > self.limits.max_headers_size
                            {
                                self.state = ParseState::Headers {
                                    request,
                                    header_start,
                                };
                                return Err(ParseError::HeadersTooLarge);
                            }
                            self.state = ParseState::Headers {
                                request,
                                header_start,
                            };
                            return Ok(ParseStatus::Incomplete);
                        }
                    };

                    let body_start = header_end + 4;
                    let header_block_len = body_start - header_start;
                    if header_block_len > self.limits.max_headers_size {
                        return Err(ParseError::HeadersTooLarge);
                    }
                    let header_slice = &self.buffer[header_start..body_start];
                    let headers = HeadersParser::parse_with_limits(header_slice, &self.limits)?;

                    // Use optimized insert to avoid double allocation
                    for header in headers.iter() {
                        let header = header?;
                        request
                            .headers_mut()
                            .insert_from_slice(header.name(), header.value());
                    }

                    let body_length = headers.body_length();
                    if matches!(body_length, BodyLength::None) {
                        let consumed = body_start;
                        self.consume(consumed);
                        return Ok(ParseStatus::Complete { request, consumed });
                    }

                    self.state = ParseState::Body {
                        request,
                        body_length,
                        body_start,
                    };
                }
                ParseState::Body {
                    mut request,
                    body_length,
                    body_start,
                } => {
                    let body_slice = &self.buffer[body_start..];
                    match parse_body_with_consumed(body_slice, body_length, &self.body_config) {
                        Ok((body, body_consumed)) => {
                            if let Some(body) = body {
                                request.set_body(Body::Bytes(body));
                            }
                            let consumed = body_start + body_consumed;
                            self.consume(consumed);
                            return Ok(ParseStatus::Complete { request, consumed });
                        }
                        Err(err) => {
                            let mapped = map_body_error(err);
                            if matches!(mapped, ParseError::Incomplete) {
                                self.state = ParseState::Body {
                                    request,
                                    body_length,
                                    body_start,
                                };
                                return Ok(ParseStatus::Incomplete);
                            }
                            return Err(mapped);
                        }
                    }
                }
            }
        }
    }

    fn consume(&mut self, consumed: usize) {
        if consumed >= self.buffer.len() {
            self.buffer.clear();
        } else {
            self.buffer.drain(..consumed);
        }
        self.state = ParseState::RequestLine;
    }
}

impl Default for StatefulParser {
    fn default() -> Self {
        Self::new()
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

fn find_header_end_from(buffer: &[u8], start: usize) -> Option<usize> {
    find_header_end(&buffer[start..]).map(|pos| start + pos)
}

fn parse_request_line_with_len_limit(
    buffer: &[u8],
    max_len: usize,
) -> Result<(Request, usize), ParseError> {
    let line_end = buffer
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or(ParseError::Incomplete)?;
    if line_end > max_len {
        return Err(ParseError::RequestLineTooLong);
    }
    let request = parse_request_line(&buffer[..line_end])?;
    Ok((request, line_end + 2))
}

fn map_body_error(error: BodyError) -> ParseError {
    match error {
        BodyError::TooLarge { .. } => ParseError::TooLarge,
        BodyError::Incomplete { .. } | BodyError::UnexpectedEof => ParseError::Incomplete,
        BodyError::Parse(err) => err,
        BodyError::InvalidChunkedEncoding { .. } => ParseError::InvalidHeader,
    }
}

fn parse_request_line(line: &[u8]) -> Result<Request, ParseError> {
    if has_invalid_request_line_bytes(line) {
        return Err(ParseError::InvalidRequestLine);
    }
    let mut parts = line.split(|&b| b == b' ');

    let method_bytes = parts.next().ok_or(ParseError::InvalidRequestLine)?;
    let method = Method::from_bytes(method_bytes).ok_or(ParseError::InvalidMethod)?;

    let uri_bytes = parts.next().ok_or(ParseError::InvalidRequestLine)?;
    let uri = std::str::from_utf8(uri_bytes).map_err(|_| ParseError::InvalidRequestLine)?;

    // Parse HTTP version
    let version_bytes = parts.next().ok_or(ParseError::InvalidRequestLine)?;
    let version_str =
        std::str::from_utf8(version_bytes).map_err(|_| ParseError::InvalidRequestLine)?;
    let http_version = HttpVersion::parse(version_str).ok_or(ParseError::InvalidRequestLine)?;

    // Reject request lines with extra parts (RFC 7230 Section 3.1.1)
    if parts.next().is_some() {
        return Err(ParseError::InvalidRequestLine);
    }

    // Split path and query
    let (path, query) = if let Some(q_pos) = uri.find('?') {
        (
            percent_decode_path(&uri[..q_pos])?,
            Some(uri[q_pos + 1..].to_string()),
        )
    } else {
        (percent_decode_path(uri)?, None)
    };

    let path = match path {
        Cow::Borrowed(borrowed) => borrowed.to_string(),
        Cow::Owned(owned) => owned,
    };

    let mut request = Request::with_version(method, path, http_version);
    request.set_query(query);
    request.set_raw_target(uri);
    Ok(request)
}

/// Percent-decode a path segment.
///
/// Returns `Cow::Borrowed` if no decoding was needed, or `Cow::Owned` if
/// percent sequences were decoded. Plus signs are preserved (no space decoding).
///
/// Invalid percent sequences are left as-is.
fn percent_decode_path(s: &str) -> Result<Cow<'_, str>, ParseError> {
    if !s.contains('%') {
        return Ok(Cow::Borrowed(s));
    }

    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    result.push(hi << 4 | lo);
                    i += 3;
                } else {
                    result.push(b'%');
                    i += 1;
                }
            }
            b => {
                result.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8(result)
        .map(Cow::Owned)
        .map_err(|_| ParseError::InvalidRequestLine)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // RequestLine Tests
    // ========================================================================

    #[test]
    fn percent_decode_path_no_encoding() {
        let decoded = percent_decode_path("/simple/path").unwrap();
        assert!(matches!(decoded, Cow::Borrowed(_)));
        assert_eq!(&*decoded, "/simple/path");
    }

    #[test]
    fn percent_decode_path_simple() {
        assert_eq!(
            &*percent_decode_path("/hello%20world").unwrap(),
            "/hello world"
        );
        assert_eq!(&*percent_decode_path("%2F").unwrap(), "/");
    }

    #[test]
    fn percent_decode_path_utf8() {
        assert_eq!(&*percent_decode_path("/caf%C3%A9").unwrap(), "/café");
    }

    #[test]
    fn percent_decode_path_plus_preserved() {
        assert_eq!(&*percent_decode_path("/a+b").unwrap(), "/a+b");
    }

    #[test]
    fn percent_decode_path_rejects_invalid_utf8() {
        // %C0%AF is an overlong encoding of '/' — produces invalid UTF-8
        assert!(percent_decode_path("/%C0%AF").is_err());
    }

    #[test]
    fn parse_request_line_decodes_path() {
        let line = b"GET /hello%20world HTTP/1.1";
        let request = parse_request_line(line).expect("parse request line");
        assert_eq!(request.path(), "/hello world");
        assert_eq!(request.raw_target(), Some("/hello%20world"));
    }

    #[test]
    fn owned_parser_preserves_encoded_target_and_query_duplicates() {
        let request = Parser::new()
            .parse(b"GET /a%3Fpart%23frag%2Fend?tag=a&tag=b&next=x%26y HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .expect("encoded request parses");
        assert_eq!(request.path(), "/a?part#frag/end");
        assert_eq!(request.query(), Some("tag=a&tag=b&next=x%26y"));
        assert_eq!(
            request.raw_target(),
            Some("/a%3Fpart%23frag%2Fend?tag=a&tag=b&next=x%26y")
        );
    }

    #[test]
    fn incremental_parser_preserves_target_across_partial_headers_and_body() {
        let mut parser = StatefulParser::new();
        assert!(matches!(
            parser
                .feed(b"POST /caf%C3%A9%3Fpart?x=1&x=2 HTTP/1.1\r\nHost: exam")
                .unwrap(),
            ParseStatus::Incomplete
        ));
        assert!(matches!(
            parser
                .feed(b"ple.com\r\nContent-Length: 3\r\n\r\nab")
                .unwrap(),
            ParseStatus::Incomplete
        ));
        let ParseStatus::Complete { request, .. } = parser.feed(b"c").unwrap() else {
            panic!("complete body must complete the request");
        };
        assert_eq!(request.path(), "/café?part");
        assert_eq!(request.query(), Some("x=1&x=2"));
        assert_eq!(request.raw_target(), Some("/caf%C3%A9%3Fpart?x=1&x=2"));
        assert!(matches!(request.body(), Body::Bytes(body) if body == b"abc"));
    }

    #[test]
    fn raw_target_preserves_unsupported_forms_without_changing_parser_acceptance() {
        for target in ["@evil.example/", "http://example.com/", "*"] {
            let bytes = format!("GET {target} HTTP/1.1\r\nHost: example.com\r\n\r\n");
            let request = Parser::new()
                .parse(bytes.as_bytes())
                .expect("existing target acceptance");
            assert_eq!(request.raw_target(), Some(target));
        }
    }

    #[test]
    fn request_line_simple_get() {
        let buffer = b"GET /path HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.method(), Method::Get);
        assert_eq!(line.path(), "/path");
        assert_eq!(line.query(), None);
        assert_eq!(line.version(), "HTTP/1.1");
        assert!(line.is_http11());
        assert!(!line.is_http10());
    }

    #[test]
    fn request_line_with_query() {
        let buffer = b"GET /items?q=test&page=1 HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.method(), Method::Get);
        assert_eq!(line.uri(), "/items?q=test&page=1");
        assert_eq!(line.path(), "/items");
        assert_eq!(line.query(), Some("q=test&page=1"));
        assert_eq!(line.version(), "HTTP/1.1");
    }

    #[test]
    fn request_line_uri_without_query_matches_path() {
        let buffer = b"GET /path HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert_eq!(line.uri(), "/path");
        assert_eq!(line.path(), "/path");
        assert_eq!(line.query(), None);
    }

    #[test]
    fn request_line_post() {
        let buffer = b"POST /api/users HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.method(), Method::Post);
        assert_eq!(line.path(), "/api/users");
    }

    #[test]
    fn request_line_all_methods() {
        let methods = [
            ("GET", Method::Get),
            ("POST", Method::Post),
            ("PUT", Method::Put),
            ("DELETE", Method::Delete),
            ("PATCH", Method::Patch),
            ("OPTIONS", Method::Options),
            ("HEAD", Method::Head),
            ("TRACE", Method::Trace),
        ];

        for (method_str, expected_method) in methods {
            let buffer = format!("{method_str} /path HTTP/1.1\r\n");
            let line = RequestLine::parse(buffer.as_bytes()).unwrap();
            assert_eq!(line.method(), expected_method, "Failed for {method_str}");
        }
    }

    #[test]
    fn request_line_http10() {
        let buffer = b"GET /legacy HTTP/1.0\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.version(), "HTTP/1.0");
        assert!(line.is_http10());
        assert!(!line.is_http11());
    }

    #[test]
    fn request_line_without_crlf() {
        // Should still parse if no \r\n (end of buffer)
        let buffer = b"GET /path HTTP/1.1";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.method(), Method::Get);
        assert_eq!(line.path(), "/path");
    }

    #[test]
    fn request_line_with_len() {
        let buffer = b"GET /path HTTP/1.1\r\nHost: example.com\r\n";
        let (line, consumed) = RequestLine::parse_with_len(buffer).unwrap();

        assert_eq!(line.method(), Method::Get);
        assert_eq!(line.path(), "/path");
        assert_eq!(consumed, 20); // "GET /path HTTP/1.1\r\n" = 20 bytes
    }

    #[test]
    fn request_line_invalid_method() {
        let buffer = b"INVALID /path HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);

        assert!(matches!(result, Err(ParseError::InvalidMethod)));
    }

    #[test]
    fn request_line_missing_path() {
        let buffer = b"GET\r\n";
        let result = RequestLine::parse(buffer);

        assert!(matches!(result, Err(ParseError::InvalidRequestLine)));
    }

    #[test]
    fn request_line_missing_version() {
        let buffer = b"GET /path\r\n";
        let result = RequestLine::parse(buffer);

        assert!(matches!(result, Err(ParseError::InvalidRequestLine)));
    }

    #[test]
    fn request_line_complex_path() {
        let buffer = b"GET /api/v1/users/123/posts HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.path(), "/api/v1/users/123/posts");
    }

    #[test]
    fn request_line_query_with_special_chars() {
        let buffer = b"GET /search?q=hello%20world&filter=a%3Db HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        assert_eq!(line.path(), "/search");
        assert_eq!(line.query(), Some("q=hello%20world&filter=a%3Db"));
    }

    // ========================================================================
    // Header Tests
    // ========================================================================

    #[test]
    fn header_simple() {
        let buffer = b"Host: example.com\r\n";
        let mut iter = HeadersIter::new(buffer);

        let header = iter.next().unwrap().unwrap();
        assert_eq!(header.name(), "Host");
        assert_eq!(header.value(), b"example.com");
        assert_eq!(header.value_str(), Some("example.com"));

        assert!(iter.next().is_none());
    }

    #[test]
    fn headers_multiple() {
        let buffer =
            b"Host: example.com\r\nContent-Type: application/json\r\nContent-Length: 42\r\n";
        let headers: Vec<_> = HeadersIter::new(buffer).collect();

        assert_eq!(headers.len(), 3);

        let h0 = headers[0].as_ref().unwrap();
        assert_eq!(h0.name(), "Host");
        assert_eq!(h0.value_str(), Some("example.com"));

        let h1 = headers[1].as_ref().unwrap();
        assert_eq!(h1.name(), "Content-Type");
        assert_eq!(h1.value_str(), Some("application/json"));

        let h2 = headers[2].as_ref().unwrap();
        assert_eq!(h2.name(), "Content-Length");
        assert_eq!(h2.value_str(), Some("42"));
    }

    #[test]
    fn headers_with_empty_line() {
        let buffer = b"Host: example.com\r\n\r\n";
        let headers: Vec<_> = HeadersIter::new(buffer).collect();

        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn header_with_leading_space() {
        let buffer = b"Host:   example.com\r\n";
        let mut iter = HeadersIter::new(buffer);

        let header = iter.next().unwrap().unwrap();
        assert_eq!(header.name(), "Host");
        assert_eq!(header.value_str(), Some("example.com"));
    }

    #[test]
    fn header_binary_value() {
        let buffer = b"X-Binary: \xff\xfe\r\n";
        let mut iter = HeadersIter::new(buffer);

        let header = iter.next().unwrap().unwrap();
        assert_eq!(header.name(), "X-Binary");
        assert_eq!(header.value(), b"\xff\xfe");
        assert!(header.value_str().is_none()); // Invalid UTF-8
    }

    #[test]
    fn header_missing_colon() {
        let buffer = b"InvalidHeader\r\n";
        let mut iter = HeadersIter::new(buffer);

        let result = iter.next().unwrap();
        assert!(matches!(result, Err(ParseError::InvalidHeader)));
    }

    // ========================================================================
    // Zero-Copy Verification Tests
    // ========================================================================

    #[test]
    fn request_line_borrows_from_buffer() {
        let buffer = b"GET /borrowed/path HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();

        // Verify that path is a slice into the original buffer
        // Using safe pointer range comparison
        let buffer_range = buffer.as_ptr_range();
        let path_ptr = line.path().as_ptr();

        // Path pointer should be within buffer bounds
        assert!(buffer_range.contains(&path_ptr));
    }

    #[test]
    fn header_borrows_from_buffer() {
        let buffer = b"Host: borrowed.example.com\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        // Verify that value is a slice into the original buffer
        // Using safe pointer range comparison
        let buffer_range = buffer.as_ptr_range();
        let value_ptr = header.value().as_ptr();

        assert!(buffer_range.contains(&value_ptr));
    }

    // ========================================================================
    // Header Enhancement Tests
    // ========================================================================

    #[test]
    fn header_as_bytes_pair() {
        let buffer = b"Content-Type: application/json\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        let (name, value) = header.as_bytes_pair();
        assert_eq!(name, b"Content-Type");
        assert_eq!(value, b"application/json");
    }

    #[test]
    fn header_name_bytes() {
        let buffer = b"X-Custom-Header: value\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert_eq!(header.name_bytes(), b"X-Custom-Header");
        assert_eq!(header.name(), "X-Custom-Header");
    }

    #[test]
    fn header_case_insensitive_match() {
        let buffer = b"content-type: text/html\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert!(header.name_eq_ignore_case("Content-Type"));
        assert!(header.name_eq_ignore_case("CONTENT-TYPE"));
        assert!(header.name_eq_ignore_case("content-type"));
        assert!(!header.name_eq_ignore_case("Content-Length"));
    }

    #[test]
    fn header_content_length_detection() {
        let buffer = b"Content-Length: 1234\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert!(header.is_content_length());
        assert!(!header.is_transfer_encoding());
        assert_eq!(header.as_content_length(), Some(1234));
    }

    #[test]
    fn header_content_length_case_insensitive() {
        let buffer = b"content-length: 5678\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert!(header.is_content_length());
        assert_eq!(header.as_content_length(), Some(5678));
    }

    #[test]
    fn header_transfer_encoding_chunked() {
        let buffer = b"Transfer-Encoding: chunked\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert!(header.is_transfer_encoding());
        assert!(header.is_chunked_encoding());
        assert!(!header.is_content_length());
    }

    #[test]
    fn header_transfer_encoding_gzip_chunked_is_not_chunked() {
        // Compound TE like "gzip, chunked" must NOT be accepted as chunked
        // since the server does not support gzip transfer coding.
        let buffer = b"Transfer-Encoding: gzip, chunked\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert!(header.is_transfer_encoding());
        assert!(!header.is_chunked_encoding());
    }

    #[test]
    fn header_transfer_encoding_not_chunked() {
        let buffer = b"Transfer-Encoding: gzip\r\n";
        let mut iter = HeadersIter::new(buffer);
        let header = iter.next().unwrap().unwrap();

        assert!(header.is_transfer_encoding());
        assert!(!header.is_chunked_encoding());
    }

    // ========================================================================
    // HeadersParser Tests
    // ========================================================================

    #[test]
    fn headers_parser_content_length() {
        let buffer = b"Host: example.com\r\nContent-Length: 42\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        assert_eq!(parser.content_length(), Some(42));
        assert!(!parser.is_chunked());
        assert_eq!(parser.body_length(), BodyLength::ContentLength(42));
    }

    #[test]
    fn headers_parser_chunked() {
        let buffer = b"Host: example.com\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        assert_eq!(parser.content_length(), None);
        assert!(parser.is_chunked());
        assert_eq!(parser.body_length(), BodyLength::Chunked);
    }

    #[test]
    fn headers_parser_no_body() {
        let buffer = b"Host: example.com\r\nAccept: */*\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        assert_eq!(parser.content_length(), None);
        assert!(!parser.is_chunked());
        assert_eq!(parser.body_length(), BodyLength::None);
    }

    #[test]
    fn headers_parser_chunked_takes_precedence() {
        // Strict parsing rejects ambiguous body length.
        let buffer =
            b"Host: example.com\r\nContent-Length: 100\r\nTransfer-Encoding: chunked\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        assert!(matches!(result, Err(ParseError::AmbiguousBodyLength)));
    }

    #[test]
    fn headers_parser_invalid_transfer_encoding() {
        let buffer = b"Host: example.com\r\nTransfer-Encoding: gzip\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        assert!(matches!(result, Err(ParseError::InvalidTransferEncoding)));
    }

    #[test]
    fn headers_parser_bytes_consumed() {
        let buffer = b"Host: example.com\r\nContent-Length: 5\r\n\r\nHello";
        let parser = HeadersParser::parse(buffer).unwrap();

        // Headers section is "Host: example.com\r\nContent-Length: 5\r\n\r\n" = 40 bytes
        assert_eq!(parser.bytes_consumed(), 40);
    }

    #[test]
    fn headers_parser_get_header() {
        let buffer = b"Host: example.com\r\nContent-Type: application/json\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        let host = parser.get("Host").unwrap();
        assert_eq!(host.value_str(), Some("example.com"));

        let ct = parser.get("content-type").unwrap();
        assert_eq!(ct.value_str(), Some("application/json"));

        assert!(parser.get("X-Missing").is_none());
    }

    #[test]
    fn headers_parser_get_all() {
        let buffer = b"Set-Cookie: a=1\r\nSet-Cookie: b=2\r\nHost: example.com\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        let cookies: Vec<_> = parser.get_all("Set-Cookie").collect();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].value_str(), Some("a=1"));
        assert_eq!(cookies[1].value_str(), Some("b=2"));
    }

    #[test]
    fn headers_parser_iter() {
        let buffer = b"A: 1\r\nB: 2\r\nC: 3\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        let headers: Vec<_> = parser.iter().collect();
        assert_eq!(headers.len(), 3);
    }

    #[test]
    fn headers_parser_conflicting_content_length() {
        // Two different Content-Length values should error
        let buffer = b"Content-Length: 10\r\nContent-Length: 20\r\n\r\n";
        let result = HeadersParser::parse(buffer);

        assert!(matches!(result, Err(ParseError::InvalidHeader)));
    }

    #[test]
    fn headers_parser_duplicate_same_content_length() {
        // Two identical Content-Length values are OK (per some implementations)
        let buffer = b"Content-Length: 42\r\nContent-Length: 42\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();

        assert_eq!(parser.content_length(), Some(42));
    }

    // ========================================================================
    // Header Limits / Security Tests
    // ========================================================================

    #[test]
    fn headers_parser_too_many_headers() {
        let mut limits = ParseLimits::default();
        limits.max_header_count = 1;
        let buffer = b"A: 1\r\nB: 2\r\n\r\n";
        let result = HeadersParser::parse_with_limits(buffer, &limits);
        assert!(matches!(result, Err(ParseError::TooManyHeaders)));
    }

    #[test]
    fn headers_parser_header_line_too_long() {
        let mut limits = ParseLimits::default();
        limits.max_header_line_len = 8;
        let buffer = b"Long-Header: 123\r\n\r\n";
        let result = HeadersParser::parse_with_limits(buffer, &limits);
        assert!(matches!(result, Err(ParseError::HeaderLineTooLong)));
    }

    #[test]
    fn headers_parser_headers_too_large() {
        let mut limits = ParseLimits::default();
        limits.max_headers_size = 7;
        let buffer = b"A: 1\r\n\r\n";
        let result = HeadersParser::parse_with_limits(buffer, &limits);
        assert!(matches!(result, Err(ParseError::HeadersTooLarge)));
    }

    #[test]
    fn headers_parser_invalid_header_name() {
        let buffer = b"Bad Header: value\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        assert!(matches!(result, Err(ParseError::InvalidHeaderName)));
    }

    #[test]
    fn headers_parser_invalid_header_bytes() {
        let buffer = b"X-Test: hi\0there\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        assert!(matches!(result, Err(ParseError::InvalidHeaderBytes)));
    }

    #[test]
    fn request_line_too_long() {
        let mut limits = ParseLimits::default();
        limits.max_request_line_len = 8;
        let mut parser = StatefulParser::new().with_limits(limits);
        let result = parser.feed(b"GET /toolong HTTP/1.1\r\n\r\n");
        assert!(matches!(result, Err(ParseError::RequestLineTooLong)));
    }

    // ========================================================================
    // Stateful Parser Tests
    // ========================================================================

    #[test]
    fn stateful_parser_content_length_partial() {
        let mut parser = StatefulParser::new();
        let full = b"GET /hello HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nHello";

        let status = parser.feed(&full[..full.len() - 3]).unwrap();
        assert!(matches!(status, ParseStatus::Incomplete));

        let status = parser.feed(&full[full.len() - 3..]).unwrap();
        let (request, consumed) = match status {
            ParseStatus::Complete { request, consumed } => (request, consumed),
            ParseStatus::Incomplete => panic!("expected complete request"),
        };

        assert_eq!(consumed, full.len());
        assert_eq!(request.method(), Method::Get);
        assert_eq!(request.path(), "/hello");
        assert!(request.query().is_none());

        match request.body() {
            Body::Bytes(bytes) => assert_eq!(bytes, b"Hello"),
            _ => panic!("expected bytes body"),
        }

        assert_eq!(parser.buffered_len(), 0);
    }

    #[test]
    fn stateful_parser_headers_partial() {
        let mut parser = StatefulParser::new();
        let part1 = b"GET /x HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n";
        let part2 = b"\r\nHello";

        let status = parser.feed(part1).unwrap();
        assert!(matches!(status, ParseStatus::Incomplete));

        let status = parser.feed(part2).unwrap();
        let (request, consumed) = match status {
            ParseStatus::Complete { request, consumed } => (request, consumed),
            ParseStatus::Incomplete => panic!("expected complete request"),
        };

        assert_eq!(consumed, part1.len() + part2.len());
        assert_eq!(request.path(), "/x");
        match request.body() {
            Body::Bytes(bytes) => assert_eq!(bytes, b"Hello"),
            _ => panic!("expected bytes body"),
        }
    }

    #[test]
    fn stateful_parser_chunked_body() {
        let mut parser = StatefulParser::new();
        let full = b"GET /chunk HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";

        let status = parser.feed(full).unwrap();
        let (request, consumed) = match status {
            ParseStatus::Complete { request, consumed } => (request, consumed),
            ParseStatus::Incomplete => panic!("expected complete request"),
        };

        assert_eq!(consumed, full.len());
        assert_eq!(request.path(), "/chunk");
        match request.body() {
            Body::Bytes(bytes) => assert_eq!(bytes, b"Wikipedia"),
            _ => panic!("expected bytes body"),
        }
    }

    #[test]
    fn stateful_parser_pipelined_requests() {
        let mut parser = StatefulParser::new();
        let req1 = b"GET /a HTTP/1.1\r\nContent-Length: 1\r\n\r\na";
        let req2 = b"GET /b HTTP/1.1\r\nContent-Length: 1\r\n\r\nb";
        let mut combined = Vec::new();
        combined.extend_from_slice(req1);
        combined.extend_from_slice(req2);

        let status = parser.feed(&combined).unwrap();
        let (request, consumed) = match status {
            ParseStatus::Complete { request, consumed } => (request, consumed),
            ParseStatus::Incomplete => panic!("expected complete request"),
        };

        assert_eq!(consumed, req1.len());
        assert_eq!(request.path(), "/a");
        assert_eq!(parser.buffered_len(), req2.len());

        let status = parser.feed(&[]).unwrap();
        let (request, consumed) = match status {
            ParseStatus::Complete { request, consumed } => (request, consumed),
            ParseStatus::Incomplete => panic!("expected second request"),
        };

        assert_eq!(consumed, req2.len());
        assert_eq!(request.path(), "/b");
        assert_eq!(parser.buffered_len(), 0);
    }

    // ========================================================================
    // HTTP Request Smuggling Tests (Security)
    // ========================================================================

    #[test]
    fn security_rejects_cl_te_smuggling_attempt() {
        // CL.TE smuggling: Both Content-Length and Transfer-Encoding present
        // This should be rejected as ambiguous per RFC 7230
        let buffer =
            b"POST /admin HTTP/1.1\r\nContent-Length: 13\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nSMUGGLED";
        let parser = Parser::new();
        let result = parser.parse(buffer);
        // Should fail due to ambiguous body length
        assert!(
            result.is_err() || {
                // If it doesn't fail, at least verify it doesn't parse the smuggled content
                let req = result.unwrap();
                !matches!(req.body(), Body::Bytes(b) if b == b"SMUGGLED")
            }
        );
    }

    #[test]
    fn security_ambiguous_body_length_rejected() {
        // Headers-only test: both CL and TE present
        let buffer = b"Content-Length: 100\r\nTransfer-Encoding: chunked\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        assert!(matches!(result, Err(ParseError::AmbiguousBodyLength)));
    }

    #[test]
    fn security_crlf_injection_in_path_rejected() {
        // Attempt to inject a header via CRLF in path
        let buffer = b"GET /path\r\nX-Injected: evil HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let result = RequestLine::parse(buffer);
        // This should either fail or not include the injected header in the path
        match result {
            Err(_) => {} // Good - rejected
            Ok(line) => {
                // If parsed, the path should not contain CRLF
                assert!(!line.path().contains('\r'));
                assert!(!line.path().contains('\n'));
            }
        }
    }

    #[test]
    fn security_null_byte_in_request_line_rejected() {
        let buffer = b"GET /path\x00evil HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);
        assert!(matches!(result, Err(ParseError::InvalidRequestLine)));
    }

    #[test]
    fn security_null_byte_in_header_name_rejected() {
        let buffer = b"X-Test\x00Header: value\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        assert!(result.is_err());
    }

    #[test]
    fn security_header_injection_via_value() {
        // Header value with CRLF should be rejected
        let buffer = b"X-Test: value\r\nX-Injected: evil\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        // The parser should see "X-Test" and "X-Injected" as separate headers
        // (which is normal behavior) - this tests that CRLF in values doesn't
        // create extra headers unexpectedly
        assert!(parser.get("X-Test").is_some());
        assert!(parser.get("X-Injected").is_some());
    }

    #[test]
    fn security_oversized_chunk_size_rejected() {
        // Attempt to use a chunk size that overflows usize (must be rejected).
        let mut parser = StatefulParser::new();
        let buffer = b"GET /chunk HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFF\r\n";
        let result = parser.feed(buffer);
        assert!(matches!(result, Err(ParseError::InvalidHeader)));
    }

    #[test]
    fn security_negative_content_length_rejected() {
        // Content-Length with non-numeric value
        let buffer = b"Content-Length: -1\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        // Should fail because -1 is not a valid usize
        assert!(matches!(result, Err(ParseError::InvalidHeader)));
    }

    #[test]
    fn security_content_length_overflow() {
        // Extremely large Content-Length
        let buffer = b"Content-Length: 99999999999999999999999999\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        // Should fail due to parse error
        assert!(matches!(result, Err(ParseError::InvalidHeader)));
    }

    #[test]
    fn security_request_line_space_injection() {
        // Extra spaces shouldn't create unexpected behavior
        let buffer = b"GET  /path  HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);
        // Should handle gracefully - either reject or parse with extra spaces in path
        match result {
            Ok(line) => {
                // If parsed, path includes leading space (zero-copy parser is lenient)
                // The important thing is it doesn't crash and we can inspect the result
                let _ = line.path();
                let _ = line.version();
            }
            Err(_) => {} // Also acceptable to reject
        }
    }

    #[test]
    fn security_obs_fold_header_rejected() {
        // Obsolete line folding (RFC 7230 deprecated)
        // Line starting with space/tab is continuation of previous header
        let buffer = b"X-Test: value\r\n continuation\r\n\r\n";
        let result = HeadersParser::parse(buffer);
        // Should reject obs-fold per RFC 7230
        assert!(matches!(result, Err(ParseError::InvalidHeader)));
    }

    #[test]
    fn security_duplicate_transfer_encoding() {
        // Multiple Transfer-Encoding headers
        let buffer = b"Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        // Should still recognize as chunked (parser handles duplicates)
        assert!(parser.is_chunked());
    }

    // ========================================================================
    // Edge Case Tests - Request Line
    // ========================================================================

    #[test]
    fn edge_root_path() {
        let buffer = b"GET / HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert_eq!(line.path(), "/");
        assert_eq!(line.query(), None);
    }

    #[test]
    fn edge_root_path_with_query() {
        let buffer = b"GET /?key=value HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert_eq!(line.path(), "/");
        assert_eq!(line.query(), Some("key=value"));
    }

    #[test]
    fn edge_empty_query_string() {
        let buffer = b"GET /path? HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert_eq!(line.path(), "/path");
        assert_eq!(line.query(), Some(""));
    }

    #[test]
    fn edge_double_slashes_in_path() {
        let buffer = b"GET //api//v1//users HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert_eq!(line.path(), "//api//v1//users");
    }

    #[test]
    fn edge_dot_segments_in_path() {
        let buffer = b"GET /api/../admin/./config HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        // Parser doesn't normalize - that's router's job
        assert_eq!(line.path(), "/api/../admin/./config");
    }

    #[test]
    fn edge_percent_encoded_slash() {
        let buffer = b"GET /path%2Fwith%2Fslashes HTTP/1.1\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        // Zero-copy parser keeps encoding
        assert!(line.path().contains("%2F") || line.path().contains("/with/"));
    }

    #[test]
    fn edge_very_long_path() {
        let long_path = "/".to_string() + &"a".repeat(4000);
        let buffer = format!("GET {} HTTP/1.1\r\n", long_path);
        let line = RequestLine::parse(buffer.as_bytes()).unwrap();
        assert_eq!(line.path().len(), 4001);
    }

    #[test]
    fn edge_unicode_in_path() {
        // UTF-8 bytes for "café"
        let buffer = b"GET /caf\xc3\xa9 HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);
        // Should handle UTF-8 gracefully
        match result {
            Ok(line) => assert!(line.path().len() > 0),
            Err(_) => {} // Also acceptable to reject
        }
    }

    #[test]
    fn edge_http_version_http10() {
        let buffer = b"GET /path HTTP/1.0\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert!(line.is_http10());
        assert!(!line.is_http11());
    }

    #[test]
    fn edge_http_version_unknown() {
        let buffer = b"GET /path HTTP/2.0\r\n";
        let line = RequestLine::parse(buffer).unwrap();
        assert_eq!(line.version(), "HTTP/2.0");
        assert!(!line.is_http10());
        assert!(!line.is_http11());
    }

    #[test]
    fn edge_lowercase_method_rejected() {
        let buffer = b"get /path HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);
        assert!(matches!(result, Err(ParseError::InvalidMethod)));
    }

    #[test]
    fn edge_mixed_case_method_rejected() {
        let buffer = b"Get /path HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);
        assert!(matches!(result, Err(ParseError::InvalidMethod)));
    }

    #[test]
    fn edge_connect_method() {
        let buffer = b"CONNECT example.com:443 HTTP/1.1\r\n";
        let result = RequestLine::parse(buffer);
        // CONNECT might not be supported
        match result {
            Ok(line) => assert_eq!(line.path(), "example.com:443"),
            Err(ParseError::InvalidMethod) => {} // Also acceptable
            Err(_) => panic!("unexpected error"),
        }
    }

    // ========================================================================
    // Edge Case Tests - Headers
    // ========================================================================

    #[test]
    fn edge_empty_header_value() {
        let buffer = b"X-Empty:\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        let header = parser.get("X-Empty").unwrap();
        assert_eq!(header.value(), b"");
    }

    #[test]
    fn edge_header_with_only_spaces() {
        let buffer = b"X-Spaces:   \r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        let header = parser.get("X-Spaces").unwrap();
        // Leading whitespace trimmed, trailing may remain
        assert!(header.value().is_empty() || header.value() == b"   ");
    }

    #[test]
    fn edge_very_long_header_value() {
        let long_value = "x".repeat(7000);
        let buffer = format!("X-Long: {}\r\n\r\n", long_value);
        let parser = HeadersParser::parse(buffer.as_bytes()).unwrap();
        let header = parser.get("X-Long").unwrap();
        assert_eq!(header.value().len(), 7000);
    }

    #[test]
    fn edge_header_with_colon_in_value() {
        let buffer = b"X-Time: 12:30:45\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        let header = parser.get("X-Time").unwrap();
        assert_eq!(header.value_str(), Some("12:30:45"));
    }

    #[test]
    fn edge_header_name_with_numbers() {
        let buffer = b"X-Header-123: value\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        assert!(parser.get("X-Header-123").is_some());
    }

    #[test]
    fn edge_header_value_with_utf8() {
        let buffer = "X-Message: Hello, 世界!\r\n\r\n".as_bytes();
        let parser = HeadersParser::parse(buffer).unwrap();
        let header = parser.get("X-Message").unwrap();
        assert!(header.value_str().is_some());
    }

    #[test]
    fn edge_many_small_headers() {
        let mut buffer = String::new();
        for i in 0..50 {
            buffer.push_str(&format!("X-H{}: v{}\r\n", i, i));
        }
        buffer.push_str("\r\n");
        let parser = HeadersParser::parse(buffer.as_bytes()).unwrap();
        assert!(parser.get("X-H0").is_some());
        assert!(parser.get("X-H49").is_some());
    }

    #[test]
    fn edge_content_length_with_leading_zeros() {
        let buffer = b"Content-Length: 00042\r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        assert_eq!(parser.content_length(), Some(42));
    }

    #[test]
    fn edge_content_length_with_whitespace() {
        let buffer = b"Content-Length:   42  \r\n\r\n";
        let parser = HeadersParser::parse(buffer).unwrap();
        assert_eq!(parser.content_length(), Some(42));
    }

    // ========================================================================
    // High-Level Parser Tests
    // ========================================================================

    #[test]
    fn parser_simple_get() {
        let parser = Parser::new();
        let buffer = b"GET /api/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let request = parser.parse(buffer).unwrap();

        assert_eq!(request.method(), Method::Get);
        assert_eq!(request.path(), "/api/users");
        assert!(request.query().is_none());
    }

    #[test]
    fn parser_post_with_json_body() {
        let parser = Parser::new();
        let buffer = b"POST /api/items HTTP/1.1\r\nHost: example.com\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"id\": \"123\"}";
        let request = parser.parse(buffer).unwrap();

        assert_eq!(request.method(), Method::Post);
        assert_eq!(request.path(), "/api/items");
        match request.body() {
            Body::Bytes(bytes) => assert_eq!(bytes, b"{\"id\": \"123\"}"),
            _ => panic!("expected bytes body"),
        }
    }

    #[test]
    fn parser_request_with_query() {
        let parser = Parser::new();
        let buffer = b"GET /search?q=rust&limit=10 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let request = parser.parse(buffer).unwrap();

        assert_eq!(request.path(), "/search");
        assert_eq!(
            request.query(),
            Some("q=rust&limit=10".to_string()).as_deref()
        );
    }

    #[test]
    fn parser_max_size_respected() {
        let parser = Parser::new().with_max_size(50);
        let buffer = b"GET /path HTTP/1.1\r\nHost: example.com\r\nX-Long: this is a very long header that exceeds the limit\r\n\r\n";
        let result = parser.parse(buffer);
        assert!(matches!(result, Err(ParseError::TooLarge)));
    }

    #[test]
    fn parser_custom_limits() {
        let limits = ParseLimits {
            max_request_size: 1024,
            max_request_line_len: 100,
            max_header_count: 10,
            max_header_line_len: 200,
            max_headers_size: 500,
        };
        let parser = Parser::new().with_limits(limits);
        let buffer = b"GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let request = parser.parse(buffer).unwrap();
        assert_eq!(request.method(), Method::Get);
    }

    #[test]
    fn parser_incomplete_request() {
        let parser = Parser::new();
        let buffer = b"GET /path HTTP/1.1\r\nHost: example.com\r\n";
        // Missing final \r\n
        let result = parser.parse(buffer);
        assert!(matches!(result, Err(ParseError::Incomplete)));
    }

    #[test]
    fn parser_preserves_headers() {
        let parser = Parser::new();
        let buffer =
            b"GET /path HTTP/1.1\r\nHost: example.com\r\nX-Custom: my-value\r\nAccept: */*\r\n\r\n";
        let request = parser.parse(buffer).unwrap();

        assert!(request.headers().get("Host").is_some());
        assert!(request.headers().get("X-Custom").is_some());
        assert!(request.headers().get("Accept").is_some());
    }

    // ========================================================================
    // Malformed Request Tests
    // ========================================================================

    #[test]
    fn malformed_empty_input() {
        let parser = Parser::new();
        let result = parser.parse(b"");
        assert!(matches!(result, Err(ParseError::Incomplete)));
    }

    #[test]
    fn malformed_only_crlf() {
        let parser = Parser::new();
        let result = parser.parse(b"\r\n\r\n");
        assert!(result.is_err());
    }

    #[test]
    fn malformed_no_method() {
        let buffer = b"/path HTTP/1.1\r\n\r\n";
        let result = RequestLine::parse(buffer);
        assert!(result.is_err());
    }

    #[test]
    fn malformed_garbage_input() {
        let parser = Parser::new();
        let result = parser.parse(b"not a valid http request at all");
        assert!(result.is_err());
    }

    #[test]
    fn malformed_binary_garbage() {
        let parser = Parser::new();
        let result = parser.parse(b"\x00\x01\x02\x03\x04\x05\r\n\r\n");
        assert!(result.is_err());
    }

    #[test]
    fn malformed_tab_instead_of_space() {
        let buffer = b"GET\t/path\tHTTP/1.1\r\n\r\n";
        let result = RequestLine::parse(buffer);
        // Should fail since tab is not space
        assert!(result.is_err());
    }

    #[test]
    fn malformed_lf_only_line_ending() {
        let buffer = b"GET /path HTTP/1.1\nHost: example.com\n\n";
        let parser = Parser::new();
        let result = parser.parse(buffer);
        // LF-only should fail (CRLF required)
        assert!(result.is_err());
    }

    #[test]
    fn malformed_cr_only_line_ending() {
        let buffer = b"GET /path HTTP/1.1\rHost: example.com\r\r";
        let parser = Parser::new();
        let result = parser.parse(buffer);
        assert!(result.is_err());
    }

    // ========================================================================
    // StatefulParser Additional Tests
    // ========================================================================

    #[test]
    fn stateful_parser_clear_resets_state() {
        let mut parser = StatefulParser::new();
        parser.feed(b"GET /partial").unwrap();
        assert!(parser.buffered_len() > 0);

        parser.clear();
        assert_eq!(parser.buffered_len(), 0);

        // Should be able to parse fresh - needs a proper request with headers
        let result = parser
            .feed(b"GET /new HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .unwrap();
        assert!(matches!(result, ParseStatus::Complete { .. }));
    }

    #[test]
    fn stateful_parser_byte_at_a_time() {
        let mut parser = StatefulParser::new();
        let request = b"GET /path HTTP/1.1\r\nHost: x\r\n\r\n";

        for (i, byte) in request.iter().enumerate() {
            let result = parser.feed(&[*byte]).unwrap();
            if i == request.len() - 1 {
                assert!(matches!(result, ParseStatus::Complete { .. }));
            } else {
                assert!(matches!(result, ParseStatus::Incomplete));
            }
        }
    }

    #[test]
    fn stateful_parser_with_body_config() {
        use crate::body::BodyConfig;

        let config = BodyConfig::new().with_max_size(10);
        let mut parser = StatefulParser::new().with_body_config(config);

        // Body within limit
        let result = parser.feed(b"GET /path HTTP/1.1\r\nContent-Length: 5\r\n\r\nHello");
        assert!(result.is_ok());
    }

    #[test]
    fn stateful_parser_pipelining_two_requests_in_one_buffer() {
        let mut parser = StatefulParser::new();

        // Send two complete requests in a single buffer (HTTP pipelining)
        let pipelined = b"GET /first HTTP/1.1\r\nHost: localhost\r\n\r\nGET /second HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let result = parser.feed(pipelined).unwrap();

        // First request should be returned
        match result {
            ParseStatus::Complete { request, .. } => {
                assert_eq!(request.path(), "/first");
            }
            ParseStatus::Incomplete => panic!("expected first request"),
        }

        // Second request should already be buffered — feed empty to retrieve it
        let result = parser.feed(&[]).unwrap();
        match result {
            ParseStatus::Complete { request, .. } => {
                assert_eq!(request.path(), "/second");
            }
            ParseStatus::Incomplete => panic!("expected second pipelined request"),
        }
    }

    #[test]
    fn stateful_parser_pipelining_three_requests() {
        let mut parser = StatefulParser::new();

        let pipelined = b"GET /a HTTP/1.1\r\nHost: h\r\n\r\n\
                           GET /b HTTP/1.1\r\nHost: h\r\n\r\n\
                           GET /c HTTP/1.1\r\nHost: h\r\n\r\n";
        let r1 = parser.feed(pipelined).unwrap();
        assert!(matches!(r1, ParseStatus::Complete { .. }));

        let r2 = parser.feed(&[]).unwrap();
        assert!(matches!(r2, ParseStatus::Complete { .. }));

        let r3 = parser.feed(&[]).unwrap();
        assert!(matches!(r3, ParseStatus::Complete { .. }));

        // No more requests buffered
        let r4 = parser.feed(&[]).unwrap();
        assert!(matches!(r4, ParseStatus::Incomplete));
    }
}
