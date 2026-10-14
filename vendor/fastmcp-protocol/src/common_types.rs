//! Final MCP common wire types kept separate while the legacy type surface is migrated.
//!
//! This module owns structural admission and byte-preserving serialization for the PRT-02.A
//! common-type slice.  Scheme-specific fetching, rendering, and authorization policies remain
//! outside this wire layer.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// A structural rejection at the protocol wire boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommonTypeError {
    /// A value did not have the final-schema shape.
    Invalid(&'static str),
    /// A supplied value exceeded a bounded wire limit.
    TooLong(&'static str),
}

impl fmt::Display for CommonTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid {field}"),
            Self::TooLong(field) => write!(formatter, "{field} exceeds its wire limit"),
        }
    }
}

impl std::error::Error for CommonTypeError {}

/// Maximum encoded bytes admitted for ordinary URI wire fields.
pub const MAX_ABSOLUTE_URI_BYTES: usize = 64 * 1024;
/// Maximum bytes in a `data:` icon media-type and parameter prefix, through the comma.
pub const MAX_ICON_DATA_URI_PREFIX_BYTES: usize = 1024;
/// Maximum decoded bytes represented by a raw icon `data:` URI.
pub const MAX_ICON_DATA_URI_DECODED_BYTES: usize = 8 * 1024 * 1024;
/// Maximum encoded bytes in a raw icon `data:` URI, including its prefix.
pub const MAX_ICON_DATA_URI_ENCODED_BYTES: usize =
    4 * MAX_ICON_DATA_URI_DECODED_BYTES.div_ceil(3) + MAX_ICON_DATA_URI_PREFIX_BYTES;
/// Historical cancellation-reason size used by earlier bounded profiles.
///
/// MCP 2024-11-05 and MCP 2026-07-28 do not impose this wire limit. Exact
/// cancellation decoding therefore does not enforce this value.
pub const MAX_CANCELLATION_REASON_BYTES: usize = 4 * 1024;
/// Maximum number of retained open metadata entries.
pub const MAX_METADATA_ENTRIES: usize = 128;
/// Maximum UTF-8 bytes in an individual metadata key.
pub const MAX_METADATA_KEY_BYTES: usize = 512;
/// Maximum canonical JSON bytes in an individual metadata value.
pub const MAX_METADATA_VALUE_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 bytes in a present pagination cursor.
pub const MAX_CURSOR_BYTES: usize = 4 * 1024;
/// Maximum icon size strings retained from one peer icon.
pub const MAX_ICON_SIZE_ENTRIES: usize = 32;
/// Maximum UTF-8 bytes in an individual peer icon size string.
pub const MAX_ICON_SIZE_BYTES: usize = 128;
/// Maximum encoded bytes in one common binary content payload.
pub const MAX_CONTENT_ENCODED_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes in each W3C trace field.
pub const MAX_TRACE_FIELD_BYTES: usize = 4 * 1024;
/// Maximum bytes retained for one exact finite final progress number lexeme.
pub const MAX_EXACT_PROGRESS_NUMBER_BYTES: usize = 256;
/// Largest absolute decimal exponent accepted for one exact finite final progress number.
pub const MAX_EXACT_PROGRESS_EXPONENT_ABS: i32 = 9_999;
/// Maximum bytes retained for one arbitrary-width JSON integer token.
pub const MAX_JSON_INTEGER_BYTES: usize = 4 * 1024;
/// Largest absolute decimal exponent admitted for one JSON integer token.
pub const MAX_JSON_INTEGER_EXPONENT_ABS: i32 = 10_000;

/// A schema-valid RFC 3986 URI with a required ASCII scheme.
///
/// This type deliberately preserves the original wire spelling. It does not fetch, normalize,
/// lowercase, resolve, or otherwise authorize the URI.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AbsoluteUri(String);

impl AbsoluteUri {
    /// Parses an absolute URI without changing its bytes.
    pub fn parse(value: impl Into<String>) -> Result<Self, CommonTypeError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_ABSOLUTE_URI_BYTES {
            return Err(if value.len() > MAX_ABSOLUTE_URI_BYTES {
                CommonTypeError::TooLong("URI")
            } else {
                CommonTypeError::Invalid("absolute URI")
            });
        }
        if !value.is_ascii() || value.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
            return Err(CommonTypeError::Invalid("absolute URI"));
        }
        let Some(colon) = value.find(':') else {
            return Err(CommonTypeError::Invalid("URI scheme"));
        };
        let scheme = &value[..colon];
        if scheme.is_empty()
            || !scheme.as_bytes()[0].is_ascii_alphabetic()
            || !scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
            || !valid_uri_remainder(&value[colon + 1..])
        {
            return Err(CommonTypeError::Invalid("absolute URI"));
        }
        Ok(Self(value))
    }

    /// Returns the exact schema-valid wire string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the original scheme spelling.
    #[must_use]
    pub fn scheme(&self) -> &str {
        &self.0[..self.0.find(':').expect("validated URI has a scheme")]
    }

    /// Tests a scheme with RFC 3986 ASCII-case-insensitive comparison.
    #[must_use]
    pub fn has_scheme(&self, scheme: &str) -> bool {
        self.scheme().eq_ignore_ascii_case(scheme)
    }
}

impl<'de> Deserialize<'de> for AbsoluteUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|value| Self::parse(value).map_err(serde::de::Error::custom))
    }
}

/// A raw icon source URI with a dedicated data-image budget.
///
/// This is a wire-admission type only: it preserves a schema-valid source exactly and does not
/// fetch, render, or otherwise grant authority over it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RawIconSourceUri(String);

impl RawIconSourceUri {
    /// Parses a raw icon source without normalizing its wire spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, CommonTypeError> {
        let value = value.into();
        let Some(colon) = value.find(':') else {
            return Err(CommonTypeError::Invalid("URI scheme"));
        };
        let scheme = &value[..colon];
        let limit = if scheme.eq_ignore_ascii_case("data") {
            MAX_ICON_DATA_URI_ENCODED_BYTES
        } else {
            MAX_ABSOLUTE_URI_BYTES
        };
        if value.is_empty() || value.len() > limit {
            return Err(if value.len() > limit {
                CommonTypeError::TooLong("icon source URI")
            } else {
                CommonTypeError::Invalid("absolute URI")
            });
        }
        if !value.is_ascii()
            || value.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
            || scheme.is_empty()
            || !scheme.as_bytes()[0].is_ascii_alphabetic()
            || !scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
            || !valid_uri_remainder(&value[colon + 1..])
        {
            return Err(CommonTypeError::Invalid("absolute URI"));
        }
        if scheme.eq_ignore_ascii_case("data") {
            let before_fragment = value
                .split_once('#')
                .map_or(value.as_str(), |(before_fragment, _)| before_fragment);
            let structural_data_uri = before_fragment
                .split_once('?')
                .map_or(before_fragment, |(before_query, _)| before_query);
            validate_icon_data_uri(structural_data_uri)?;
        }
        Ok(Self(value))
    }

    /// Returns the exact schema-valid source spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RawIconSourceUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|value| Self::parse(value).map_err(serde::de::Error::custom))
    }
}

fn validate_icon_data_uri(value: &str) -> Result<(), CommonTypeError> {
    let data = &value["data:".len()..];
    let Some((prefix, payload)) = data.split_once(',') else {
        return Err(CommonTypeError::Invalid("icon data URI"));
    };
    if prefix.len() + 1 > MAX_ICON_DATA_URI_PREFIX_BYTES {
        return Err(CommonTypeError::TooLong("icon data URI prefix"));
    }
    let Some(media_type) = prefix.strip_suffix(";base64") else {
        return Err(CommonTypeError::Invalid("icon data URI"));
    };
    if !media_type
        .split_once('/')
        .is_some_and(|(kind, _)| kind.eq_ignore_ascii_case("image"))
        || !valid_mime_type(media_type)
    {
        return Err(CommonTypeError::Invalid("icon data MIME type"));
    }
    let decoded_upper_bound = base64_decoded_upper_bound(payload)?;
    if decoded_upper_bound > MAX_ICON_DATA_URI_DECODED_BYTES {
        return Err(CommonTypeError::TooLong("icon data URI"));
    }
    validate_standard_base64(payload)
}

fn base64_decoded_upper_bound(value: &str) -> Result<usize, CommonTypeError> {
    let unpadded = value.trim_end_matches('=');
    if unpadded.len() % 4 == 1 || value[..unpadded.len()].contains('=') {
        return Err(CommonTypeError::Invalid("base64 content"));
    }
    let groups = unpadded.len() / 4;
    let remainder = unpadded.len() % 4;
    groups
        .checked_mul(3)
        .and_then(|size| size.checked_add(if remainder == 0 { 0 } else { remainder - 1 }))
        .ok_or(CommonTypeError::TooLong("base64 content"))
}

fn valid_uri_remainder(value: &str) -> bool {
    let (before_fragment, fragment) = match value.split_once('#') {
        Some((before_fragment, fragment)) if !fragment.contains('#') => {
            (before_fragment, Some(fragment))
        }
        Some(_) => return false,
        None => (value, None),
    };
    let (hier_part, query) = match before_fragment.split_once('?') {
        Some((hier_part, query)) if !query.contains('?') => (hier_part, Some(query)),
        Some((hier_part, query)) => (hier_part, Some(query)),
        None => (before_fragment, None),
    };
    valid_hier_part(hier_part)
        && query.is_none_or(valid_query_or_fragment)
        && fragment.is_none_or(valid_query_or_fragment)
}

fn valid_hier_part(value: &str) -> bool {
    if let Some(authority_and_path) = value.strip_prefix("//") {
        let (authority, path) = authority_and_path
            .split_once('/')
            .map_or((authority_and_path, ""), |(authority, suffix)| {
                (authority, suffix)
            });
        valid_authority(authority) && valid_path(path)
    } else {
        valid_path(value)
    }
}

fn valid_authority(value: &str) -> bool {
    let (userinfo, host_and_port) = match value.rsplit_once('@') {
        Some((userinfo, host_and_port)) if !userinfo.contains('@') && valid_userinfo(userinfo) => {
            (Some(userinfo), host_and_port)
        }
        Some(_) => return false,
        None => (None, value),
    };
    let _ = userinfo;
    if let Some(host) = host_and_port.strip_prefix('[') {
        let Some((literal, port)) = host.split_once(']') else {
            return false;
        };
        if port.contains(']') || !valid_port(port) {
            return false;
        }
        return valid_ip_literal(literal);
    }
    if host_and_port.contains('[') || host_and_port.contains(']') {
        return false;
    }
    let (host, port) = host_and_port
        .rsplit_once(':')
        .map_or((host_and_port, None), |(host, port)| (host, Some(port)));
    valid_reg_name(host) && port.is_none_or(|port| port.bytes().all(|byte| byte.is_ascii_digit()))
}

fn valid_ip_literal(value: &str) -> bool {
    value.parse::<std::net::Ipv6Addr>().is_ok()
        || value
            .strip_prefix('v')
            .or_else(|| value.strip_prefix('V'))
            .is_some_and(|future| {
                let Some((version, address)) = future.split_once('.') else {
                    return false;
                };
                !version.is_empty()
                    && version.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && !address.is_empty()
                    && address.bytes().all(is_ip_future_character)
            })
}

fn is_ip_future_character(byte: u8) -> bool {
    is_unreserved(byte) || is_sub_delim(byte) || byte == b':'
}

fn valid_port(value: &str) -> bool {
    value.is_empty()
        || (value.starts_with(':') && value[1..].bytes().all(|byte| byte.is_ascii_digit()))
}

fn valid_userinfo(value: &str) -> bool {
    valid_component(value, |byte| is_pchar(byte) || byte == b':')
}

fn valid_reg_name(value: &str) -> bool {
    valid_component(value, |byte| is_unreserved(byte) || is_sub_delim(byte))
}

fn valid_path(value: &str) -> bool {
    valid_component(value, |byte| is_pchar(byte) || byte == b'/')
}

fn valid_query_or_fragment(value: &str) -> bool {
    valid_component(value, |byte| is_pchar(byte) || matches!(byte, b'/' | b'?'))
}

fn valid_component(value: &str, permits: impl Fn(u8) -> bool) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else if permits(bytes[index]) {
            index += 1;
        } else {
            return false;
        }
    }
    true
}

fn is_pchar(byte: u8) -> bool {
    is_unreserved(byte) || is_sub_delim(byte) || matches!(byte, b':' | b'@')
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn is_sub_delim(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}

/// An opaque cursor where only absence means the end of a paginated result set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpaqueCursor {
    /// The wire field was absent.
    Absent,
    /// The wire field was present, including the valid empty string.
    Present(String),
}

impl Serialize for OpaqueCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            // A standalone value cannot represent an absent object member. Enclosing wire
            // objects must omit an absent cursor; serializing it as `null` would conflate two
            // distinct wire states.
            Self::Absent => Err(serde::ser::Error::custom(
                "an absent cursor must be omitted from its enclosing object",
            )),
            Self::Present(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for OpaqueCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).and_then(|value| {
            Self::try_from_presence(Some(value)).map_err(serde::de::Error::custom)
        })
    }
}

impl OpaqueCursor {
    /// Preserves absent, empty, and nonempty cursor states distinctly.
    #[must_use]
    pub fn from_presence(value: Option<String>) -> Self {
        value.map_or(Self::Absent, Self::Present)
    }

    /// Admits a bounded cursor while preserving absent and empty states.
    pub fn try_from_presence(value: Option<String>) -> Result<Self, CommonTypeError> {
        if value
            .as_ref()
            .is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES)
        {
            return Err(CommonTypeError::TooLong("pagination cursor"));
        }
        Ok(Self::from_presence(value))
    }

    /// Returns the present value, if the field occurred on the wire.
    #[must_use]
    pub fn as_present(&self) -> Option<&str> {
        match self {
            Self::Absent => None,
            Self::Present(value) => Some(value),
        }
    }
}

/// A JSON integer retained without an implementation-width bound.
///
/// The final schema uses JSON Schema's `integer` type, which is not limited to
/// Rust's fixed-width integer types. The workspace enables serde_json's
/// arbitrary-precision feature, so retaining the original [`serde_json::Number`]
/// preserves both large positive and negative integer spellings on re-encode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonInteger(serde_json::Number);

impl JsonInteger {
    /// Admits one JSON number only when it is a mathematical integer.
    pub fn try_from_number(value: serde_json::Number) -> Result<Self, CommonTypeError> {
        validate_json_integer(value.as_str())?;
        Ok(Self(value))
    }

    /// Returns the exact retained JSON integer spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the retained number for direct `Value::Number` construction.
    ///
    /// Embedding a `JsonInteger` through `json!`/`to_value` re-parses the
    /// number and canonicalizes its spelling; building the `Value` from this
    /// accessor keeps the retained lexeme.
    #[must_use]
    pub fn to_number(&self) -> serde_json::Number {
        self.0.clone()
    }

    /// Returns the mathematical value when it fits the legacy signed 32-bit
    /// error-code domain.
    ///
    /// Integral JSON spellings with a fractional part or exponent, such as
    /// `-32600.0` and `-326e2`, are accepted without changing the retained
    /// wire lexeme.
    #[must_use]
    pub fn as_i32(&self) -> Option<i32> {
        json_integer_as_i32(self.as_str())
    }
}

impl std::str::FromStr for JsonInteger {
    type Err = CommonTypeError;

    /// Parses one JSON integer token without normalizing its spelling.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_json_integer(value)?;

        // `validate_json_integer` verifies JSON-number grammar, bounds, and mathematical
        // integrality before retaining the caller's exact token. serde_json's
        // parser inserts `+` into a positive exponent, so parsing first would
        // lose a valid lexeme such as `-326e2`.
        Ok(Self(serde_json::Number::from_string_unchecked(
            value.to_owned(),
        )))
    }
}

