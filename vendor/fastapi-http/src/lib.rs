//! Zero-copy HTTP/1.1 parser.
//!
//! This crate provides a minimal, zero-copy HTTP parser optimized for
//! the fastapi_rust framework. It parses directly from byte buffers
//! without allocating for most operations.
//!
//! # Features
//!
//! - Zero-copy request parsing
//! - HTTP/1.1 compliance (subset)
//! - Response building with pre-allocated buffers
//! - Request body handling (Content-Length and chunked encoding)
//! - Query string parsing with percent-decoding
//! - Streaming response support
//!
//! # Role In The System
//!
//! `fastapi-http` is the protocol boundary. It parses HTTP/1.1 bytes into the
//! `fastapi-core` request/response model, manages body streaming, and provides
//! the TCP server scaffolding used by `App::serve`. Higher-level crates build on
//! this layer without needing to care about socket I/O details.
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::Parser;
//!
//! let bytes = b"GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n";
//! let request = Parser::parse(bytes)?;
//! ```

#![deny(unsafe_code)]
// Pedantic clippy lints allowed (style suggestions, not correctness issues)
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::single_char_pattern)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::match_wildcard_for_single_variants)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::single_match_else)]
#![allow(clippy::struct_field_names)]
#![allow(clippy::manual_strip)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::match_wild_err_arm)]
#![allow(clippy::format_push_string)]
#![allow(clippy::match_wildcard_for_single_variants)]
#![allow(clippy::manual_contains)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::iter_without_into_iter)]
#![allow(clippy::single_match)]
#![allow(clippy::len_zero)]
#![allow(clippy::len_without_is_empty)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::unused_async)]
#![allow(clippy::used_underscore_binding)]
#![allow(clippy::duplicated_attributes)]

pub mod body;
pub mod connection;
pub mod expect;
pub mod http2;
pub mod multipart;
mod parser;
mod query;
pub mod range;
mod response;
mod server;
pub mod streaming;
pub mod websocket;

pub use body::{
    AsyncChunkedStream, AsyncContentLengthStream, BodyConfig, BodyError, ChunkedReader,
    ContentLengthReader, DEFAULT_MAX_BODY_SIZE, DEFAULT_STREAMING_THRESHOLD, StreamingBodyConfig,
    create_chunked_stream, create_content_length_stream, parse_body, parse_body_with_consumed,
    validate_content_length,
};
pub use connection::{
    ConnectionInfo, STANDARD_HOP_BY_HOP_HEADERS, is_standard_hop_by_hop_header,
    parse_connection_header, should_keep_alive, strip_hop_by_hop_headers,
};
pub use expect::{
    CONTINUE_RESPONSE, EXPECT_100_CONTINUE, ExpectHandler, ExpectResult, FnValidator,
    PreBodyValidator, PreBodyValidators,
};
pub use parser::{
    BodyLength, Header, HeadersIter, HeadersParser, ParseError, ParseLimits, ParseStatus, Parser,
    RequestLine, StatefulParser,
};
pub use query::{QueryString, percent_decode};
pub use range::{
    ByteRange, IfRangeResult, RangeError, RangeSpec, accept_ranges_bytes, check_if_range,
    content_range_unsatisfiable, parse_range_header, parse_range_spec, supports_ranges,
};
pub use response::{ChunkedEncoder, ResponseWrite, ResponseWriter, Trailers};
pub use server::{
    AppServeExt, DEFAULT_DRAIN_TIMEOUT_SECS, DEFAULT_KEEP_ALIVE_TIMEOUT_SECS,
    DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_REQUESTS_PER_CONNECTION, DEFAULT_READ_BUFFER_SIZE,
    DEFAULT_REQUEST_TIMEOUT_SECS, ServeError, Server, ServerConfig, ServerError, ServerMetrics,
    TcpServer, process_connection, read_into_buffer, serve, serve_with_config, write_all,
    write_response,
};

// Re-export signal types for graceful shutdown
pub use asupersync::signal::{GracefulOutcome, ShutdownController, ShutdownReceiver};
pub use multipart::{
    DEFAULT_MAX_FIELDS, DEFAULT_MAX_FILE_SIZE, DEFAULT_MAX_TOTAL_SIZE, MultipartConfig,
    MultipartError, MultipartForm, MultipartParser, Part, UploadFile, parse_boundary,
};
pub use streaming::{
    CancelAwareStream, ChunkedBytes, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_BUFFER_SIZE, FileStream,
    StreamConfig, StreamError, StreamingResponseExt,
};
pub use websocket::{
    CloseCode, DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_MESSAGE_SIZE, Message, Opcode, WebSocket,
    WebSocketConfig, WebSocketError, accept_key, build_accept_response, validate_upgrade_request,
};
