//! HTTP Connection header handling.
//!
//! This module provides proper parsing and handling of the HTTP `Connection` header
//! per RFC 7230, including:
//!
//! - Parsing comma-separated connection tokens
//! - Handling `close`, `keep-alive`, and `upgrade` directives
//! - Extracting hop-by-hop header names for stripping
//! - HTTP version-aware default behavior
//!
//! # Connection Header Semantics
//!
//! The Connection header is a comma-separated list of tokens. Each token is either:
//! - A connection option (`close`, `keep-alive`, `upgrade`)
//! - The name of a hop-by-hop header field to be stripped when forwarding
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::connection::{ConnectionInfo, parse_connection_header};
//!
//! let info = parse_connection_header(Some(b"keep-alive, X-Custom-Header"));
//! assert!(info.keep_alive);
//! assert!(info.hop_by_hop_headers.contains(&"x-custom-header".to_string()));
//! ```

use fastapi_core::{HttpVersion, Request};

/// Standard hop-by-hop headers that should always be stripped when forwarding.
///
/// These headers are connection-specific and must not be forwarded by proxies,
/// regardless of whether they appear in the Connection header.
pub const STANDARD_HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Parsed Connection header information.
#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    /// Whether `close` token was present.
    pub close: bool,
    /// Whether `keep-alive` token was present.
    pub keep_alive: bool,
    /// Whether `upgrade` token was present.
    pub upgrade: bool,
    /// Hop-by-hop header names to strip (lowercased).
    ///
    /// These are header field names that appeared in the Connection header
    /// and should be removed when forwarding the message.
    pub hop_by_hop_headers: Vec<String>,
}

impl ConnectionInfo {
    /// Creates an empty ConnectionInfo.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses Connection header value(s).
    ///
    /// The value should be a comma-separated list of tokens. Tokens are
    /// case-insensitive and whitespace around commas is ignored.
    #[must_use]
    pub fn parse(value: &[u8]) -> Self {
        let mut info = Self::new();

        let value_str = match std::str::from_utf8(value) {
            Ok(s) => s,
            Err(_) => return info,
        };

        for part in value_str.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }

            // Case-insensitive match without allocation for known tokens
            if part.eq_ignore_ascii_case("close") {
                info.close = true;
            } else if part.eq_ignore_ascii_case("keep-alive") {
                info.keep_alive = true;
            } else if part.eq_ignore_ascii_case("upgrade") {
                info.upgrade = true;
            } else {
                // Only allocate for custom hop-by-hop headers (rare path)
                let lower = part.to_ascii_lowercase();
                // Don't add standard hop-by-hop headers again
                if !STANDARD_HOP_BY_HOP_HEADERS.contains(&lower.as_str()) {
                    info.hop_by_hop_headers.push(lower);
                }
            }
        }

        info
    }

    /// Returns whether the connection should be kept alive based on HTTP version.
    ///
    /// - HTTP/1.1: defaults to keep-alive unless `close` is present
    /// - HTTP/1.0: defaults to close unless `keep-alive` is present
    #[must_use]
    pub fn should_keep_alive(&self, version: HttpVersion) -> bool {
        // Explicit close always wins
        if self.close {
            return false;
        }

        // Explicit keep-alive always wins
        if self.keep_alive {
            return true;
        }

        // Default behavior based on HTTP version
        match version {
            HttpVersion::Http11 => true,  // HTTP/1.1 defaults to keep-alive
            HttpVersion::Http10 => false, // HTTP/1.0 defaults to close
            HttpVersion::Http2 => true, // HTTP/2 uses persistent connections (no Connection header semantics)
        }
    }
}

/// Parses the Connection header from a request and returns connection info.
///
/// # Arguments
///
/// * `value` - The raw Connection header value, or None if header is missing
///
/// # Returns
///
/// Parsed ConnectionInfo with all directives and hop-by-hop header names.
#[must_use]
pub fn parse_connection_header(value: Option<&[u8]>) -> ConnectionInfo {
    match value {
        Some(v) => ConnectionInfo::parse(v),
        None => ConnectionInfo::new(),
    }
}