impl TryFrom<&str> for JsonInteger {
    type Error = CommonTypeError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for JsonInteger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<i32> for JsonInteger {
    fn from(value: i32) -> Self {
        Self(serde_json::Number::from(value))
    }
}

impl From<fastmcp_core::McpErrorCode> for JsonInteger {
    fn from(value: fastmcp_core::McpErrorCode) -> Self {
        Self::from(i32::from(value))
    }
}

impl From<i64> for JsonInteger {
    fn from(value: i64) -> Self {
        Self(serde_json::Number::from(value))
    }
}

impl From<u64> for JsonInteger {
    fn from(value: u64) -> Self {
        Self(serde_json::Number::from(value))
    }
}

impl Serialize for JsonInteger {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

/// serde_json's private raw-value newtype token (stable: serde_json is
/// pinned exactly).
const SERDE_JSON_RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";
/// serde_json's private arbitrary-precision number map key (stable: pinned).
const SERDE_JSON_NUMBER_TOKEN: &str = "$serde_json::private::Number";

impl<'de> Deserialize<'de> for JsonInteger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // A plain Box<RawValue> decode preserves the wire lexeme but cannot
        // survive serde's buffered Content replay, which every untagged
        // context uses - most critically the JsonRpcMessage enum, where it
        // made this crate unable to parse error responses it had itself
        // encoded. Requesting the raw-value newtype directly and accepting
        // BOTH magic map spellings keeps the exact lexeme on the direct
        // parser path while still decoding through buffered replay (whose
        // parser-canonicalized number spelling is the best available there).
        struct JsonIntegerVisitor;

        impl<'de> serde::de::Visitor<'de> for JsonIntegerVisitor {
            type Value = JsonInteger;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a mathematically integral JSON number")
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<JsonInteger, E> {
                Ok(JsonInteger::from(value))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<JsonInteger, E> {
                Ok(JsonInteger::from(value))
            }

            fn visit_i128<E: serde::de::Error>(self, value: i128) -> Result<JsonInteger, E> {
                value.to_string().parse().map_err(E::custom)
            }

            fn visit_u128<E: serde::de::Error>(self, value: u128) -> Result<JsonInteger, E> {
                value.to_string().parse().map_err(E::custom)
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<JsonInteger, E> {
                serde_json::Number::from_f64(value)
                    .ok_or_else(|| E::custom("JSON integer must be finite"))
                    .and_then(|number| JsonInteger::try_from_number(number).map_err(E::custom))
            }

            fn visit_newtype_struct<D2>(self, deserializer: D2) -> Result<JsonInteger, D2::Error>
            where
                D2: serde::Deserializer<'de>,
            {
                deserializer.deserialize_any(self)
            }

            fn visit_map<A>(self, mut map: A) -> Result<JsonInteger, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let Some(key) = map.next_key::<std::borrow::Cow<'_, str>>()? else {
                    return Err(serde::de::Error::custom("JSON integer cannot be an object"));
                };
                if key != SERDE_JSON_RAW_VALUE_TOKEN && key != SERDE_JSON_NUMBER_TOKEN {
                    return Err(serde::de::Error::custom("JSON integer cannot be an object"));
                }
                let lexeme = map.next_value::<std::borrow::Cow<'_, str>>()?;
                lexeme.parse().map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_newtype_struct(SERDE_JSON_RAW_VALUE_TOKEN, JsonIntegerVisitor)
    }
}

/// A bounded, exact, finite JSON number used by final progress notifications.
///
/// The original JSON-number lexeme is retained for wire re-encoding. Ordering
/// is computed from bounded decimal components, never through IEEE-754.
#[derive(Clone, Debug)]
pub struct ExactNonNegativeJsonNumber {
    raw: Box<RawValue>,
    negative: bool,
    significant_digits: String,
    decimal_point: i32,
}

impl ExactNonNegativeJsonNumber {
    /// Admits one bounded finite JSON number while retaining its spelling.
    pub fn try_from_number(number: serde_json::Number) -> Result<Self, CommonTypeError> {
        Self::parse(number.as_str())
    }

    /// Parses one exact progress number from its JSON-number lexeme.
    pub fn parse(lexeme: &str) -> Result<Self, CommonTypeError> {
        let raw = RawValue::from_string(lexeme.to_owned())
            .map_err(|_| CommonTypeError::Invalid("JSON progress number"))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: Box<RawValue>) -> Result<Self, CommonTypeError> {
        let lexeme = raw.get();
        if lexeme.len() > MAX_EXACT_PROGRESS_NUMBER_BYTES {
            return Err(CommonTypeError::TooLong("exact progress number"));
        }
        let (negative, unsigned_lexeme) = match lexeme.strip_prefix('-') {
            Some(unsigned_lexeme) => (true, unsigned_lexeme),
            None => (false, lexeme),
        };

        let (mantissa, exponent) =
            match unsigned_lexeme.find(|character| matches!(character, 'e' | 'E')) {
                Some(index) => (
                    &unsigned_lexeme[..index],
                    parse_bounded_progress_exponent(&unsigned_lexeme[index + 1..])?,
                ),
                None => (unsigned_lexeme, 0),
            };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(CommonTypeError::Invalid("JSON progress number"));
        }
        let digits = [whole, fraction].concat();
        let first_significant = digits
            .bytes()
            .position(|byte| byte != b'0')
            .unwrap_or(digits.len());
        let significant_digits = if first_significant == digits.len() {
            "0".to_owned()
        } else {
            digits[first_significant..].to_owned()
        };
        let decimal_point = i32::try_from(whole.len())
            .map_err(|_| CommonTypeError::TooLong("exact progress number"))?
            .checked_sub(
                i32::try_from(first_significant)
                    .map_err(|_| CommonTypeError::TooLong("exact progress number"))?,
            )
            .and_then(|point| point.checked_add(exponent))
            .ok_or(CommonTypeError::TooLong("exact progress number"))?;

        Ok(Self {
            raw,
            negative,
            significant_digits,
            decimal_point,
        })
    }

    /// Returns the exact JSON-number spelling retained from the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.raw.get()
    }

    fn is_zero(&self) -> bool {
        self.significant_digits == "0"
    }

    fn cmp_magnitude(&self, other: &Self) -> std::cmp::Ordering {
        match self.decimal_point.cmp(&other.decimal_point) {
            std::cmp::Ordering::Equal => {}
            order => return order,
        }
        for index in 0..self
            .significant_digits
            .len()
            .max(other.significant_digits.len())
        {
            match self
                .significant_digits
                .as_bytes()
                .get(index)
                .copied()
                .unwrap_or(b'0')
                .cmp(
                    &other
                        .significant_digits
                        .as_bytes()
                        .get(index)
                        .copied()
                        .unwrap_or(b'0'),
                ) {
                std::cmp::Ordering::Equal => {}
                order => return order,
            }
        }
        std::cmp::Ordering::Equal
    }
}

impl PartialEq for ExactNonNegativeJsonNumber {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for ExactNonNegativeJsonNumber {}

impl PartialOrd for ExactNonNegativeJsonNumber {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExactNonNegativeJsonNumber {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return std::cmp::Ordering::Equal,
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            (false, false) => {}
        }
        match (self.negative, other.negative) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (true, true) => other.cmp_magnitude(self),
            (false, false) => self.cmp_magnitude(other),
        }
    }
}

impl Serialize for ExactNonNegativeJsonNumber {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExactNonNegativeJsonNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ExactJsonNumberVisitor;

        impl<'de> serde::de::Visitor<'de> for ExactJsonNumberVisitor {
            type Value = ExactNonNegativeJsonNumber;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded finite JSON progress number")
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                ExactNonNegativeJsonNumber::parse(&value.to_string()).map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                ExactNonNegativeJsonNumber::parse(&value.to_string()).map_err(E::custom)
            }

            fn visit_i128<E: serde::de::Error>(self, value: i128) -> Result<Self::Value, E> {
                ExactNonNegativeJsonNumber::parse(&value.to_string()).map_err(E::custom)
            }

            fn visit_u128<E: serde::de::Error>(self, value: u128) -> Result<Self::Value, E> {
                ExactNonNegativeJsonNumber::parse(&value.to_string()).map_err(E::custom)
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .ok_or_else(|| E::custom("JSON progress number must be finite"))
                    .and_then(|number| {
                        ExactNonNegativeJsonNumber::try_from_number(number).map_err(E::custom)
                    })
            }

            fn visit_newtype_struct<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
            where
                D2: serde::Deserializer<'de>,
            {
                deserializer.deserialize_any(self)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let Some(key) = map.next_key::<std::borrow::Cow<'_, str>>()? else {
                    return Err(serde::de::Error::custom(
                        "JSON progress number cannot be an object",
                    ));
                };
                if key != SERDE_JSON_RAW_VALUE_TOKEN && key != SERDE_JSON_NUMBER_TOKEN {
                    return Err(serde::de::Error::custom(
                        "JSON progress number cannot be an object",
                    ));
                }
                let lexeme = map.next_value::<std::borrow::Cow<'_, str>>()?;
                ExactNonNegativeJsonNumber::parse(&lexeme).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_newtype_struct(SERDE_JSON_RAW_VALUE_TOKEN, ExactJsonNumberVisitor)
    }
}

fn parse_bounded_progress_exponent(value: &str) -> Result<i32, CommonTypeError> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CommonTypeError::Invalid("JSON progress number"));
    }
    let magnitude = digits.bytes().try_fold(0_i32, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(i32::from(byte - b'0')))
    });
    let Some(magnitude) = magnitude else {
        return Err(CommonTypeError::TooLong("progress number exponent"));
    };
    if magnitude > MAX_EXACT_PROGRESS_EXPONENT_ABS {
        return Err(CommonTypeError::TooLong("progress number exponent"));
    }
    Ok(if negative { -magnitude } else { magnitude })
}

fn validate_json_integer(value: &str) -> Result<(), CommonTypeError> {
    if value.len() > MAX_JSON_INTEGER_BYTES {
        return Err(CommonTypeError::TooLong("JSON integer"));
    }
    let (mantissa, exponent) = match value.find(|character| matches!(character, 'e' | 'E')) {
        Some(index) => (
            &value[..index],
            parse_bounded_json_integer_exponent(&value[index + 1..])?,
        ),
        None => (value, 0),
    };
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    if whole.is_empty()
        || !(whole == "0"
            || (whole.as_bytes()[0].is_ascii_digit()
                && whole.as_bytes()[0] != b'0'
                && whole.bytes().all(|byte| byte.is_ascii_digit())))
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(CommonTypeError::Invalid("JSON integer"));
    }
    let fraction = fraction.unwrap_or("");
    let digits = [whole, fraction].concat();
    if digits.bytes().all(|byte| byte == b'0') {
        return Ok(());
    }
    let scale = (fraction.len() as isize)
        .checked_sub(exponent)
        .ok_or(CommonTypeError::TooLong("JSON integer exponent"))?;
    if scale <= 0
        || digits
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'0')
            .count()
            >= usize::try_from(scale).unwrap_or(usize::MAX)
    {
        Ok(())
    } else {
        Err(CommonTypeError::Invalid("JSON integer"))
    }
}

fn json_integer_as_i32(value: &str) -> Option<i32> {
    let (mantissa, exponent) = match value.find(|character| matches!(character, 'e' | 'E')) {
        Some(index) => (
            &value[..index],
            parse_bounded_json_integer_exponent(&value[index + 1..]).ok()?,
        ),
        None => (value, 0),
    };
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(mantissa) => (true, mantissa),
        None => (false, mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole
        .bytes()
        .chain(fraction.bytes())
        .all(|digit| digit == b'0')
    {
        return Some(0);
    }
    let scale = (fraction.len() as isize).checked_sub(exponent)?;
    let source_length = whole.len().checked_add(fraction.len())?;
    let retained_source_length = if scale.is_positive() {
        source_length.saturating_sub(usize::try_from(scale).ok()?)
    } else {
        source_length
    };
    let appended_zeroes = if scale.is_negative() {
        scale.unsigned_abs()
    } else {
        0
    };
    let maximum = if negative {
        i64::from(i32::MAX) + 1
    } else {
        i64::from(i32::MAX)
    };

    let mut magnitude = 0_i64;
    let mut saw_nonzero = false;
    for digit in whole
        .bytes()
        .chain(fraction.bytes())
        .take(retained_source_length)
    {
        if !saw_nonzero && digit == b'0' {
            continue;
        }
        saw_nonzero = true;
        magnitude = magnitude
            .checked_mul(10)?
            .checked_add(i64::from(digit - b'0'))?;
        if magnitude > maximum {
            return None;
        }
    }

    if !saw_nonzero {
        return Some(0);
    }
    if appended_zeroes >= 10 {
        return None;
    }
    for _ in 0..appended_zeroes {
        magnitude = magnitude.checked_mul(10)?;
        if magnitude > maximum {
            return None;
        }
    }

    if negative {
        if magnitude == i64::from(i32::MAX) + 1 {
            Some(i32::MIN)
        } else {
            Some(-(magnitude as i32))
        }
    } else {
        Some(magnitude as i32)
    }
}

fn parse_bounded_json_integer_exponent(value: &str) -> Result<isize, CommonTypeError> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CommonTypeError::Invalid("JSON integer"));
    }
    let magnitude = digits.bytes().try_fold(0_i32, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(i32::from(byte - b'0')))
    });
    let Some(magnitude) = magnitude else {
        return Err(CommonTypeError::TooLong("JSON integer exponent"));
    };
    if magnitude > MAX_JSON_INTEGER_EXPONENT_ABS {
        return Err(CommonTypeError::TooLong("JSON integer exponent"));
    }
    let exponent = isize::try_from(magnitude)
        .map_err(|_| CommonTypeError::TooLong("JSON integer exponent"))?;
    Ok(if negative { -exponent } else { exponent })
}

/// Final implementation identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    /// Programmatic implementation name.
    pub name: String,
    /// Implementation version.
    pub version: String,
    /// Optional display title. An empty present title remains present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional, untrusted website identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website_url: Option<AbsoluteUri>,
    /// Optional wire-preserving icon set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<RawIcon>,
    /// Schema-allowed members retained without assigning them protocol meaning.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, Value>,
}

/// Final MCP logging severities, aligned to RFC 5424 names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoggingLevel {
    /// Debug diagnostic information.
    Debug,
    /// Informational event.
    Info,
    /// Significant normal event.
    Notice,
    /// Warning event.
    Warning,
    /// Error event.
    Error,
    /// Critical event.
    Critical,
    /// Alert event.
    Alert,
    /// Emergency event.
    Emergency,
}

impl Implementation {
    /// Constructs an implementation identity with required nonempty fields.
    pub fn try_new(
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, CommonTypeError> {
        let name = name.into();
        let version = version.into();
        if name.is_empty() || version.is_empty() {
            return Err(CommonTypeError::Invalid("implementation identity"));
        }
        Ok(Self {
            name,
            version,
            title: None,
            description: None,
            website_url: None,
            icons: Vec::new(),
            additional: BTreeMap::new(),
        })
    }

    /// Returns the effective display name without synthesizing a wire title.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImplementationWire {
    name: String,
    version: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    website_url: Option<AbsoluteUri>,
    #[serde(default)]
    icons: Vec<RawIcon>,
    #[serde(flatten, default)]
    additional: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for Implementation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        reject_explicit_null_fields(&value, &["title", "description", "websiteUrl", "icons"])
            .map_err(serde::de::Error::custom)?;
        let wire: ImplementationWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let mut implementation =
            Self::try_new(wire.name, wire.version).map_err(serde::de::Error::custom)?;
        implementation.title = wire.title;
        implementation.description = wire.description;
        implementation.website_url = wire.website_url;
        implementation.icons = wire.icons;
        implementation.additional = wire.additional;
        Ok(implementation)
    }
}

