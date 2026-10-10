//! JSON-RPC 2.0 message types.

use std::borrow::Cow;
use std::collections::BTreeSet;

use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::common_types::JsonInteger;

/// The JSON-RPC version string. Used as a static reference to avoid allocations.
pub const JSONRPC_VERSION: &str = "2.0";

/// Maximum encoded bytes in one JSON-RPC string ID, including quotes.
pub const MAX_JSONRPC_STRING_ID_ENCODED_BYTES: usize = 256;

/// Default maximum nesting depth for raw JSON admission.
pub const MAX_RAW_JSON_NESTING_DEPTH: usize = 64;
/// Default maximum aggregate object members and array elements for raw JSON admission.
pub const MAX_RAW_JSON_CONTAINER_ENTRIES: usize = 100_000;
/// Maximum encoded bytes in one JSON number token before typed decoding.
pub const MAX_RAW_JSON_NUMBER_BYTES: usize = 4 * 1024;
/// Maximum aggregate encoded number bytes in one admitted JSON document.
pub const MAX_RAW_JSON_AGGREGATE_NUMBER_BYTES: usize = 256 * 1024;
/// Maximum absolute decimal exponent accepted by raw JSON admission.
pub const MAX_RAW_JSON_EXPONENT: usize = 10_000;

/// A stable reason why raw JSON was rejected before typed decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawJsonAdmissionError {
    DocumentTooLarge,
    InvalidUtf8,
    ByteOrderMark,
    InvalidSyntax,
    TopLevelBatch,
    TopLevelNotObject,
    DuplicateObjectMember,
    NestingTooDeep,
    TooManyContainerEntries,
    NumberTooLong,
    TooManyNumberBytes,
    ExponentTooLarge,
    TooManyDecodedStringBytes,
}

impl std::fmt::Display for RawJsonAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::DocumentTooLarge => "JSON document exceeds the configured byte limit",
            Self::InvalidUtf8 => "JSON document is not strict UTF-8",
            Self::ByteOrderMark => "JSON document contains a UTF-8 byte-order mark",
            Self::InvalidSyntax => "invalid JSON syntax during raw admission",
            Self::TopLevelBatch => "JSON-RPC batch arrays are not supported",
            Self::TopLevelNotObject => "JSON-RPC top-level value must be an object",
            Self::DuplicateObjectMember => "duplicate JSON object member",
            Self::NestingTooDeep => "JSON nesting limit exceeded",
            Self::TooManyContainerEntries => "JSON container-entry limit exceeded",
            Self::NumberTooLong => "JSON number-token limit exceeded",
            Self::TooManyNumberBytes => "aggregate JSON number-byte limit exceeded",
            Self::ExponentTooLarge => "JSON exponent limit exceeded",
            Self::TooManyDecodedStringBytes => "decoded JSON string-byte limit exceeded",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RawJsonAdmissionError {}

/// Maximum retained bytes in one redacted raw-admission path segment.
pub const MAX_RAW_JSON_PATH_SEGMENT_BYTES: usize = 64;
/// Maximum rendered segments in one redacted raw-admission path.
pub const MAX_RAW_JSON_PATH_SEGMENTS: usize = 16;

/// Structural policy for the single top-level value a raw document may carry.
///
/// The bounded scanning pass itself is role-neutral: duplicate members,
/// depth, container entries, number lexemes, decoded string bytes, strict
/// UTF-8, and byte-order marks are enforced identically for every consumer.
/// Only the admitted top-level shape differs, so a security document is never
/// classified with JSON-RPC batch vocabulary and a JSON-RPC frame still
/// refuses a batch by name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawJsonTopLevel {
    /// One top-level JSON object. A top-level array is reported as
    /// [`RawJsonAdmissionError::TopLevelBatch`] because JSON-RPC ingress
    /// refuses batches deliberately and by name.
    JsonRpcObject,
    /// One top-level JSON object for a security-bearing document such as
    /// OAuth/OIDC/PRM/CIMD/DCR metadata, a token response, a JWKS or JWK, or
    /// a decoded compact-JWS protected header or claims set. A top-level
    /// array is an ordinary shape violation here, not a JSON-RPC batch.
    SecurityDocumentObject,
    /// One top-level JSON value of any kind, still fully bounded.
    AnyValue,
}

/// One structural step in a redacted raw-admission path.
#[derive(Clone, Debug, Eq, PartialEq)]
enum RawJsonPathSegment {
    Member(String),
    Index(usize),
}

/// A raw admission failure together with the redacted structural path at
/// which the bounded scanner detected it.
///
/// The path names only structure: object member names are reduced to
/// `[A-Za-z0-9_.-]` with every other byte replaced by `*` and the segment
/// truncated to [`MAX_RAW_JSON_PATH_SEGMENT_BYTES`], and array steps carry
/// only their index. No admitted value, string content, or number lexeme is
/// ever reproduced, so a rejection diagnostic cannot leak the document that
/// caused it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawJsonAdmissionFailure {
    error: RawJsonAdmissionError,
    path: String,
}

impl RawJsonAdmissionFailure {
    /// Returns the stable structural reason the document was refused.
    #[must_use]
    pub const fn error(&self) -> RawJsonAdmissionError {
        self.error
    }

    /// Returns the redacted JSON-Pointer-style path of the refusal.
    ///
    /// The whole-document boundary — byte limit, byte-order mark, strict
    /// UTF-8, and top-level shape — reports the empty root path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    const fn at_root(error: RawJsonAdmissionError) -> Self {
        Self {
            error,
            path: String::new(),
        }
    }
}

impl std::fmt::Display for RawJsonAdmissionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            self.error.fmt(formatter)
        } else {
            write!(formatter, "{} at {}", self.error, self.path)
        }
    }
}

impl std::error::Error for RawJsonAdmissionFailure {}

impl From<RawJsonAdmissionFailure> for RawJsonAdmissionError {
    fn from(failure: RawJsonAdmissionFailure) -> Self {
        failure.error
    }
}

/// Admission failure for a complete strict JSON-RPC document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonRpcAdmissionError {
    /// The raw JSON boundary rejected the document before typed decoding.
    Raw(RawJsonAdmissionError),
    /// The raw document was valid JSON but not a valid JSON-RPC envelope.
    InvalidEnvelope,
}

impl std::fmt::Display for JsonRpcAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raw(error) => error.fmt(formatter),
            Self::InvalidEnvelope => formatter.write_str("invalid JSON-RPC envelope"),
        }
    }
}

impl std::error::Error for JsonRpcAdmissionError {}

/// Admit one complete raw JSON-RPC document before any typed decoding.
///
/// The caller chooses the document/body byte bound. The fixed structural
/// limits prevent duplicate-member ambiguity and bound recursive parsing,
/// decoded string bytes, and numeric lexemes before `serde_json` receives the
/// document. Only one top-level object is admitted; JSON-RPC batch arrays are
/// rejected deliberately.
pub fn admit_raw_jsonrpc_document(
    bytes: &[u8],
    document_byte_limit: usize,
) -> Result<(), RawJsonAdmissionError> {
    admit_raw_json_document(bytes, document_byte_limit, RawJsonTopLevel::JsonRpcObject)
        .map_err(RawJsonAdmissionFailure::into)
}

/// Admit one complete raw JSON document before any typed decoding.
///
/// This is the single bounded streaming pass every FastMCP JSON consumer
/// shares. It is deliberately not a JSON-RPC-only serde hook: the caller
/// selects the document byte bound and the admitted top-level shape, and
/// every other rule — duplicate object members at every nesting level,
/// nesting depth, aggregate container entries, per-number and aggregate
/// numeric lexeme bytes, absolute exponent, decoded string bytes, strict
/// UTF-8, and byte-order-mark rejection — is identical for JSON-RPC
/// envelopes and for security-bearing documents such as OAuth/OIDC/PRM/CIMD/
/// DCR metadata, token responses, JWKS and JWK documents, and decoded
/// compact-JWS protected headers and claim sets.
///
/// Refusals carry a redacted structural path so two consumers of the same
/// document receive the same stable diagnostic without either of them
/// reproducing admitted content.
pub fn admit_raw_json_document(
    bytes: &[u8],
    document_byte_limit: usize,
    top_level: RawJsonTopLevel,
) -> Result<(), RawJsonAdmissionFailure> {
    if bytes.len() > document_byte_limit {
        return Err(RawJsonAdmissionFailure::at_root(
            RawJsonAdmissionError::DocumentTooLarge,
        ));
    }
    if bytes.windows(3).any(|window| window == [0xef, 0xbb, 0xbf]) {
        return Err(RawJsonAdmissionFailure::at_root(
            RawJsonAdmissionError::ByteOrderMark,
        ));
    }
    let Ok(input) = std::str::from_utf8(bytes) else {
        return Err(RawJsonAdmissionFailure::at_root(
            RawJsonAdmissionError::InvalidUtf8,
        ));
    };
    let mut scanner = RawJsonScanner::new(input, document_byte_limit);
    scanner.skip_whitespace();
    let scanned = match (scanner.peek(), top_level) {
        (Some(b'{'), _) => scanner.parse_object(0),
        (Some(b'['), RawJsonTopLevel::JsonRpcObject) => {
            return Err(RawJsonAdmissionFailure::at_root(
                RawJsonAdmissionError::TopLevelBatch,
            ));
        }
        (Some(b'['), RawJsonTopLevel::SecurityDocumentObject) => {
            return Err(RawJsonAdmissionFailure::at_root(
                RawJsonAdmissionError::TopLevelNotObject,
            ));
        }
        (Some(_), RawJsonTopLevel::AnyValue) => scanner.parse_value(0),
        _ => {
            return Err(RawJsonAdmissionFailure::at_root(
                RawJsonAdmissionError::TopLevelNotObject,
            ));
        }
    };
    if let Err(error) = scanned {
        return Err(scanner.failure(error));
    }
    scanner.skip_whitespace();
    if scanner.position != scanner.bytes.len() {
        return Err(RawJsonAdmissionFailure::at_root(
            RawJsonAdmissionError::InvalidSyntax,
        ));
    }
    Ok(())
}

