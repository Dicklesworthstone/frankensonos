//! Query string parsing utilities.
//!
//! This module provides zero-copy query string parsing that handles:
//! - Key-value pair extraction
//! - Multi-value parameters (same key appearing multiple times)
//! - Percent-decoding
//! - Edge cases (empty values, missing values)
//!
//! # Example
//!
//! ```
//! use fastapi_http::QueryString;
//!
//! let qs = QueryString::parse("a=1&b=2&a=3");
//!
//! // Single value access
//! assert_eq!(qs.get("a"), Some("1"));
//! assert_eq!(qs.get("b"), Some("2"));
//!
//! // Multi-value access
//! let a_values: Vec<_> = qs.get_all("a").collect();
//! assert_eq!(a_values, vec!["1", "3"]);
//! ```

use std::borrow::Cow;

/// Maximum number of query parameters to parse.
///
/// This limit prevents algorithmic complexity DoS attacks where an attacker
/// sends a query string with thousands of parameters. Parameters beyond this
/// limit are silently ignored.
pub const MAX_QUERY_PARAMS: usize = 256;

/// A parsed query string with efficient access to parameters.
///
/// Query strings are parsed lazily - the input is stored and parsed
/// on each access. For repeated access patterns, consider using
/// `to_pairs()` to materialize the results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryString<'a> {
    raw: &'a str,
}

impl<'a> QueryString<'a> {
    /// Parse a query string (without the leading `?`).
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("name=alice&age=30");
    /// assert_eq!(qs.get("name"), Some("alice"));
    /// ```
    #[must_use]
    pub fn parse(raw: &'a str) -> Self {
        Self { raw }
    }

    /// Returns true if the query string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Returns the raw query string.
    #[must_use]
    pub fn raw(&self) -> &'a str {
        self.raw
    }

    /// Get the first value for a key.
    ///
    /// Returns `None` if the key doesn't exist.
    /// Returns the raw (percent-encoded) value. Use `get_decoded` for decoded values.
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("name=alice&name=bob");
    /// assert_eq!(qs.get("name"), Some("alice")); // First value
    /// assert_eq!(qs.get("missing"), None);
    /// ```
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.pairs().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// Get all values for a key.
    ///
    /// Returns an iterator over all values for the given key.
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("color=red&color=blue&color=green");
    /// let colors: Vec<_> = qs.get_all("color").collect();
    /// assert_eq!(colors, vec!["red", "blue", "green"]);
    /// ```
    pub fn get_all(&self, key: &str) -> impl Iterator<Item = &'a str> {
        self.pairs().filter(move |(k, _)| *k == key).map(|(_, v)| v)
    }

    /// Get the first value for a key, percent-decoded.
    ///
    /// Returns a `Cow` that is borrowed if no decoding was needed,
    /// or owned if percent-decoding was performed.
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("msg=hello%20world");
    /// assert_eq!(qs.get_decoded("msg").as_deref(), Some("hello world"));
    /// ```
    #[must_use]
    pub fn get_decoded(&self, key: &str) -> Option<Cow<'a, str>> {
        self.get(key).map(percent_decode)
    }

    /// Check if a key exists in the query string.
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("flag&name=alice");
    /// assert!(qs.contains("flag"));
    /// assert!(qs.contains("name"));
    /// assert!(!qs.contains("missing"));
    /// ```
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.pairs().any(|(k, _)| k == key)
    }

    /// Returns an iterator over all key-value pairs.
    ///
    /// Keys without values (like `?flag`) have empty string values.
    /// Values are NOT percent-decoded; use `pairs_decoded` for that.
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("a=1&b=2&flag");
    /// let pairs: Vec<_> = qs.pairs().collect();
    /// assert_eq!(pairs, vec![("a", "1"), ("b", "2"), ("flag", "")]);
    /// ```
    pub fn pairs(&self) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.raw
            .split('&')
            .filter(|s| !s.is_empty())
            .take(MAX_QUERY_PARAMS) // Limit to prevent DoS
            .map(|pair| {
                if let Some(eq_pos) = pair.find('=') {
                    (&pair[..eq_pos], &pair[eq_pos + 1..])
                } else {
                    // Key without value: "flag" -> ("flag", "")
                    (pair, "")
                }
            })
    }

    /// Returns an iterator over all key-value pairs, with values percent-decoded.
    ///
    /// # Example
    ///
    /// ```
    /// use fastapi_http::QueryString;
    ///
    /// let qs = QueryString::parse("name=hello%20world&id=123");
    /// let pairs: Vec<_> = qs.pairs_decoded().collect();
    /// assert_eq!(pairs[0].0, "name");
    /// assert_eq!(&*pairs[0].1, "hello world");
    /// ```
    pub fn pairs_decoded(&self) -> impl Iterator<Item = (&'a str, Cow<'a, str>)> {
        self.pairs().map(|(k, v)| (k, percent_decode(v)))
    }

    /// Collect all pairs into a vector.
    ///
    /// Useful when you need to iterate multiple times.
    #[must_use]
    pub fn to_pairs(&self) -> Vec<(&'a str, &'a str)> {
        self.pairs().collect()
    }

    /// Count the number of parameters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs().count()
    }
}