/// Open `_meta` values with typed access to final reserved keys.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct OpenMetadata(BTreeMap<String, Value>);

impl OpenMetadata {
    /// Validates every key and preserves valid unknown peer entries exactly.
    pub fn try_from_entries(
        entries: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Self, CommonTypeError> {
        let metadata = Self::try_from_open_entries(entries, valid_metadata_key)?;
        metadata.validate_reserved_values()?;
        Ok(metadata)
    }

    /// Validates notification-role metadata without assigning request/result
    /// semantics to otherwise schema-open reserved keys.
    ///
    /// The final schema gives only `subscriptionId` a typed meaning in
    /// notification metadata. Every other syntactically valid key/value is
    /// retained exactly and remains inert.
    pub fn try_from_notification_entries(
        entries: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Self, CommonTypeError> {
        let metadata = Self::try_from_open_entries(entries, valid_open_metadata_key)?;
        metadata.validate_notification_values()?;
        Ok(metadata)
    }

    fn try_from_open_entries(
        entries: impl IntoIterator<Item = (String, Value)>,
        valid_key: fn(&str) -> bool,
    ) -> Result<Self, CommonTypeError> {
        let mut values = BTreeMap::new();
        for (key, value) in entries {
            let value_bytes = serde_json::to_vec(&value)
                .map_err(|_| CommonTypeError::Invalid("metadata value"))?
                .len();
            if values.len() == MAX_METADATA_ENTRIES
                || key.len() > MAX_METADATA_KEY_BYTES
                || value_bytes > MAX_METADATA_VALUE_BYTES
                || !valid_key(&key)
                || values.insert(key, value).is_some()
            {
                return Err(CommonTypeError::Invalid("metadata key"));
            }
        }
        Ok(Self(values))
    }

    /// Returns the exact retained entry for a valid unknown key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Returns all retained entries without granting them authority.
    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, Value> {
        &self.0
    }

    /// Reads the typed protocol version, if this metadata role declares it.
    pub fn protocol_version(&self) -> Result<Option<&str>, CommonTypeError> {
        self.optional_string("io.modelcontextprotocol/protocolVersion")
    }

    /// Reads typed client capabilities, if present.
    pub fn client_capabilities(
        &self,
    ) -> Result<Option<&serde_json::Map<String, Value>>, CommonTypeError> {
        match self.0.get("io.modelcontextprotocol/clientCapabilities") {
            None => Ok(None),
            Some(Value::Object(value)) => Ok(Some(value)),
            Some(_) => Err(CommonTypeError::Invalid("client capabilities")),
        }
    }

    /// Decodes the self-reported client identity without treating it as authority.
    pub fn client_info(&self) -> Result<Option<Implementation>, CommonTypeError> {
        self.typed_implementation("io.modelcontextprotocol/clientInfo")
    }

    /// Decodes the self-reported server identity from final result metadata.
    ///
    /// Final result envelopes carry this only under
    /// `io.modelcontextprotocol/serverInfo` in `_meta`.
    pub fn server_info(&self) -> Result<Option<Implementation>, CommonTypeError> {
        self.typed_implementation("io.modelcontextprotocol/serverInfo")
    }

    /// Reads the exact optional logging-level metadata value.
    pub fn log_level(&self) -> Result<Option<LoggingLevel>, CommonTypeError> {
        self.0
            .get("io.modelcontextprotocol/logLevel")
            .map_or(Ok(None), |value| {
                serde_json::from_value(value.clone())
                    .map(Some)
                    .map_err(|_| CommonTypeError::Invalid("logging level metadata"))
            })
    }

    fn optional_string(&self, key: &str) -> Result<Option<&str>, CommonTypeError> {
        match self.0.get(key) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value)),
            Some(_) => Err(CommonTypeError::Invalid("metadata string")),
        }
    }

    fn typed_implementation(&self, key: &str) -> Result<Option<Implementation>, CommonTypeError> {
        self.0.get(key).map_or(Ok(None), |value| {
            serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|_| CommonTypeError::Invalid("implementation metadata"))
        })
    }

    fn validate_reserved_values(&self) -> Result<(), CommonTypeError> {
        if self.protocol_version()?.is_some() && self.client_capabilities()?.is_none() {
            return Err(CommonTypeError::Invalid("client capabilities"));
        }
        for key in [
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/serverInfo",
        ] {
            if self.0.contains_key(key) && self.typed_implementation(key)?.is_none() {
                return Err(CommonTypeError::Invalid("implementation metadata"));
            }
        }
        let _ = self.log_level()?;
        if let Some(value) = self.0.get("io.modelcontextprotocol/subscriptionId") {
            let valid = matches!(value, Value::String(_))
                || matches!(value, Value::Number(number) if JsonInteger::try_from_number(number.clone()).is_ok());
            if !valid {
                return Err(CommonTypeError::Invalid("subscription ID metadata"));
            }
        }
        Ok(())
    }

    fn validate_notification_values(&self) -> Result<(), CommonTypeError> {
        if let Some(value) = self.0.get("io.modelcontextprotocol/subscriptionId") {
            let valid = matches!(value, Value::String(_))
                || matches!(value, Value::Number(number) if JsonInteger::try_from_number(number.clone()).is_ok());
            if !valid {
                return Err(CommonTypeError::Invalid("subscription ID metadata"));
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for OpenMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        BTreeMap::<String, Value>::deserialize(deserializer)
            .and_then(|entries| Self::try_from_entries(entries).map_err(serde::de::Error::custom))
    }
}

fn valid_metadata_key(key: &str) -> bool {
    let Some((prefix, name)) = split_metadata_key(key) else {
        return false;
    };
    if prefix == Some("io.modelcontextprotocol")
        && !matches!(
            name,
            "protocolVersion"
                | "clientCapabilities"
                | "clientInfo"
                | "logLevel"
                | "serverInfo"
                | "subscriptionId"
        )
    {
        return false;
    }
    valid_metadata_name(name)
}

fn valid_open_metadata_key(key: &str) -> bool {
    split_metadata_key(key).is_some_and(|(_, name)| valid_metadata_name(name))
}

fn split_metadata_key(key: &str) -> Option<(Option<&str>, &str)> {
    let (prefix, name) = match key.split_once('/') {
        Some((prefix, name)) if !name.contains('/') => (Some(prefix), name),
        Some(_) => return None,
        None => (None, key),
    };
    if let Some(prefix) = prefix {
        if !valid_reverse_dns_prefix(prefix) {
            return None;
        }
    }
    Some((prefix, name))
}

fn valid_metadata_name(name: &str) -> bool {
    name.is_empty()
        || (name.as_bytes()[0].is_ascii_alphanumeric()
            && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
}

fn reject_bare_unknown_members(value: &Value, known: &[&str]) -> Result<(), CommonTypeError> {
    let object = value
        .as_object()
        .ok_or(CommonTypeError::Invalid("wire object"))?;
    for key in object.keys() {
        let qualified = matches!(split_metadata_key(key), Some((Some(_), _)));
        if !known.contains(&key.as_str()) && !qualified {
            return Err(CommonTypeError::Invalid("unrecognized bare wire member"));
        }
    }
    Ok(())
}

fn reject_explicit_null_fields(value: &Value, fields: &[&str]) -> Result<(), CommonTypeError> {
    let object = value
        .as_object()
        .ok_or(CommonTypeError::Invalid("wire object"))?;
    if fields
        .iter()
        .any(|field| object.get(*field).is_some_and(Value::is_null))
    {
        return Err(CommonTypeError::Invalid("optional non-null field"));
    }
    Ok(())
}

fn valid_reverse_dns_prefix(prefix: &str) -> bool {
    let labels = prefix.split('.').collect::<Vec<_>>();
    !labels.is_empty()
        && labels.into_iter().all(|label| {
            !label.is_empty()
                && label.as_bytes()[0].is_ascii_alphabetic()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Bounded trace-context fields preserved from open metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceContext {
    /// W3C trace parent value.
    pub traceparent: Option<String>,
    /// W3C trace state value.
    pub tracestate: Option<String>,
    /// W3C baggage value.
    pub baggage: Option<String>,
}

impl TraceContext {
    /// Extracts trace fields only when they are strings in valid metadata.
    pub fn try_from_metadata(metadata: &OpenMetadata) -> Result<Self, CommonTypeError> {
        let field = |name| {
            metadata.optional_string(name).and_then(|value| {
                if value.is_some_and(|field| {
                    field.len() > MAX_TRACE_FIELD_BYTES
                        || !field.is_ascii()
                        || field.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
                }) {
                    Err(CommonTypeError::TooLong("trace context"))
                } else {
                    Ok(value.map(ToOwned::to_owned))
                }
            })
        };
        let traceparent = field("traceparent")?;
        if traceparent
            .as_deref()
            .is_some_and(|value| !valid_traceparent(value))
        {
            return Err(CommonTypeError::Invalid("traceparent"));
        }
        Ok(Self {
            traceparent,
            tracestate: field("tracestate")?,
            baggage: field("baggage")?,
        })
    }
}

fn valid_traceparent(value: &str) -> bool {
    let mut fields = value.split('-');
    let (Some(version), Some(trace_id), Some(parent_id), Some(flags), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return false;
    };
    valid_lower_hex(version, 2)
        && version != "ff"
        && valid_lower_hex(trace_id, 32)
        && trace_id.bytes().any(|byte| byte != b'0')
        && valid_lower_hex(parent_id, 16)
        && parent_id.bytes().any(|byte| byte != b'0')
        && valid_lower_hex(flags, 2)
}

fn valid_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// A peer cancellation request identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationRequestId {
    /// String JSON-RPC identifier.
    String(String),
    /// Canonically spelled signed 64-bit JSON-RPC integer identifier.
    Integer(i64),
    /// Mathematical JSON-RPC integer whose original numeric lexeme must be retained.
    IntegerExact(JsonInteger),
}

impl Serialize for CancellationRequestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::String(value) => value.serialize(serializer),
            Self::Integer(value) => value.serialize(serializer),
            Self::IntegerExact(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CancellationRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::String(value) => Ok(Self::String(value)),
            Value::Number(value) => {
                let integer =
                    JsonInteger::try_from_number(value).map_err(serde::de::Error::custom)?;
                match integer.as_str().parse::<i64>() {
                    Ok(value) if integer.as_str() == value.to_string() => Ok(Self::Integer(value)),
                    _ => Ok(Self::IntegerExact(integer)),
                }
            }
            _ => Err(serde::de::Error::custom(
                "cancellation request ID must be a string or mathematical integer",
            )),
        }
    }
}

/// A peer-provided cancellation reason that deliberately has no raw string
/// accessor, formatter, or serializer.
#[derive(Clone, Eq, PartialEq)]
pub struct UntrustedCancellationReason(String);

/// Final `notifications/cancelled` payload.
#[derive(Clone, Eq, PartialEq)]
pub struct CancellationNotification {
    /// Required non-null request identifier.
    pub request_id: CancellationRequestId,
    reason: Option<UntrustedCancellationReason>,
}

impl CancellationNotification {
    /// Constructs an exact cancellation payload while retaining an untrusted
    /// reason without rendering or interpreting it.
    pub fn try_new(
        request_id: CancellationRequestId,
        reason: Option<String>,
    ) -> Result<Self, CommonTypeError> {
        let reason = reason.map(UntrustedCancellationReason);
        Ok(Self { request_id, reason })
    }

    /// Indicates whether a peer reason was present without rendering or exposing it.
    #[must_use]
    pub fn has_untrusted_reason(&self) -> bool {
        self.reason.is_some()
    }
}

/// Icon theme values admitted by the final schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconTheme {
    /// An icon designed for a light background.
    Light,
    /// An icon designed for a dark background.
    Dark,
}

/// A raw, structurally valid icon source. Rendering admission is deliberately separate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawIcon {
    /// Required schema URI source.
    pub src: RawIconSourceUri,
    /// Optional peer MIME declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Presence-aware size strings. Empty is distinct from absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<String>>,
    /// Optional theme without serialization defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<IconTheme>,
    /// Schema-allowed members retained without assigning them display semantics.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, Value>,
}

impl RawIcon {
    /// Constructs a raw icon with a required absolute source.
    pub fn try_new(src: impl Into<String>) -> Result<Self, CommonTypeError> {
        Ok(Self {
            src: RawIconSourceUri::parse(src)?,
            mime_type: None,
            sizes: None,
            theme: None,
            additional: BTreeMap::new(),
        })
    }

    /// Adds exact wire-preserving optional icon fields with bounded peer sizes.
    pub fn try_with_details(
        src: impl Into<String>,
        mime_type: Option<String>,
        sizes: Option<Vec<String>>,
        theme: Option<IconTheme>,
    ) -> Result<Self, CommonTypeError> {
        if sizes.as_ref().is_some_and(|values| {
            values.len() > MAX_ICON_SIZE_ENTRIES
                || values.iter().any(|value| value.len() > MAX_ICON_SIZE_BYTES)
        }) {
            return Err(CommonTypeError::TooLong("icon sizes"));
        }
        Ok(Self {
            src: RawIconSourceUri::parse(src)?,
            mime_type,
            sizes,
            theme,
            additional: BTreeMap::new(),
        })
    }

    /// Returns the documented effective size class without changing wire presence.
    #[must_use]
    pub fn effective_any_size(&self) -> bool {
        self.sizes.is_none()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIconWire {
    src: RawIconSourceUri,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    sizes: Option<Vec<String>>,
    #[serde(default)]
    theme: Option<IconTheme>,
    #[serde(flatten, default)]
    additional: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for RawIcon {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        reject_explicit_null_fields(&value, &["mimeType", "sizes", "theme"])
            .map_err(serde::de::Error::custom)?;
        let wire: RawIconWire = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let mut icon =
            Self::try_with_details(wire.src.as_str(), wire.mime_type, wire.sizes, wire.theme)
                .map_err(serde::de::Error::custom)?;
        icon.additional = wire.additional;
        Ok(icon)
    }
}

/// Annotation audience values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationAudience {
    /// Intended for a user.
    User,
    /// Intended for an assistant.
    Assistant,
}

/// Optional content annotations.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotations {
    /// Intended audience roles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<AnnotationAudience>>,
    /// Finite inclusive priority hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    /// Peer timestamp string preserved without inventing a schema rejection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// Schema-allowed members retained without assigning them annotation semantics.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, Value>,
}

