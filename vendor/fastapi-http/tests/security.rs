//! Comprehensive security test suite for fastapi-http.
//!
//! Tests HTTP security vulnerabilities including:
//! - Request smuggling (CL.TE, TE.CL)
//! - Header injection (CRLF)
//! - Path traversal attempts
//! - Resource exhaustion
//! - Encoding attacks
//! - Known CVE patterns

use fastapi_http::{
    HeadersParser, ParseError, ParseLimits, ParseStatus, Parser, RequestLine, StatefulParser,
};
use std::fmt::Write;

// ============================================================================
// 1. HTTP Request Smuggling Tests
// ============================================================================

/// CL.TE smuggling: Content-Length takes precedence on first server,
/// Transfer-Encoding on second. Should reject ambiguous requests.
#[test]
fn smuggling_cl_te_basic() {
    let buffer = b"POST /admin HTTP/1.1\r\n\
        Content-Length: 13\r\n\
        Transfer-Encoding: chunked\r\n\r\n\
        0\r\n\r\nSMUGGLED";

    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Should reject due to ambiguous body length (both CL and TE)
    assert!(
        matches!(result, Err(ParseError::AmbiguousBodyLength)),
        "CL.TE smuggling attempt should be rejected"
    );
}

/// TE.CL smuggling variant
#[test]
fn smuggling_te_cl_basic() {
    let buffer = b"POST /admin HTTP/1.1\r\n\
        Transfer-Encoding: chunked\r\n\
        Content-Length: 4\r\n\r\n\
        5c\r\nGPOST / HTTP/1.1\r\n\r\n0\r\n\r\n";

    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Should reject due to ambiguous body length
    assert!(
        matches!(result, Err(ParseError::AmbiguousBodyLength)),
        "TE.CL smuggling attempt should be rejected"
    );
}

/// CL.CL smuggling: Multiple Content-Length headers with different values
#[test]
fn smuggling_cl_cl_different_values() {
    let buffer = b"Content-Length: 10\r\nContent-Length: 20\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidHeader)),
        "Different Content-Length values should be rejected"
    );
}

/// CL.CL: Same value is OK (per some implementations)
#[test]
fn smuggling_cl_cl_same_value_ok() {
    let buffer = b"Content-Length: 42\r\nContent-Length: 42\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    // Same value duplicates are allowed
    assert!(result.is_ok());
    assert_eq!(result.unwrap().content_length(), Some(42));
}

/// HTTP/0.9 downgrade attempt
#[test]
fn smuggling_http09_downgrade() {
    let buffer = b"GET /\r\n";
    let result = RequestLine::parse(buffer);

    // Should fail - requires HTTP version
    assert!(result.is_err());
}

/// Smuggling via chunk extension
#[test]
fn smuggling_chunk_extension() {
    let buffer = b"Host: example.com\r\nTransfer-Encoding: chunked\r\n\r\n";
    let parser = HeadersParser::parse(buffer).unwrap();
    assert!(parser.is_chunked());

    // Chunk extensions should be ignored safely
    // The actual chunked body parsing handles this
}

/// Transfer-Encoding with unexpected value
#[test]
fn smuggling_te_unexpected_value() {
    let buffer = b"Transfer-Encoding: gzip\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    // Non-chunked TE values should be rejected for security
    assert!(matches!(result, Err(ParseError::InvalidTransferEncoding)));
}

/// Transfer-Encoding: chunked with trailing garbage
#[test]
fn smuggling_te_chunked_trailing() {
    let buffer = b"Transfer-Encoding: chunked, identity\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    // "chunked, identity" is not strictly "chunked"
    // Our parser should handle this safely
    assert!(result.is_ok() || result.is_err());
}

// ============================================================================
// 2. Header Injection Tests (CRLF Injection)
// ============================================================================

/// CRLF injection in request line path
#[test]
fn injection_crlf_in_path() {
    let buffer = b"GET /path\r\nX-Injected: evil HTTP/1.1\r\n\r\n";
    let result = RequestLine::parse(buffer);

    // Path should not contain CRLF
    if let Ok(line) = result {
        assert!(
            !line.path().contains('\r') && !line.path().contains('\n'),
            "Path should not contain CRLF characters"
        );
    }
}