/// Decode a complete JSON-RPC message only after raw-document admission.
pub fn decode_strict_jsonrpc_message(
    bytes: &[u8],
    document_byte_limit: usize,
) -> Result<JsonRpcMessage, JsonRpcAdmissionError> {
    admit_raw_jsonrpc_document(bytes, document_byte_limit).map_err(JsonRpcAdmissionError::Raw)?;
    match serde_json::from_slice::<JsonRpcRequest>(bytes) {
        Ok(request) => Ok(JsonRpcMessage::Request(request)),
        Err(_) => serde_json::from_slice::<JsonRpcResponse>(bytes)
            .map(JsonRpcMessage::Response)
            .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope),
    }
}

impl JsonRpcRequest {
    /// Strictly decodes one request while retaining an exact parameter-source
    /// sidecar for method-specific ingress decoding.
    ///
    /// The returned `String`, when present, is the exact JSON source of the
    /// request's `params` member. It is deliberately separate from
    /// [`JsonRpcRequest`] so existing public struct literals and ordinary
    /// typed request APIs retain their established shape. A consumer may only
    /// attach this source to the returned request after comparing its
    /// materialized `params` value; final core dispatch performs that equality
    /// check before it uses the raw source.
    ///
    /// This applies the same bounded raw-document admission as
    /// [`decode_strict_jsonrpc_message`], including duplicate-member
    /// rejection, before producing either the typed request or its sidecar.
    pub fn decode_strict_with_raw_params(
        bytes: &[u8],
        document_byte_limit: usize,
    ) -> Result<(Self, Option<String>), JsonRpcAdmissionError> {
        admit_raw_jsonrpc_document(bytes, document_byte_limit)
            .map_err(JsonRpcAdmissionError::Raw)?;
        let wire =
            JsonRpcRequestRawWire::deserialize(&mut serde_json::Deserializer::from_slice(bytes))
                .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)?;
        let raw_params = wire.params.as_deref().map(RawValue::get).map(str::to_owned);
        let params = raw_params
            .as_deref()
            .map(|source| {
                crate::messages::validate_raw_final_completion_params(&wire.method, source)
                    .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)?;
                serde_json::from_str(source).map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)
            })
            .transpose()?;
        let request = Self {
            jsonrpc: wire.jsonrpc,
            method: wire.method,
            params,
            id: wire.id,
        };
        request
            .validate()
            .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)?;
        Ok((request, raw_params))
    }
}

/// One strictly admitted JSON-RPC response paired with the exact source JSON
/// of its result member.
///
/// `raw_result` is absent for an error response and present even when a success
/// result is the explicit JSON value `null`. It is retained only for local
/// method-specific result decoding; the public [`JsonRpcResponse`] remains
/// unchanged and existing typed transport APIs keep their established shape.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcResponseAdmission {
    response: JsonRpcResponse,
    raw_result: Option<String>,
}

impl JsonRpcResponseAdmission {
    /// Returns the ordinary typed response.
    #[must_use]
    pub const fn response(&self) -> &JsonRpcResponse {
        &self.response
    }

    /// Returns the exact result-member source JSON, including member order and
    /// number lexemes, when the response is successful.
    #[must_use]
    pub fn raw_result(&self) -> Option<&str> {
        self.raw_result.as_deref()
    }

    /// Splits this admission into its typed response and exact result source.
    #[must_use]
    pub fn into_parts(self) -> (JsonRpcResponse, Option<String>) {
        (self.response, self.raw_result)
    }
}

/// Strictly decodes one JSON-RPC response while retaining its exact result
/// member source for the final result algebra.
///
/// This applies the same bounded raw-document admission as
/// [`decode_strict_jsonrpc_message`]. Callers that already decoded a frame may
/// compare the returned typed response with that first decode before attaching
/// `raw_result` to its correlation owner.
pub fn decode_strict_jsonrpc_response(
    bytes: &[u8],
    document_byte_limit: usize,
) -> Result<JsonRpcResponseAdmission, JsonRpcAdmissionError> {
    admit_raw_jsonrpc_document(bytes, document_byte_limit).map_err(JsonRpcAdmissionError::Raw)?;
    let wire =
        JsonRpcResponseRawWire::deserialize(&mut serde_json::Deserializer::from_slice(bytes))
            .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)?;
    let raw_result = wire.result.map(|result| result.get().to_owned());
    let result = raw_result
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)?;
    let response = JsonRpcResponse {
        jsonrpc: wire.jsonrpc,
        result,
        error: wire.error,
        id: wire.id,
    };
    response
        .validate()
        .map_err(|_| JsonRpcAdmissionError::InvalidEnvelope)?;
    Ok(JsonRpcResponseAdmission {
        response,
        raw_result,
    })
}

struct RawJsonScanner<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
    container_entries: usize,
    number_bytes: usize,
    decoded_string_bytes: usize,
    decoded_string_byte_limit: usize,
    path: Vec<RawJsonPathSegment>,
}