impl Annotations {
    /// Validates the only schema-constrained numeric field.
    pub fn try_with_priority(priority: f64) -> Result<Self, CommonTypeError> {
        if !priority.is_finite() || !(0.0..=1.0).contains(&priority) {
            return Err(CommonTypeError::Invalid("annotation priority"));
        }
        Ok(Self {
            priority: Some(priority),
            ..Self::default()
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnnotationsWire {
    #[serde(default)]
    audience: Option<Vec<AnnotationAudience>>,
    #[serde(default)]
    priority: Option<f64>,
    #[serde(default)]
    last_modified: Option<String>,
    #[serde(flatten, default)]
    additional: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for Annotations {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        reject_explicit_null_fields(&value, &["audience", "priority", "lastModified"])
            .map_err(serde::de::Error::custom)?;
        let wire: AnnotationsWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        if let Some(priority) = wire.priority {
            Self::try_with_priority(priority).map_err(serde::de::Error::custom)?;
        }
        Ok(Self {
            audience: wire.audience,
            priority: wire.priority,
            last_modified: wire.last_modified,
            additional: wire.additional,
        })
    }
}

/// A final `resource_link` content block.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceLink {
    /// Optional sized icons for display.
    pub icons: Option<Vec<RawIcon>>,
    /// Required programmatic resource name.
    pub name: String,
    /// Optional user-facing resource title.
    pub title: Option<String>,
    /// Exact resource identity.
    pub uri: AbsoluteUri,
    /// Optional description of the resource.
    pub description: Option<String>,
    /// Optional MIME type declared for the resource.
    pub mime_type: Option<String>,
    /// Optional link annotations.
    pub annotations: Option<Annotations>,
    /// Optional raw size of the resource in bytes.
    pub size: Option<JsonInteger>,
    /// Preserved open metadata.
    pub meta: Option<OpenMetadata>,
    /// Schema-allowed members retained without assigning them protocol meaning.
    pub additional: BTreeMap<String, Value>,
}

impl Serialize for ResourceLink {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let field_count = 3
            + usize::from(self.icons.is_some())
            + usize::from(self.title.is_some())
            + usize::from(self.description.is_some())
            + usize::from(self.mime_type.is_some())
            + usize::from(self.annotations.is_some())
            + usize::from(self.size.is_some())
            + usize::from(self.meta.is_some())
            + self.additional.len();
        let mut state = serializer.serialize_map(Some(field_count))?;
        state.serialize_entry("type", "resource_link")?;
        if let Some(icons) = &self.icons {
            state.serialize_entry("icons", icons)?;
        }
        state.serialize_entry("name", &self.name)?;
        if let Some(title) = &self.title {
            state.serialize_entry("title", title)?;
        }
        state.serialize_entry("uri", &self.uri)?;
        if let Some(description) = &self.description {
            state.serialize_entry("description", description)?;
        }
        if let Some(mime_type) = &self.mime_type {
            state.serialize_entry("mimeType", mime_type)?;
        }
        if let Some(annotations) = &self.annotations {
            state.serialize_entry("annotations", annotations)?;
        }
        if let Some(size) = &self.size {
            state.serialize_entry("size", size)?;
        }
        if let Some(meta) = &self.meta {
            state.serialize_entry("_meta", meta)?;
        }
        for (name, value) in &self.additional {
            state.serialize_entry(name, value)?;
        }
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceLinkWire {
    #[serde(rename = "type")]
    kind: ResourceLinkKind,
    #[serde(default)]
    icons: Option<Vec<RawIcon>>,
    name: String,
    #[serde(default)]
    title: Option<String>,
    uri: AbsoluteUri,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(default)]
    annotations: Option<Annotations>,
    #[serde(default)]
    size: Option<JsonInteger>,
    #[serde(rename = "_meta", default)]
    meta: Option<OpenMetadata>,
    #[serde(flatten, default)]
    additional: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
enum ResourceLinkKind {
    #[serde(rename = "resource_link")]
    ResourceLink,
}

impl<'de> Deserialize<'de> for ResourceLink {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        reject_explicit_null_fields(
            &value,
            &[
                "icons",
                "title",
                "description",
                "mimeType",
                "annotations",
                "size",
                "_meta",
            ],
        )
        .map_err(serde::de::Error::custom)?;
        let wire: ResourceLinkWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let ResourceLinkKind::ResourceLink = wire.kind;
        Ok(Self {
            icons: wire.icons,
            name: wire.name,
            title: wire.title,
            uri: wire.uri,
            description: wire.description,
            mime_type: wire.mime_type,
            annotations: wire.annotations,
            size: wire.size,
            meta: wire.meta,
            additional: wire.additional,
        })
    }
}

/// Text or blob resource contents embedded in content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub enum EmbeddedResourceContents {
    /// Text resource contents.
    Text {
        uri: AbsoluteUri,
        text: String,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
    /// Blob resource contents.
    Blob {
        uri: AbsoluteUri,
        blob: String,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
enum EmbeddedResourceContentsWire {
    Text {
        uri: AbsoluteUri,
        text: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
    Blob {
        uri: AbsoluteUri,
        blob: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
}

impl<'de> Deserialize<'de> for EmbeddedResourceContents {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("embedded resource must be an object"))?;
        let has_text = object.contains_key("text");
        let has_blob = object.contains_key("blob");
        if has_text == has_blob {
            return Err(serde::de::Error::custom(
                "embedded resource requires exactly one of text or blob",
            ));
        }
        // Schema-allowed additional properties are namespaced extension
        // members; bare strangers (for example a snake_case mime_type
        // shadowing the canonical mimeType) must reject.
        reject_bare_unknown_members(&value, &["uri", "text", "blob", "mimeType", "_meta"])
            .map_err(serde::de::Error::custom)?;
        reject_explicit_null_fields(&value, &["mimeType", "_meta"])
            .map_err(serde::de::Error::custom)?;
        let wire: EmbeddedResourceContentsWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let resource = match wire {
            EmbeddedResourceContentsWire::Text {
                uri,
                text,
                mime_type,
                meta,
                additional,
            } => Self::Text {
                uri,
                text,
                mime_type,
                meta,
                additional,
            },
            EmbeddedResourceContentsWire::Blob {
                uri,
                blob,
                mime_type,
                meta,
                additional,
            } => Self::Blob {
                uri,
                blob,
                mime_type,
                meta,
                additional,
            },
        };
        validate_embedded_resource(&resource).map_err(serde::de::Error::custom)?;
        Ok(resource)
    }
}

/// Final common content discriminators.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Text content.
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
    /// Binary image content.
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
    /// Binary audio content.
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
    /// A resource link uses the exact `resource_link` discriminator.
    ResourceLink {
        #[serde(skip_serializing_if = "Option::is_none")]
        icons: Option<Vec<RawIcon>>,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        uri: AbsoluteUri,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<JsonInteger>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
    /// An embedded resource uses the exact `resource` discriminator.
    Resource {
        resource: EmbeddedResourceContents,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten)]
        additional: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockWire {
    Text {
        text: String,
        #[serde(default)]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
    ResourceLink {
        #[serde(default)]
        icons: Option<Vec<RawIcon>>,
        name: String,
        #[serde(default)]
        title: Option<String>,
        uri: AbsoluteUri,
        #[serde(default)]
        description: Option<String>,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
        #[serde(default)]
        annotations: Option<Annotations>,
        #[serde(default)]
        size: Option<JsonInteger>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
    Resource {
        resource: EmbeddedResourceContents,
        #[serde(default)]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default)]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default)]
        additional: BTreeMap<String, Value>,
    },
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("missing content discriminator"))?;
        if !matches!(
            kind,
            "text" | "image" | "audio" | "resource_link" | "resource"
        ) {
            return Err(serde::de::Error::custom("content discriminator"));
        }
        // Schema-allowed additional properties are namespaced extension
        // members; a bare unknown member is a shadow/squatting risk and must
        // reject without consuming the block.
        let known_members: &[&str] = match kind {
            "text" => &["type", "text", "annotations", "_meta"],
            "image" | "audio" => &["type", "data", "mimeType", "annotations", "_meta"],
            "resource_link" => &[
                "type",
                "icons",
                "name",
                "title",
                "uri",
                "description",
                "mimeType",
                "annotations",
                "size",
                "_meta",
            ],
            _ => &["type", "resource", "annotations", "_meta"],
        };
        reject_bare_unknown_members(&value, known_members).map_err(serde::de::Error::custom)?;
        let optional_non_null_fields = match kind {
            "resource_link" => &[
                "icons",
                "title",
                "description",
                "mimeType",
                "annotations",
                "size",
                "_meta",
            ][..],
            _ => &["annotations", "_meta"][..],
        };
        reject_explicit_null_fields(&value, optional_non_null_fields)
            .map_err(serde::de::Error::custom)?;
        if kind == "resource" {
            let resource = value
                .get("resource")
                .ok_or_else(|| serde::de::Error::custom("missing embedded resource"))?;
            let _ = serde_json::from_value::<EmbeddedResourceContents>(resource.clone())
                .map_err(serde::de::Error::custom)?;
        }
        let wire: ContentBlockWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let content = match wire {
            ContentBlockWire::Text {
                text,
                annotations,
                meta,
                additional,
            } => Self::Text {
                text,
                annotations,
                meta,
                additional,
            },
            ContentBlockWire::Image {
                data,
                mime_type,
                annotations,
                meta,
                additional,
            } => {
                valid_binary_content(&data, &mime_type, "image/")
                    .map_err(serde::de::Error::custom)?;
                Self::Image {
                    data,
                    mime_type,
                    annotations,
                    meta,
                    additional,
                }
            }
            ContentBlockWire::Audio {
                data,
                mime_type,
                annotations,
                meta,
                additional,
            } => {
                valid_binary_content(&data, &mime_type, "audio/")
                    .map_err(serde::de::Error::custom)?;
                Self::Audio {
                    data,
                    mime_type,
                    annotations,
                    meta,
                    additional,
                }
            }
            ContentBlockWire::ResourceLink {
                icons,
                name,
                title,
                uri,
                description,
                mime_type,
                annotations,
                size,
                meta,
                additional,
            } => Self::ResourceLink {
                icons,
                name,
                title,
                uri,
                description,
                mime_type,
                annotations,
                size,
                meta,
                additional,
            },
            ContentBlockWire::Resource {
                resource,
                annotations,
                meta,
                additional,
            } => {
                validate_embedded_resource(&resource).map_err(serde::de::Error::custom)?;
                Self::Resource {
                    resource,
                    annotations,
                    meta,
                    additional,
                }
            }
        };
        FinalCommonTypesSchema::validate_content(&content).map_err(serde::de::Error::custom)?;
        Ok(content)
    }
}

impl ContentBlock {
    /// Constructs text content.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            annotations: None,
            meta: None,
            additional: BTreeMap::new(),
        }
    }

    /// Constructs image content after validating base64 and image MIME shape.
    pub fn image(
        data: impl Into<String>,
        mime_type: impl Into<String>,
    ) -> Result<Self, CommonTypeError> {
        let data = data.into();
        let mime_type = mime_type.into();
        valid_binary_content(&data, &mime_type, "image/")?;
        Ok(Self::Image {
            data,
            mime_type,
            annotations: None,
            meta: None,
            additional: BTreeMap::new(),
        })
    }

    /// Constructs audio content after validating base64 and audio MIME shape.
    pub fn audio(
        data: impl Into<String>,
        mime_type: impl Into<String>,
    ) -> Result<Self, CommonTypeError> {
        let data = data.into();
        let mime_type = mime_type.into();
        valid_binary_content(&data, &mime_type, "audio/")?;
        Ok(Self::Audio {
            data,
            mime_type,
            annotations: None,
            meta: None,
            additional: BTreeMap::new(),
        })
    }

    /// Constructs a resource-link content block.
    pub fn resource_link(
        uri: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, CommonTypeError> {
        Ok(Self::ResourceLink {
            icons: None,
            name: name.into(),
            title: None,
            uri: AbsoluteUri::parse(uri)?,
            description: None,
            mime_type: None,
            annotations: None,
            size: None,
            meta: None,
            additional: BTreeMap::new(),
        })
    }

    /// Constructs embedded text resource content.
    pub fn resource(
        uri: impl Into<String>,
        text: impl Into<String>,
        mime_type: Option<String>,
    ) -> Result<Self, CommonTypeError> {
        Ok(Self::Resource {
            resource: EmbeddedResourceContents::Text {
                uri: AbsoluteUri::parse(uri)?,
                text: text.into(),
                mime_type,
                meta: None,
                additional: BTreeMap::new(),
            },
            annotations: None,
            meta: None,
            additional: BTreeMap::new(),
        })
    }
}

/// Final sampling-only content blocks.
///
/// Tool use and tool result are intentionally absent from [`ContentBlock`]:
/// they are legal only in the final sampling message/result union. Tool result
/// bodies, in turn, use the general content union and therefore cannot nest
/// further tool-use/result blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SamplingContentBlock {
    /// Text sampling content.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, Value>,
    },
    /// Image sampling content.
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, Value>,
    },
    /// Audio sampling content.
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, Value>,
    },
    /// A requested assistant tool call.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Map<String, Value>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, Value>,
    },
    /// A result for a preceding tool call.
    ToolResult {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        content: Vec<ContentBlock>,
        /// Presence remains distinct from the protocol default of false.
        #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(
            rename = "structuredContent",
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_present_json_value"
        )]
        structured_content: Option<Value>,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        meta: Option<OpenMetadata>,
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, Value>,
    },
}

fn deserialize_present_json_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn valid_binary_content(
    data: &str,
    mime_type: &str,
    required_prefix: &str,
) -> Result<(), CommonTypeError> {
    if data.len() > MAX_CONTENT_ENCODED_BYTES {
        return Err(CommonTypeError::TooLong("binary content"));
    }
    if !mime_type.starts_with(required_prefix)
        || mime_type.len() == required_prefix.len()
        || !valid_mime_type(mime_type)
    {
        return Err(CommonTypeError::Invalid("binary MIME type"));
    }
    validate_standard_base64(data)
}

fn validate_embedded_resource(resource: &EmbeddedResourceContents) -> Result<(), CommonTypeError> {
    match resource {
        EmbeddedResourceContents::Text { mime_type, .. } => {
            if mime_type
                .as_deref()
                .is_some_and(|value| !valid_mime_type(value))
            {
                return Err(CommonTypeError::Invalid("resource MIME type"));
            }
        }
        EmbeddedResourceContents::Blob {
            blob, mime_type, ..
        } => {
            if blob.len() > MAX_CONTENT_ENCODED_BYTES {
                return Err(CommonTypeError::TooLong("binary content"));
            }
            validate_standard_base64(blob)?;
            if mime_type
                .as_deref()
                .is_some_and(|value| !valid_mime_type(value))
            {
                return Err(CommonTypeError::Invalid("resource MIME type"));
            }
        }
    }
    Ok(())
}

fn validate_standard_base64(value: &str) -> Result<(), CommonTypeError> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
        .map(|_| ())
        .map_err(|_| CommonTypeError::Invalid("base64 content"))
}

fn valid_mime_type(value: &str) -> bool {
    let mut segments = value.split(';');
    let Some(media_type) = segments.next() else {
        return false;
    };
    let Some((kind, subtype)) = media_type.split_once('/') else {
        return false;
    };
    let valid_media_type = !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && kind.bytes().all(is_mime_token)
        && subtype.bytes().all(is_mime_token);
    valid_media_type
        && segments.all(|parameter| {
            let Some((name, value)) = parameter.split_once('=') else {
                return false;
            };
            let name = name.trim();
            let value = value.trim();
            !name.is_empty()
                && !value.is_empty()
                && name.bytes().all(is_mime_token)
                && value.bytes().all(is_mime_token)
        })
}

fn is_mime_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'-' | b'.' | b'+'
        )
}

/// Direction of a final common-type wire envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommonWireDirection {
    /// A client or server request.
    Request,
    /// A notification, which has no JSON-RPC response.
    Notification,
    /// A request result.
    Result,
}

/// Structural schema admission for the final common wire slice.
///
/// It validates exact spellings and direction-sensitive metadata without converting peer input
/// into a locally authorized resource, icon, or cancellation action.
#[derive(Clone, Copy, Debug, Default)]
pub struct FinalCommonTypesSchema;

impl FinalCommonTypesSchema {
    /// The exact final-schema URI owners; `Icon.src` has a separately typed raw icon source.
    pub const FINAL_URI_OWNERS: [&'static str; 12] = [
        "BlobResourceContents.uri",
        "ElicitRequestURLParams.url",
        "Icon.src",
        "Implementation.websiteUrl",
        "ReadResourceRequestParams.uri",
        "Resource.uri",
        "ResourceContents.uri",
        "ResourceLink.uri",
        "ResourceRequestParams.uri",
        "ResourceUpdatedNotificationParams.uri",
        "Root.uri",
        "TextResourceContents.uri",
    ];