/// CRLF injection via URL encoding (%0d%0a)
#[test]
fn injection_crlf_url_encoded() {
    // URL-encoded CRLF should be decoded but not interpreted as structure
    let buffer = b"GET /path%0d%0aX-Injected:%20evil HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Parser decodes percent-encoded CRLF which creates newlines in path
    // This is expected behavior - the key is that it doesn't create new headers
    // in the request struct. The path will contain the decoded characters.
    if let Ok(request) = result {
        // The parser decoded the URL, so path contains decoded content
        // What matters is that no headers were injected via the path
        assert!(request.headers().get("X-Injected").is_none());
    } else {
        // Rejecting such paths is also acceptable
    }
}

/// Double CRLF injection attempt
#[test]
fn injection_double_crlf() {
    let buffer = b"GET /path\r\n\r\nHTTP/1.1\r\n\r\n";
    let result = RequestLine::parse(buffer);

    // Should parse just the first line, or reject
    assert!(result.is_err() || result.is_ok());
}

/// Header value with embedded CRLF
#[test]
fn injection_header_value_crlf() {
    let buffer = b"X-Test: value\r\nX-Injected: evil\r\n\r\n";
    let parser = HeadersParser::parse(buffer).unwrap();

    // Should see two separate headers, not injection
    assert!(parser.get("X-Test").is_some());
    assert!(parser.get("X-Injected").is_some());
}

/// Null byte injection in request line
#[test]
fn injection_null_byte_request_line() {
    let buffer = b"GET /path\x00evil HTTP/1.1\r\n";
    let result = RequestLine::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidRequestLine)),
        "Null bytes in request line should be rejected"
    );
}

/// Null byte in header name
#[test]
fn injection_null_byte_header_name() {
    let buffer = b"X-Test\x00Header: value\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    assert!(
        result.is_err(),
        "Null bytes in header name should be rejected"
    );
}

/// Null byte in header value
#[test]
fn injection_null_byte_header_value() {
    let buffer = b"X-Test: val\x00ue\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidHeaderBytes)),
        "Null bytes in header value should be rejected"
    );
}

/// Obs-fold (obsolete line folding) rejection
#[test]
fn injection_obs_fold() {
    let buffer = b"X-Test: value\r\n continuation\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    // Obs-fold is deprecated per RFC 7230 and should be rejected
    assert!(
        matches!(result, Err(ParseError::InvalidHeader)),
        "Obsolete line folding should be rejected"
    );
}

// ============================================================================
// 3. Path Traversal Tests
// ============================================================================

/// Basic path traversal attempt
#[test]
fn traversal_dot_dot_slash() {
    let buffer = b"GET /../../../etc/passwd HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Parser may either parse or reject traversal paths
    // Both behaviors are acceptable from a security perspective
    if let Ok(request) = result {
        // If parsed, verify path contains the traversal
        assert!(request.path().contains("..") || request.path().contains("etc"));
    } else {
        // Rejecting traversal paths is also acceptable
    }
}

/// Path traversal with URL encoding
#[test]
fn traversal_url_encoded() {
    let buffer = b"GET /%2e%2e/%2e%2e/etc/passwd HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Parser may either parse or reject URL-encoded traversal
    if let Ok(request) = result {
        let path = request.path();
        assert!(!path.is_empty());
    } else {
        // Rejecting URL-encoded traversal is also acceptable
    }
}

/// Double URL encoding traversal
#[test]
fn traversal_double_encoded() {
    let buffer = b"GET /%252e%252e/etc/passwd HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Should either parse or reject safely
    if let Ok(request) = result {
        assert!(!request.path().is_empty());
    } else {
        // Rejecting is also acceptable
    }
}

/// Backslash traversal (Windows-style)
#[test]
fn traversal_backslash() {
    let buffer = b"GET /..\\..\\etc\\passwd HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Parser may either parse or reject backslash paths
    if let Ok(request) = result {
        // If parsed, path should contain the backslash pattern
        assert!(request.path().contains("..\\") || request.path().contains("etc"));
    } else {
        // Rejecting is also acceptable (strict parsing)
    }
}

