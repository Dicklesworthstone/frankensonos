//! HTTP Range request parsing and response generation (RFC 7233).
//!
//! This module provides support for:
//!
//! - Parsing `Range` headers (bytes=start-end, bytes=start-, bytes=-suffix)
//! - Validating ranges against resource sizes
//! - Generating `Content-Range` headers
//! - Building 206 Partial Content responses
//! - Handling 416 Range Not Satisfiable errors
//!
//! # Example
//!
//! ```ignore
//! use fastapi_http::range::{Range, parse_range_header};
//!
//! let range_header = "bytes=0-499";
//! let file_size = 1000;
//!
//! match parse_range_header(range_header, file_size) {
//!     Ok(ranges) => {
//!         // Handle partial content response
//!         for range in ranges {
//!             println!("Serve bytes {}-{}", range.start, range.end);
//!         }
//!     }
//!     Err(e) => {
//!         // Return 416 Range Not Satisfiable
//!     }
//! }
//! ```

use std::fmt;

/// A validated byte range within a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// Start byte offset (inclusive).
    pub start: u64,
    /// End byte offset (inclusive).
    pub end: u64,
}

impl ByteRange {
    /// Create a new byte range.
    ///
    /// # Panics
    ///
    /// Panics if start > end.
    #[must_use]
    pub fn new(start: u64, end: u64) -> Self {
        assert!(start <= end, "start must be <= end");
        Self { start, end }
    }

    /// Get the length of this range in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// Check if the range is empty (zero length).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false // A valid ByteRange always has at least 1 byte
    }

    /// Format as a Content-Range header value.
    ///
    /// Returns a string like "bytes 0-499/1000".
    #[must_use]
    pub fn content_range_header(&self, total_size: u64) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, total_size)
    }
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

/// Errors that can occur when parsing or validating range requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeError {
    /// The Range header syntax is invalid.
    InvalidSyntax(String),
    /// The range unit is not "bytes".
    UnsupportedUnit(String),
    /// The range is not satisfiable for the given resource size.
    NotSatisfiable {
        /// The size of the resource.
        resource_size: u64,
    },
    /// Too many ranges requested (limit exceeded).
    MultipleRangesNotSupported,
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSyntax(msg) => write!(f, "invalid range syntax: {msg}"),
            Self::UnsupportedUnit(unit) => write!(f, "unsupported range unit: {unit}"),
            Self::NotSatisfiable { resource_size } => {
                write!(
                    f,
                    "range not satisfiable for resource of size {resource_size}"
                )
            }
            Self::MultipleRangesNotSupported => write!(f, "too many ranges requested"),
        }
    }
}

impl std::error::Error for RangeError {}

/// A parsed range specification before validation against resource size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// bytes=start-end (both specified).
    FromTo { start: u64, end: u64 },
    /// bytes=start- (from start to end of resource).
    From { start: u64 },
    /// bytes=-suffix (last N bytes).
    Suffix { length: u64 },
}

impl RangeSpec {
    /// Validate and resolve this range specification against a resource size.
    ///
    /// Returns a concrete `ByteRange` if the range is satisfiable.
    ///
    /// # Errors
    ///
    /// Returns `RangeError::NotSatisfiable` if the range cannot be satisfied.
    pub fn resolve(self, resource_size: u64) -> Result<ByteRange, RangeError> {
        if resource_size == 0 {
            return Err(RangeError::NotSatisfiable { resource_size });
        }

        match self {
            Self::FromTo { start, end } => {
                // RFC 7233: If the last-byte-pos is >= resource size, use resource_size - 1
                let end = end.min(resource_size - 1);

                if start > end || start >= resource_size {
                    return Err(RangeError::NotSatisfiable { resource_size });
                }

                Ok(ByteRange::new(start, end))
            }
            Self::From { start } => {
                if start >= resource_size {
                    return Err(RangeError::NotSatisfiable { resource_size });
                }
                Ok(ByteRange::new(start, resource_size - 1))
            }
            Self::Suffix { length } => {
                if length == 0 {
                    return Err(RangeError::NotSatisfiable { resource_size });
                }
                // Last N bytes
                let start = resource_size.saturating_sub(length);
                Ok(ByteRange::new(start, resource_size - 1))
            }
        }
    }
}