    /// Validates a final common wire object for its declared direction.
    pub fn validate(direction: CommonWireDirection, wire: &Value) -> Result<(), CommonTypeError> {
        let object = wire
            .as_object()
            .ok_or(CommonTypeError::Invalid("common wire object"))?;
        match (direction, object.get("_meta")) {
            (CommonWireDirection::Request, Some(meta)) => Self::validate_request_metadata(meta)?,
            (CommonWireDirection::Request, None) => {
                return Err(CommonTypeError::Invalid("request metadata"));
            }
            (_, Some(meta)) => {
                let metadata = Self::validate_open_metadata(meta)?;
                let _ = TraceContext::try_from_metadata(&metadata)?;
            }
            (_, None) => {}
        }
        if let Some(kind) = object.get("type") {
            let kind = kind
                .as_str()
                .ok_or(CommonTypeError::Invalid("content discriminator"))?;
            if !matches!(
                kind,
                "text" | "image" | "audio" | "resource_link" | "resource"
            ) {
                return Err(CommonTypeError::Invalid("content discriminator"));
            }
            let content: ContentBlock = serde_json::from_value(wire.clone())
                .map_err(|_| CommonTypeError::Invalid("content block"))?;
            Self::validate_content(&content)?;
        }
        if object.contains_key("src") {
            let _ = Self::validate_icon(wire)?;
        }
        if object.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            if direction != CommonWireDirection::Notification {
                return Err(CommonTypeError::Invalid("cancellation direction"));
            }
            Self::validate_cancellation_params(
                object
                    .get("params")
                    .ok_or(CommonTypeError::Invalid("cancellation params"))?,
            )?;
        }
        Ok(())
    }

    /// Validates the raw icon wire shape while preserving absent versus present-empty sizes.
    pub fn validate_icon(wire: &Value) -> Result<RawIcon, CommonTypeError> {
        let object = wire
            .as_object()
            .ok_or(CommonTypeError::Invalid("icon object"))?;
        for field in ["mimeType", "sizes", "theme"] {
            if object.get(field).is_some_and(Value::is_null) {
                return Err(CommonTypeError::Invalid("icon optional field"));
            }
        }
        let icon: RawIcon =
            serde_json::from_value(wire.clone()).map_err(|_| CommonTypeError::Invalid("icon"))?;
        RawIcon::try_with_details(
            icon.src.as_str(),
            icon.mime_type.clone(),
            icon.sizes.clone(),
            icon.theme,
        )?;
        Ok(icon)
    }

    /// Produces the deterministic JSON form used by frozen golden-wire records.
    pub fn canonical_json(wire: &Value) -> Result<String, CommonTypeError> {
        serde_json::to_string(wire).map_err(|_| CommonTypeError::Invalid("canonical JSON"))
    }

    /// Validates a wire object and requires its canonical JSON to equal the frozen golden.
    pub fn validate_golden(
        direction: CommonWireDirection,
        wire: &Value,
        golden: &str,
    ) -> Result<(), CommonTypeError> {
        Self::validate(direction, wire)?;
        if Self::canonical_json(wire)? != golden {
            return Err(CommonTypeError::Invalid("golden wire"));
        }
        Ok(())
    }

    fn validate_open_metadata(meta: &Value) -> Result<OpenMetadata, CommonTypeError> {
        let entries = meta
            .as_object()
            .ok_or(CommonTypeError::Invalid("metadata object"))?
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()));
        OpenMetadata::try_from_entries(entries)
    }

    fn validate_request_metadata(meta: &Value) -> Result<(), CommonTypeError> {
        let metadata = Self::validate_open_metadata(meta)?;
        if metadata.protocol_version()?.is_none() || metadata.client_capabilities()?.is_none() {
            return Err(CommonTypeError::Invalid("required request metadata"));
        }
        let _ = metadata.client_info()?;
        let _ = TraceContext::try_from_metadata(&metadata)?;
        Ok(())
    }

    fn validate_content(content: &ContentBlock) -> Result<(), CommonTypeError> {
        match content {
            ContentBlock::Image {
                data,
                mime_type,
                annotations,
                ..
            } => {
                Self::validate_annotations(annotations)?;
                valid_binary_content(data, mime_type, "image/")
            }
            ContentBlock::Audio {
                data,
                mime_type,
                annotations,
                ..
            } => {
                Self::validate_annotations(annotations)?;
                valid_binary_content(data, mime_type, "audio/")
            }
            ContentBlock::ResourceLink {
                uri,
                icons,
                annotations,
                ..
            } => {
                Self::validate_annotations(annotations)?;
                Self::validate_icons(icons)?;
                AbsoluteUri::parse(uri.as_str()).map(|_| ())
            }
            ContentBlock::Resource {
                resource,
                annotations,
                ..
            } => {
                Self::validate_annotations(annotations)?;
                validate_embedded_resource(resource)?;
                match resource {
                    EmbeddedResourceContents::Text { uri, .. }
                    | EmbeddedResourceContents::Blob { uri, .. } => {
                        AbsoluteUri::parse(uri.as_str()).map(|_| ())
                    }
                }
            }
            ContentBlock::Text { annotations, .. } => Self::validate_annotations(annotations),
        }
    }

    fn validate_annotations(annotations: &Option<Annotations>) -> Result<(), CommonTypeError> {
        if annotations
            .as_ref()
            .and_then(|value| value.priority)
            .is_some_and(|priority| !priority.is_finite() || !(0.0..=1.0).contains(&priority))
        {
            return Err(CommonTypeError::Invalid("annotation priority"));
        }
        Ok(())
    }

    fn validate_icons(icons: &Option<Vec<RawIcon>>) -> Result<(), CommonTypeError> {
        if let Some(icons) = icons {
            for icon in icons {
                let _ = RawIcon::try_with_details(
                    icon.src.as_str(),
                    icon.mime_type.clone(),
                    icon.sizes.clone(),
                    icon.theme,
                )?;
            }
        }
        Ok(())
    }

    fn validate_cancellation_params(params: &Value) -> Result<(), CommonTypeError> {
        let params = params
            .as_object()
            .ok_or(CommonTypeError::Invalid("cancellation params"))?;
        let request_id = params
            .get("requestId")
            .ok_or(CommonTypeError::Invalid("cancellation request ID"))?;
        let request_id = serde_json::from_value::<CancellationRequestId>(request_id.clone())
            .map_err(|_| CommonTypeError::Invalid("cancellation request ID"))?;
        let reason = match params.get("reason") {
            None => None,
            Some(Value::String(value)) => Some(value.clone()),
            Some(_) => return Err(CommonTypeError::Invalid("cancellation reason")),
        };
        match params.get("_meta") {
            None => {}
            Some(Value::Object(entries)) => {
                let _ = OpenMetadata::try_from_notification_entries(
                    entries.clone().into_iter().collect::<BTreeMap<_, _>>(),
                )?;
            }
            Some(_) => return Err(CommonTypeError::Invalid("cancellation metadata")),
        }
        let _ = CancellationNotification::try_new(request_id, reason)?;
        Ok(())
    }
}

/// Maximum bytes accepted when digesting a PRT-02 evaluator manifest.
pub const MAX_PRT_02_MANIFEST_BYTES: usize = 4 * 1024;

/// The canonical `PRT02-A-METADATA-v1` rows: ordered subcases `PRT-A-01.01`
/// through `PRT-A-01.08`.
///
/// This is an executable acceptance input, not a hash of this source file. It
/// is LF-canonical and LF-terminated, carries no CR, no blank line and no
/// trailing whitespace, and its header rows bind the producing Bead, the
/// revision the rows were authored against, and the shipped public entrypoints
/// this slice is proved through. Each subcase row is exactly
/// `<id> <name> floor=<N>`, where `floor` is the minimum number of
/// observations an integrating evaluator must actually perform for that
/// subcase. The floors are this producer's own definitions and are never a
/// count borrowed back from a consumer: a gate whose reference values come
/// from the thing being gated confirms arithmetic rather than agreement.
///
/// `producer-revision` is a provenance record of where these rows were
/// authored. Nothing asserts it equals HEAD, and it must never be rewritten to
/// make a gate pass.
pub const PRT_02_A_METADATA_MANIFEST_V1: &str = concat!(
    "PRT02-A-METADATA-v1\n",
    "producer-bead bd-mcp-prt-02-a-zaok\n",
    "producer-revision 256f35155cbe8173ae759a43398774b7f4e7fd6a\n",
    "entrypoint fastmcp_protocol::common_types::OpenMetadata::try_from_entries\n",
    "entrypoint fastmcp_protocol::common_types::Implementation::try_new\n",
    "entrypoint fastmcp_protocol::common_types::TraceContext::try_from_metadata\n",
    "PRT-A-01.01 known-final-key floor=1\n",
    "PRT-A-01.02 reserved-mcp-key floor=1\n",
    "PRT-A-01.03 empty-key floor=1\n",
    "PRT-A-01.04 unprefixed-extension-key floor=1\n",
    "PRT-A-01.05 valid-reverse-dns-extension-key floor=1\n",
    "PRT-A-01.06 malformed-reverse-dns-key floor=1\n",
    "PRT-A-01.07 client-info floor=1\n",
    "PRT-A-01.08 trace-context floor=1\n",
);

/// The canonical `PRT02-A-URI-CURSOR-CANCEL-v1` rows: ordered subcases
/// `PRT-A-02.01` through `PRT-A-02.16`.
///
/// Same contract as [`PRT_02_A_METADATA_MANIFEST_V1`]. The cursor subcases are
/// deliberately three, not two: absent, present-and-valid, and explicit null
/// are three distinct wire states, and a matrix that omits the null row would
/// look complete while proving nothing about null.
pub const PRT_02_A_URI_CURSOR_CANCEL_MANIFEST_V1: &str = concat!(
    "PRT02-A-URI-CURSOR-CANCEL-v1\n",
    "producer-bead bd-mcp-prt-02-a-zaok\n",
    "producer-revision 256f35155cbe8173ae759a43398774b7f4e7fd6a\n",
    "entrypoint fastmcp_protocol::common_types::AbsoluteUri::parse\n",
    "entrypoint fastmcp_protocol::common_types::OpaqueCursor::from_presence\n",
    "entrypoint fastmcp_protocol::common_types::CancellationNotification::try_new\n",
    "PRT-A-02.01 uri-urn-opaque floor=1\n",
    "PRT-A-02.02 uri-custom-scheme floor=1\n",
    "PRT-A-02.03 uri-authority-userinfo-port floor=1\n",
    "PRT-A-02.04 uri-ipv6-literal floor=1\n",
    "PRT-A-02.05 uri-ipvfuture-literal floor=1\n",
    "PRT-A-02.06 uri-percent-encoded-path floor=1\n",
    "PRT-A-02.07 uri-query-and-fragment-preserved floor=1\n",
    "PRT-A-02.08 uri-scheme-case-preserved floor=1\n",
    "PRT-A-02.09 uri-https-simple-path floor=1\n",
    "PRT-A-02.10 uri-byte-preserving-reencode floor=1\n",
    "PRT-A-02.11 uri-relative-refused floor=1\n",
    "PRT-A-02.12 uri-bound-enforced floor=1\n",
    "PRT-A-02.13 subscription-id-non-uri-exception floor=1\n",
    "PRT-A-02.14 cursor-absent floor=1\n",
    "PRT-A-02.15 cursor-present-including-empty floor=2\n",
    "PRT-A-02.16 cursor-explicit-null-refused floor=1\n",
);

/// The canonical `PRT02-A-ICON-CONTENT-v1` rows: ordered subcases
/// `PRT-A-03.01` through `PRT-A-03.10`.
///
/// Same contract as [`PRT_02_A_METADATA_MANIFEST_V1`].
pub const PRT_02_A_ICON_CONTENT_MANIFEST_V1: &str = concat!(
    "PRT02-A-ICON-CONTENT-v1\n",
    "producer-bead bd-mcp-prt-02-a-zaok\n",
    "producer-revision 256f35155cbe8173ae759a43398774b7f4e7fd6a\n",
    "entrypoint fastmcp_protocol::common_types::RawIcon::try_new\n",
    "entrypoint fastmcp_protocol::common_types::ContentBlock::text\n",
    "PRT-A-03.01 icon-source-absolute-uri floor=1\n",
    "PRT-A-03.02 icon-data-uri-base64-image floor=1\n",
    "PRT-A-03.03 icon-sizes floor=1\n",
    "PRT-A-03.04 icon-theme floor=1\n",
    "PRT-A-03.05 content-text floor=1\n",
    "PRT-A-03.06 content-image-mime floor=1\n",
    "PRT-A-03.07 content-audio-mime floor=1\n",
    "PRT-A-03.08 content-resource-link floor=1\n",
    "PRT-A-03.09 content-embedded-resource floor=1\n",
    "PRT-A-03.10 content-annotations-and-meta floor=1\n",
);

/// Returns the canonical `PRT02-A-METADATA-v1` digest.
///
/// The digest binds the exact published bytes of the manifest. An integrating
/// consumer recomputes it over those same bytes, so a digest that no longer
/// reproduces means the published rows and the digest have drifted apart.
#[must_use]
pub fn prt_02_a_metadata_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_A_METADATA_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 A metadata manifest is within its exact byte bound")
}

/// Returns the canonical `PRT02-A-URI-CURSOR-CANCEL-v1` digest.
#[must_use]
pub fn prt_02_a_uri_cursor_cancel_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_A_URI_CURSOR_CANCEL_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 A URI/cursor/cancel manifest is within its exact byte bound")
}

/// Returns the canonical `PRT02-A-ICON-CONTENT-v1` digest.
#[must_use]
pub fn prt_02_a_icon_content_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_A_ICON_CONTENT_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 A icon/content manifest is within its exact byte bound")
}

/// The canonical `PRT02-B-SERDE-SCHEMA-v1` rows: ordered subcases
/// `PRT-B-01.01` through `PRT-B-01.15`.
///
/// Same contract as the A manifests. The three wire-cursor rows are the
/// serde-level counterpart of `PRT-A-02.14`..`.16`: `OpaqueCursor` models
/// presence correctly on its own, but the property that matters on the wire is
/// whether an enclosing typed struct preserves the distinction, which only a
/// serde row can observe.
pub const PRT_02_B_SERDE_SCHEMA_MANIFEST_V1: &str = concat!(
    "PRT02-B-SERDE-SCHEMA-v1\n",
    "producer-bead bd-mcp-prt-02-b-nnva\n",
    "producer-revision 0733ee3e4b7b04a1ad0a33d78a8e1b0f6e6f6d2f\n",
    "entrypoint serde_json::to_value\n",
    "entrypoint serde_json::from_value\n",
    "entrypoint fastmcp_protocol::common_types::FinalCommonTypesSchema::validate\n",
    "PRT-B-01.01 implementation-round-trip floor=1\n",
    "PRT-B-01.02 open-metadata-round-trip floor=1\n",
    "PRT-B-01.03 annotations-round-trip floor=1\n",
    "PRT-B-01.04 absolute-uri-round-trip floor=1\n",
    "PRT-B-01.05 cursor-value-round-trip floor=1\n",
    "PRT-B-01.06 cancellation-request-id-round-trip floor=1\n",
    "PRT-B-01.07 icon-schema-verdict floor=1\n",
    "PRT-B-01.08 content-text-round-trip floor=1\n",
    "PRT-B-01.09 content-image-round-trip floor=1\n",
    "PRT-B-01.10 embedded-resource-round-trip floor=1\n",
    "PRT-B-01.11 trace-context-round-trip floor=1\n",
    "PRT-B-01.12 subscription-exception-round-trip floor=1\n",
    "PRT-B-01.13 wire-cursor-absent floor=1\n",
    "PRT-B-01.14 wire-cursor-present floor=2\n",
    "PRT-B-01.15 wire-cursor-explicit-null-refused floor=1\n",
);