impl<'a> RawJsonScanner<'a> {
    fn new(input: &'a str, decoded_string_byte_limit: usize) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            container_entries: 0,
            number_bytes: 0,
            decoded_string_bytes: 0,
            decoded_string_byte_limit,
            path: Vec::new(),
        }
    }

    /// Renders the structural path held at the moment of refusal.
    ///
    /// The scanner unwinds by returning early, so the stack still describes
    /// the failing position when this runs.
    fn failure(&self, error: RawJsonAdmissionError) -> RawJsonAdmissionFailure {
        RawJsonAdmissionFailure {
            error,
            path: redact_raw_json_path(&self.path),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<(), RawJsonAdmissionError> {
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string(false).map(|_| ()),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(RawJsonAdmissionError::InvalidSyntax),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), RawJsonAdmissionError> {
        let nested_depth = self.enter_container(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }

        let mut names = BTreeSet::new();
        loop {
            self.charge_container_entry()?;
            let name = self
                .parse_string(true)?
                .ok_or(RawJsonAdmissionError::InvalidSyntax)?;
            // The member joins the path before the duplicate check so a
            // refusal names the member it refused, and stays on the path for
            // the whole member value so a nested refusal is located exactly.
            self.path.push(RawJsonPathSegment::Member(name.clone()));
            if !names.insert(name) {
                return Err(RawJsonAdmissionError::DuplicateObjectMember);
            }
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            self.skip_whitespace();
            self.parse_value(nested_depth)?;
            self.path.pop();
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), RawJsonAdmissionError> {
        let nested_depth = self.enter_container(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        let mut index = 0_usize;
        loop {
            self.charge_container_entry()?;
            self.path.push(RawJsonPathSegment::Index(index));
            self.parse_value(nested_depth)?;
            self.path.pop();
            index = index.saturating_add(1);
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            self.skip_whitespace();
        }
    }

    fn enter_container(&self, depth: usize) -> Result<usize, RawJsonAdmissionError> {
        let nested_depth = depth
            .checked_add(1)
            .ok_or(RawJsonAdmissionError::NestingTooDeep)?;
        if nested_depth > MAX_RAW_JSON_NESTING_DEPTH {
            Err(RawJsonAdmissionError::NestingTooDeep)
        } else {
            Ok(nested_depth)
        }
    }

    fn charge_container_entry(&mut self) -> Result<(), RawJsonAdmissionError> {
        self.container_entries = self
            .container_entries
            .checked_add(1)
            .ok_or(RawJsonAdmissionError::TooManyContainerEntries)?;
        if self.container_entries > MAX_RAW_JSON_CONTAINER_ENTRIES {
            Err(RawJsonAdmissionError::TooManyContainerEntries)
        } else {
            Ok(())
        }
    }

    fn parse_string(&mut self, capture: bool) -> Result<Option<String>, RawJsonAdmissionError> {
        if !self.consume(b'"') {
            return Err(RawJsonAdmissionError::InvalidSyntax);
        }
        let mut decoded = capture.then(String::new);
        loop {
            let byte = self.peek().ok_or(RawJsonAdmissionError::InvalidSyntax)?;
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(decoded);
                }
                b'\\' => {
                    self.position += 1;
                    let character = self.parse_escape()?;
                    self.charge_string_bytes(character.len_utf8())?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(character);
                    }
                }
                0x00..=0x1f => return Err(RawJsonAdmissionError::InvalidSyntax),
                0x20..=0x7f => {
                    self.position += 1;
                    self.charge_string_bytes(1)?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(char::from(byte));
                    }
                }
                _ => {
                    let character = self.input[self.position..]
                        .chars()
                        .next()
                        .ok_or(RawJsonAdmissionError::InvalidSyntax)?;
                    self.position += character.len_utf8();
                    self.charge_string_bytes(character.len_utf8())?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(character);
                    }
                }
            }
        }
    }

    fn parse_escape(&mut self) -> Result<char, RawJsonAdmissionError> {
        let escape = self.peek().ok_or(RawJsonAdmissionError::InvalidSyntax)?;
        self.position += 1;
        match escape {
            b'"' => Ok('"'),
            b'\\' => Ok('\\'),
            b'/' => Ok('/'),
            b'b' => Ok('\u{0008}'),
            b'f' => Ok('\u{000c}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'u' => self.parse_unicode_escape(),
            _ => Err(RawJsonAdmissionError::InvalidSyntax),
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, RawJsonAdmissionError> {
        let first = self.parse_hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if !self.consume(b'\\') || !self.consume(b'u') {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            let second = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + u32::from(second) - 0xdc00
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(RawJsonAdmissionError::InvalidSyntax);
        } else {
            u32::from(first)
        };
        char::from_u32(scalar).ok_or(RawJsonAdmissionError::InvalidSyntax)
    }

    fn parse_hex_quad(&mut self) -> Result<u16, RawJsonAdmissionError> {
        let end = self
            .position
            .checked_add(4)
            .ok_or(RawJsonAdmissionError::InvalidSyntax)?;
        let digits = self
            .bytes
            .get(self.position..end)
            .ok_or(RawJsonAdmissionError::InvalidSyntax)?;
        let mut value = 0_u16;
        for digit in digits {
            let nibble = match digit {
                b'0'..=b'9' => u16::from(*digit - b'0'),
                b'a'..=b'f' => u16::from(*digit - b'a' + 10),
                b'A'..=b'F' => u16::from(*digit - b'A' + 10),
                _ => return Err(RawJsonAdmissionError::InvalidSyntax),
            };
            value = (value << 4) | nibble;
        }
        self.position = end;
        Ok(value)
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), RawJsonAdmissionError> {
        let end = self
            .position
            .checked_add(literal.len())
            .ok_or(RawJsonAdmissionError::InvalidSyntax)?;
        if self.bytes.get(self.position..end) == Some(literal) {
            self.position = end;
            Ok(())
        } else {
            Err(RawJsonAdmissionError::InvalidSyntax)
        }
    }

    fn parse_number(&mut self) -> Result<(), RawJsonAdmissionError> {
        let start = self.position;
        self.consume(b'-');
        match self.peek() {
            Some(b'0') => {
                self.position += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(RawJsonAdmissionError::InvalidSyntax);
                }
            }
            Some(b'1'..=b'9') => {
                self.position += 1;
                self.consume_digits();
            }
            _ => return Err(RawJsonAdmissionError::InvalidSyntax),
        }
        if self.consume(b'.') {
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            self.consume_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            let exponent_start = self.position;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(RawJsonAdmissionError::InvalidSyntax);
            }
            self.consume_digits();
            if exponent_exceeds_raw_limit(&self.bytes[exponent_start..self.position]) {
                return Err(RawJsonAdmissionError::ExponentTooLarge);
            }
        }
        let length = self.position - start;
        if length > MAX_RAW_JSON_NUMBER_BYTES {
            return Err(RawJsonAdmissionError::NumberTooLong);
        }
        self.number_bytes = self
            .number_bytes
            .checked_add(length)
            .ok_or(RawJsonAdmissionError::TooManyNumberBytes)?;
        if self.number_bytes > MAX_RAW_JSON_AGGREGATE_NUMBER_BYTES {
            Err(RawJsonAdmissionError::TooManyNumberBytes)
        } else {
            Ok(())
        }
    }

    fn consume_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
    }
    fn charge_string_bytes(&mut self, bytes: usize) -> Result<(), RawJsonAdmissionError> {
        self.decoded_string_bytes = self
            .decoded_string_bytes
            .checked_add(bytes)
            .ok_or(RawJsonAdmissionError::TooManyDecodedStringBytes)?;
        if self.decoded_string_bytes > self.decoded_string_byte_limit {
            Err(RawJsonAdmissionError::TooManyDecodedStringBytes)
        } else {
            Ok(())
        }
    }
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }
    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }
}

/// Renders a scanner path as a redacted JSON-Pointer-style string.
///
/// Only structure survives. An object member keeps `[A-Za-z0-9_.-]` and every
/// other byte becomes `*`, so an attacker-chosen member name can neither
/// smuggle content into a log line nor change the shape of the diagnostic.
/// Segment and path lengths are bounded, and array steps carry only indices.
fn redact_raw_json_path(path: &[RawJsonPathSegment]) -> String {
    let mut rendered = String::new();
    for segment in path.iter().take(MAX_RAW_JSON_PATH_SEGMENTS) {
        rendered.push('/');
        match segment {
            RawJsonPathSegment::Member(name) => {
                // `retained` counts CHARACTERS and is compared against a BYTE
                // budget. That is correct here only because every branch below
                // pushes exactly one ASCII byte: the first is guarded to ASCII
                // alphanumerics plus `_ . -`, and the second is a literal '*'.
                // So character count and byte count are equal for this loop.
                //
                // The equivalence is invisible from the constant's name and
                // depends on the sanitiser immediately below. If either branch
                // ever pushes a multi-byte character -- a Unicode replacement
                // or an ellipsis, say -- this stops bounding bytes and the
                // truncation boundary moves silently. Count bytes explicitly if
                // that happens; do not widen the budget.
                for (retained, character) in name.chars().enumerate() {
                    if retained >= MAX_RAW_JSON_PATH_SEGMENT_BYTES {
                        rendered.push('~');
                        break;
                    }
                    if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
                        rendered.push(character);
                    } else {
                        rendered.push('*');
                    }
                }
            }
            RawJsonPathSegment::Index(index) => {
                rendered.push_str(&index.to_string());
            }
        }
    }
    if path.len() > MAX_RAW_JSON_PATH_SEGMENTS {
        rendered.push_str("/~");
    }
    rendered
}

fn exponent_exceeds_raw_limit(digits: &[u8]) -> bool {
    let first_significant = digits
        .iter()
        .position(|digit| *digit != b'0')
        .unwrap_or(digits.len());
    let significant = &digits[first_significant..];
    significant.len() > 5
        || significant.iter().fold(0_usize, |value, digit| {
            value * 10 + usize::from(*digit - b'0')
        }) > MAX_RAW_JSON_EXPONENT
}

/// Serializes the jsonrpc version field.
fn serialize_jsonrpc_version<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if value == JSONRPC_VERSION {
        serializer.serialize_str(JSONRPC_VERSION)
    } else {
        Err(S::Error::custom("jsonrpc must be exactly \"2.0\""))
    }
}

/// Deserializes the required JSON-RPC version, rejecting every value but
/// exactly `"2.0"`.
fn deserialize_jsonrpc_version<'de, D>(deserializer: D) -> Result<Cow<'static, str>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Cow<'de, str> = Cow::deserialize(deserializer)?;
    if s == JSONRPC_VERSION {
        Ok(Cow::Borrowed(JSONRPC_VERSION))
    } else {
        Err(D::Error::custom("jsonrpc must be exactly \"2.0\""))
    }
}

/// JSON-RPC request ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    /// Integer ID.
    Number(i64),
    /// An arbitrary-precision mathematical-integer ID preserving its admitted
    /// JSON number lexeme for an exact response echo.
    Integer(String),
    /// String ID.
    String(String),
}

/// Canonical map/registry key for a JSON-RPC request ID.
///
/// Numeric spellings are normalized by exact mathematical value; string IDs
/// remain byte-for-byte distinct from numeric IDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CorrelationKey {
    /// A string request ID, retained byte-for-byte.
    String(String),
    /// A canonical decimal mathematical-integer value.
    Integer(String),
}

impl RequestId {
    /// Verifies that this ID can be represented within the JSON-RPC wire
    /// limits enforced by this crate.
    ///
    /// # Errors
    ///
    /// Returns an error for a string ID whose canonical JSON encoding exceeds
    /// [`MAX_JSONRPC_STRING_ID_ENCODED_BYTES`]. Raw decoders must additionally
    /// enforce the byte length of the received token before escape decoding.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::String(value)
                if encoded_json_string_len(value) > MAX_JSONRPC_STRING_ID_ENCODED_BYTES =>
            {
                return Err("JSON-RPC string id exceeds byte limit");
            }
            Self::Integer(lexeme) if !is_mathematical_integer(lexeme) => {
                return Err("JSON-RPC numeric id must be a mathematical integer");
            }
            _ => {}
        }
        Ok(())
    }

    /// Produces the canonical key used by request registries and correlation.
    pub fn correlation_key(&self) -> Result<CorrelationKey, &'static str> {
        self.validate()?;
        match self {
            Self::Number(value) => Ok(CorrelationKey::Integer(value.to_string())),
            Self::Integer(lexeme) => Ok(CorrelationKey::Integer(canonical_integer_lexeme(lexeme))),
            Self::String(value) => Ok(CorrelationKey::String(value.clone())),
        }
    }

    /// Returns whether two wire IDs identify the same JSON-RPC request.
    ///
    /// Numeric spellings compare by exact mathematical-integer value, while a
    /// string remains distinct from every numeric ID.
    #[must_use]
    pub fn correlates_with(&self, other: &Self) -> bool {
        matches!(
            (self.correlation_key(), other.correlation_key()),
            (Ok(left), Ok(right)) if left == right
        )
    }
}