impl Default for QueryString<'_> {
    fn default() -> Self {
        Self { raw: "" }
    }
}

/// Percent-decode a string.
///
/// Returns a `Cow::Borrowed` if no decoding was needed (most common case),
/// or `Cow::Owned` if percent sequences were decoded.
///
/// Handles:
/// - Standard percent-encoding (%XX)
/// - UTF-8 multi-byte sequences
/// - Plus sign as space (common in form data)
///
/// Invalid sequences are left as-is for robustness.
///
/// # Example
///
/// ```
/// use fastapi_http::percent_decode;
///
/// // No decoding needed - returns borrowed
/// let simple = percent_decode("hello");
/// assert!(matches!(simple, std::borrow::Cow::Borrowed(_)));
///
/// // Decoding needed - returns owned
/// let encoded = percent_decode("hello%20world");
/// assert_eq!(&*encoded, "hello world");
///
/// // Plus as space
/// let plus = percent_decode("hello+world");
/// assert_eq!(&*plus, "hello world");
/// ```
pub fn percent_decode(s: &str) -> Cow<'_, str> {
    // Fast path: no encoding
    if !s.contains('%') && !s.contains('+') {
        return Cow::Borrowed(s);
    }

    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                // Try to decode hex pair
                if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    result.push(hi << 4 | lo);
                    i += 3;
                } else {
                    // Invalid hex, keep as-is
                    result.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                // Plus as space (application/x-www-form-urlencoded)
                result.push(b' ');
                i += 1;
            }
            b => {
                result.push(b);
                i += 1;
            }
        }
    }

    // SAFETY: We only decode valid UTF-8 percent sequences,
    // and non-encoded bytes pass through unchanged.
    // Invalid UTF-8 will be handled by from_utf8_lossy.
    Cow::Owned(String::from_utf8_lossy(&result).into_owned())
}