/// Parse a Range header value and resolve it against a resource size.
///
/// Supports the following formats:
/// - `bytes=0-499` - First 500 bytes
/// - `bytes=500-999` - Bytes 500-999
/// - `bytes=500-` - From byte 500 to end
/// - `bytes=-500` - Last 500 bytes
/// - `bytes=0-0, 500-999` - Multiple ranges
///
/// # Errors
///
/// Returns an error if:
/// - The syntax is invalid
/// - The unit is not "bytes"
/// - Too many ranges are specified (limit exceeded)
/// - The range is not satisfiable for the given resource size
///
/// # Examples
///
/// ```
/// use fastapi_http::range::parse_range_header;
///
/// // First 500 bytes of a 1000-byte resource
/// let ranges = parse_range_header("bytes=0-499", 1000).unwrap();
/// assert_eq!(ranges[0].start, 0);
/// assert_eq!(ranges[0].end, 499);
/// assert_eq!(ranges[0].len(), 500);
///
/// // Last 100 bytes
/// let ranges = parse_range_header("bytes=-100", 1000).unwrap();
/// assert_eq!(ranges[0].start, 900);
/// assert_eq!(ranges[0].end, 999);
///
/// // From byte 500 to end
/// let ranges = parse_range_header("bytes=500-", 1000).unwrap();
/// assert_eq!(ranges[0].start, 500);
/// assert_eq!(ranges[0].end, 999);
/// ```
pub fn parse_range_header(header: &str, resource_size: u64) -> Result<Vec<ByteRange>, RangeError> {
    let specs = parse_range_spec(header)?;

    let mut ranges = Vec::with_capacity(specs.len());
    for spec in specs {
        match spec.resolve(resource_size) {
            Ok(r) => ranges.push(r),
            Err(RangeError::NotSatisfiable { .. }) => {
                // Ignore individual unsatisfiable ranges; if none overlap at all,
                // return a 416 for the full request.
            }
            Err(e) => return Err(e),
        }
    }

    if ranges.is_empty() {
        return Err(RangeError::NotSatisfiable { resource_size });
    }

    normalize_ranges(&mut ranges);
    Ok(ranges)
}

/// Parse a Range header into `RangeSpec` entries without validating against resource size.
///
/// This is useful when you want to parse the header before knowing the resource size.
///
/// # Errors
///
/// Returns an error if the syntax is invalid or too many ranges are specified.
pub fn parse_range_spec(header: &str) -> Result<Vec<RangeSpec>, RangeError> {
    let header = header.trim();

    // Split on '='
    let (unit, range_set) = header
        .split_once('=')
        .ok_or_else(|| RangeError::InvalidSyntax("missing '=' separator".to_string()))?;

    let unit = unit.trim();
    let range_set = range_set.trim();

    // Only support "bytes" unit
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(RangeError::UnsupportedUnit(unit.to_string()));
    }

    const MAX_RANGES: usize = 16;

    let mut specs = Vec::new();
    for part in range_set.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(RangeError::InvalidSyntax("empty range".to_string()));
        }
        specs.push(parse_single_range(part)?);
        if specs.len() > MAX_RANGES {
            return Err(RangeError::MultipleRangesNotSupported);
        }
    }

    Ok(specs)
}

/// Parse a single range specification (without the unit prefix).
fn parse_single_range(range: &str) -> Result<RangeSpec, RangeError> {
    let range = range.trim();

    if range.is_empty() {
        return Err(RangeError::InvalidSyntax("empty range".to_string()));
    }

    // Check for suffix range: -500
    if range.starts_with('-') {
        let suffix = &range[1..];
        let length: u64 = suffix
            .parse()
            .map_err(|_| RangeError::InvalidSyntax(format!("invalid suffix length: {suffix}")))?;
        return Ok(RangeSpec::Suffix { length });
    }

    // Split on '-'
    let (start_str, end_str) = range
        .split_once('-')
        .ok_or_else(|| RangeError::InvalidSyntax("missing '-' separator".to_string()))?;

    let start: u64 = start_str
        .trim()
        .parse()
        .map_err(|_| RangeError::InvalidSyntax(format!("invalid start: {start_str}")))?;

    let end_str = end_str.trim();

    if end_str.is_empty() {
        // Open-ended: bytes=500-
        Ok(RangeSpec::From { start })
    } else {
        // Bounded: bytes=0-499
        let end: u64 = end_str
            .parse()
            .map_err(|_| RangeError::InvalidSyntax(format!("invalid end: {end_str}")))?;
        Ok(RangeSpec::FromTo { start, end })
    }
}

fn normalize_ranges(ranges: &mut Vec<ByteRange>) {
    ranges.sort_by_key(|r| r.start);

    let mut out: Vec<ByteRange> = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        match out.last_mut() {
            None => out.push(r),
            Some(last) => {
                // Merge overlapping or adjacent ranges (e.g., 0-10 and 11-20).
                if r.start <= last.end.saturating_add(1) {
                    last.end = last.end.max(r.end);
                } else {
                    out.push(r);
                }
            }
        }
    }
    *ranges = out;
}