impl Serialize for RequestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        match self {
            Self::Number(number) => serializer.serialize_i64(*number),
            Self::Integer(lexeme) => serde_json::from_str::<serde_json::Number>(lexeme)
                .map_err(S::Error::custom)?
                .serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::Number(number) => {
                let lexeme = number.to_string();
                if !lexeme.contains(['.', 'e', 'E'])
                    && lexeme != "-0"
                    && let Ok(value) = lexeme.parse::<i64>()
                {
                    Ok(RequestId::Number(value))
                } else if is_mathematical_integer(&lexeme) {
                    Ok(RequestId::Integer(lexeme))
                } else {
                    Err(D::Error::custom(
                        "JSON-RPC numeric id must be a mathematical integer",
                    ))
                }
            }
            Value::String(value) => {
                if encoded_json_string_len(&value) > MAX_JSONRPC_STRING_ID_ENCODED_BYTES {
                    return Err(D::Error::custom("JSON-RPC string id exceeds byte limit"));
                }
                Ok(RequestId::String(value))
            }
            _ => Err(D::Error::custom(
                "JSON-RPC id must be a string or mathematical integer",
            )),
        }
    }
}

fn is_mathematical_integer(lexeme: &str) -> bool {
    if lexeme.len() > MAX_RAW_JSON_NUMBER_BYTES {
        return false;
    }
    let bytes = lexeme.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'-')));
    if index == bytes.len() {
        return false;
    }
    let integer_start = index;
    if bytes.get(index) == Some(&b'0') {
        index += 1;
        if matches!(bytes.get(index), Some(b'0'..=b'9')) {
            return false;
        }
    } else if matches!(bytes.get(index), Some(b'1'..=b'9')) {
        index += 1;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
    } else {
        return false;
    }
    let mut fraction_digits = 0_usize;
    let mut trailing_zeroes = 0_usize;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
        fraction_digits = index - fraction_start;
        if fraction_digits == 0 {
            return false;
        }
    }
    let coefficient_end = index;
    let coefficient = &bytes[integer_start..coefficient_end];
    for digit in coefficient.iter().rev() {
        if *digit == b'0' {
            trailing_zeroes += 1;
        } else if *digit != b'.' {
            break;
        }
    }
    let exponent = if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        let negative = if bytes.get(index) == Some(&b'-') {
            index += 1;
            true
        } else {
            if bytes.get(index) == Some(&b'+') {
                index += 1;
            }
            false
        };
        let exponent_start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
        if index == exponent_start || index != bytes.len() {
            return false;
        }
        let magnitude = std::str::from_utf8(&bytes[exponent_start..index])
            .ok()
            .and_then(|value| value.parse::<i64>().ok());
        match magnitude {
            Some(value) if value <= MAX_RAW_JSON_EXPONENT as i64 && negative => -value,
            Some(value) if value <= MAX_RAW_JSON_EXPONENT as i64 => value,
            _ => return false,
        }
    } else {
        if index != bytes.len() {
            return false;
        }
        0
    };
    let scale = i64::try_from(fraction_digits).unwrap_or(i64::MAX) - exponent;
    scale <= 0
        || usize::try_from(scale).is_ok_and(|required_zeroes| trailing_zeroes >= required_zeroes)
}

fn canonical_integer_lexeme(lexeme: &str) -> String {
    debug_assert!(is_mathematical_integer(lexeme));
    let bytes = lexeme.as_bytes();
    let negative = bytes.first() == Some(&b'-');
    let unsigned = if negative { &lexeme[1..] } else { lexeme };
    let (coefficient, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().unwrap_or(0),
        ),
        None => (unsigned, 0),
    };
    let (whole, fraction) = coefficient.split_once('.').unwrap_or((coefficient, ""));
    let mut digits = format!("{whole}{fraction}");
    let leading = digits.bytes().take_while(|digit| *digit == b'0').count();
    digits.drain(..leading);
    if digits.is_empty() {
        return "0".to_owned();
    }
    let scale = i64::try_from(fraction.len()).unwrap_or(i64::MAX) - exponent;
    if scale > 0 {
        let removable = usize::try_from(scale).unwrap_or(usize::MAX);
        let retained = digits.len().saturating_sub(removable);
        digits.truncate(retained);
    } else {
        let zeroes = usize::try_from(scale.unsigned_abs()).unwrap_or(usize::MAX);
        digits.extend(std::iter::repeat_n('0', zeroes));
    }
    if negative {
        format!("-{digits}")
    } else {
        digits
    }
}

fn encoded_json_string_len(value: &str) -> usize {
    value.chars().fold(2_usize, |length, character| {
        let encoded = match character {
            '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        length.saturating_add(encoded)
    })
}

fn deserialize_request_id<'de, D>(deserializer: D) -> Result<Option<RequestId>, D::Error>
where
    D: Deserializer<'de>,
{
    RequestId::deserialize(deserializer).map(Some)
}

impl From<i64> for RequestId {
    fn from(id: i64) -> Self {
        RequestId::Number(id)
    }
}

impl From<String> for RequestId {
    fn from(id: String) -> Self {
        RequestId::String(id)
    }
}

impl From<&str> for RequestId {
    fn from(id: &str) -> Self {
        RequestId::String(id.to_owned())
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::Integer(lexeme) => f.write_str(lexeme),
            RequestId::String(s) => write!(f, "{s}"),
        }
    }
}

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcRequest {
    /// Protocol version (always "2.0").
    #[serde(
        serialize_with = "serialize_jsonrpc_version",
        deserialize_with = "deserialize_jsonrpc_version"
    )]
    pub jsonrpc: Cow<'static, str>,
    /// Method name.
    pub method: String,
    /// Request parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Request ID (absent for notifications).
    ///
    /// An explicit JSON `null` is rejected instead of being conflated with an
    /// absent member. Notifications omit `id` entirely.
    #[serde(
        default,
        deserialize_with = "deserialize_request_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<RequestId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcRequestRawWire<'a> {
    #[serde(deserialize_with = "deserialize_jsonrpc_version")]
    jsonrpc: Cow<'static, str>,
    method: String,
    #[serde(borrow, default)]
    params: Option<Cow<'a, RawValue>>,
    #[serde(default, deserialize_with = "deserialize_request_id")]
    id: Option<RequestId>,
}

impl<'de> Deserialize<'de> for JsonRpcRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = JsonRpcRequestRawWire::deserialize(deserializer)?;
        let params = wire
            .params
            .as_deref()
            .map(RawValue::get)
            .map(|source| {
                crate::messages::validate_raw_final_completion_params(&wire.method, source)
                    .map_err(D::Error::custom)?;
                serde_json::from_str(source).map_err(D::Error::custom)
            })
            .transpose()?;

        Ok(Self {
            jsonrpc: wire.jsonrpc,
            method: wire.method,
            params,
            id: wire.id,
        })
    }
}

impl JsonRpcRequest {
    /// Creates a new request with the given method and parameters.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>, id: impl Into<RequestId>) -> Self {
        Self {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            method: method.into(),
            params,
            id: Some(id.into()),
        }
    }

    /// Creates a notification (request without ID).
    #[must_use]
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            method: method.into(),
            params,
            id: None,
        }
    }

    /// Creates the MCP lifecycle `notifications/initialized` notification.
    ///
    /// Uses the spec-correct method name (`notifications/initialized`), avoiding
    /// the bare `initialized` spelling that compliant servers do not route as the
    /// lifecycle ack.
    #[must_use]
    pub fn initialized_notification() -> Self {
        Self::notification(crate::methods::NOTIFICATIONS_INITIALIZED, None)
    }

    /// Returns true if this is a notification (no ID).
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// Verifies invariants that can otherwise be bypassed by constructing or
    /// mutating this public protocol type directly.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-standard protocol version or invalid ID.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err("jsonrpc must be exactly \"2.0\"");
        }
        if let Some(id) = &self.id {
            id.validate()?;
        }
        Ok(())
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code retained without an implementation-width bound.
    pub code: JsonInteger,
    /// Error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Immutable local endpoint role for raw JSON-RPC ingress disposition.
///
/// The role is chosen by local transport construction. It is deliberately a
/// closed value rather than a peer-provided header/body setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonRpcEndpointRole {
    /// This endpoint receives client-to-server JSON-RPC traffic.
    ServerIngress,
    /// This endpoint receives server-to-client JSON-RPC traffic.
    ClientIngress,
}

/// The direction attached by the local transport to a decoded message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonRpcMessageDirection {
    /// Client-to-server traffic.
    ClientToServer,
    /// Server-to-client traffic.
    ServerToClient,
}

/// Transport ownership for a client-ingress raw protocol failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientIngressFailureScope {
    /// The malformed body belongs to one request/response exchange.
    OwningExchange,
    /// The malformed body arrived on a multiplexed/shared channel.
    SharedChannel,
}

/// An error response that is deliberately uncorrelated and omits `id`.
///
/// It is distinct from [`JsonRpcResponse`], so safe code cannot accidentally
/// use an absent ID as an ordinary response correlation key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UncorrelatedJsonRpcErrorResponse {
    #[serde(
        serialize_with = "serialize_jsonrpc_version",
        deserialize_with = "deserialize_jsonrpc_version"
    )]
    jsonrpc: Cow<'static, str>,
    error: JsonRpcError,
}

impl UncorrelatedJsonRpcErrorResponse {
    fn parse_or_invalid_request(message: impl Into<String>, code: i32) -> Self {
        Self {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            error: JsonRpcError {
                code: code.into(),
                message: message.into(),
                data: None,
            },
        }
    }