/// Determines if a connection should be kept alive based on request headers and version.
///
/// This is a convenience function that combines Connection header parsing with
/// HTTP version-aware keep-alive logic.
///
/// # Arguments
///
/// * `request` - The HTTP request to check
///
/// # Returns
///
/// `true` if the connection should be kept alive, `false` otherwise.
///
/// # Behavior
///
/// - HTTP/1.1 defaults to keep-alive unless `Connection: close` is present
/// - HTTP/1.0 requires explicit `Connection: keep-alive` to stay open
/// - `Connection: close` always closes the connection
/// - `Connection: keep-alive` always keeps the connection open
#[must_use]
pub fn should_keep_alive(request: &Request) -> bool {
    let connection = request.headers().get("connection");
    let info = parse_connection_header(connection);
    info.should_keep_alive(request.version())
}

/// Strip hop-by-hop headers from a request.
///
/// Removes both standard hop-by-hop headers and any headers listed in the
/// Connection header from the request.
///
/// # Arguments
///
/// * `request` - The request to modify
///
/// This is typically used when forwarding requests through a proxy or gateway.
pub fn strip_hop_by_hop_headers(request: &mut Request) {
    // Parse Connection header to find custom hop-by-hop headers
    let connection = request.headers().get("connection").map(<[u8]>::to_vec);
    let info = parse_connection_header(connection.as_deref());

    // Remove standard hop-by-hop headers
    for header in STANDARD_HOP_BY_HOP_HEADERS {
        request.headers_mut().remove(header);
    }

    // Remove custom hop-by-hop headers listed in Connection
    for header in &info.hop_by_hop_headers {
        request.headers_mut().remove(header);
    }
}