/// Check if a request supports range requests based on Accept-Ranges.
///
/// Returns `true` if the resource can serve partial content.
#[must_use]
pub fn supports_ranges(accept_ranges: Option<&str>) -> bool {
    match accept_ranges {
        Some(value) => value
            .split(',')
            .map(str::trim)
            .any(|unit| unit.eq_ignore_ascii_case("bytes")),
        None => false,
    }
}

/// Generate the Accept-Ranges header value for byte range support.
#[must_use]
pub const fn accept_ranges_bytes() -> &'static str {
    "bytes"
}

/// Generate a Content-Range header for an unsatisfiable range.
///
/// Returns a string like "bytes */1000" for a 416 response.
#[must_use]
pub fn content_range_unsatisfiable(resource_size: u64) -> String {
    format!("bytes */{resource_size}")
}

/// Result of validating an If-Range precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfRangeResult {
    /// The condition passed - serve partial content.
    ServePartial,
    /// The condition failed - serve full content (ignore Range header).
    ServeFull,
}

/// Check an If-Range precondition header against a validator.
///
/// The If-Range header contains either:
/// - An ETag value (e.g., `"abc123"`)
/// - A Last-Modified date (e.g., `Wed, 21 Oct 2015 07:28:00 GMT`)
///
/// If the If-Range value matches the current resource, the Range request
/// should be honored (return 206 Partial Content). Otherwise, the full
/// resource should be returned (ignore the Range header, return 200 OK).
///
/// # Arguments
///
/// * `if_range` - The If-Range header value from the request
/// * `etag` - The current ETag of the resource (if available)
/// * `last_modified` - The current Last-Modified of the resource (if available)
///
/// # Returns
///
/// - `IfRangeResult::ServePartial` if the condition is satisfied
/// - `IfRangeResult::ServeFull` if the condition fails or no validators are available
///
/// # Example
///
/// ```
/// use fastapi_http::range::{check_if_range, IfRangeResult};
///
/// // ETag match
/// let result = check_if_range(
///     "\"abc123\"",
///     Some("\"abc123\""),
///     None,
/// );
/// assert_eq!(result, IfRangeResult::ServePartial);
///
/// // ETag mismatch
/// let result = check_if_range(
///     "\"abc123\"",
///     Some("\"def456\""),
///     None,
/// );
/// assert_eq!(result, IfRangeResult::ServeFull);
/// ```
#[must_use]
pub fn check_if_range(
    if_range: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> IfRangeResult {
    let if_range = if_range.trim();

    // Empty If-Range means the condition is satisfied
    if if_range.is_empty() {
        return IfRangeResult::ServePartial;
    }

    // Check if it looks like an ETag (starts with " or W/)
    if if_range.starts_with('"') || if_range.starts_with("W/") {
        // Compare as ETag
        if let Some(current_etag) = etag {
            // Strong comparison for If-Range (weak ETags don't match)
            if etag_strong_match(if_range, current_etag) {
                return IfRangeResult::ServePartial;
            }
        }
        IfRangeResult::ServeFull
    } else {
        // Assume it's a date, compare as Last-Modified
        if let Some(current_last_modified) = last_modified {
            // Simple string comparison (dates should be in HTTP date format)
            if if_range == current_last_modified {
                return IfRangeResult::ServePartial;
            }
        }
        IfRangeResult::ServeFull
    }
}

/// Check if two ETags match using strong comparison.
///
/// For If-Range, weak ETags (W/"...") don't match. Only strong ETags match.
fn etag_strong_match(etag1: &str, etag2: &str) -> bool {
    // Weak ETags start with W/
    if etag1.starts_with("W/") || etag2.starts_with("W/") {
        return false;
    }

    // Both must be strong ETags and equal
    etag1 == etag2
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // ByteRange tests
    // =========================================================================

    #[test]
    fn byte_range_new() {
        let range = ByteRange::new(0, 499);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 499);
    }

    #[test]
    fn byte_range_len() {
        let range = ByteRange::new(0, 499);
        assert_eq!(range.len(), 500);

        let range = ByteRange::new(0, 0);
        assert_eq!(range.len(), 1);

        let range = ByteRange::new(100, 199);
        assert_eq!(range.len(), 100);
    }

    #[test]
    fn byte_range_content_range_header() {
        let range = ByteRange::new(0, 499);
        assert_eq!(range.content_range_header(1000), "bytes 0-499/1000");

        let range = ByteRange::new(500, 999);
        assert_eq!(range.content_range_header(1000), "bytes 500-999/1000");
    }

    #[test]
    fn byte_range_display() {
        let range = ByteRange::new(0, 499);
        assert_eq!(format!("{range}"), "0-499");
    }

    #[test]
    #[should_panic(expected = "start must be <= end")]
    fn byte_range_invalid() {
        let _ = ByteRange::new(500, 100);
    }

    // =========================================================================
    // RangeSpec tests
    // =========================================================================

    #[test]
    fn range_spec_from_to_valid() {
        let spec = RangeSpec::FromTo { start: 0, end: 499 };
        let range = spec.resolve(1000).unwrap();
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 499);
    }

    #[test]
    fn range_spec_from_to_clamped() {
        // End exceeds resource size, should be clamped
        let spec = RangeSpec::FromTo {
            start: 0,
            end: 9999,
        };
        let range = spec.resolve(1000).unwrap();
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 999);
    }

    #[test]
    fn range_spec_from_to_not_satisfiable() {
        let spec = RangeSpec::FromTo {
            start: 1000,
            end: 1500,
        };
        let err = spec.resolve(1000).unwrap_err();
        assert_eq!(
            err,
            RangeError::NotSatisfiable {
                resource_size: 1000
            }
        );
    }

    #[test]
    fn range_spec_from_valid() {
        let spec = RangeSpec::From { start: 500 };
        let range = spec.resolve(1000).unwrap();
        assert_eq!(range.start, 500);
        assert_eq!(range.end, 999);
    }

    #[test]
    fn range_spec_from_not_satisfiable() {
        let spec = RangeSpec::From { start: 1000 };
        let err = spec.resolve(1000).unwrap_err();
        assert_eq!(
            err,
            RangeError::NotSatisfiable {
                resource_size: 1000
            }
        );
    }

    #[test]
    fn range_spec_suffix_valid() {
        let spec = RangeSpec::Suffix { length: 100 };
        let range = spec.resolve(1000).unwrap();
        assert_eq!(range.start, 900);
        assert_eq!(range.end, 999);
    }

    #[test]
    fn range_spec_suffix_exceeds_size() {
        // Suffix larger than resource, returns entire resource
        let spec = RangeSpec::Suffix { length: 2000 };
        let range = spec.resolve(1000).unwrap();
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 999);
    }

    #[test]
    fn range_spec_suffix_zero() {
        let spec = RangeSpec::Suffix { length: 0 };
        let err = spec.resolve(1000).unwrap_err();
        assert_eq!(
            err,
            RangeError::NotSatisfiable {
                resource_size: 1000
            }
        );
    }

    #[test]
    fn range_spec_empty_resource() {
        let spec = RangeSpec::From { start: 0 };
        let err = spec.resolve(0).unwrap_err();
        assert_eq!(err, RangeError::NotSatisfiable { resource_size: 0 });
    }

    // =========================================================================
    // parse_range_header tests
    // =========================================================================

    #[test]
    fn parse_range_from_to() {
        let ranges = parse_range_header("bytes=0-499", 1000).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].end, 499);
        assert_eq!(ranges[0].len(), 500);
    }

    #[test]
    fn parse_range_from() {
        let ranges = parse_range_header("bytes=500-", 1000).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 500);
        assert_eq!(ranges[0].end, 999);
    }

    #[test]
    fn parse_range_suffix() {
        let ranges = parse_range_header("bytes=-100", 1000).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 900);
        assert_eq!(ranges[0].end, 999);
    }

    #[test]
    fn parse_range_with_spaces() {
        let ranges = parse_range_header("  bytes = 0 - 499  ", 1000).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[0].end, 499);
    }

    #[test]
    fn parse_range_invalid_unit() {
        let err = parse_range_header("items=0-10", 100).unwrap_err();
        assert!(matches!(err, RangeError::UnsupportedUnit(_)));
    }

    #[test]
    fn parse_range_multiple_ranges() {
        let ranges = parse_range_header("bytes=0-10, 20-30", 100).unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], ByteRange::new(0, 10));
        assert_eq!(ranges[1], ByteRange::new(20, 30));
    }

    #[test]
    fn parse_range_too_many_ranges_rejected() {
        let header = (0..17)
            .map(|i| format!("{i}-{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let header = format!("bytes={header}");
        let err = parse_range_header(&header, 1000).unwrap_err();
        assert_eq!(err, RangeError::MultipleRangesNotSupported);
    }

    #[test]
    fn parse_range_invalid_syntax_no_equals() {
        let err = parse_range_header("bytes 0-10", 100).unwrap_err();
        assert!(matches!(err, RangeError::InvalidSyntax(_)));
    }

    #[test]
    fn parse_range_invalid_syntax_no_dash() {
        let err = parse_range_header("bytes=100", 100).unwrap_err();
        assert!(matches!(err, RangeError::InvalidSyntax(_)));
    }

    #[test]
    fn parse_range_invalid_start() {
        let err = parse_range_header("bytes=abc-100", 1000).unwrap_err();
        assert!(matches!(err, RangeError::InvalidSyntax(_)));
    }

    #[test]
    fn parse_range_invalid_end() {
        let err = parse_range_header("bytes=0-xyz", 1000).unwrap_err();
        assert!(matches!(err, RangeError::InvalidSyntax(_)));
    }

    #[test]
    fn parse_range_not_satisfiable() {
        let err = parse_range_header("bytes=1000-2000", 500).unwrap_err();
        assert_eq!(err, RangeError::NotSatisfiable { resource_size: 500 });
    }

    // =========================================================================
    // Helper function tests
    // =========================================================================

    #[test]
    fn test_accept_ranges_bytes() {
        assert_eq!(accept_ranges_bytes(), "bytes");
    }

    #[test]
    fn test_content_range_unsatisfiable() {
        assert_eq!(content_range_unsatisfiable(1000), "bytes */1000");
    }

    #[test]
    fn test_supports_ranges() {
        assert!(supports_ranges(Some("bytes")));
        assert!(supports_ranges(Some("Bytes")));
        assert!(supports_ranges(Some("bytes, other")));
        assert!(!supports_ranges(Some("none")));
        assert!(!supports_ranges(Some("None")));
        assert!(!supports_ranges(Some("items")));
        assert!(!supports_ranges(Some("")));
        assert!(!supports_ranges(Some("   ")));
        assert!(!supports_ranges(None));
    }

    // =========================================================================
    // RangeError Display tests
    // =========================================================================

    #[test]
    fn range_error_display() {
        let err = RangeError::InvalidSyntax("test".to_string());
        assert!(format!("{err}").contains("invalid range syntax"));

        let err = RangeError::UnsupportedUnit("items".to_string());
        assert!(format!("{err}").contains("unsupported range unit: items"));

        let err = RangeError::NotSatisfiable { resource_size: 500 };
        assert!(format!("{err}").contains("range not satisfiable"));

        let err = RangeError::MultipleRangesNotSupported;
        assert!(format!("{err}").contains("too many ranges requested"));
    }

    // =========================================================================
    // If-Range tests
    // =========================================================================

    #[test]
    fn if_range_etag_match() {
        let result = check_if_range("\"abc123\"", Some("\"abc123\""), None);
        assert_eq!(result, IfRangeResult::ServePartial);
    }

    #[test]
    fn if_range_etag_mismatch() {
        let result = check_if_range("\"abc123\"", Some("\"def456\""), None);
        assert_eq!(result, IfRangeResult::ServeFull);
    }

    #[test]
    fn if_range_etag_no_current() {
        let result = check_if_range("\"abc123\"", None, None);
        assert_eq!(result, IfRangeResult::ServeFull);
    }

    #[test]
    fn if_range_weak_etag_never_matches() {
        // Weak ETag in If-Range
        let result = check_if_range("W/\"abc123\"", Some("W/\"abc123\""), None);
        assert_eq!(result, IfRangeResult::ServeFull);

        // Weak ETag in current
        let result = check_if_range("\"abc123\"", Some("W/\"abc123\""), None);
        assert_eq!(result, IfRangeResult::ServeFull);
    }

    #[test]
    fn if_range_date_match() {
        let date = "Wed, 21 Oct 2015 07:28:00 GMT";
        let result = check_if_range(date, None, Some(date));
        assert_eq!(result, IfRangeResult::ServePartial);
    }

    #[test]
    fn if_range_date_mismatch() {
        let result = check_if_range(
            "Wed, 21 Oct 2015 07:28:00 GMT",
            None,
            Some("Thu, 22 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(result, IfRangeResult::ServeFull);
    }

    #[test]
    fn if_range_date_no_current() {
        let result = check_if_range("Wed, 21 Oct 2015 07:28:00 GMT", None, None);
        assert_eq!(result, IfRangeResult::ServeFull);
    }

    #[test]
    fn if_range_empty_header() {
        let result = check_if_range("", None, None);
        assert_eq!(result, IfRangeResult::ServePartial);
    }

    #[test]
    fn if_range_whitespace_trimmed() {
        let result = check_if_range("  \"abc123\"  ", Some("\"abc123\""), None);
        assert_eq!(result, IfRangeResult::ServePartial);
    }
}