/// Convert a hex digit to its numeric value.
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

    #[test]
    fn empty_query_string() {
        let qs = QueryString::parse("");
        assert!(qs.is_empty());
        assert_eq!(qs.len(), 0);
        assert_eq!(qs.get("any"), None);
    }

    #[test]
    fn single_param() {
        let qs = QueryString::parse("name=alice");
        assert!(!qs.is_empty());
        assert_eq!(qs.len(), 1);
        assert_eq!(qs.get("name"), Some("alice"));
        assert_eq!(qs.get("other"), None);
    }

    #[test]
    fn multiple_params() {
        let qs = QueryString::parse("a=1&b=2&c=3");
        assert_eq!(qs.len(), 3);
        assert_eq!(qs.get("a"), Some("1"));
        assert_eq!(qs.get("b"), Some("2"));
        assert_eq!(qs.get("c"), Some("3"));
    }

    #[test]
    fn duplicate_keys() {
        let qs = QueryString::parse("a=1&b=2&a=3");

        // get() returns first value
        assert_eq!(qs.get("a"), Some("1"));

        // get_all() returns all values
        let all_a: Vec<_> = qs.get_all("a").collect();
        assert_eq!(all_a, vec!["1", "3"]);
    }

    #[test]
    fn empty_value() {
        let qs = QueryString::parse("name=&age=30");
        assert_eq!(qs.get("name"), Some(""));
        assert_eq!(qs.get("age"), Some("30"));
    }

    #[test]
    fn key_without_value() {
        let qs = QueryString::parse("flag&name=alice");
        assert!(qs.contains("flag"));
        assert_eq!(qs.get("flag"), Some(""));
        assert_eq!(qs.get("name"), Some("alice"));
    }

    #[test]
    fn percent_encoded_value() {
        let qs = QueryString::parse("msg=hello%20world");
        assert_eq!(qs.get("msg"), Some("hello%20world")); // raw
        assert_eq!(qs.get_decoded("msg").as_deref(), Some("hello world")); // decoded
    }

    #[test]
    fn plus_as_space() {
        let qs = QueryString::parse("msg=hello+world");
        assert_eq!(qs.get("msg"), Some("hello+world")); // raw
        assert_eq!(qs.get_decoded("msg").as_deref(), Some("hello world")); // decoded
    }

    #[test]
    fn utf8_encoded() {
        // "café" encoded: caf%C3%A9
        let qs = QueryString::parse("word=caf%C3%A9");
        assert_eq!(qs.get_decoded("word").as_deref(), Some("café"));
    }

    #[test]
    fn special_chars_encoded() {
        // & encoded as %26, = encoded as %3D
        let qs = QueryString::parse("data=a%26b%3Dc");
        assert_eq!(qs.get_decoded("data").as_deref(), Some("a&b=c"));
    }

    #[test]
    fn pairs_iterator() {
        let qs = QueryString::parse("a=1&b=2&c=3");
        let pairs: Vec<_> = qs.pairs().collect();
        assert_eq!(pairs, vec![("a", "1"), ("b", "2"), ("c", "3")]);
    }

    #[test]
    fn pairs_decoded_iterator() {
        let qs = QueryString::parse("name=hello%20world&id=123");
        let pairs: Vec<_> = qs.pairs_decoded().collect();
        assert_eq!(pairs[0].0, "name");
        assert_eq!(&*pairs[0].1, "hello world");
        assert_eq!(pairs[1].0, "id");
        assert_eq!(&*pairs[1].1, "123");
    }

    #[test]
    fn to_pairs() {
        let qs = QueryString::parse("x=1&y=2");
        let pairs = qs.to_pairs();
        assert_eq!(pairs, vec![("x", "1"), ("y", "2")]);
    }

    #[test]
    fn contains() {
        let qs = QueryString::parse("a=1&b=2");
        assert!(qs.contains("a"));
        assert!(qs.contains("b"));
        assert!(!qs.contains("c"));
    }

    #[test]
    fn raw_accessor() {
        let qs = QueryString::parse("a=1&b=2");
        assert_eq!(qs.raw(), "a=1&b=2");
    }

    #[test]
    fn trailing_ampersand() {
        let qs = QueryString::parse("a=1&b=2&");
        assert_eq!(qs.len(), 2); // Empty segment is filtered
        assert_eq!(qs.get("a"), Some("1"));
        assert_eq!(qs.get("b"), Some("2"));
    }

    #[test]
    fn leading_ampersand() {
        let qs = QueryString::parse("&a=1&b=2");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs.get("a"), Some("1"));
        assert_eq!(qs.get("b"), Some("2"));
    }

    #[test]
    fn percent_decode_no_encoding() {
        let s = "hello";
        let decoded = percent_decode(s);
        assert!(matches!(decoded, Cow::Borrowed(_)));
        assert_eq!(&*decoded, "hello");
    }

    #[test]
    fn percent_decode_simple() {
        assert_eq!(&*percent_decode("hello%20world"), "hello world");
        assert_eq!(&*percent_decode("%2F"), "/");
        assert_eq!(&*percent_decode("%3D"), "=");
    }

    #[test]
    fn percent_decode_invalid_hex() {
        // Invalid hex should be kept as-is
        assert_eq!(&*percent_decode("%ZZ"), "%ZZ");
        assert_eq!(&*percent_decode("%2"), "%2"); // Incomplete
    }

    #[test]
    fn percent_decode_mixed() {
        assert_eq!(&*percent_decode("a%20b%20c"), "a b c");
        assert_eq!(&*percent_decode("hello+world%21"), "hello world!");
    }

    #[test]
    fn hex_digit_values() {
        assert_eq!(hex_digit(b'0'), Some(0));
        assert_eq!(hex_digit(b'9'), Some(9));
        assert_eq!(hex_digit(b'a'), Some(10));
        assert_eq!(hex_digit(b'f'), Some(15));
        assert_eq!(hex_digit(b'A'), Some(10));
        assert_eq!(hex_digit(b'F'), Some(15));
        assert_eq!(hex_digit(b'g'), None);
        assert_eq!(hex_digit(b'Z'), None);
    }

    #[test]
    fn default_is_empty() {
        let qs = QueryString::default();
        assert!(qs.is_empty());
        assert_eq!(qs.len(), 0);
    }

    #[test]
    fn acceptance_criteria_test() {
        // Test the exact example from acceptance criteria:
        // Parses ?a=1&b=2&a=3 into multi-value map correctly
        let qs = QueryString::parse("a=1&b=2&a=3");

        // First value for 'a'
        assert_eq!(qs.get("a"), Some("1"));

        // All values for 'a'
        let all_a: Vec<_> = qs.get_all("a").collect();
        assert_eq!(all_a, vec!["1", "3"]);

        // Value for 'b'
        assert_eq!(qs.get("b"), Some("2"));

        // Total count
        assert_eq!(qs.len(), 3);
    }
}