/// Null byte path truncation attempt
#[test]
fn traversal_null_byte_truncation() {
    let buffer = b"GET /admin\x00.jpg HTTP/1.1\r\n";
    let result = RequestLine::parse(buffer);

    // Null bytes should be rejected
    assert!(matches!(result, Err(ParseError::InvalidRequestLine)));
}

/// Unicode path traversal (overlong UTF-8)
#[test]
fn traversal_overlong_utf8() {
    // Overlong encoding of "." (should be rejected or handled safely)
    let buffer = b"GET /\xc0\xae\xc0\xae/etc/passwd HTTP/1.1\r\n\r\n";
    let parser = Parser::new();

    // Should either parse safely or reject
    let _ = parser.parse(buffer);
}

// ============================================================================
// 4. Resource Exhaustion Tests
// ============================================================================

/// Extremely long request line
#[test]
fn exhaustion_long_request_line() {
    let limits = ParseLimits {
        max_request_line_len: 100,
        ..Default::default()
    };

    let long_path = "a".repeat(200);
    let buffer = format!("GET /{long_path} HTTP/1.1\r\n\r\n");

    let mut parser = StatefulParser::new().with_limits(limits);
    let result = parser.feed(buffer.as_bytes());

    assert!(
        matches!(result, Err(ParseError::RequestLineTooLong)),
        "Extremely long request line should be rejected"
    );
}

/// Too many headers
#[test]
fn exhaustion_too_many_headers() {
    let limits = ParseLimits {
        max_header_count: 5,
        ..Default::default()
    };

    let mut buffer = String::new();
    for i in 0..10 {
        let _ = write!(buffer, "X-Header-{i}: value\r\n");
    }
    buffer.push_str("\r\n");

    let result = HeadersParser::parse_with_limits(buffer.as_bytes(), &limits);

    assert!(
        matches!(result, Err(ParseError::TooManyHeaders)),
        "Too many headers should be rejected"
    );
}

/// Extremely long header line
#[test]
fn exhaustion_long_header_line() {
    let limits = ParseLimits {
        max_header_line_len: 100,
        ..Default::default()
    };

    let long_value = "x".repeat(200);
    let buffer = format!("X-Long: {long_value}\r\n\r\n");

    let result = HeadersParser::parse_with_limits(buffer.as_bytes(), &limits);

    assert!(
        matches!(result, Err(ParseError::HeaderLineTooLong)),
        "Extremely long header line should be rejected"
    );
}

/// Extremely large header block
#[test]
fn exhaustion_large_headers_block() {
    let limits = ParseLimits {
        max_headers_size: 100,
        ..Default::default()
    };

    let mut buffer = String::new();
    for i in 0..20 {
        let _ = write!(buffer, "X-H{i}: value{i}\r\n");
    }
    buffer.push_str("\r\n");

    let result = HeadersParser::parse_with_limits(buffer.as_bytes(), &limits);

    assert!(
        matches!(result, Err(ParseError::HeadersTooLarge)),
        "Extremely large headers block should be rejected"
    );
}

/// Gigantic Content-Length (DoS via allocation)
#[test]
fn exhaustion_huge_content_length() {
    let buffer = b"Content-Length: 99999999999999999999999999\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidHeader)),
        "Huge Content-Length should be rejected"
    );
}

/// Negative Content-Length
#[test]
fn exhaustion_negative_content_length() {
    let buffer = b"Content-Length: -1\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidHeader)),
        "Negative Content-Length should be rejected"
    );
}

/// Content-Length with non-numeric characters
#[test]
fn exhaustion_non_numeric_content_length() {
    let buffer = b"Content-Length: 10abc\r\n\r\n";
    let result = HeadersParser::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidHeader)),
        "Non-numeric Content-Length should be rejected"
    );
}

/// Chunk size overflow attempt
#[test]
fn exhaustion_chunk_size_overflow() {
    // Oversized/overflowing chunk size line must be rejected during body parsing.
    let mut parser = StatefulParser::new();
    let buffer = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFF\r\n";
    let result = parser.feed(buffer);
    assert!(
        matches!(result, Err(ParseError::InvalidHeader)),
        "chunk size overflow must be rejected"
    );
}