    /// Returns the error payload without exposing an ID-bearing response.
    #[must_use]
    pub fn error(&self) -> &JsonRpcError {
        &self.error
    }
}

/// Role-aware disposition of a raw malformed JSON-RPC document.
#[derive(Debug, Clone, PartialEq)]
pub enum RawJsonRpcDisposition {
    /// Server ingress can emit an error correlated to the one readable ID.
    CorrelatedError(JsonRpcResponse),
    /// Server ingress can emit an explicitly uncorrelated parse/invalid error.
    UncorrelatedError(UncorrelatedJsonRpcErrorResponse),
    /// Client ingress emits no JSON-RPC response and fails only its owning exchange.
    ClientOwningFailure,
    /// Client ingress emits no JSON-RPC response and reports a shared-channel failure.
    ClientSharedChannelFailure,
    /// The direction is not an ingress path for this endpoint and emits nothing.
    NoAction,
}

/// Convert a raw admission failure into an endpoint-safe disposition.
///
/// A valid request ID is echoed only at server ingress for client-to-server
/// traffic. Client ingress never obtains a response-emitting branch.
#[must_use]
pub fn dispose_raw_jsonrpc_failure(
    role: JsonRpcEndpointRole,
    direction: JsonRpcMessageDirection,
    readable_id: Option<RequestId>,
    failure_scope: ClientIngressFailureScope,
) -> RawJsonRpcDisposition {
    match (role, direction) {
        (JsonRpcEndpointRole::ServerIngress, JsonRpcMessageDirection::ClientToServer) => {
            if let Some(id) = readable_id {
                RawJsonRpcDisposition::CorrelatedError(JsonRpcResponse::error(
                    Some(id),
                    JsonRpcError {
                        code: (-32600).into(),
                        message: "Invalid Request".to_owned(),
                        data: None,
                    },
                ))
            } else {
                RawJsonRpcDisposition::UncorrelatedError(
                    UncorrelatedJsonRpcErrorResponse::parse_or_invalid_request(
                        "Parse error",
                        -32700,
                    ),
                )
            }
        }
        (JsonRpcEndpointRole::ClientIngress, JsonRpcMessageDirection::ServerToClient) => {
            match failure_scope {
                ClientIngressFailureScope::OwningExchange => {
                    RawJsonRpcDisposition::ClientOwningFailure
                }
                ClientIngressFailureScope::SharedChannel => {
                    RawJsonRpcDisposition::ClientSharedChannelFailure
                }
            }
        }
        _ => RawJsonRpcDisposition::NoAction,
    }
}

impl From<fastmcp_core::McpError> for JsonRpcError {
    fn from(err: fastmcp_core::McpError) -> Self {
        Self {
            code: err.code.into(),
            message: err.message,
            data: err.data,
        }
    }
}

fn deserialize_response_result<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn deserialize_response_error<'de, D>(deserializer: D) -> Result<Option<JsonRpcError>, D::Error>
where
    D: Deserializer<'de>,
{
    JsonRpcError::deserialize(deserializer).map(Some)
}

fn deserialize_raw_response_result<'de, D>(
    deserializer: D,
) -> Result<Option<Box<RawValue>>, D::Error>
where
    D: Deserializer<'de>,
{
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcResponseRawWire {
    #[serde(deserialize_with = "deserialize_jsonrpc_version")]
    jsonrpc: Cow<'static, str>,
    #[serde(default, deserialize_with = "deserialize_raw_response_result")]
    result: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "deserialize_response_error")]
    error: Option<JsonRpcError>,
    #[serde(default, deserialize_with = "deserialize_request_id")]
    id: Option<RequestId>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcResponseWire {
    /// Protocol version (always "2.0").
    #[serde(
        serialize_with = "serialize_jsonrpc_version",
        deserialize_with = "deserialize_jsonrpc_version"
    )]
    jsonrpc: Cow<'static, str>,
    #[serde(
        default,
        deserialize_with = "deserialize_response_result",
        skip_serializing_if = "Option::is_none"
    )]
    result: Option<Value>,
    #[serde(
        default,
        deserialize_with = "deserialize_response_error",
        skip_serializing_if = "Option::is_none"
    )]
    error: Option<JsonRpcError>,
    #[serde(
        default,
        deserialize_with = "deserialize_request_id",
        skip_serializing_if = "Option::is_none"
    )]
    id: Option<RequestId>,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcResponse {
    /// Protocol version (always "2.0").
    pub jsonrpc: Cow<'static, str>,
    /// Result (present on success, including an explicit JSON `null`).
    pub result: Option<Value>,
    /// Error (present on failure).
    pub error: Option<JsonRpcError>,
    /// Request ID this is responding to.
    pub id: Option<RequestId>,
}

impl Serialize for JsonRpcResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;

        JsonRpcResponseWire {
            jsonrpc: self.jsonrpc.clone(),
            result: self.result.clone(),
            error: self.error.clone(),
            id: self.id.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonRpcResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = JsonRpcResponseWire::deserialize(deserializer)?;
        let response = Self {
            jsonrpc: wire.jsonrpc,
            result: wire.result,
            error: wire.error,
            id: wire.id,
        };
        response.validate().map_err(D::Error::custom)?;
        Ok(response)
    }
}

impl JsonRpcResponse {
    /// Creates a success response.
    #[must_use]
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            result: Some(result),
            error: None,
            id: Some(id),
        }
    }

    /// Creates an error response.
    #[must_use]
    pub fn error(id: Option<RequestId>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            result: None,
            error: Some(error),
            id,
        }
    }

    /// Returns true if this is an error response.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Verifies invariants that can otherwise be bypassed by constructing or
    /// mutating this public protocol type directly.
    ///
    /// # Errors
    ///
    /// Returns an error unless the protocol version is exact, exactly one
    /// outcome member is present, every ID is valid, and success is correlated
    /// to a request ID.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err("jsonrpc must be exactly \"2.0\"");
        }
        if self.result.is_some() == self.error.is_some() {
            return Err("JSON-RPC response must contain exactly one of result or error");
        }
        if self.result.is_some() && self.id.is_none() {
            return Err("JSON-RPC success response must contain an id");
        }
        if let Some(id) = &self.id {
            id.validate()?;
        }
        Ok(())
    }
}

/// A JSON-RPC message (request, response, or notification).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// A request or notification.
    Request(JsonRpcRequest),
    /// A response.
    Response(JsonRpcResponse),
}