/// The canonical `PRT02-B-BOUNDS-DIRECTION-v1` rows: ordered subcases
/// `PRT-B-02.01` through `PRT-B-02.12`.
///
/// Each bounded row is evaluated at N-1 and N, and its N+1 counterpart is
/// evaluated in the planted negative. Same contract as the A manifests.
pub const PRT_02_B_BOUNDS_DIRECTION_MANIFEST_V1: &str = concat!(
    "PRT02-B-BOUNDS-DIRECTION-v1\n",
    "producer-bead bd-mcp-prt-02-b-nnva\n",
    "producer-revision 0733ee3e4b7b04a1ad0a33d78a8e1b0f6e6f6d2f\n",
    "entrypoint fastmcp_protocol::common_types::OpenMetadata::try_from_entries\n",
    "entrypoint fastmcp_protocol::common_types::AbsoluteUri::parse\n",
    "entrypoint fastmcp_protocol::common_types::OpaqueCursor::try_from_presence\n",
    "PRT-B-02.01 metadata-entries-n-minus-one floor=1\n",
    "PRT-B-02.02 metadata-entries-n floor=1\n",
    "PRT-B-02.03 uri-bytes-n-minus-one floor=1\n",
    "PRT-B-02.04 uri-bytes-n floor=1\n",
    "PRT-B-02.05 cursor-bytes-n-minus-one floor=1\n",
    "PRT-B-02.06 cursor-bytes-n floor=1\n",
    "PRT-B-02.07 icon-sizes-n-minus-one floor=1\n",
    "PRT-B-02.08 icon-sizes-n floor=1\n",
    "PRT-B-02.09 content-bytes-n-minus-one floor=1\n",
    "PRT-B-02.10 cancellation-reason-unbounded-by-schema floor=1\n",
    "PRT-B-02.11 request-direction-accepted floor=1\n",
    "PRT-B-02.12 notification-direction-accepted floor=1\n",
);

/// The canonical `PRT02-B-OPEN-GOLDENS-v1` rows: ordered subcases
/// `PRT-B-03.01` through `PRT-B-03.08`.
///
/// Same contract as the A manifests. A golden row asserts exact canonical
/// bytes, so an unrecognized or malformed row can never quietly become a
/// compatibility alias for a recognized one.
pub const PRT_02_B_OPEN_GOLDENS_MANIFEST_V1: &str = concat!(
    "PRT02-B-OPEN-GOLDENS-v1\n",
    "producer-bead bd-mcp-prt-02-b-nnva\n",
    "producer-revision 0733ee3e4b7b04a1ad0a33d78a8e1b0f6e6f6d2f\n",
    "entrypoint fastmcp_protocol::common_types::FinalCommonTypesSchema::validate_golden\n",
    "PRT-B-03.01 golden-known-metadata floor=1\n",
    "PRT-B-03.02 golden-valid-open-metadata floor=1\n",
    "PRT-B-03.03 golden-unknown-open-preserved floor=1\n",
    "PRT-B-03.04 golden-absolute-uri floor=1\n",
    "PRT-B-03.05 golden-cursor floor=1\n",
    "PRT-B-03.06 golden-cancellation floor=1\n",
    "PRT-B-03.07 golden-icon floor=1\n",
    "PRT-B-03.08 golden-trace floor=1\n",
);

/// Returns the canonical `PRT02-B-SERDE-SCHEMA-v1` digest.
#[must_use]
pub fn prt_02_b_serde_schema_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_B_SERDE_SCHEMA_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 B serde/schema manifest is within its exact byte bound")
}

/// Returns the canonical `PRT02-B-BOUNDS-DIRECTION-v1` digest.
#[must_use]
pub fn prt_02_b_bounds_direction_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_B_BOUNDS_DIRECTION_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 B bounds/direction manifest is within its exact byte bound")
}

/// Returns the canonical `PRT02-B-OPEN-GOLDENS-v1` digest.
#[must_use]
pub fn prt_02_b_open_goldens_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_B_OPEN_GOLDENS_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 B open-goldens manifest is within its exact byte bound")
}

/// The canonical `PRT02-I-PUBLIC-JOIN-v1` rows: ordered subcases
/// `PRT-I-01.01` through `PRT-I-01.12`.
///
/// These are the rows the PRT-02 integration leaf must exercise through
/// shipped public entrypoints only. Same row contract as the A and B
/// manifests.
pub const PRT_02_I_PUBLIC_JOIN_MANIFEST_V1: &str = concat!(
    "PRT02-I-PUBLIC-JOIN-v1\n",
    "producer-bead bd-mcp-prt-02-integration-n3ur\n",
    "producer-revision 0689341b0b2f4a1c6d3e9f5a7b8c0d1e2f3a4b5c\n",
    "entrypoint fastmcp_rust public facade\n",
    "PRT-I-01.01 implementation-try-new floor=1\n",
    "PRT-I-01.02 open-metadata-try-from-entries floor=1\n",
    "PRT-I-01.03 absolute-uri-parse floor=1\n",
    "PRT-I-01.04 opaque-cursor-from-presence floor=1\n",
    "PRT-I-01.05 cancellation-notification-try-new floor=1\n",
    "PRT-I-01.06 raw-icon-try-new floor=1\n",
    "PRT-I-01.07 content-block-constructors floor=1\n",
    "PRT-I-01.08 serde-to-value floor=1\n",
    "PRT-I-01.09 serde-from-value floor=1\n",
    "PRT-I-01.10 schema-validate floor=1\n",
    "PRT-I-01.11 a-manifests-structurally-sound floor=3\n",
    "PRT-I-01.12 b-manifests-structurally-sound floor=3\n",
);

/// The canonical `PRT02-I-OPEN-FINAL-JOIN-v1` rows: ordered subcases
/// `PRT-I-02.01` through `PRT-I-02.08`.
pub const PRT_02_I_OPEN_FINAL_JOIN_MANIFEST_V1: &str = concat!(
    "PRT02-I-OPEN-FINAL-JOIN-v1\n",
    "producer-bead bd-mcp-prt-02-integration-n3ur\n",
    "producer-revision 0689341b0b2f4a1c6d3e9f5a7b8c0d1e2f3a4b5c\n",
    "entrypoint fastmcp_rust public facade\n",
    "PRT-I-02.01 final-known-metadata floor=1\n",
    "PRT-I-02.02 valid-open-metadata-preserved floor=1\n",
    "PRT-I-02.03 invalid-open-metadata-refused floor=1\n",
    "PRT-I-02.04 uri-policy-classes floor=2\n",
    "PRT-I-02.05 cursor-presence-three-states floor=3\n",
    "PRT-I-02.06 cancellation-direction floor=1\n",
    "PRT-I-02.07 icon-and-content floor=2\n",
    "PRT-I-02.08 trace-propagation floor=1\n",
);

/// Returns the canonical `PRT02-I-PUBLIC-JOIN-v1` digest.
#[must_use]
pub fn prt_02_i_public_join_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_I_PUBLIC_JOIN_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 I public-join manifest is within its exact byte bound")
}

/// Returns the canonical `PRT02-I-OPEN-FINAL-JOIN-v1` digest.
#[must_use]
pub fn prt_02_i_open_final_join_manifest_digest() -> fastmcp_core::Sha256Digest {
    fastmcp_core::sha256_bounded(
        PRT_02_I_OPEN_FINAL_JOIN_MANIFEST_V1.as_bytes(),
        MAX_PRT_02_MANIFEST_BYTES,
    )
    .expect("the fixed PRT-02 I open-final-join manifest is within its exact byte bound")
}

/// Every published PRT-02 manifest, in role order: the three A manifests, the
/// three B manifests, then the two integration manifests.
///
/// An integrating consumer uses this to check properties that span the whole
/// family — that no two manifests share a subcase identifier, that each row
/// set is well formed, and that the eight canonical digests are distinct —
/// rather than re-hashing bytes it already holds, which would only restate the
/// hash function back to itself.
#[must_use]
pub const fn prt_02_all_manifests() -> [&'static str; 8] {
    [
        PRT_02_A_METADATA_MANIFEST_V1,
        PRT_02_A_URI_CURSOR_CANCEL_MANIFEST_V1,
        PRT_02_A_ICON_CONTENT_MANIFEST_V1,
        PRT_02_B_SERDE_SCHEMA_MANIFEST_V1,
        PRT_02_B_BOUNDS_DIRECTION_MANIFEST_V1,
        PRT_02_B_OPEN_GOLDENS_MANIFEST_V1,
        PRT_02_I_PUBLIC_JOIN_MANIFEST_V1,
        PRT_02_I_OPEN_FINAL_JOIN_MANIFEST_V1,
    ]
}