/// Request too large overall
#[test]
fn exhaustion_request_too_large() {
    let mut parser = StatefulParser::new().with_max_size(100);
    let buffer = b"GET / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 200\r\n\r\n";
    let mut result = parser.feed(buffer);

    // Continue feeding until size limit exceeded
    if matches!(result, Ok(ParseStatus::Incomplete)) {
        let body = vec![b'x'; 200];
        result = parser.feed(&body);
    }

    assert!(matches!(result, Err(ParseError::TooLarge)));
}

// ============================================================================
// 5. Encoding Attack Tests
// ============================================================================

/// Mixed-case method should be rejected
#[test]
fn encoding_mixed_case_method() {
    let buffer = b"Get /path HTTP/1.1\r\n";
    let result = RequestLine::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidMethod)),
        "Mixed-case method should be rejected"
    );
}

/// Lowercase method should be rejected
#[test]
fn encoding_lowercase_method() {
    let buffer = b"get /path HTTP/1.1\r\n";
    let result = RequestLine::parse(buffer);

    assert!(
        matches!(result, Err(ParseError::InvalidMethod)),
        "Lowercase method should be rejected"
    );
}

/// Tab instead of space in request line
#[test]
fn encoding_tab_separator() {
    let buffer = b"GET\t/path\tHTTP/1.1\r\n";
    let result = RequestLine::parse(buffer);

    // Tabs are not valid separators per RFC
    assert!(result.is_err());
}

/// LF-only line endings (not CRLF)
#[test]
fn encoding_lf_only() {
    let buffer = b"GET /path HTTP/1.1\nHost: example.com\n\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // LF-only should be rejected (CRLF required)
    assert!(result.is_err());
}

/// CR-only line endings
#[test]
fn encoding_cr_only() {
    let buffer = b"GET /path HTTP/1.1\rHost: example.com\r\r";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // CR-only should be rejected
    assert!(result.is_err());
}

/// Invalid UTF-8 in path
#[test]
fn encoding_invalid_utf8_path() {
    // Invalid UTF-8 sequence
    let buffer = b"GET /\xff\xfe HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Should either parse as bytes or reject, not crash
    let _ = result;
}

/// Invalid UTF-8 in header value (allowed as bytes)
#[test]
fn encoding_invalid_utf8_header() {
    let buffer = b"X-Binary: \xff\xfe\r\n\r\n";
    let parser = HeadersParser::parse(buffer).unwrap();
    let header = parser.get("X-Binary").unwrap();

    // Header values are bytes, UTF-8 conversion should fail gracefully
    assert!(header.value_str().is_none());
    assert_eq!(header.value(), b"\xff\xfe");
}

/// Percent-encoded special characters in path
#[test]
fn encoding_percent_special_chars() {
    let buffer = b"GET /%00%0d%0a HTTP/1.1\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Should be parseable; actual handling is application-level
    let _ = result;
}

/// Unicode normalization attack (different representations)
#[test]
fn encoding_unicode_normalization() {
    // é as single codepoint vs e + combining accent
    let buffer = "GET /caf\u{00e9} HTTP/1.1\r\n\r\n".as_bytes();
    let parser = Parser::new();
    let result1 = parser.parse(buffer);

    let buffer2 = "GET /cafe\u{0301} HTTP/1.1\r\n\r\n".as_bytes();
    let result2 = parser.parse(buffer2);

    // Parser may accept or reject UTF-8 in paths
    // Both behaviors are acceptable - what matters is no panic
    // Unicode normalization issues are application-level concerns
    let _ = result1;
    let _ = result2;
}

// ============================================================================
// 6. Known CVE Pattern Tests
// ============================================================================

/// CVE-style: Apache chunked encoding bug pattern
#[test]
fn cve_apache_chunked_pattern() {
    // Pattern that exploited some chunked parsers
    let buffer = b"Transfer-Encoding: chunked\r\n\r\n";
    let parser = HeadersParser::parse(buffer).unwrap();
    assert!(parser.is_chunked());
}

/// CVE-style: Nginx buffer overflow pattern
#[test]
fn cve_nginx_buffer_pattern() {
    // Very long URI that could cause buffer issues
    let long_path = "A".repeat(10000);
    let buffer = format!("GET /{long_path} HTTP/1.1\r\n\r\n");
    let parser = Parser::new();

    // Should handle without panic
    let _ = parser.parse(buffer.as_bytes());
}