impl JsonRpcMessage {
    /// Verifies the contained request or response invariants.
    ///
    /// Typed transports that do not serialize through [`serde_json`] should
    /// call this before accepting a message.
    ///
    /// # Errors
    ///
    /// Returns the first violated request or response invariant.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Request(request) => request.validate(),
            Self::Response(response) => response.validate(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct AdmittedFrames {
        bytes: Vec<Vec<u8>>,
    }

    fn admit_frame(
        state: &mut AdmittedFrames,
        frame: &[u8],
    ) -> Result<JsonRpcMessage, JsonRpcAdmissionError> {
        let message = decode_strict_jsonrpc_message(frame, 4 * 1024)?;
        state.bytes.push(frame.to_vec());
        Ok(message)
    }

    #[test]
    fn request_and_response_envelopes_decode() {
        let request = br#"{"jsonrpc":"2.0","method":"tools/list","id":42}"#;
        let notification = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let success = br#"{"jsonrpc":"2.0","result":null,"id":"request-42"}"#;
        let error = br#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"missing"},"id":42}"#;

        assert!(matches!(
            decode_strict_jsonrpc_message(request, 4 * 1024),
            Ok(JsonRpcMessage::Request(JsonRpcRequest {
                id: Some(RequestId::Number(42)),
                ..
            }))
        ));
        assert!(matches!(
            decode_strict_jsonrpc_message(notification, 4 * 1024),
            Ok(JsonRpcMessage::Request(JsonRpcRequest { id: None, .. }))
        ));
        assert!(matches!(
            decode_strict_jsonrpc_message(success, 4 * 1024),
            Ok(JsonRpcMessage::Response(JsonRpcResponse {
                result: Some(Value::Null),
                error: None,
                ..
            }))
        ));
        assert!(matches!(
            decode_strict_jsonrpc_message(error, 4 * 1024),
            Ok(JsonRpcMessage::Response(JsonRpcResponse {
                result: None,
                error: Some(_),
                ..
            }))
        ));

        assert!(matches!(
            dispose_raw_jsonrpc_failure(
                JsonRpcEndpointRole::ServerIngress,
                JsonRpcMessageDirection::ClientToServer,
                Some(RequestId::String("known".to_owned())),
                ClientIngressFailureScope::OwningExchange,
            ),
            RawJsonRpcDisposition::CorrelatedError(JsonRpcResponse {
                id: Some(RequestId::String(_)),
                ..
            })
        ));
        assert!(matches!(
            dispose_raw_jsonrpc_failure(
                JsonRpcEndpointRole::ClientIngress,
                JsonRpcMessageDirection::ServerToClient,
                None,
                ClientIngressFailureScope::SharedChannel,
            ),
            RawJsonRpcDisposition::ClientSharedChannelFailure
        ));
    }

    #[test]
    fn strict_response_admission_retains_exact_result_source() {
        let frame = br#"{"jsonrpc":"2.0","result":{"zeta":1.20e+4,"alpha":{"second":2,"first":1},"middle":null},"id":73}"#;
        let admission = decode_strict_jsonrpc_response(frame, 4 * 1024)
            .expect("strict response admission accepts the bounded frame");
        assert_eq!(admission.response().id, Some(RequestId::Number(73)));
        assert_eq!(
            admission.raw_result(),
            Some(r#"{"zeta":1.20e+4,"alpha":{"second":2,"first":1},"middle":null}"#),
            "the exact result substring retains top-level and nested member order plus number lexemes"
        );

        let explicit_null =
            decode_strict_jsonrpc_response(br#"{"jsonrpc":"2.0","result":null,"id":74}"#, 4 * 1024)
                .expect("an explicit null success remains present");
        assert_eq!(explicit_null.raw_result(), Some("null"));

        let error = decode_strict_jsonrpc_response(
            br#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"missing"},"id":75}"#,
            4 * 1024,
        )
        .expect("an error response has no result source");
        assert!(error.raw_result().is_none());
    }

    #[test]
    fn raw_admission_accepts_public_request_surface() {
        let frame = br#"{"jsonrpc":"2.0","method":"tools/list","id":"public-request"}"#;
        let mut state = AdmittedFrames::default();
        let admitted = admit_frame(&mut state, frame)
            .expect("the protocol-owned raw gate admits a strict public request envelope");
        assert!(matches!(admitted, JsonRpcMessage::Request(_)));
        assert_eq!(state.bytes, vec![frame.to_vec()]);
        assert!(matches!(
            dispose_raw_jsonrpc_failure(
                JsonRpcEndpointRole::ClientIngress,
                JsonRpcMessageDirection::ServerToClient,
                None,
                ClientIngressFailureScope::OwningExchange,
            ),
            RawJsonRpcDisposition::ClientOwningFailure
        ));
    }

    #[test]
    fn duplicate_envelope_member_is_rejected_without_state_change() {
        let baseline = br#"{"jsonrpc":"2.0","method":"tools/list","id":42}"#;
        let planted = br#"{"jsonrpc":"2.0","method":"tools/list","id":42,"id":42}"#;
        let mut state = AdmittedFrames::default();
        admit_frame(&mut state, baseline).expect("the unmodified envelope is admitted");
        let state_before = state.clone();

        assert!(
            matches!(
                admit_frame(&mut state, planted),
                Err(JsonRpcAdmissionError::Raw(
                    RawJsonAdmissionError::DuplicateObjectMember
                ))
            ),
            "changing only the second id member must reach production raw admission"
        );
        assert_eq!(
            state, state_before,
            "rejected raw JSON cannot mutate admitted state"
        );
    }

    #[test]
    fn bom_is_rejected_without_state_change() {
        let baseline = br#"{"jsonrpc":"2.0","method":"tools/list","id":"public-request"}"#;
        let mut planted = baseline.to_vec();
        planted.splice(0..0, [0xef, 0xbb, 0xbf]);
        let mut state = AdmittedFrames::default();
        admit_frame(&mut state, baseline).expect("the baseline is admitted");
        let state_before = state.clone();

        assert!(
            matches!(
                admit_frame(&mut state, &planted),
                Err(JsonRpcAdmissionError::Raw(
                    RawJsonAdmissionError::ByteOrderMark
                ))
            ),
            "inserting only a UTF-8 BOM must reach the typed raw-admission refusal"
        );
        assert_eq!(
            state, state_before,
            "rejected raw bytes leave admitted state unchanged"
        );
    }

    #[test]
    fn request_id_correlation_key_normalizes_numeric_aliases() {
        let numeric = RequestId::Number(1);
        let string = RequestId::String("1".to_owned());
        assert_ne!(
            numeric, string,
            "string and numeric request IDs are disjoint"
        );
        assert_eq!(
            numeric.correlation_key().expect("valid numeric ID"),
            RequestId::Integer("1.0".to_owned())
                .correlation_key()
                .expect("valid mathematical integer ID"),
            "numeric aliases share one exact mathematical correlation key"
        );
        assert_eq!(
            numeric.correlation_key().expect("valid numeric ID"),
            RequestId::Integer("1e0".to_owned())
                .correlation_key()
                .expect("valid mathematical integer ID"),
            "exponent-form integer aliases share one exact mathematical correlation key"
        );
        assert_ne!(
            numeric.correlation_key().expect("valid numeric ID"),
            string.correlation_key().expect("valid string ID"),
            "a string ID never aliases its numeric spelling"
        );
        assert!(numeric.correlates_with(&RequestId::Integer("1.0".to_owned())));
        assert!(numeric.correlates_with(&RequestId::Integer("1e0".to_owned())));
        assert!(!numeric.correlates_with(&string));
        assert_eq!(
            JsonRpcResponse::success(numeric.clone(), Value::Null).id,
            Some(numeric),
            "a correlated success preserves its accepted request ID"
        );
        let large = "922337203685477580812345678901234567890";
        let raw = format!(r#"{{"jsonrpc":"2.0","method":"tools/list","id":{large}}}"#);
        let decoded = decode_strict_jsonrpc_message(raw.as_bytes(), 4 * 1024)
            .expect("an arbitrary-precision mathematical integer is admitted");
        let JsonRpcMessage::Request(request) = decoded else {
            panic!("the admitted envelope remains a request");
        };
        assert_eq!(request.id, Some(RequestId::Integer(large.to_owned())));
        let echoed = JsonRpcResponse::success(
            request
                .id
                .expect("admitted request keeps its original ID lexeme"),
            Value::Null,
        );
        assert!(
            serde_json::to_string(&echoed)
                .expect("the exact admitted ID can be echoed")
                .contains(large),
            "response serialization preserves the accepted arbitrary-precision ID lexeme"
        );
        assert!(matches!(
            dispose_raw_jsonrpc_failure(
                JsonRpcEndpointRole::ServerIngress,
                JsonRpcMessageDirection::ClientToServer,
                Some(RequestId::Number(9)),
                ClientIngressFailureScope::OwningExchange,
            ),
            RawJsonRpcDisposition::CorrelatedError(JsonRpcResponse {
                id: Some(RequestId::Number(9)),
                ..
            })
        ));
    }

    #[test]
    fn request_id_fractional_lexeme_is_rejected_before_correlation() {
        let baseline = br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let planted = br#"{"jsonrpc":"2.0","method":"tools/list","id":1.5}"#;
        let mut state = AdmittedFrames::default();
        admit_frame(&mut state, baseline).expect("integer request ID is admitted");
        let state_before = state.clone();

        assert!(
            matches!(
                admit_frame(&mut state, planted),
                Err(JsonRpcAdmissionError::InvalidEnvelope)
            ),
            "changing only the ID to a fractional number must be rejected"
        );
        assert_eq!(
            state, state_before,
            "a rejected fractional ID cannot claim a correlation slot"
        );
    }

    #[test]
    fn duplicate_nested_member_is_rejected_without_state_change() {
        let baseline = br#"{"jsonrpc":"2.0","method":"tools/list","params":{"cursor":"a"}}"#;
        let planted =
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"cursor":"a","cursor":"b"}}"#;
        let mut state = AdmittedFrames::default();
        admit_frame(&mut state, baseline).expect("baseline nested object is admitted");
        let state_before = state.clone();

        assert!(
            matches!(
                admit_frame(&mut state, planted),
                Err(JsonRpcAdmissionError::Raw(
                    RawJsonAdmissionError::DuplicateObjectMember
                ))
            ),
            "a one-member duplicate must fail before typed params decoding"
        );
        assert_eq!(
            state, state_before,
            "duplicate raw members cannot mutate admitted state"
        );
    }

    #[test]
    fn top_level_batches_are_rejected_without_state_change() {
        let baseline = br#"{"jsonrpc":"2.0","method":"tools/list"}"#;
        let array_of_one = br#"[{"jsonrpc":"2.0","method":"tools/list"}]"#;
        let mixed_array = br#"[{"jsonrpc":"2.0","method":"tools/list"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#;
        let mut state = AdmittedFrames::default();
        admit_frame(&mut state, baseline).expect("one top-level request object is admitted");
        let state_before = state.clone();

        for planted in [array_of_one.as_slice(), mixed_array.as_slice()] {
            assert!(
                matches!(
                    admit_frame(&mut state, planted),
                    Err(JsonRpcAdmissionError::Raw(
                        RawJsonAdmissionError::TopLevelBatch
                    ))
                ),
                "a top-level batch fails before envelope construction"
            );
            assert_eq!(
                state, state_before,
                "rejected batch traffic has no admitted state effect"
            );
        }
    }

    // ========================================================================
    // RequestId Tests
    // ========================================================================

    #[test]
    fn request_id_number_serialization() {
        let id = RequestId::Number(42);
        let value = serde_json::to_value(&id).expect("serialize");
        assert_eq!(value, 42);
    }

    #[test]
    fn request_id_string_serialization() {
        let id = RequestId::String("req-1".to_string());
        let value = serde_json::to_value(&id).expect("serialize");
        assert_eq!(value, "req-1");
    }

    #[test]
    fn request_id_number_deserialization() {
        let id: RequestId = serde_json::from_value(json!(99)).expect("deserialize");
        assert_eq!(id, RequestId::Number(99));
    }

    #[test]
    fn request_id_string_deserialization() {
        let id: RequestId = serde_json::from_value(json!("abc")).expect("deserialize");
        assert_eq!(id, RequestId::String("abc".to_string()));
    }

    #[test]
    fn request_id_string_enforces_encoded_byte_limit() {
        let exact = "a".repeat(MAX_JSONRPC_STRING_ID_ENCODED_BYTES - 2);
        let too_long = "a".repeat(MAX_JSONRPC_STRING_ID_ENCODED_BYTES - 1);
        let exact_json = format!("\"{exact}\"");
        let too_long_json = format!("\"{too_long}\"");

        assert!(serde_json::from_str::<RequestId>(&exact_json).is_ok());
        assert!(serde_json::from_str::<RequestId>(&too_long_json).is_err());
        assert!(serde_json::to_string(&RequestId::String(exact)).is_ok());
        assert!(serde_json::to_string(&RequestId::String(too_long)).is_err());

        let escaped_exact = format!("\"{}\"", "\\u0001".repeat(42));
        let escaped_too_long = format!("\"{}\"", "\\u0001".repeat(43));
        assert_eq!(escaped_exact.len(), 254);
        assert!(serde_json::from_str::<RequestId>(&escaped_exact).is_ok());
        assert!(serde_json::from_str::<RequestId>(&escaped_too_long).is_err());
    }

    #[test]
    fn request_id_validation_catches_direct_construction_bypass() {
        let too_long = RequestId::String("a".repeat(MAX_JSONRPC_STRING_ID_ENCODED_BYTES));

        assert_eq!(
            too_long.validate(),
            Err("JSON-RPC string id exceeds byte limit")
        );
    }

    #[test]
    fn request_rejects_explicit_null_id_but_accepts_absent_id() {
        let error = serde_json::from_str::<JsonRpcRequest>(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","id":null}"#,
        )
        .expect_err("an explicit null id must not become a notification");
        assert!(
            error
                .to_string()
                .contains("JSON-RPC id must be a string or mathematical integer")
        );

        let notification = serde_json::from_str::<JsonRpcRequest>(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .expect("an absent id denotes a notification");
        assert!(notification.is_notification());
    }

    #[test]
    fn request_id_from_i64() {
        let id: RequestId = 7i64.into();
        assert_eq!(id, RequestId::Number(7));
    }

    #[test]
    fn request_id_from_string() {
        let id: RequestId = "test-id".to_string().into();
        assert_eq!(id, RequestId::String("test-id".to_string()));
    }

    #[test]
    fn request_id_from_str() {
        let id: RequestId = "test-id".into();
        assert_eq!(id, RequestId::String("test-id".to_string()));
    }

    #[test]
    fn request_id_display() {
        assert_eq!(format!("{}", RequestId::Number(42)), "42");
        assert_eq!(
            format!("{}", RequestId::String("req-1".to_string())),
            "req-1"
        );
    }

    #[test]
    fn request_id_equality() {
        assert_eq!(RequestId::Number(1), RequestId::Number(1));
        assert_ne!(RequestId::Number(1), RequestId::Number(2));
        assert_eq!(
            RequestId::String("a".to_string()),
            RequestId::String("a".to_string())
        );
        assert_ne!(RequestId::Number(1), RequestId::String("1".to_string()));
    }

    // ========================================================================
    // JsonRpcRequest Tests
    // ========================================================================

    #[test]
    fn jsonrpc_version_deserialize_borrows_static_for_request() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#)
                .expect("deserialize");
        assert!(matches!(req.jsonrpc, Cow::Borrowed(JSONRPC_VERSION)));
    }

    #[test]
    fn request_rejects_nonstandard_missing_and_non_string_jsonrpc_versions() {
        for input in [
            r#"{"jsonrpc":"2.1","method":"tools/list","id":1}"#,
            r#"{"jsonrpc":"1.0","method":"tools/list","id":1}"#,
            r#"{"jsonrpc":null,"method":"tools/list","id":1}"#,
            r#"{"method":"tools/list","id":1}"#,
        ] {
            let error = serde_json::from_str::<JsonRpcRequest>(input).unwrap_err();
            assert!(error.is_data(), "unexpected error for {input}: {error}");
        }
    }

    #[test]
    fn request_serialization() {
        let req = JsonRpcRequest::new("tools/list", None, 1i64);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"tools/list\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn request_with_params() {
        let params = json!({"name": "greet", "arguments": {"name": "World"}});
        let req = JsonRpcRequest::new("tools/call", Some(params.clone()), 2i64);
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "tools/call");
        assert_eq!(value["params"]["name"], "greet");
        assert_eq!(value["id"], 2);
    }

    #[test]
    fn strict_request_raw_params_sidecar_preserves_exact_source_and_rejects_duplicates() {
        let raw_params = r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"weather","inputResponses":{"second":{"roots":[]},"first":{"roots":[]}},"requestState":"retry-1"}"#;
        let frame =
            format!(r#"{{"jsonrpc":"2.0","method":"tools/call","params":{raw_params},"id":7}}"#);
        let (request, sidecar) =
            JsonRpcRequest::decode_strict_with_raw_params(frame.as_bytes(), frame.len())
                .expect("strict request admission retains the exact parameter source");
        assert_eq!(sidecar.as_deref(), Some(raw_params));
        assert_eq!(
            request.params,
            Some(serde_json::from_str(raw_params).expect("raw params materialize")),
            "the sidecar belongs to the same admitted typed request"
        );

        let duplicate = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"inputResponses":{"roots":{"roots":[]},"roots":{"roots":[]}}},"id":7}"#;
        assert!(matches!(
            JsonRpcRequest::decode_strict_with_raw_params(duplicate.as_bytes(), duplicate.len()),
            Err(JsonRpcAdmissionError::Raw(
                RawJsonAdmissionError::DuplicateObjectMember
            ))
        ));
    }

    #[test]
    fn request_without_params_omits_field() {
        let req = JsonRpcRequest::new("tools/list", None, 1i64);
        let value = serde_json::to_value(&req).expect("serialize");
        assert!(value.get("params").is_none());
    }

    #[test]
    fn notification_has_no_id() {
        let notif = JsonRpcRequest::notification("notifications/progress", None);
        assert!(notif.is_notification());
        assert!(notif.id.is_none());
        let value = serde_json::to_value(&notif).expect("serialize");
        assert!(value.get("id").is_none());
    }

    #[test]
    fn notification_with_params() {
        let params = json!({"uri": "file://changed.txt"});
        let notif = JsonRpcRequest::notification("notifications/resources/updated", Some(params));
        assert!(notif.is_notification());
        let value = serde_json::to_value(&notif).expect("serialize");
        assert_eq!(value["params"]["uri"], "file://changed.txt");
    }

    #[test]
    fn request_is_not_notification() {
        let req = JsonRpcRequest::new("tools/list", None, 1i64);
        assert!(!req.is_notification());
    }

    #[test]
    fn request_with_string_id() {
        let req = JsonRpcRequest::new("tools/list", None, "req-abc");
        let value = serde_json::to_value(&req).expect("serialize");
        assert_eq!(value["id"], "req-abc");
    }

    #[test]
    fn request_round_trip() {
        let original = JsonRpcRequest::new(
            "tools/call",
            Some(json!({"name": "add", "arguments": {"a": 1, "b": 2}})),
            42i64,
        );
        let json_str = serde_json::to_string(&original).expect("serialize");
        let deserialized: JsonRpcRequest = serde_json::from_str(&json_str).expect("deserialize");
        assert_eq!(deserialized.method, "tools/call");
        assert_eq!(deserialized.id, Some(RequestId::Number(42)));
        assert!(deserialized.params.is_some());
    }

    #[test]
    fn request_rejects_unknown_top_level_envelope_members() {
        let error = serde_json::from_str::<JsonRpcRequest>(
            r#"{"jsonrpc":"2.0","method":"tools/list","id":1,"extension":true}"#,
        )
        .expect_err("request envelopes are closed");

        assert!(error.to_string().contains("unknown field"));
    }

    // ========================================================================
    // JsonRpcError Tests
    // ========================================================================

    #[test]
    fn jsonrpc_error_from_mcp_error_preserves_code_message_and_data() {
        let err = fastmcp_core::McpError::with_data(
            fastmcp_core::McpErrorCode::InvalidParams,
            "bad params",
            json!({"field":"name"}),
        );
        let rpc_err: JsonRpcError = err.into();
        assert_eq!(rpc_err.code.as_i32(), Some(-32602));
        assert_eq!(rpc_err.message, "bad params");
        assert_eq!(rpc_err.data, Some(json!({"field":"name"})));
    }

    #[test]
    fn jsonrpc_error_serialization() {
        let error = JsonRpcError {
            code: (-32600).into(),
            message: "Invalid Request".to_string(),
            data: None,
        };
        let value = serde_json::to_value(&error).expect("serialize");
        assert_eq!(value["code"], -32600);
        assert_eq!(value["message"], "Invalid Request");
        assert!(value.get("data").is_none());
    }

    #[test]
    fn jsonrpc_error_with_data() {
        let error = JsonRpcError {
            code: (-32602).into(),
            message: "Invalid params".to_string(),
            data: Some(json!({"field": "name", "reason": "required"})),
        };
        let value = serde_json::to_value(&error).expect("serialize");
        assert_eq!(value["code"], -32602);
        assert_eq!(value["data"]["field"], "name");
    }

    #[test]
    fn jsonrpc_error_preserves_arbitrary_width_integer_code() {
        let source = r#"{"code":-340282366920938463463374607431768211457,"message":"unbounded"}"#;
        let error: JsonRpcError = serde_json::from_str(source).expect("decode arbitrary code");

        assert_eq!(
            error.code.as_str(),
            "-340282366920938463463374607431768211457"
        );
        assert_eq!(
            serde_json::to_string(&error).expect("re-encode arbitrary code"),
            source
        );
    }

    #[test]
    fn jsonrpc_error_rejects_nearby_fractional_code() {
        let source =
            r#"{"code":-340282366920938463463374607431768211457.5,"message":"not integer"}"#;

        assert!(serde_json::from_str::<JsonRpcError>(source).is_err());
    }

    #[test]
    fn jsonrpc_error_standard_codes() {
        // Parse error
        let err = JsonRpcError {
            code: (-32700).into(),
            message: "Parse error".to_string(),
            data: None,
        };
        assert_eq!(serde_json::to_value(&err).unwrap()["code"], -32700);

        // Method not found
        let err = JsonRpcError {
            code: (-32601).into(),
            message: "Method not found".to_string(),
            data: None,
        };
        assert_eq!(serde_json::to_value(&err).unwrap()["code"], -32601);

        // Internal error
        let err = JsonRpcError {
            code: (-32603).into(),
            message: "Internal error".to_string(),
            data: None,
        };
        assert_eq!(serde_json::to_value(&err).unwrap()["code"], -32603);
    }

    // ========================================================================
    // JsonRpcResponse Tests
    // ========================================================================

    #[test]
    fn jsonrpc_version_deserialize_borrows_static_for_response() {
        let resp: JsonRpcResponse =
            serde_json::from_str(r#"{"jsonrpc":"2.0","result":{"tools":[]},"id":1}"#)
                .expect("deserialize");
        assert!(matches!(resp.jsonrpc, Cow::Borrowed(JSONRPC_VERSION)));
    }

    #[test]
    fn response_rejects_nonstandard_missing_and_non_string_jsonrpc_versions() {
        for input in [
            r#"{"jsonrpc":"2.1","result":{"tools":[]},"id":1}"#,
            r#"{"jsonrpc":"1.0","result":{"tools":[]},"id":1}"#,
            r#"{"jsonrpc":null,"result":{"tools":[]},"id":1}"#,
            r#"{"result":{"tools":[]},"id":1}"#,
        ] {
            let error = serde_json::from_str::<JsonRpcResponse>(input).unwrap_err();
            assert!(error.is_data(), "unexpected error for {input}: {error}");
        }
    }

    #[test]
    fn serialization_rejects_mutated_nonstandard_jsonrpc_version() {
        let mut request = JsonRpcRequest::new("tools/list", None, 1_i64);
        request.jsonrpc = Cow::Borrowed("2.1");
        assert!(serde_json::to_string(&request).is_err());

        let mut response = JsonRpcResponse::success(RequestId::Number(1), Value::Null);
        response.jsonrpc = Cow::Borrowed("1.0");
        assert!(serde_json::to_string(&response).is_err());
    }

    #[test]
    fn response_success() {
        let resp = JsonRpcResponse::success(RequestId::Number(1), json!({"result": "ok"}));
        let value = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["result"]["result"], "ok");
        assert_eq!(value["id"], 1);
        assert!(value.get("error").is_none());
        assert!(!resp.is_error());
    }

    #[test]
    fn response_error() {
        let error = JsonRpcError {
            code: (-32601).into(),
            message: "Method not found".to_string(),
            data: None,
        };
        let resp = JsonRpcResponse::error(Some(RequestId::Number(1)), error);
        let value = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(value["jsonrpc"], "2.0");
        assert!(value.get("result").is_none());
        assert_eq!(value["error"]["code"], -32601);
        assert_eq!(value["error"]["message"], "Method not found");
        assert_eq!(value["id"], 1);
        assert!(resp.is_error());
    }

    #[test]
    fn uncorrelated_response_error_omits_id() {
        let error = JsonRpcError {
            code: (-32700).into(),
            message: "Parse error".to_string(),
            data: None,
        };
        let resp = JsonRpcResponse::error(None, error);
        let value = serde_json::to_value(&resp).expect("serialize");
        assert!(value.get("id").is_none());
    }

    #[test]
    fn response_rejects_explicit_null_id_but_accepts_absent_id() {
        let explicit_null = serde_json::from_str::<JsonRpcResponse>(
            r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}"#,
        );
        assert!(explicit_null.is_err());

        let absent = serde_json::from_str::<JsonRpcResponse>(
            r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"}}"#,
        )
        .expect("an uncorrelated MCP error omits id");
        assert!(absent.id.is_none());
    }

    #[test]
    fn response_round_trip() {
        let original =
            JsonRpcResponse::success(RequestId::String("abc".to_string()), json!({"tools": []}));
        let json_str = serde_json::to_string(&original).expect("serialize");
        let deserialized: JsonRpcResponse = serde_json::from_str(&json_str).expect("deserialize");
        assert!(!deserialized.is_error());
        assert!(deserialized.result.is_some());
        assert_eq!(deserialized.id, Some(RequestId::String("abc".to_string())));
    }

    #[test]
    fn response_null_result_round_trip_preserves_member_presence() {
        let raw = r#"{"jsonrpc":"2.0","result":null,"id":1}"#;
        let response: JsonRpcResponse = serde_json::from_str(raw).expect("deserialize response");

        assert_eq!(response.result, Some(Value::Null));
        assert!(response.error.is_none());

        let encoded = serde_json::to_value(response).expect("serialize response");
        assert_eq!(encoded.get("result"), Some(&Value::Null));
        assert!(encoded.get("error").is_none());
    }

    #[test]
    fn response_rejects_both_or_neither_outcome_members() {
        for raw in [
            r#"{"jsonrpc":"2.0","result":null,"error":{"code":-32603,"message":"failure"},"id":1}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
        ] {
            let error = serde_json::from_str::<JsonRpcResponse>(raw)
                .expect_err("invalid response envelope must be rejected");
            assert!(error.to_string().contains("exactly one"));
        }

        let both = JsonRpcResponse {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            result: Some(Value::Null),
            error: Some(JsonRpcError {
                code: (-32_603).into(),
                message: "failure".to_string(),
                data: None,
            }),
            id: Some(RequestId::Number(1)),
        };
        assert!(serde_json::to_value(both).is_err());

        let neither = JsonRpcResponse {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(RequestId::Number(1)),
        };
        assert!(serde_json::to_value(neither).is_err());
    }

    #[test]
    fn response_rejects_unknown_top_level_envelope_members() {
        let error = serde_json::from_str::<JsonRpcResponse>(
            r#"{"jsonrpc":"2.0","result":null,"id":1,"extension":true}"#,
        )
        .expect_err("response envelopes are closed");

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn response_validation_rejects_uncorrelated_success() {
        let response = JsonRpcResponse {
            jsonrpc: Cow::Borrowed(JSONRPC_VERSION),
            result: Some(Value::Null),
            error: None,
            id: None,
        };

        assert_eq!(
            response.validate(),
            Err("JSON-RPC success response must contain an id")
        );
        assert!(serde_json::to_value(response).is_err());
        assert!(
            serde_json::from_str::<JsonRpcResponse>(r#"{"jsonrpc":"2.0","result":null}"#).is_err()
        );
    }

    // ========================================================================
    // JsonRpcMessage Tests
    // ========================================================================

    #[test]
    fn message_request_variant() {
        let req = JsonRpcRequest::new("tools/list", None, 1i64);
        let msg = JsonRpcMessage::Request(req);
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["method"], "tools/list");
    }

    #[test]
    fn message_response_variant() {
        let resp = JsonRpcResponse::success(RequestId::Number(1), json!("ok"));
        let msg = JsonRpcMessage::Response(resp);
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["result"], "ok");
    }

    #[test]
    fn message_deserialize_as_request() {
        let json_str = r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json_str).expect("deserialize");
        let (method, id) = match msg {
            JsonRpcMessage::Request(req) => (req.method, req.id),
            JsonRpcMessage::Response(_) => (String::new(), None),
        };
        assert_eq!(method, "tools/list");
        assert_eq!(id, Some(RequestId::Number(1)));
    }

    #[test]
    fn message_deserialize_as_response() {
        let json_str = r#"{"jsonrpc":"2.0","result":{"tools":[]},"id":1}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json_str).expect("deserialize");
        let (is_error, id) = match msg {
            JsonRpcMessage::Response(resp) => (resp.is_error(), resp.id),
            JsonRpcMessage::Request(_) => (true, None),
        };
        assert!(!is_error);
        assert_eq!(id, Some(RequestId::Number(1)));
    }

    #[test]
    fn message_rejects_mixed_request_and_response_envelopes() {
        for raw in [
            r#"{"jsonrpc":"2.0","method":"tools/list","result":null,"id":1}"#,
            r#"{"jsonrpc":"2.0","params":{},"error":{"code":-32603,"message":"failure"},"id":1}"#,
        ] {
            assert!(
                serde_json::from_str::<JsonRpcMessage>(raw).is_err(),
                "mixed envelope was accepted: {raw}"
            );
        }
    }

    #[test]
    fn message_validation_catches_public_field_mutation() {
        let mut request = JsonRpcRequest::new("tools/list", None, 1_i64);
        request.jsonrpc = Cow::Borrowed("2.1");
        let message = JsonRpcMessage::Request(request);

        assert_eq!(message.validate(), Err("jsonrpc must be exactly \"2.0\""));
    }

    // ========================================================================
    // JSONRPC_VERSION constant test
    // ========================================================================

    #[test]
    fn jsonrpc_version_constant() {
        assert_eq!(JSONRPC_VERSION, "2.0");
    }
}