/// Check if a header name is a hop-by-hop header.
///
/// Returns true if the header is in the standard hop-by-hop list.
/// Note: This doesn't check if it was listed in the Connection header.
#[must_use]
pub fn is_standard_hop_by_hop_header(name: &str) -> bool {
    // Case-insensitive comparison without allocation
    STANDARD_HOP_BY_HOP_HEADERS
        .iter()
        .any(|&h| name.eq_ignore_ascii_case(h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi_core::Method;

    #[test]
    fn connection_info_parse_close() {
        let info = ConnectionInfo::parse(b"close");
        assert!(info.close);
        assert!(!info.keep_alive);
        assert!(!info.upgrade);
        assert!(info.hop_by_hop_headers.is_empty());
    }

    #[test]
    fn connection_info_parse_keep_alive() {
        let info = ConnectionInfo::parse(b"keep-alive");
        assert!(!info.close);
        assert!(info.keep_alive);
        assert!(!info.upgrade);
    }

    #[test]
    fn connection_info_parse_upgrade() {
        let info = ConnectionInfo::parse(b"upgrade");
        assert!(!info.close);
        assert!(!info.keep_alive);
        assert!(info.upgrade);
    }

    #[test]
    fn connection_info_parse_multiple_tokens() {
        let info = ConnectionInfo::parse(b"keep-alive, upgrade");
        assert!(!info.close);
        assert!(info.keep_alive);
        assert!(info.upgrade);
    }

    #[test]
    fn connection_info_parse_with_custom_headers() {
        let info = ConnectionInfo::parse(b"keep-alive, X-Custom-Header, X-Another");
        assert!(info.keep_alive);
        assert_eq!(info.hop_by_hop_headers.len(), 2);
        assert!(
            info.hop_by_hop_headers
                .contains(&"x-custom-header".to_string())
        );
        assert!(info.hop_by_hop_headers.contains(&"x-another".to_string()));
    }

    #[test]
    fn connection_info_parse_case_insensitive() {
        let info = ConnectionInfo::parse(b"CLOSE");
        assert!(info.close);

        let info = ConnectionInfo::parse(b"Keep-Alive");
        assert!(info.keep_alive);

        let info = ConnectionInfo::parse(b"UPGRADE");
        assert!(info.upgrade);
    }

    #[test]
    fn connection_info_parse_with_whitespace() {
        let info = ConnectionInfo::parse(b"  keep-alive  ,  close  ");
        assert!(info.close);
        assert!(info.keep_alive);
    }

    #[test]
    fn connection_info_parse_empty() {
        let info = ConnectionInfo::parse(b"");
        assert!(!info.close);
        assert!(!info.keep_alive);
        assert!(!info.upgrade);
        assert!(info.hop_by_hop_headers.is_empty());
    }

    #[test]
    fn connection_info_parse_invalid_utf8() {
        let info = ConnectionInfo::parse(&[0xFF, 0xFE]);
        assert!(!info.close);
        assert!(!info.keep_alive);
    }

    #[test]
    fn should_keep_alive_http11_default() {
        let info = ConnectionInfo::new();
        assert!(info.should_keep_alive(HttpVersion::Http11));
    }

    #[test]
    fn should_keep_alive_http10_default() {
        let info = ConnectionInfo::new();
        assert!(!info.should_keep_alive(HttpVersion::Http10));
    }

    #[test]
    fn should_keep_alive_http11_with_close() {
        let info = ConnectionInfo::parse(b"close");
        assert!(!info.should_keep_alive(HttpVersion::Http11));
    }

    #[test]
    fn should_keep_alive_http10_with_keep_alive() {
        let info = ConnectionInfo::parse(b"keep-alive");
        assert!(info.should_keep_alive(HttpVersion::Http10));
    }

    #[test]
    fn should_keep_alive_close_overrides_keep_alive() {
        // When both are present, close wins
        let info = ConnectionInfo::parse(b"keep-alive, close");
        assert!(!info.should_keep_alive(HttpVersion::Http11));
        assert!(!info.should_keep_alive(HttpVersion::Http10));
    }

    #[test]
    fn should_keep_alive_request_http11_default() {
        let request = Request::with_version(Method::Get, "/", HttpVersion::Http11);
        assert!(should_keep_alive(&request));
    }

    #[test]
    fn should_keep_alive_request_http10_default() {
        let request = Request::with_version(Method::Get, "/", HttpVersion::Http10);
        assert!(!should_keep_alive(&request));
    }

    #[test]
    fn should_keep_alive_request_with_close_header() {
        let mut request = Request::with_version(Method::Get, "/", HttpVersion::Http11);
        request
            .headers_mut()
            .insert("connection", b"close".to_vec());
        assert!(!should_keep_alive(&request));
    }

    #[test]
    fn should_keep_alive_request_http10_with_keep_alive() {
        let mut request = Request::with_version(Method::Get, "/", HttpVersion::Http10);
        request
            .headers_mut()
            .insert("connection", b"keep-alive".to_vec());
        assert!(should_keep_alive(&request));
    }

    #[test]
    fn strip_hop_by_hop_headers_removes_standard() {
        let mut request = Request::new(Method::Get, "/");
        request
            .headers_mut()
            .insert("connection", b"close".to_vec());
        request
            .headers_mut()
            .insert("keep-alive", b"timeout=5".to_vec());
        request
            .headers_mut()
            .insert("transfer-encoding", b"chunked".to_vec());
        request
            .headers_mut()
            .insert("host", b"example.com".to_vec());

        strip_hop_by_hop_headers(&mut request);

        assert!(request.headers().get("connection").is_none());
        assert!(request.headers().get("keep-alive").is_none());
        assert!(request.headers().get("transfer-encoding").is_none());
        // Non-hop-by-hop headers should remain
        assert!(request.headers().get("host").is_some());
    }

    #[test]
    fn strip_hop_by_hop_headers_removes_custom() {
        let mut request = Request::new(Method::Get, "/");
        request
            .headers_mut()
            .insert("connection", b"X-Custom-Header".to_vec());
        request
            .headers_mut()
            .insert("x-custom-header", b"value".to_vec());
        request
            .headers_mut()
            .insert("host", b"example.com".to_vec());

        strip_hop_by_hop_headers(&mut request);

        assert!(request.headers().get("x-custom-header").is_none());
        assert!(request.headers().get("host").is_some());
    }

    #[test]
    fn is_standard_hop_by_hop_header_works() {
        assert!(is_standard_hop_by_hop_header("connection"));
        assert!(is_standard_hop_by_hop_header("Connection"));
        assert!(is_standard_hop_by_hop_header("KEEP-ALIVE"));
        assert!(is_standard_hop_by_hop_header("transfer-encoding"));

        assert!(!is_standard_hop_by_hop_header("host"));
        assert!(!is_standard_hop_by_hop_header("content-type"));
        assert!(!is_standard_hop_by_hop_header("x-custom"));
    }

    #[test]
    fn standard_hop_by_hop_not_duplicated_in_custom() {
        // Standard headers listed in Connection shouldn't appear in hop_by_hop_headers
        let info = ConnectionInfo::parse(b"keep-alive, transfer-encoding, X-Custom");
        assert_eq!(info.hop_by_hop_headers.len(), 1);
        assert!(info.hop_by_hop_headers.contains(&"x-custom".to_string()));
    }
}