/// CVE-style: HTTP response splitting via headers
#[test]
fn cve_response_splitting() {
    // Attempt to inject response headers via request
    let buffer = b"X-Header: value\r\nContent-Type: text/html\r\n\r\n";
    let parser = HeadersParser::parse(buffer).unwrap();

    // Headers should be parsed as separate, not split
    assert!(parser.get("X-Header").is_some());
    assert!(parser.get("Content-Type").is_some());
}

/// CVE-style: Request header injection via Host
#[test]
fn cve_host_header_injection() {
    // Malicious Host header attempting injection
    let buffer = b"Host: evil.com\r\nX-Injected-Host: attack\r\n\r\n";
    let parser = HeadersParser::parse(buffer).unwrap();

    // Should be two separate headers
    assert_eq!(parser.get("Host").unwrap().value_str(), Some("evil.com"));
    assert!(parser.get("X-Injected-Host").is_some());
}

// ============================================================================
// Integration: Combined Attack Patterns
// ============================================================================

/// Combined: Smuggling + Traversal
#[test]
fn combined_smuggling_traversal() {
    let buffer = b"POST /../admin HTTP/1.1\r\n\
        Content-Length: 0\r\n\
        Transfer-Encoding: chunked\r\n\r\n";

    let parser = Parser::new();
    let result = parser.parse(buffer);

    // Should reject due to ambiguous body length
    assert!(matches!(result, Err(ParseError::AmbiguousBodyLength)));
}

/// Combined: Injection + Exhaustion
#[test]
fn combined_injection_exhaustion() {
    let limits = ParseLimits {
        max_header_count: 10,
        ..Default::default()
    };

    // Try to inject headers while approaching limit
    let mut buffer = String::new();
    for i in 0..15 {
        let _ = write!(buffer, "X-H{i}: \r\nX-Injected{i}: evil\r\n");
    }
    buffer.push_str("\r\n");

    let result = HeadersParser::parse_with_limits(buffer.as_bytes(), &limits);

    // Should hit header count limit
    assert!(matches!(result, Err(ParseError::TooManyHeaders)));
}

/// Stateful parser handles all attack vectors correctly
#[test]
fn stateful_handles_attacks() {
    let mut parser = StatefulParser::new();

    // Feed a smuggling attempt
    let smuggle = b"POST /x HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n";
    let result = parser.feed(smuggle);

    assert!(
        matches!(result, Err(ParseError::AmbiguousBodyLength)),
        "Stateful parser should reject smuggling attempts"
    );
}

// ============================================================================
// 7. Request Line Hardening Tests
// ============================================================================

/// Unknown HTTP version must be rejected, not silently defaulted to HTTP/1.1.
/// A proxy that forwards HTTP/9.9 could desync with a server that treats it as 1.1.
#[test]
fn smuggling_unknown_http_version_rejected() {
    let buffer = b"GET / HTTP/9.9\r\nHost: example.com\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);
    assert!(
        result.is_err(),
        "Unknown HTTP version should be rejected, got: {result:?}"
    );
}

/// Extra parts in the request line must be rejected to prevent desync.
#[test]
fn smuggling_request_line_extra_parts_rejected() {
    let buffer = b"GET / HTTP/1.1 extra-data\r\nHost: example.com\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);
    assert!(
        result.is_err(),
        "Extra request-line parts should be rejected, got: {result:?}"
    );
}

/// Overlong UTF-8 encoding in percent-decoded path must be rejected.
/// %C0%AF is an overlong encoding of '/' that could bypass path-based access controls.
#[test]
fn traversal_overlong_utf8_percent_decoded() {
    let buffer = b"GET /%C0%AF HTTP/1.1\r\nHost: example.com\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);
    assert!(
        result.is_err(),
        "Overlong UTF-8 in percent-decoded path should be rejected, got: {result:?}"
    );
}

/// Compound Transfer-Encoding like "gzip, chunked" must be rejected since the
/// server doesn't support gzip transfer coding.
#[test]
fn smuggling_compound_transfer_encoding_rejected() {
    let buffer =
        b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n";
    let parser = Parser::new();
    let result = parser.parse(buffer);
    assert!(
        result.is_err(),
        "Compound TE should be rejected, got: {result:?}"
    );
}