/// Parses one published PRT-02 manifest into its ordered `(id, name, floor)`
/// subcase rows.
///
/// Header rows are those before the first row whose identifier starts with
/// `PRT-`. The parse is deliberately strict: a malformed row, a non-numeric
/// floor, or a duplicate identifier returns `None`, so a consumer cannot
/// silently evaluate a manifest it only partly understood.
#[must_use]
pub fn parse_prt_02_manifest_rows(manifest: &str) -> Option<Vec<(String, String, usize)>> {
    let mut rows: Vec<(String, String, usize)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if !manifest.ends_with('\n') || manifest.contains('\r') || manifest.contains("\n\n") {
        return None;
    }
    for line in manifest.lines() {
        if !line.starts_with("PRT-") {
            continue;
        }
        let mut parts = line.split(' ');
        let (Some(id), Some(name), Some(floor), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let floor: usize = floor.strip_prefix("floor=")?.parse().ok()?;
        if floor == 0 || !seen.insert(id.to_owned()) {
            return None;
        }
        rows.push((id.to_owned(), name.to_owned(), floor));
    }
    (!rows.is_empty()).then_some(rows)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn assert_json_integer_rejected_by_public_constructors(
        source: &str,
        expected: CommonTypeError,
    ) {
        assert_eq!(source.parse::<JsonInteger>(), Err(expected.clone()));
        assert_eq!(JsonInteger::try_from(source), Err(expected.clone()));
        let number = serde_json::from_str::<serde_json::Number>(source)
            .expect("bounded test token is valid JSON");
        assert_eq!(JsonInteger::try_from_number(number), Err(expected));
        assert!(
            serde_json::from_str::<JsonInteger>(source).is_err(),
            "deserialization must apply the same admission bound"
        );
    }

    #[test]
    fn json_integer_bounded_i32_adapters_accept_equivalent_integral_spellings() {
        for (source, expected) in [
            ("-32600.0", -32_600),
            ("-326e2", -32_600),
            ("2147483647.0", i32::MAX),
            ("-2147483648e0", i32::MIN),
        ] {
            let value = JsonInteger::try_from(source).expect("integral JSON integer");

            assert_eq!(value.as_i32(), Some(expected));
            assert_eq!(value.as_str(), source, "the input lexeme remains exact");
            assert_eq!(
                serde_json::to_string(&value).expect("integer serializes"),
                source,
                "serialization does not normalize the input lexeme"
            );
        }
    }

    #[test]
    fn json_integer_from_value_accepts_full_width_integer_visitors() {
        for source in ["9007199254740993123456789", "-9007199254740993123456789"] {
            let value = serde_json::from_str::<Value>(source).expect("valid arbitrary-width JSON");
            let integer = serde_json::from_value::<JsonInteger>(value)
                .expect("Value replay retains an arbitrary-width mathematical integer");

            assert_eq!(integer.as_str(), source);
        }
    }

    #[test]
    fn json_integer_bounded_i32_adapters_reject_fractional_and_out_of_range_values() {
        for source in ["-32600.1", "2147483647.1"] {
            assert_eq!(
                JsonInteger::try_from(source),
                Err(CommonTypeError::Invalid("JSON integer")),
                "changing only the nonzero fractional digit rejects {source}"
            );
        }

        for source in ["2147483648.0", "-2147483649e0"] {
            let value = source
                .parse::<JsonInteger>()
                .expect("exact out-of-range integer");

            assert_eq!(value.as_i32(), None);
            assert_eq!(
                value.as_str(),
                source,
                "the out-of-range lexeme remains exact"
            );
        }
    }

    #[test]
    fn json_integer_public_constructors_enforce_token_and_exponent_bounds() {
        let at_token_limit = "1".repeat(MAX_JSON_INTEGER_BYTES);
        for value in [
            at_token_limit
                .parse::<JsonInteger>()
                .expect("token at the retention bound parses"),
            JsonInteger::try_from(at_token_limit.as_str())
                .expect("TryFrom accepts token at the retention bound"),
            JsonInteger::try_from_number(
                serde_json::from_str::<serde_json::Number>(&at_token_limit)
                    .expect("token at the retention bound is JSON"),
            )
            .expect("number constructor accepts token at the retention bound"),
            serde_json::from_str::<JsonInteger>(&at_token_limit)
                .expect("deserialization accepts token at the retention bound"),
        ] {
            assert_eq!(value.as_str(), at_token_limit);
        }
        assert_json_integer_rejected_by_public_constructors(
            &format!("{at_token_limit}0"),
            CommonTypeError::TooLong("JSON integer"),
        );

        let at_positive_exponent_limit = format!("1e{MAX_JSON_INTEGER_EXPONENT_ABS}");
        let at_negative_exponent_limit = format!("0e-{MAX_JSON_INTEGER_EXPONENT_ABS}");
        for source in [&at_positive_exponent_limit, &at_negative_exponent_limit] {
            assert!(source.parse::<JsonInteger>().is_ok(), "{source}");
            assert!(JsonInteger::try_from(source.as_str()).is_ok(), "{source}");
            assert!(
                JsonInteger::try_from_number(
                    serde_json::from_str::<serde_json::Number>(source)
                        .expect("exponent-bound token is JSON"),
                )
                .is_ok(),
                "{source}"
            );
            assert!(
                serde_json::from_str::<JsonInteger>(source).is_ok(),
                "{source}"
            );
        }
        assert_json_integer_rejected_by_public_constructors(
            &format!("1e{}", MAX_JSON_INTEGER_EXPONENT_ABS + 1),
            CommonTypeError::TooLong("JSON integer exponent"),
        );
        assert_json_integer_rejected_by_public_constructors(
            &format!("0e-{}", MAX_JSON_INTEGER_EXPONENT_ABS + 1),
            CommonTypeError::TooLong("JSON integer exponent"),
        );
    }

    #[test]
    fn json_integer_from_str_and_try_from_preserve_huge_lexemes() {
        const HUGE: &str = "12345678901234567890123456789012345678901234567890";

        let parsed = HUGE.parse::<JsonInteger>().expect("huge integer parses");
        let converted = JsonInteger::try_from(HUGE).expect("huge integer converts");

        assert_eq!(parsed.as_str(), HUGE);
        assert_eq!(converted.as_str(), HUGE);
        assert_eq!(
            serde_json::to_string(&parsed).expect("huge integer serializes"),
            HUGE
        );
        let exponent = serde_json::from_str::<JsonInteger>("-326e2")
            .expect("integral exponent JSON token deserializes");
        assert_eq!(exponent.as_str(), "-326e2");
        assert_eq!(
            serde_json::to_string(&exponent).expect("deserialized exponent serializes"),
            "-326e2"
        );
        for invalid_json_number in ["01", "1."] {
            assert_eq!(
                JsonInteger::try_from(invalid_json_number),
                Err(CommonTypeError::Invalid("JSON integer")),
                "the string conversion only admits JSON number grammar"
            );
        }
    }

    #[test]
    fn exact_finite_json_numbers_preserve_signed_lexemes_and_compare_mathematically() {
        let large = ExactNonNegativeJsonNumber::parse("123456789012345678901234567890")
            .expect("large integer exact progress number");
        let decimal = ExactNonNegativeJsonNumber::parse("1.20e+4")
            .expect("decimal exponent exact progress number");
        let equivalent = ExactNonNegativeJsonNumber::parse("12000.0")
            .expect("equivalent decimal exact progress number");
        let greater =
            ExactNonNegativeJsonNumber::parse("12000.0001").expect("greater exact progress number");
        let negative =
            ExactNonNegativeJsonNumber::parse("-1.20e+4").expect("negative exact progress number");
        let more_negative = ExactNonNegativeJsonNumber::parse("-12000.0001")
            .expect("more negative exact progress number");

        assert_eq!(large.as_str(), "123456789012345678901234567890");
        assert_eq!(decimal.as_str(), "1.20e+4");
        assert_eq!(decimal, equivalent);
        assert!(greater > decimal);
        assert!(more_negative < negative);
        assert!(negative < decimal);
        assert_eq!(
            serde_json::to_string(&decimal).expect("exact progress number serializes"),
            "1.20e+4",
            "the decimal/exponent lexeme re-encodes without an IEEE-754 conversion"
        );
        assert_eq!(
            serde_json::to_string(&negative).expect("negative exact progress number serializes"),
            "-1.20e+4",
            "the signed decimal/exponent lexeme re-encodes without an IEEE-754 conversion"
        );
        assert_eq!(
            ExactNonNegativeJsonNumber::parse("1e10000"),
            Err(CommonTypeError::TooLong("progress number exponent")),
            "the exact comparison representation bounds decimal exponents"
        );
    }

    #[test]
    fn exact_finite_json_number_deserialization_retains_direct_wire_exponents() {
        for source in ["1e400", "1.20e+4", "-7.30E-12"] {
            let number = serde_json::from_str::<ExactNonNegativeJsonNumber>(source)
                .expect("bounded exact progress number deserializes");

            assert_eq!(number.as_str(), source);
            assert_eq!(
                serde_json::to_string(&number).expect("exact progress number re-serializes"),
                source
            );
        }
    }

    #[test]
    fn exact_finite_json_number_deserialization_rejects_only_the_bound_violation() {
        let accepted = serde_json::from_str::<ExactNonNegativeJsonNumber>("1e9999")
            .expect("largest admitted exponent deserializes");
        assert_eq!(accepted.as_str(), "1e9999");

        assert!(
            serde_json::from_str::<ExactNonNegativeJsonNumber>("1e10000").is_err(),
            "only the exponent bound changes from the accepted token"
        );
        assert_eq!(
            serde_json::to_string(&accepted).expect("accepted boundary re-serializes"),
            "1e9999",
            "rejecting the adjacent exponent cannot mutate the accepted value"
        );

        let at_byte_limit = "1".repeat(MAX_EXACT_PROGRESS_NUMBER_BYTES);
        let bounded = serde_json::from_str::<ExactNonNegativeJsonNumber>(&at_byte_limit)
            .expect("an exact progress number at the byte ceiling deserializes");
        assert_eq!(bounded.as_str(), at_byte_limit);
        let oversized = format!("1{}", "0".repeat(MAX_EXACT_PROGRESS_NUMBER_BYTES));
        assert_eq!(oversized.len(), MAX_EXACT_PROGRESS_NUMBER_BYTES + 1);
        assert!(
            serde_json::from_str::<ExactNonNegativeJsonNumber>(&oversized).is_err(),
            "adding one byte is the only changed dimension and exceeds the byte ceiling"
        );
        assert_eq!(
            serde_json::to_string(&bounded).expect("bounded number re-serializes"),
            at_byte_limit,
            "the over-limit rejection cannot alter the admitted 256-byte lexeme"
        );
    }

    #[test]
    fn exact_finite_json_number_deserialization_handles_all_finite_number_visitors() {
        let from_i64 = <ExactNonNegativeJsonNumber as serde::Deserialize>::deserialize(
            serde::de::value::I64Deserializer::<serde::de::value::Error>::new(i64::MIN),
        )
        .expect("i64 visitor admits a finite JSON number");
        let from_u64 = <ExactNonNegativeJsonNumber as serde::Deserialize>::deserialize(
            serde::de::value::U64Deserializer::<serde::de::value::Error>::new(u64::MAX),
        )
        .expect("u64 visitor admits a finite JSON number");
        let from_i128 = <ExactNonNegativeJsonNumber as serde::Deserialize>::deserialize(
            serde::de::value::I128Deserializer::<serde::de::value::Error>::new(i128::MIN),
        )
        .expect("i128 visitor admits a finite JSON number");
        let from_u128 = <ExactNonNegativeJsonNumber as serde::Deserialize>::deserialize(
            serde::de::value::U128Deserializer::<serde::de::value::Error>::new(u128::MAX),
        )
        .expect("u128 visitor admits a finite JSON number");
        let from_f64 = <ExactNonNegativeJsonNumber as serde::Deserialize>::deserialize(
            serde::de::value::F64Deserializer::<serde::de::value::Error>::new(1.25),
        )
        .expect("finite f64 visitor admits a JSON number");

        assert_eq!(from_i64.as_str(), "-9223372036854775808");
        assert_eq!(from_u64.as_str(), "18446744073709551615");
        assert_eq!(
            from_i128.as_str(),
            "-170141183460469231731687303715884105728"
        );
        assert_eq!(
            from_u128.as_str(),
            "340282366920938463463374607431768211455"
        );
        assert_eq!(from_f64.as_str(), "1.25");

        assert!(
            <ExactNonNegativeJsonNumber as serde::Deserialize>::deserialize(
                serde::de::value::F64Deserializer::<serde::de::value::Error>::new(f64::NAN),
            )
            .is_err(),
            "changing only the finite f64 to NaN must fail closed"
        );
        assert_eq!(
            serde_json::to_string(&from_f64).expect("finite f64 result re-serializes"),
            "1.25",
            "the rejected non-finite visitor cannot change the prior finite result"
        );
    }

    #[test]
    fn exact_finite_json_number_value_replay_uses_the_value_number_representation() {
        let direct = serde_json::from_str::<ExactNonNegativeJsonNumber>("1.20e+4")
            .expect("direct raw wire lexeme is admitted");
        let value = serde_json::from_str::<Value>("1.20e+4")
            .expect("the same raw number enters a serde Value replay");
        let expected = value
            .as_number()
            .expect("Value remains a number")
            .as_str()
            .to_owned();
        let replayed = serde_json::from_value::<ExactNonNegativeJsonNumber>(value)
            .expect("Value replay admits the finite number representation");

        assert_eq!(direct.as_str(), "1.20e+4");
        assert_eq!(replayed.as_str(), expected);
        assert_eq!(
            serde_json::to_string(&replayed).expect("replayed number re-serializes"),
            replayed.as_str(),
            "replay must not apply another numeric normalization"
        );
    }

    // These four deliberately do NOT carry the frozen PRT-02 A/B acceptance
    // IDs, for the same two reasons the FND-03 family records in lib.rs. A
    // `cfg(test)` function cannot prove shipped behavior (PL-3), and a frozen
    // ID must resolve to exactly one definition workspace-wide (PL-1) so that
    // `cargo test <id> -- --exact` discovers exactly one test. Before this
    // rename each of `prt_02_a_positive`, `prt_02_a_planted_negative`,
    // `prt_02_b_positive` and `prt_02_b_planted_negative` had two definitions
    // — one here and one in the external public-surface consumer — so an
    // exact-set run discovered two rows per required ID. The frozen pairs live
    // in `crates/fastmcp-protocol/tests/prt_02_common_types.rs`; these retain
    // their assertions as in-crate coverage.
    #[test]
    fn prt_02_a_unit_positive() {
        let implementation = Implementation::try_new("fastmcp", "0.1.0").expect("implementation");
        let metadata = OpenMetadata::try_from_entries([
            ("".to_owned(), json!("empty name is valid")),
            ("com.example/".to_owned(), json!({"future": true})),
            (
                "io.modelcontextprotocol/protocolVersion".to_owned(),
                json!("2026-07-28"),
            ),
            (
                "io.modelcontextprotocol/clientCapabilities".to_owned(),
                json!({}),
            ),
            (
                "io.modelcontextprotocol/clientInfo".to_owned(),
                serde_json::to_value(&implementation).expect("identity JSON"),
            ),
            (
                "traceparent".to_owned(),
                json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"),
            ),
        ])
        .expect("metadata");
        assert_eq!(
            metadata.protocol_version().expect("version"),
            Some("2026-07-28")
        );
        assert_eq!(
            TraceContext::try_from_metadata(&metadata)
                .expect("trace")
                .traceparent
                .as_deref(),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
        );

        let icon = RawIcon::try_new("https://example.test/icon.png?variant=1#exact").expect("icon");
        assert!(icon.effective_any_size());
        assert_eq!(
            OpaqueCursor::from_presence(Some(String::new())).as_present(),
            Some("")
        );
        let content = ContentBlock::image("aGVsbG8=", "image/png").expect("image");
        let encoded = serde_json::to_value(&content).expect("serialize content");
        assert_eq!(encoded["type"], "image");
        assert_eq!(
            serde_json::from_value::<ContentBlock>(encoded).expect("round trip"),
            content
        );
    }

    #[test]
    fn prt_02_a_unit_planted_negative() {
        let accepted = OpenMetadata::try_from_entries([(
            "com.example/valid".to_owned(),
            json!({"kept": true}),
        )])
        .expect("accepted baseline");
        let baseline = accepted.clone();
        let rejection = OpenMetadata::try_from_entries([(
            "com..example/valid".to_owned(),
            json!({"kept": true}),
        )]);
        assert_eq!(rejection, Err(CommonTypeError::Invalid("metadata key")));
        assert_eq!(
            accepted, baseline,
            "the rejected one-variable key change cannot mutate accepted state"
        );
    }

    #[test]
    fn notification_metadata_remains_schema_open_but_bounded() {
        let at_entry_limit =
            (0..MAX_METADATA_ENTRIES).map(|index| (format!("com.example/key{index}"), Value::Null));
        OpenMetadata::try_from_notification_entries(at_entry_limit)
            .expect("notification metadata accepts N entries");
        let over_entry_limit = (0..=MAX_METADATA_ENTRIES)
            .map(|index| (format!("com.example/key{index}"), Value::Null));
        assert_eq!(
            OpenMetadata::try_from_notification_entries(over_entry_limit),
            Err(CommonTypeError::Invalid("metadata key"))
        );

        let at_key_limit = "a".repeat(MAX_METADATA_KEY_BYTES);
        OpenMetadata::try_from_notification_entries([(at_key_limit, Value::Null)])
            .expect("notification metadata accepts an N-byte key");
        let over_key_limit = "a".repeat(MAX_METADATA_KEY_BYTES + 1);
        assert_eq!(
            OpenMetadata::try_from_notification_entries([(over_key_limit, Value::Null)]),
            Err(CommonTypeError::Invalid("metadata key"))
        );

        let at_value_limit = json!("x".repeat(MAX_METADATA_VALUE_BYTES - 2));
        assert_eq!(
            serde_json::to_vec(&at_value_limit)
                .expect("bounded metadata value serializes")
                .len(),
            MAX_METADATA_VALUE_BYTES
        );
        OpenMetadata::try_from_notification_entries([("future".to_owned(), at_value_limit)])
            .expect("notification metadata accepts an N-byte value");
        let over_value_limit = json!("x".repeat(MAX_METADATA_VALUE_BYTES - 1));
        assert_eq!(
            serde_json::to_vec(&over_value_limit)
                .expect("oversized metadata value serializes")
                .len(),
            MAX_METADATA_VALUE_BYTES + 1
        );
        assert_eq!(
            OpenMetadata::try_from_notification_entries([("future".to_owned(), over_value_limit,)]),
            Err(CommonTypeError::Invalid("metadata key"))
        );

        OpenMetadata::try_from_notification_entries([(
            "io.modelcontextprotocol/futureCancellationHint".to_owned(),
            json!({"schemaOpen": true}),
        )])
        .expect("unknown reserved notification metadata remains inert and admitted");
    }

    #[test]
    fn prt_02_b_unit_positive() {
        let request = json!({
            "_meta": {
                "com.example/future": {"nullIsData": null},
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"
            }
        });
        FinalCommonTypesSchema::validate(CommonWireDirection::Request, &request)
            .expect("request metadata schema");
        let golden = "{\"_meta\":{\"com.example/future\":{\"nullIsData\":null},\"io.modelcontextprotocol/clientCapabilities\":{},\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"traceparent\":\"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00\"}}";
        assert_eq!(
            FinalCommonTypesSchema::canonical_json(&request).expect("canonical JSON"),
            golden
        );
        FinalCommonTypesSchema::validate_golden(CommonWireDirection::Request, &request, golden)
            .expect("exact request golden");

        let content = ContentBlock::image("aGVsbG8=", "image/png").expect("image content");
        let wire = serde_json::to_value(&content).expect("content wire");
        FinalCommonTypesSchema::validate(CommonWireDirection::Result, &wire)
            .expect("content schema");
        assert_eq!(wire["type"], "image");
        assert_eq!(
            serde_json::from_value::<ContentBlock>(wire).expect("content round trip"),
            content
        );
        assert_eq!(
            OpaqueCursor::try_from_presence(Some(String::new()))
                .expect("bounded empty cursor")
                .as_present(),
            Some("")
        );
        let icon = json!({
            "src": "HTTPS://example.test/icon.svg?variant=1",
            "sizes": [],
            "theme": "dark"
        });
        let icon = FinalCommonTypesSchema::validate_icon(&icon).expect("icon schema");
        assert!(
            !icon.effective_any_size(),
            "present empty sizes stay present"
        );
        let cancellation = json!({
            "method": "notifications/cancelled",
            "params": {"requestId": 9, "reason": "bounded"}
        });
        FinalCommonTypesSchema::validate(CommonWireDirection::Notification, &cancellation)
            .expect("notification-only cancellation");
        let bounded_cursor = OpaqueCursor::try_from_presence(Some("x".repeat(MAX_CURSOR_BYTES)))
            .expect("cursor at exact bound");
        assert_eq!(
            bounded_cursor.as_present().map(str::len),
            Some(MAX_CURSOR_BYTES)
        );
        assert_eq!(FinalCommonTypesSchema::FINAL_URI_OWNERS.len(), 12);
    }

    #[test]
    fn final_common_content_bridge_round_trips_complete_resource_link() {
        let icon = RawIcon::try_with_details(
            "https://example.test/icons/report.svg",
            Some("image/svg+xml".to_owned()),
            Some(vec!["48x48".to_owned(), "any".to_owned()]),
            Some(IconTheme::Dark),
        )
        .expect("final icon");
        let annotations = Annotations {
            audience: Some(vec![
                AnnotationAudience::User,
                AnnotationAudience::Assistant,
            ]),
            priority: Some(0.75),
            last_modified: Some("2026-07-28T15:00:58Z".to_owned()),
            additional: BTreeMap::new(),
        };
        let metadata = OpenMetadata::try_from_entries([(
            "com.example/renderHint".to_owned(),
            json!({"preserve": true}),
        )])
        .expect("content metadata");
        let resource_link = ResourceLink {
            icons: Some(vec![icon.clone()]),
            name: "report".to_owned(),
            title: Some("Quarterly report".to_owned()),
            uri: AbsoluteUri::parse("https://example.test/reports/q3").expect("resource URI"),
            description: Some("Raw quarterly figures".to_owned()),
            mime_type: Some("text/markdown".to_owned()),
            annotations: Some(annotations.clone()),
            size: Some(JsonInteger::from(4096_i64)),
            meta: Some(metadata.clone()),
            additional: BTreeMap::new(),
        };
        let resource_link_wire = serde_json::to_value(&resource_link).expect("resource link");
        assert_eq!(resource_link_wire["type"], "resource_link");
        assert_eq!(
            resource_link_wire["icons"][0]["sizes"],
            json!(["48x48", "any"])
        );
        assert_eq!(resource_link_wire["icons"][0]["theme"], "dark");
        assert_eq!(
            resource_link_wire["annotations"]["audience"],
            json!(["user", "assistant"])
        );
        assert_eq!(
            resource_link_wire["_meta"]["com.example/renderHint"]["preserve"],
            true
        );
        assert_eq!(
            serde_json::from_value::<ResourceLink>(resource_link_wire.clone())
                .expect("resource link round trip"),
            resource_link
        );
        FinalCommonTypesSchema::validate(CommonWireDirection::Result, &resource_link_wire)
            .expect("final resource link schema");

        let content = ContentBlock::ResourceLink {
            icons: Some(vec![icon]),
            name: "report".to_owned(),
            title: Some("Quarterly report".to_owned()),
            uri: AbsoluteUri::parse("https://example.test/reports/q3").expect("content URI"),
            description: Some("Raw quarterly figures".to_owned()),
            mime_type: Some("text/markdown".to_owned()),
            annotations: Some(annotations),
            size: Some(JsonInteger::from(4096_i64)),
            meta: Some(metadata),
            additional: BTreeMap::new(),
        };
        let content_wire = serde_json::to_value(&content).expect("content wire");
        assert_eq!(
            serde_json::from_value::<ContentBlock>(content_wire).expect("content round trip"),
            content
        );

        for (level, wire) in [
            (LoggingLevel::Debug, "debug"),
            (LoggingLevel::Info, "info"),
            (LoggingLevel::Notice, "notice"),
            (LoggingLevel::Warning, "warning"),
            (LoggingLevel::Error, "error"),
            (LoggingLevel::Critical, "critical"),
            (LoggingLevel::Alert, "alert"),
            (LoggingLevel::Emergency, "emergency"),
        ] {
            assert_eq!(serde_json::to_value(level).expect("logging level"), wire);
            assert_eq!(
                serde_json::from_value::<LoggingLevel>(json!(wire)).expect("logging level"),
                level
            );
        }
    }

    #[test]
    fn final_resource_link_rejects_legacy_icon_sizes_without_mutating_accepted_wire() {
        let accepted = json!({
            "type": "resource_link",
            "icons": [{
                "src": "https://example.test/icons/report.svg",
                "sizes": ["48x48"],
                "theme": "dark"
            }],
            "name": "report",
            "uri": "https://example.test/reports/q3"
        });
        FinalCommonTypesSchema::validate(CommonWireDirection::Result, &accepted)
            .expect("accepted final resource link");
        let baseline = accepted.clone();
        let mut planted = accepted.clone();
        planted["icons"][0]["sizes"] = json!("48x48");
        assert_eq!(
            FinalCommonTypesSchema::validate(CommonWireDirection::Result, &planted),
            Err(CommonTypeError::Invalid("content block"))
        );
        assert_eq!(
            accepted, baseline,
            "the rejected one-field legacy size spelling cannot mutate final wire state"
        );
    }

    #[test]
    fn final_resource_link_size_is_an_optional_integer() {
        let accepted: Value = serde_json::from_str(
            r#"{
                "type":"resource_link",
                "name":"report",
                "uri":"https://example.test/reports/q3",
                "size":922337203685477580812345678901234567890
            }"#,
        )
        .expect("large integer wire parses");
        let resource_link: ResourceLink = serde_json::from_value(accepted.clone())
            .expect("integer resource-link size is admitted");
        assert_eq!(
            resource_link.size.as_ref().map(JsonInteger::as_str),
            Some("922337203685477580812345678901234567890")
        );
        assert_eq!(
            serde_json::to_value(&resource_link).expect("integer resource-link size encodes"),
            accepted
        );

        let negative: Value = serde_json::from_str(
            r#"{
                "type":"resource_link",
                "name":"report",
                "uri":"https://example.test/reports/q3",
                "size":-922337203685477580812345678901234567890
            }"#,
        )
        .expect("large negative integer wire parses");
        let negative_link: ResourceLink = serde_json::from_value(negative.clone())
            .expect("a schema-integer resource-link size may be negative");
        assert_eq!(
            negative_link.size.as_ref().map(JsonInteger::as_str),
            Some("-922337203685477580812345678901234567890")
        );
        assert_eq!(
            serde_json::to_value(&negative_link)
                .expect("negative integer resource-link size encodes"),
            negative
        );

        let negative_content: ContentBlock = serde_json::from_value(negative.clone())
            .expect("content resource links preserve schema-integer sizes");
        assert_eq!(
            serde_json::to_value(&negative_content)
                .expect("negative content resource-link size encodes"),
            negative
        );

        let missing = json!({
            "type": "resource_link",
            "name": "report",
            "uri": "https://example.test/reports/q3"
        });
        let missing_size: ResourceLink =
            serde_json::from_value(missing.clone()).expect("resource-link size is optional");
        assert_eq!(missing_size.size, None);
        assert_eq!(
            serde_json::to_value(missing_size).expect("absent size remains absent"),
            missing
        );

        let wrong_type = json!({
            "type": "resource_link",
            "name": "report",
            "uri": "https://example.test/reports/q3",
            "size": 4096.5
        });
        assert!(
            serde_json::from_value::<ResourceLink>(wrong_type).is_err(),
            "a fractional resource-link size is not an integer"
        );
    }

    #[test]
    fn final_common_types_preserve_schema_allowed_additional_properties() {
        let implementation = json!({
            "name": "FastMCP",
            "version": "0.1",
            "com.example/implementation": {"stable": true}
        });
        let implementation: Implementation = serde_json::from_value(implementation.clone())
            .expect("schema-allowed implementation property is retained");
        assert_eq!(
            implementation.additional.get("com.example/implementation"),
            Some(&json!({"stable": true}))
        );
        assert_eq!(
            serde_json::to_value(&implementation).expect("implementation property re-emits"),
            json!({
                "name": "FastMCP",
                "version": "0.1",
                "com.example/implementation": {"stable": true}
            })
        );

        let resource_link = json!({
            "type": "resource_link",
            "name": "report",
            "uri": "https://example.test/reports/q3",
            "com.example/resourceLink": ["preserved"]
        });
        let resource_link: ResourceLink = serde_json::from_value(resource_link.clone())
            .expect("schema-allowed resource-link property is retained");
        assert_eq!(
            resource_link.additional.get("com.example/resourceLink"),
            Some(&json!(["preserved"]))
        );
        assert_eq!(
            serde_json::to_value(&resource_link).expect("resource-link property re-emits"),
            json!({
                "type": "resource_link",
                "name": "report",
                "uri": "https://example.test/reports/q3",
                "com.example/resourceLink": ["preserved"]
            })
        );
        FinalCommonTypesSchema::validate(
            CommonWireDirection::Result,
            &serde_json::to_value(&resource_link).expect("resource-link validation wire"),
        )
        .expect("schema-allowed resource-link property remains valid");

        let content = json!({
            "type": "text",
            "text": "report ready",
            "com.example/content": {"priority": "display"}
        });
        let content: ContentBlock = serde_json::from_value(content.clone())
            .expect("schema-allowed content property is retained");
        assert_eq!(
            serde_json::to_value(&content).expect("content property re-emits"),
            json!({
                "type": "text",
                "text": "report ready",
                "com.example/content": {"priority": "display"}
            })
        );
        FinalCommonTypesSchema::validate(
            CommonWireDirection::Result,
            &serde_json::to_value(&content).expect("content validation wire"),
        )
        .expect("schema-allowed content property remains valid");
    }

    #[test]
    fn final_common_nested_open_fields_and_subscription_integer_round_trip() {
        let resource_link = json!({
            "type": "resource_link",
            "icons": [{
                "src": "https://example.test/icons/report.svg",
                "com.example/icon": {"retained": true}
            }],
            "name": "report",
            "uri": "https://example.test/reports/q3",
            "annotations": {
                "com.example/annotation": ["retained"]
            }
        });
        let resource_link: ResourceLink = serde_json::from_value(resource_link.clone())
            .expect("schema-open icon and annotation fields decode");
        assert_eq!(
            serde_json::to_value(&resource_link).expect("nested extensions re-encode"),
            json!({
                "type": "resource_link",
                "icons": [{
                    "src": "https://example.test/icons/report.svg",
                    "com.example/icon": {"retained": true}
                }],
                "name": "report",
                "uri": "https://example.test/reports/q3",
                "annotations": {
                    "com.example/annotation": ["retained"]
                }
            })
        );

        let embedded = json!({
            "type": "resource",
            "resource": {
                "uri": "https://example.test/resources/report",
                "text": "ready",
                "_meta": {"com.example/source": "cache"},
                "com.example/resource": {"retained": true}
            }
        });
        let embedded_content: ContentBlock = serde_json::from_value(embedded.clone())
            .expect("embedded resource metadata and open fields decode");
        assert_eq!(
            serde_json::to_value(&embedded_content).expect("embedded resource re-encodes"),
            embedded
        );

        let sampling = json!({
            "type": "tool_result",
            "toolUseId": "call-7",
            "content": [{"type": "text", "text": "done"}],
            "com.example/sampling": {"retained": true}
        });
        let sampling_content: SamplingContentBlock =
            serde_json::from_value(sampling.clone()).expect("sampling extension decodes");
        assert_eq!(
            serde_json::to_value(&sampling_content).expect("sampling extension re-encodes"),
            sampling
        );

        let subscription: Value = serde_json::from_str(
            r#"{
                "io.modelcontextprotocol/subscriptionId":922337203685477580812345678901234567890
            }"#,
        )
        .expect("large subscription ID wire parses");
        let metadata: OpenMetadata = serde_json::from_value(subscription.clone())
            .expect("arbitrary-precision subscription ID decodes");
        assert_eq!(
            serde_json::to_value(metadata).expect("subscription ID re-encodes"),
            subscription
        );

        let cancellation: Value = serde_json::from_str(
            r#"{
                "method":"notifications/cancelled",
                "params":{"requestId":922337203685477580812345678901234567890}
            }"#,
        )
        .expect("large cancellation ID wire parses");
        FinalCommonTypesSchema::validate(CommonWireDirection::Notification, &cancellation)
            .expect("arbitrary-precision cancellation ID is admitted");
        assert!(matches!(
            serde_json::from_value::<CancellationRequestId>(
                cancellation["params"]["requestId"].clone()
            )
            .expect("large cancellation ID decodes"),
            CancellationRequestId::IntegerExact(value)
                if value.as_str() == "922337203685477580812345678901234567890"
        ));
    }

    #[test]
    fn embedded_resource_accepts_bounded_mime_parameters_and_rejects_missing_values() {
        let accepted = json!({
            "uri": "ui://weather/dashboard",
            "text": "<main>weather</main>",
            "mimeType": "text/html;profile=mcp-app",
        });
        let resource: EmbeddedResourceContents = serde_json::from_value(accepted.clone())
            .expect("the official MCP Apps MIME parameter is valid resource content");
        assert_eq!(
            serde_json::to_value(resource).expect("parameterized MIME type re-encodes"),
            accepted
        );

        let mut missing_value = accepted;
        missing_value["mimeType"] = json!("text/html;profile");
        assert!(
            serde_json::from_value::<EmbeddedResourceContents>(missing_value).is_err(),
            "removing only the MIME parameter value must reject"
        );
    }

    #[test]
    fn prt_02_b_unit_planted_negative() {
        let accepted = json!({
            "_meta": {
                "com.example/future": {"kept": true},
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/protocolVersion": "2026-07-28"
            }
        });
        FinalCommonTypesSchema::validate(CommonWireDirection::Request, &accepted)
            .expect("accepted baseline");
        let baseline = accepted.clone();
        let mut planted = accepted.clone();
        let meta = planted
            .get_mut("_meta")
            .and_then(Value::as_object_mut)
            .expect("metadata object");
        let preserved = meta
            .remove("com.example/future")
            .expect("one valid open key");
        meta.insert("io.modelcontextprotocol/future".to_owned(), preserved);
        assert_eq!(
            FinalCommonTypesSchema::validate(CommonWireDirection::Request, &planted),
            Err(CommonTypeError::Invalid("metadata key"))
        );
        assert_eq!(
            accepted, baseline,
            "the one-key rejection cannot mutate retained wire state"
        );
    }

    #[test]
    fn cancellation_request_id_preserves_integer_lexemes_and_rejects_fractional_values() {
        let accepted: Value = serde_json::from_str(
            r#"{"method":"notifications/cancelled","params":{"requestId":-0}}"#,
        )
        .expect("negative-zero cancellation wire parses");
        FinalCommonTypesSchema::validate(CommonWireDirection::Notification, &accepted)
            .expect("a schema-valid negative-zero cancellation ID is admitted");
        let typed: CancellationRequestId =
            serde_json::from_value(accepted["params"]["requestId"].clone())
                .expect("negative-zero cancellation ID decodes");
        // The pinned serde_json (=1.0.151, arbitrary_precision) normalizes
        // every natively representable number at parse time, so a wire `-0`
        // reaches typed decoding as the integer 0; exact-lexeme retention
        // applies only to integers that overflow the native representations.
        assert!(matches!(&typed, CancellationRequestId::Integer(0)));
        assert_eq!(
            serde_json::to_value(&typed).expect("negative-zero ID re-encodes"),
            serde_json::json!(0),
            "a natively representable cancellation ID round-trips by value"
        );

        let baseline = accepted.clone();
        let mut planted = accepted.clone();
        planted
            .get_mut("params")
            .and_then(Value::as_object_mut)
            .expect("cancellation parameter object")
            .insert(
                "requestId".to_owned(),
                serde_json::from_str("-0.5").expect("fractional JSON value parses"),
            );
        assert_eq!(
            FinalCommonTypesSchema::validate(CommonWireDirection::Notification, &planted),
            Err(CommonTypeError::Invalid("cancellation request ID")),
            "changing only requestId to a fractional value rejects cancellation"
        );
        assert!(
            serde_json::from_value::<CancellationRequestId>(planted["params"]["requestId"].clone())
                .is_err(),
            "typed cancellation ID decoding rejects the same fractional field"
        );
        assert_eq!(
            serde_json::to_value(&typed).expect("accepted ID remains serializable"),
            baseline["params"]["requestId"].clone(),
            "fractional rejection cannot mutate the admitted cancellation ID"
        );
    }

    #[test]
    fn final_sampling_tool_content_round_trips_without_widening_general_content() {
        let wire = json!({
            "type": "tool_result",
            "toolUseId": "call-7",
            "content": [{"type": "text", "text": "done"}],
            "structuredContent": {"ok": true},
            "_meta": {"com.example/cache": "hit"}
        });
        let content: SamplingContentBlock =
            serde_json::from_value(wire.clone()).expect("final tool-result content is admitted");
        assert!(matches!(content, SamplingContentBlock::ToolResult { .. }));
        assert_eq!(
            serde_json::to_value(&content).expect("tool-result re-encodes"),
            wire
        );

        assert!(
            serde_json::from_value::<ContentBlock>(wire).is_err(),
            "sampling-only tool_result never widens the general content union"
        );
    }

    #[test]
    fn final_sampling_tool_result_preserves_absent_and_explicit_null_structured_content() {
        let absent_wire = json!({
            "type": "tool_result",
            "toolUseId": "call-8",
            "content": []
        });
        let absent: SamplingContentBlock =
            serde_json::from_value(absent_wire.clone()).expect("absent structuredContent is valid");
        assert_eq!(
            serde_json::to_value(absent).expect("absent structuredContent re-encodes"),
            absent_wire
        );

        let null_wire = json!({
            "type": "tool_result",
            "toolUseId": "call-8",
            "content": [],
            "structuredContent": null
        });
        let explicit_null: SamplingContentBlock = serde_json::from_value(null_wire.clone())
            .expect("explicit-null structuredContent is a present JSON value");
        assert!(matches!(
            &explicit_null,
            SamplingContentBlock::ToolResult {
                structured_content: Some(Value::Null),
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(explicit_null).expect("explicit null re-encodes"),
            null_wire
        );
    }
}
