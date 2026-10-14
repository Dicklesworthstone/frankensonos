//! Message codec for framing JSON-RPC messages.
//!
//! MCP uses newline-delimited JSON (NDJSON) for message framing.

use fastmcp_protocol::{
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, MAX_JSONRPC_STRING_ID_ENCODED_BYTES,
    RequestId, admit_raw_jsonrpc_document,
};
use std::collections::BTreeSet;

/// Codec for encoding/decoding JSON-RPC messages.
#[derive(Debug)]
pub struct Codec {
    /// Buffer for incomplete messages.
    buffer: Vec<u8>,
    /// Maximum allowed message size in bytes.
    max_message_size: usize,
}

impl Default for Codec {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum number of nested JSON arrays and objects admitted per frame.
const MAX_JSON_NESTING_DEPTH: usize = 64;

/// Maximum aggregate number of object members and array elements per frame.
const MAX_JSON_CONTAINER_ENTRIES: usize = 100_000;

/// Maximum encoded bytes in one JSON number token.
const MAX_JSON_NUMBER_BYTES: usize = 4 * 1024;

/// Maximum aggregate encoded bytes in JSON number tokens per frame.
const MAX_AGGREGATE_JSON_NUMBER_BYTES: usize = 256 * 1024;

/// Maximum absolute JSON decimal exponent admitted before typed decoding.
const MAX_ABSOLUTE_JSON_EXPONENT: usize = 10_000;

#[derive(Clone, Copy, Debug, Default)]
struct RawEnvelopeShape {
    root_is_object: bool,
    has_method: bool,
    has_params: bool,
    has_id: bool,
    has_result: bool,
    has_error: bool,
    has_unknown_member: bool,
    has_duplicate_id: bool,
    id_span: Option<(usize, usize)>,
}

impl RawEnvelopeShape {
    fn note_member(&mut self, name: &str) {
        match name {
            "jsonrpc" => {}
            "method" => self.has_method = true,
            "params" => self.has_params = true,
            "id" => self.has_id = true,
            "result" => self.has_result = true,
            "error" => self.has_error = true,
            _ => self.has_unknown_member = true,
        }
    }

    fn note_value_span(&mut self, name: &str, start: usize, end: usize) {
        if name == "id" {
            self.id_span = Some((start, end));
        }
    }

    fn note_duplicate_member(&mut self, name: &str) {
        if name == "id" {
            self.has_duplicate_id = true;
        }
    }

    fn complete_kind(self) -> InvalidMessageKind {
        if self.has_result || self.has_error {
            InvalidMessageKind::Response
        } else {
            InvalidMessageKind::Request
        }
    }

    fn partial_kind(self, forced_kind: Option<InvalidMessageKind>) -> InvalidMessageKind {
        if self.has_result || self.has_error {
            InvalidMessageKind::Response
        } else if self.has_method || self.has_params {
            InvalidMessageKind::Request
        } else {
            // An admission failure can stop before a later result/error member
            // is observed. On a generic bidirectional channel, conservatively
            // suppress a reverse response instead of risking a response loop.
            forced_kind.unwrap_or(InvalidMessageKind::Response)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct JsonAdmissionFailure {
    error: JsonAdmissionError,
    envelope: RawEnvelopeShape,
}

/// A bounded structural JSON admission pass.
///
/// The ordinary `serde_json` model intentionally uses last-write-wins object
/// maps, so it cannot report duplicate names after deserialization. This
/// parser runs first, compares decoded object names, and bounds parser work
/// before any typed JSON-RPC allocation takes place.
struct JsonAdmission<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
    container_entries: usize,
    number_bytes: usize,
    decoded_string_bytes: usize,
    decoded_string_byte_limit: usize,
    envelope: RawEnvelopeShape,
    duplicate_object_member: bool,
}

impl<'a> JsonAdmission<'a> {
    fn new(input: &'a str, decoded_string_byte_limit: usize) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            container_entries: 0,
            number_bytes: 0,
            decoded_string_bytes: 0,
            decoded_string_byte_limit,
            envelope: RawEnvelopeShape::default(),
            duplicate_object_member: false,
        }
    }

    fn admit(mut self) -> Result<RawEnvelopeShape, JsonAdmissionFailure> {
        self.skip_whitespace();
        let result = self.parse_value(0).and_then(|()| {
            self.skip_whitespace();
            if self.position == self.bytes.len() {
                Ok(())
            } else {
                Err(JsonAdmissionError::InvalidSyntax)
            }
        });
        match result {
            Ok(()) if !self.duplicate_object_member => Ok(self.envelope),
            Ok(()) => Err(JsonAdmissionFailure {
                error: JsonAdmissionError::DuplicateObjectMember,
                envelope: self.envelope,
            }),
            Err(error) => Err(JsonAdmissionFailure {
                error,
                envelope: self.envelope,
            }),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<(), JsonAdmissionError> {
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => {
                self.parse_string(false)?;
                Ok(())
            }
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(JsonAdmissionError::InvalidSyntax),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), JsonAdmissionError> {
        let is_root = depth == 0;
        if is_root {
            self.envelope.root_is_object = true;
        }
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
                .ok_or(JsonAdmissionError::InvalidSyntax)?;
            let duplicate = !names.insert(name.clone());
            if is_root {
                self.envelope.note_member(&name);
                if duplicate {
                    self.envelope.note_duplicate_member(&name);
                }
            }
            if duplicate {
                // Keep parsing a syntactically valid object so the complete
                // top-level shape can classify the rejected envelope. This is
                // especially important on bidirectional transports: a bad
                // response must never provoke another response.
                self.duplicate_object_member = true;
            }

            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            self.skip_whitespace();
            let value_start = self.position;
            self.parse_value(nested_depth)?;
            if is_root {
                self.envelope
                    .note_value_span(&name, value_start, self.position);
            }
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), JsonAdmissionError> {
        let nested_depth = self.enter_container(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }

        loop {
            self.charge_container_entry()?;
            self.parse_value(nested_depth)?;
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(());
            }
            if !self.consume(b',') {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            self.skip_whitespace();
        }
    }

    fn enter_container(&self, depth: usize) -> Result<usize, JsonAdmissionError> {
        let nested_depth = depth
            .checked_add(1)
            .ok_or(JsonAdmissionError::NestingTooDeep)?;
        if nested_depth > MAX_JSON_NESTING_DEPTH {
            Err(JsonAdmissionError::NestingTooDeep)
        } else {
            Ok(nested_depth)
        }
    }

    fn charge_container_entry(&mut self) -> Result<(), JsonAdmissionError> {
        self.container_entries = self
            .container_entries
            .checked_add(1)
            .ok_or(JsonAdmissionError::TooManyContainerEntries)?;
        if self.container_entries > MAX_JSON_CONTAINER_ENTRIES {
            Err(JsonAdmissionError::TooManyContainerEntries)
        } else {
            Ok(())
        }
    }

    fn parse_string(&mut self, capture: bool) -> Result<Option<String>, JsonAdmissionError> {
        if !self.consume(b'"') {
            return Err(JsonAdmissionError::InvalidSyntax);
        }

        let mut decoded = capture.then(String::new);
        loop {
            let byte = self.peek().ok_or(JsonAdmissionError::InvalidSyntax)?;
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(decoded);
                }
                b'\\' => {
                    self.position += 1;
                    self.parse_escape(decoded.as_mut())?;
                }
                0x00..=0x1f => return Err(JsonAdmissionError::InvalidSyntax),
                0x20..=0x7f => {
                    self.position += 1;
                    self.charge_decoded_string_bytes(1)?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(char::from(byte));
                    }
                }
                _ => {
                    let character = self.input[self.position..]
                        .chars()
                        .next()
                        .ok_or(JsonAdmissionError::InvalidSyntax)?;
                    let encoded_len = character.len_utf8();
                    self.position += encoded_len;
                    self.charge_decoded_string_bytes(encoded_len)?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(character);
                    }
                }
            }
        }
    }

    fn parse_escape(&mut self, decoded: Option<&mut String>) -> Result<(), JsonAdmissionError> {
        let escape = self.peek().ok_or(JsonAdmissionError::InvalidSyntax)?;
        self.position += 1;
        let character = match escape {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => self.parse_unicode_escape()?,
            _ => return Err(JsonAdmissionError::InvalidSyntax),
        };
        self.charge_decoded_string_bytes(character.len_utf8())?;
        if let Some(value) = decoded {
            value.push(character);
        }
        Ok(())
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonAdmissionError> {
        let first = self.parse_hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if !self.consume(b'\\') || !self.consume(b'u') {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            let second = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(JsonAdmissionError::InvalidSyntax);
        } else {
            u32::from(first)
        };
        char::from_u32(scalar).ok_or(JsonAdmissionError::InvalidSyntax)
    }

    fn parse_hex_quad(&mut self) -> Result<u16, JsonAdmissionError> {
        let end = self
            .position
            .checked_add(4)
            .ok_or(JsonAdmissionError::InvalidSyntax)?;
        let digits = self
            .bytes
            .get(self.position..end)
            .ok_or(JsonAdmissionError::InvalidSyntax)?;
        let mut value = 0_u16;
        for digit in digits {
            let nibble = match digit {
                b'0'..=b'9' => u16::from(*digit - b'0'),
                b'a'..=b'f' => u16::from(*digit - b'a' + 10),
                b'A'..=b'F' => u16::from(*digit - b'A' + 10),
                _ => return Err(JsonAdmissionError::InvalidSyntax),
            };
            value = (value << 4) | nibble;
        }
        self.position = end;
        Ok(value)
    }

    fn charge_decoded_string_bytes(&mut self, bytes: usize) -> Result<(), JsonAdmissionError> {
        self.decoded_string_bytes = self
            .decoded_string_bytes
            .checked_add(bytes)
            .ok_or(JsonAdmissionError::TooManyDecodedStringBytes)?;
        if self.decoded_string_bytes > self.decoded_string_byte_limit {
            Err(JsonAdmissionError::TooManyDecodedStringBytes)
        } else {
            Ok(())
        }
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), JsonAdmissionError> {
        let end = self
            .position
            .checked_add(literal.len())
            .ok_or(JsonAdmissionError::InvalidSyntax)?;
        if self.bytes.get(self.position..end) == Some(literal) {
            self.position = end;
            Ok(())
        } else {
            Err(JsonAdmissionError::InvalidSyntax)
        }
    }

    fn parse_number(&mut self) -> Result<(), JsonAdmissionError> {
        let start = self.position;
        self.consume(b'-');
        self.ensure_number_token_within_limit(start)?;

        match self.peek() {
            Some(b'0') => {
                self.position += 1;
                self.ensure_number_token_within_limit(start)?;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(JsonAdmissionError::InvalidSyntax);
                }
            }
            Some(b'1'..=b'9') => {
                self.position += 1;
                self.ensure_number_token_within_limit(start)?;
                self.consume_number_digits(start)?;
            }
            _ => return Err(JsonAdmissionError::InvalidSyntax),
        }

        if self.consume(b'.') {
            self.ensure_number_token_within_limit(start)?;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            self.consume_number_digits(start)?;
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            self.ensure_number_token_within_limit(start)?;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
                self.ensure_number_token_within_limit(start)?;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonAdmissionError::InvalidSyntax);
            }
            let exponent_start = self.position;
            self.consume_number_digits(start)?;
            if exponent_exceeds_limit(&self.bytes[exponent_start..self.position]) {
                return Err(JsonAdmissionError::ExponentTooLarge);
            }
        }

        let token_bytes = self.position - start;
        self.number_bytes = self
            .number_bytes
            .checked_add(token_bytes)
            .ok_or(JsonAdmissionError::TooManyNumberBytes)?;
        if self.number_bytes > MAX_AGGREGATE_JSON_NUMBER_BYTES {
            Err(JsonAdmissionError::TooManyNumberBytes)
        } else {
            Ok(())
        }
    }

    fn consume_number_digits(&mut self, start: usize) -> Result<(), JsonAdmissionError> {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
            self.ensure_number_token_within_limit(start)?;
        }
        Ok(())
    }

    fn ensure_number_token_within_limit(&self, start: usize) -> Result<(), JsonAdmissionError> {
        if self.position - start > MAX_JSON_NUMBER_BYTES {
            Err(JsonAdmissionError::NumberTooLong)
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

fn exponent_exceeds_limit(digits: &[u8]) -> bool {
    let leading_zeroes = digits
        .iter()
        .position(|digit| *digit != b'0')
        .unwrap_or(digits.len());
    let significant = &digits[leading_zeroes..];
    if significant.len() > 5 {
        return true;
    }
    let exponent = significant.iter().fold(0_usize, |value, digit| {
        value * 10 + usize::from(*digit - b'0')
    });
    exponent > MAX_ABSOLUTE_JSON_EXPONENT
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonAdmissionError {
    InvalidUtf8,
    InvalidSyntax,
    DuplicateObjectMember,
    NestingTooDeep,
    TooManyContainerEntries,
    NumberTooLong,
    TooManyNumberBytes,
    ExponentTooLarge,
    TooManyDecodedStringBytes,
}

impl std::fmt::Display for JsonAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidUtf8 => "invalid UTF-8 in JSON frame",
            Self::InvalidSyntax => "invalid JSON syntax during bounded admission",
            Self::DuplicateObjectMember => "duplicate JSON object member",
            Self::NestingTooDeep => "JSON nesting limit exceeded",
            Self::TooManyContainerEntries => "JSON container-entry limit exceeded",
            Self::NumberTooLong => "JSON number-token limit exceeded",
            Self::TooManyNumberBytes => "aggregate JSON number-byte limit exceeded",
            Self::ExponentTooLarge => "JSON exponent limit exceeded",
            Self::TooManyDecodedStringBytes => "decoded JSON string-byte limit exceeded",
        };
        f.write_str(message)
    }
}

fn json_admission_codec_error(error: JsonAdmissionError) -> CodecError {
    CodecError::Json(<serde_json::Error as serde::de::Error>::custom(
        error.to_string(),
    ))
}

fn invalid_message_codec_error(
    kind: InvalidMessageKind,
    request_id: Option<RequestId>,
    message: impl std::fmt::Display,
) -> CodecError {
    CodecError::InvalidMessage {
        kind,
        request_id,
        source: <serde_json::Error as serde::de::Error>::custom(message),
    }
}

fn decode_raw_request_id(
    frame: &[u8],
    envelope: RawEnvelopeShape,
) -> Result<Option<RequestId>, serde_json::Error> {
    if !envelope.has_id {
        return Ok(None);
    }
    if envelope.has_duplicate_id {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "duplicate JSON-RPC id member",
        ));
    }

    let (start, end) = envelope.id_span.ok_or_else(|| {
        <serde_json::Error as serde::de::Error>::custom(
            "JSON-RPC id value was not completely admitted",
        )
    })?;
    let token = frame.get(start..end).ok_or_else(|| {
        <serde_json::Error as serde::de::Error>::custom("invalid JSON-RPC id token span")
    })?;
    if token.first() == Some(&b'"') && token.len() > MAX_JSONRPC_STRING_ID_ENCODED_BYTES {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "JSON-RPC string id exceeds raw encoded byte limit",
        ));
    }
    serde_json::from_slice(token).map(Some)
}

#[derive(Debug)]
struct EnvelopeAdmission {
    kind: InvalidMessageKind,
    request_id: Option<RequestId>,
}

/// A serialization sink that refuses to retain a JSON frame beyond the
/// codec's configured message-size limit.
struct BoundedEncodeBuffer {
    bytes: Vec<u8>,
    max_message_size: usize,
    rejected_size: Option<usize>,
}

impl BoundedEncodeBuffer {
    fn new(max_message_size: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_message_size,
            rejected_size: None,
        }
    }
}

impl std::io::Write for BoundedEncodeBuffer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next_size = self.bytes.len().saturating_add(buffer.len());
        if next_size > self.max_message_size {
            self.rejected_size = Some(next_size);
            return Err(std::io::Error::other(
                "encoded JSON-RPC message exceeds configured size limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Codec {
    /// Creates a new codec with default settings (10MB limit).
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            max_message_size: 10 * 1024 * 1024, // 10MB
        }
    }

    /// Returns the maximum allowed message size in bytes.
    #[must_use]
    pub fn max_message_size(&self) -> usize {
        self.max_message_size
    }

    /// Sets the maximum allowed message size in bytes.
    pub fn set_max_message_size(&mut self, size: usize) {
        self.max_message_size = size;
        let buffered_frame_len = self
            .buffer
            .strip_suffix(b"\r")
            .unwrap_or(&self.buffer)
            .len();
        if buffered_frame_len > size {
            self.buffer.clear();
        }
    }

    /// Encodes a request to bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn encode_request(&self, request: &JsonRpcRequest) -> Result<Vec<u8>, CodecError> {
        request.validate().map_err(|message| {
            CodecError::Json(<serde_json::Error as serde::ser::Error>::custom(message))
        })?;
        self.encode_value(request)
    }

    /// Encodes a response to bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn encode_response(&self, response: &JsonRpcResponse) -> Result<Vec<u8>, CodecError> {
        response.validate().map_err(|message| {
            CodecError::Json(<serde_json::Error as serde::ser::Error>::custom(message))
        })?;
        self.encode_value(response)
    }

    fn encode_value<T: serde::Serialize>(&self, value: &T) -> Result<Vec<u8>, CodecError> {
        let mut output = BoundedEncodeBuffer::new(self.max_message_size);
        if let Err(error) = serde_json::to_writer(&mut output, value) {
            if let Some(size) = output.rejected_size {
                return Err(CodecError::MessageTooLarge(size));
            }
            return Err(CodecError::Json(error));
        }
        output.bytes.push(b'\n');
        Ok(output.bytes)
    }

    /// Decodes one complete JSON-RPC message frame.
    ///
    /// Unlike [`Self::decode`], this method does not perform NDJSON framing or
    /// retain partial input. It is the common admission boundary for
    /// transports whose framing already identifies exactly one message, such
    /// as HTTP, SSE, and WebSocket.
    ///
    /// # Errors
    ///
    /// Returns an error when the frame exceeds the configured size limit,
    /// violates the bounded JSON admission policy, or is not a JSON-RPC
    /// message.
    pub fn decode_complete_message(&self, frame: &[u8]) -> Result<JsonRpcMessage, CodecError> {
        let admission = self.admit_complete_frame(frame, None)?;
        match admission.kind {
            InvalidMessageKind::Request => serde_json::from_slice::<JsonRpcRequest>(frame)
                .map(JsonRpcMessage::Request)
                .map_err(|error| {
                    typed_message_codec_error(
                        error,
                        InvalidMessageKind::Request,
                        admission.request_id,
                    )
                }),
            InvalidMessageKind::Response => serde_json::from_slice::<JsonRpcResponse>(frame)
                .map(JsonRpcMessage::Response)
                .map_err(|error| {
                    typed_message_codec_error(error, InvalidMessageKind::Response, None)
                }),
        }
    }

    /// Decodes one complete JSON-RPC request frame.
    ///
    /// This applies the same bounded, duplicate-member-rejecting admission
    /// policy as [`Self::decode_complete_message`] before typed decoding.
    ///
    /// # Errors
    ///
    /// Returns an error when the frame exceeds the configured size limit,
    /// violates the bounded JSON admission policy, or is not a JSON-RPC
    /// request.
    pub fn decode_complete_request(&self, frame: &[u8]) -> Result<JsonRpcRequest, CodecError> {
        let admission = self.admit_complete_frame(frame, Some(InvalidMessageKind::Request))?;
        debug_assert_eq!(admission.kind, InvalidMessageKind::Request);
        serde_json::from_slice(frame).map_err(|error| {
            typed_message_codec_error(error, InvalidMessageKind::Request, admission.request_id)
        })
    }

    fn admit_complete_frame(
        &self,
        frame: &[u8],
        forced_kind: Option<InvalidMessageKind>,
    ) -> Result<EnvelopeAdmission, CodecError> {
        if frame.len() > self.max_message_size {
            return Err(CodecError::MessageTooLarge(frame.len()));
        }
        let text = std::str::from_utf8(frame)
            .map_err(|_| json_admission_codec_error(JsonAdmissionError::InvalidUtf8))?;
        let envelope = match JsonAdmission::new(text, self.max_message_size).admit() {
            Ok(envelope) => envelope,
            Err(failure) if failure.error == JsonAdmissionError::InvalidSyntax => {
                return Err(json_admission_codec_error(failure.error));
            }
            Err(failure) => {
                let kind = failure.envelope.partial_kind(forced_kind);
                let request_id = match kind {
                    InvalidMessageKind::Request => decode_raw_request_id(frame, failure.envelope)
                        .ok()
                        .flatten(),
                    InvalidMessageKind::Response => None,
                };
                return Err(invalid_message_codec_error(kind, request_id, failure.error));
            }
        };

        let kind = envelope.complete_kind();
        let decoded_id = decode_raw_request_id(frame, envelope).map_err(|source| {
            CodecError::InvalidMessage {
                kind,
                request_id: None,
                source,
            }
        })?;
        let request_id = match kind {
            InvalidMessageKind::Request => decoded_id,
            InvalidMessageKind::Response => None,
        };

        if !envelope.root_is_object {
            return Err(invalid_message_codec_error(
                forced_kind.unwrap_or(InvalidMessageKind::Request),
                None,
                "JSON-RPC top-level value must be an object",
            ));
        }
        if envelope.has_unknown_member {
            return Err(invalid_message_codec_error(
                kind,
                request_id,
                "unknown JSON-RPC top-level envelope member",
            ));
        }
        if kind == InvalidMessageKind::Response && (envelope.has_method || envelope.has_params) {
            return Err(invalid_message_codec_error(
                InvalidMessageKind::Response,
                None,
                "conflicting JSON-RPC request and response envelope members",
            ));
        }
        if forced_kind == Some(InvalidMessageKind::Request) && kind == InvalidMessageKind::Response
        {
            return Err(invalid_message_codec_error(
                InvalidMessageKind::Response,
                None,
                "expected a JSON-RPC request, received a response envelope",
            ));
        }

        // Keep the protocol-owned raw admission primitive in the production
        // decode path. The local scanner above retains envelope classification
        // for direction-safe codec diagnostics; this call makes the shared
        // strict UTF-8/BOM/duplicate/limit contract authoritative before any
        // `serde_json` typed envelope is constructed.
        admit_raw_jsonrpc_document(frame, self.max_message_size).map_err(|error| {
            CodecError::Json(<serde_json::Error as serde::de::Error>::custom(
                error.to_string(),
            ))
        })?;

        Ok(EnvelopeAdmission { kind, request_id })
    }

    /// Decodes bytes into a message, returning any complete messages.
    ///
    /// Incomplete data is buffered for the next call.
    ///
    /// # Errors
    ///
    /// Returns an error if a complete line fails to parse or if the buffer exceeds the limit.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<JsonRpcMessage>, CodecError> {
        let mut messages = Vec::new();
        let mut remaining = data;

        while let Some(newline_position) = remaining.iter().position(|byte| *byte == b'\n') {
            let fragment = &remaining[..newline_position];
            let raw_frame_len = self.buffer.len().saturating_add(fragment.len());
            let has_trailing_cr = fragment
                .last()
                .or_else(|| self.buffer.last())
                .is_some_and(|byte| *byte == b'\r');
            let delimiter_bytes = usize::from(has_trailing_cr);
            let frame_len = raw_frame_len.saturating_sub(delimiter_bytes);
            if frame_len > self.max_message_size {
                self.buffer.clear();
                return Err(CodecError::MessageTooLarge(frame_len));
            }

            let decoded = if self.buffer.is_empty() {
                let frame = fragment.strip_suffix(b"\r").unwrap_or(fragment);
                if frame.is_empty() {
                    None
                } else {
                    Some(self.decode_complete_message(frame))
                }
            } else {
                self.buffer.extend_from_slice(fragment);
                let frame = self.buffer.strip_suffix(b"\r").unwrap_or(&self.buffer);
                if frame.is_empty() {
                    None
                } else {
                    Some(self.decode_complete_message(frame))
                }
            };

            match decoded {
                Some(Ok(message)) => messages.push(message),
                Some(Err(error)) => {
                    // A rejected frame is terminal for all buffered data in
                    // this call. Clearing prevents replay on the next call.
                    self.buffer.clear();
                    return Err(error);
                }
                None => {}
            }
            self.buffer.clear();
            remaining = &remaining[newline_position + 1..];
        }

        let projected_size = self.buffer.len().saturating_add(remaining.len());
        let has_trailing_cr = remaining
            .last()
            .or_else(|| self.buffer.last())
            .is_some_and(|byte| *byte == b'\r');
        let delimiter_bytes = usize::from(has_trailing_cr);
        let projected_frame_size = projected_size.saturating_sub(delimiter_bytes);
        if projected_frame_size > self.max_message_size {
            self.buffer.clear();
            return Err(CodecError::MessageTooLarge(projected_frame_size));
        }
        self.buffer.extend_from_slice(remaining);

        Ok(messages)
    }

    /// Clears the internal buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

/// Codec error types.
#[derive(Debug)]
pub enum CodecError {
    /// JSON syntax, UTF-8, or outbound serialization error.
    Json(serde_json::Error),
    /// Bounded-admission, envelope, or typed JSON-RPC violation.
    InvalidMessage {
        /// Whether the rejected envelope was request-like or response-like.
        kind: InvalidMessageKind,
        /// A uniquely readable request ID that is safe to echo in an Invalid
        /// Request response. This is always `None` for response-like input.
        request_id: Option<RequestId>,
        /// Admission or typed deserialization failure retained as a source.
        source: serde_json::Error,
    },
    /// Message too large.
    MessageTooLarge(usize),
}

/// Direction classified for a rejected JSON-RPC envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidMessageKind {
    /// The envelope is request-like and may receive an Invalid Request response.
    Request,
    /// The envelope is response-like and must never trigger another response.
    Response,
}

impl CodecError {
    /// Returns a uniquely readable ID from an invalid request envelope.
    ///
    /// The codec withholds IDs from response-like input, duplicate `id`
    /// members, malformed ID values, and overlong raw string-ID tokens.
    #[must_use]
    pub fn request_id(&self) -> Option<&RequestId> {
        match self {
            Self::InvalidMessage {
                kind: InvalidMessageKind::Request,
                request_id: Some(request_id),
                ..
            } => Some(request_id),
            _ => None,
        }
    }
}

fn typed_message_codec_error(
    error: serde_json::Error,
    kind: InvalidMessageKind,
    request_id: Option<RequestId>,
) -> CodecError {
    // The bounded raw pass has already established complete JSON syntax.
    // Any failure left at this point is a type or envelope violation, even if
    // serde_json happens to attach a different internal category to it.
    CodecError::InvalidMessage {
        kind,
        request_id,
        source: error,
    }
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Json(e) => write!(f, "JSON error: {e}"),
            CodecError::InvalidMessage { kind, source, .. } => match kind {
                InvalidMessageKind::Request => {
                    write!(f, "invalid JSON-RPC request: {source}")
                }
                InvalidMessageKind::Response => {
                    write!(f, "invalid JSON-RPC response: {source}")
                }
            },
            CodecError::MessageTooLarge(size) => write!(f, "Message too large: {size} bytes"),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CodecError::Json(e) => Some(e),
            CodecError::InvalidMessage { source, .. } => Some(source),
            CodecError::MessageTooLarge(_) => None,
        }
    }
}

impl From<serde_json::Error> for CodecError {
    fn from(err: serde_json::Error) -> Self {
        CodecError::Json(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_encode_decode_roundtrip() {
        let codec = Codec::new();
        let request = JsonRpcRequest::new("test/method", None, 1i64);

        let encoded = codec.encode_request(&request).unwrap();
        assert!(encoded.ends_with(b"\n"));

        let mut codec2 = Codec::new();
        let messages = codec2.decode(&encoded).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_encode_response() {
        let codec = Codec::new();
        let response =
            JsonRpcResponse::success(RequestId::Number(1), serde_json::json!({"result": "ok"}));

        let encoded = codec.encode_response(&response).unwrap();
        assert!(encoded.ends_with(b"\n"));

        let mut codec2 = Codec::new();
        let messages = codec2.decode(&encoded).unwrap();
        assert_eq!(messages.len(), 1);

        assert!(
            matches!(&messages[0], JsonRpcMessage::Response(_)),
            "Expected response"
        );
        if let JsonRpcMessage::Response(resp) = &messages[0] {
            assert_eq!(resp.id, Some(RequestId::Number(1)));
        }
    }

    #[test]
    fn test_decode_multiple_messages() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"test1\",\"id\":1}\n{\"jsonrpc\":\"2.0\",\"method\":\"test2\",\"id\":2}\n";

        let mut codec = Codec::new();
        let messages = codec.decode(input).unwrap();

        assert_eq!(messages.len(), 2);

        assert!(
            matches!(&messages[0], JsonRpcMessage::Request(_)),
            "Expected request"
        );
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert_eq!(req.method, "test1");
        }

        assert!(
            matches!(&messages[1], JsonRpcMessage::Request(_)),
            "Expected request"
        );
        if let JsonRpcMessage::Request(req) = &messages[1] {
            assert_eq!(req.method, "test2");
        }
    }

    #[test]
    fn test_decode_allows_multiple_messages_in_separate_chunks() {
        let req1 = JsonRpcRequest::new("test1", None, 1i64);
        let req2 = JsonRpcRequest::new("test2", None, 2i64);
        let mut line1 = serde_json::to_vec(&req1).unwrap();
        let mut line2 = serde_json::to_vec(&req2).unwrap();
        line1.push(b'\n');
        line2.push(b'\n');

        let mut codec = Codec::new();
        // Set limit to accommodate one message at a time
        codec.set_max_message_size(line1.len());

        // Decode first message
        let messages1 = codec.decode(&line1).unwrap();
        assert_eq!(messages1.len(), 1);

        // Decode second message
        let messages2 = codec.decode(&line2).unwrap();
        assert_eq!(messages2.len(), 1);
    }

    #[test]
    fn test_decode_applies_size_limit_per_frame_not_per_input_batch() {
        let frame = b"{\"jsonrpc\":\"2.0\",\"method\":\"m\",\"id\":1}\n";
        let mut codec = Codec::new();
        codec.set_max_message_size(frame.len() - 1);
        let batch = frame.repeat(4);
        assert!(batch.len() > codec.max_message_size());

        let messages = codec.decode(&batch).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(codec.buffer.len(), 0);
    }

    #[test]
    fn test_decode_accepts_exact_limit_crlf_split_across_calls() {
        let frame = b"{\"jsonrpc\":\"2.0\",\"method\":\"crlf\",\"id\":1}";
        let mut codec = Codec::new();
        codec.set_max_message_size(frame.len());

        let mut first = frame.to_vec();
        first.push(b'\r');
        assert!(codec.decode(&first).unwrap().is_empty());
        let messages = codec.decode(b"\n").unwrap();

        assert_eq!(messages.len(), 1);
        let JsonRpcMessage::Request(request) = &messages[0] else {
            panic!("expected request");
        };
        assert_eq!(request.method, "crlf");
    }

    #[test]
    fn test_decode_rejects_oversized_incomplete_line() {
        let req = JsonRpcRequest::new("oversized", None, 1i64);
        let line = serde_json::to_vec(&req).unwrap();

        let mut codec = Codec::new();
        codec.max_message_size = line.len().saturating_sub(1);

        let result = codec.decode(&line);
        assert!(matches!(result, Err(CodecError::MessageTooLarge(_))));
    }

    #[test]
    fn test_decode_partial_message() {
        let mut codec = Codec::new();

        // Feed partial data without newline
        let partial = b"{\"jsonrpc\":\"2.0\",\"method\":\"test\"";
        let messages = codec.decode(partial).unwrap();
        assert_eq!(messages.len(), 0); // No complete messages yet

        // Feed the rest including newline
        let rest = b",\"id\":1}\n";
        let messages = codec.decode(rest).unwrap();
        assert_eq!(messages.len(), 1);

        assert!(
            matches!(&messages[0], JsonRpcMessage::Request(_)),
            "Expected request"
        );
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert_eq!(req.method, "test");
        }
    }

    #[test]
    fn test_decode_invalid_json() {
        let mut codec = Codec::new();
        let invalid = b"not valid json\n";

        let result = codec.decode(invalid);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(matches!(err, CodecError::Json(_)));
    }

    #[test]
    fn all_decode_entry_points_reject_nonstandard_jsonrpc_versions() {
        let codec = Codec::new();
        for frame in [
            br#"{"jsonrpc":"2.1","method":"tools/list","id":1}"#.as_slice(),
            br#"{"jsonrpc":"1.0","method":"tools/list","id":1}"#.as_slice(),
            br#"{"jsonrpc":null,"method":"tools/list","id":1}"#.as_slice(),
            br#"{"method":"tools/list","id":1}"#.as_slice(),
        ] {
            assert!(codec.decode_complete_message(frame).is_err());
            assert!(codec.decode_complete_request(frame).is_err());

            let mut ndjson = frame.to_vec();
            ndjson.push(b'\n');
            assert!(Codec::new().decode(&ndjson).is_err());
        }
    }

    #[test]
    fn test_decode_recovers_after_rejected_frame() {
        let mut codec = Codec::new();

        let error = codec.decode(b"not valid json\n").unwrap_err();
        assert!(matches!(error, CodecError::Json(_)));

        let messages = codec
            .decode(b"{\"jsonrpc\":\"2.0\",\"method\":\"fresh\",\"id\":1}\n")
            .unwrap();
        assert_eq!(messages.len(), 1);
        let JsonRpcMessage::Request(request) = &messages[0] else {
            panic!("expected request");
        };
        assert_eq!(request.method, "fresh");
    }

    #[test]
    fn test_decode_empty_line() {
        let mut codec = Codec::new();
        let input = b"\n{\"jsonrpc\":\"2.0\",\"method\":\"test\",\"id\":1}\n";

        let messages = codec.decode(input).unwrap();
        assert_eq!(messages.len(), 1); // Empty line skipped
    }

    #[test]
    fn test_clear_buffer() {
        let mut codec = Codec::new();

        // Feed partial data
        let partial = b"{\"jsonrpc\":\"2.0\"";
        codec.decode(partial).unwrap();

        // Clear and verify buffer is empty
        codec.clear();

        // Feed a complete message - should parse without old partial data
        let complete = b"{\"jsonrpc\":\"2.0\",\"method\":\"fresh\",\"id\":1}\n";
        let messages = codec.decode(complete).unwrap();

        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0], JsonRpcMessage::Request(_)),
            "Expected request"
        );
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert_eq!(req.method, "fresh");
        }
    }

    #[test]
    fn test_codec_error_display() {
        let json_err = CodecError::Json(serde_json::from_str::<()>("invalid").unwrap_err());
        let size_err = CodecError::MessageTooLarge(1000);

        assert!(json_err.to_string().contains("JSON error"));
        assert!(size_err.to_string().contains("1000"));
    }

    #[test]
    fn test_codec_error_source() {
        let json_err = CodecError::Json(serde_json::from_str::<()>("invalid").unwrap_err());
        let size_err = CodecError::MessageTooLarge(1000);

        assert!(json_err.source().is_some());
        assert!(size_err.source().is_none());
    }

    #[test]
    fn test_default_max_message_size() {
        let codec = Codec::new();
        assert_eq!(codec.max_message_size(), 10 * 1024 * 1024);
    }

    #[test]
    fn test_set_max_message_size() {
        let mut codec = Codec::new();
        codec.set_max_message_size(1024);
        assert_eq!(codec.max_message_size(), 1024);
    }

    #[test]
    fn test_set_max_message_size_clears_oversized_buffer() {
        let mut codec = Codec::new();
        // Feed some data into buffer without a newline
        let partial = b"{\"jsonrpc\":\"2.0\",\"method\":\"test\"";
        codec.decode(partial).unwrap();

        // Shrink max size to less than buffered data
        codec.set_max_message_size(5);
        assert_eq!(codec.buffer.len(), 0);

        // Restore a usable limit and prove the old prefix is not spliced onto
        // the next complete frame.
        codec.set_max_message_size(1024);
        let fresh = b"{\"jsonrpc\":\"2.0\",\"method\":\"fresh\",\"id\":1}\n";
        let messages = codec.decode(fresh).unwrap();
        assert_eq!(messages.len(), 1);
        let JsonRpcMessage::Request(request) = &messages[0] else {
            panic!("expected a fresh request");
        };
        assert_eq!(request.method, "fresh");
    }

    #[test]
    fn test_codec_default_trait() {
        let codec = Codec::default();
        assert_eq!(codec.max_message_size(), 10 * 1024 * 1024);
    }

    #[test]
    fn test_decode_oversized_projected_data() {
        let mut codec = Codec::new();
        codec.set_max_message_size(50);

        // Feed data that exceeds max when projected
        let big = vec![b'x'; 100];
        let result = codec.decode(&big);
        assert!(matches!(result, Err(CodecError::MessageTooLarge(_))));
    }

    #[test]
    fn test_large_batch_leaves_no_consumed_data_buffered() {
        let mut codec = Codec::new();

        let msg = b"{\"jsonrpc\":\"2.0\",\"method\":\"m\",\"id\":1}\n";
        let many_messages: Vec<u8> = msg.repeat(200);

        let messages = codec.decode(&many_messages).unwrap();
        assert_eq!(messages.len(), 200);
        assert_eq!(codec.buffer.len(), 0);

        let next_msg = b"{\"jsonrpc\":\"2.0\",\"method\":\"after_compact\",\"id\":2}\n";
        let messages = codec.decode(next_msg).unwrap();
        assert_eq!(messages.len(), 1);
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert_eq!(req.method, "after_compact");
        }
    }

    #[test]
    fn test_decode_utf8_message() {
        let mut codec = Codec::new();
        let json = "{\"jsonrpc\":\"2.0\",\"method\":\"test/日本語\",\"id\":1}\n";
        let messages = codec.decode(json.as_bytes()).unwrap();
        assert_eq!(messages.len(), 1);
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert_eq!(req.method, "test/日本語");
        }
    }

    #[test]
    fn test_decode_consecutive_newlines() {
        let mut codec = Codec::new();
        let input = b"\n\n{\"jsonrpc\":\"2.0\",\"method\":\"test\",\"id\":1}\n\n\n";
        let messages = codec.decode(input).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_clear_resets_state() {
        let mut codec = Codec::new();

        // Feed partial data
        codec.decode(b"{\"jsonrpc\":\"2.0\"").unwrap();
        codec.clear();

        // Verify internal state is reset by sending a fresh complete message
        let complete = b"{\"jsonrpc\":\"2.0\",\"method\":\"post_clear\",\"id\":1}\n";
        let messages = codec.decode(complete).unwrap();
        assert_eq!(messages.len(), 1);
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert_eq!(req.method, "post_clear");
        }
    }

    #[test]
    fn test_codec_error_from_serde() {
        let serde_err = serde_json::from_str::<()>("bad").unwrap_err();
        let codec_err: CodecError = serde_err.into();
        assert!(matches!(codec_err, CodecError::Json(_)));
    }

    #[test]
    fn test_encode_request_contains_newline() {
        let codec = Codec::new();
        let request = JsonRpcRequest::new("m", None, 1i64);
        let encoded = codec.encode_request(&request).unwrap();
        assert_eq!(encoded.last(), Some(&b'\n'));
        // Everything before \n should be valid JSON
        let json_part = &encoded[..encoded.len() - 1];
        let _: JsonRpcRequest = serde_json::from_slice(json_part).expect("valid JSON");
    }

    #[test]
    fn test_encode_response_contains_newline() {
        let codec = Codec::new();
        let response =
            JsonRpcResponse::success(RequestId::Number(1), serde_json::json!({"ok": true}));
        let encoded = codec.encode_response(&response).unwrap();
        assert_eq!(encoded.last(), Some(&b'\n'));
    }

    #[test]
    fn outbound_request_size_limit_accepts_exact_frame_and_rejects_one_byte_less() {
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({"payload": "bounded"})),
            1_i64,
        );
        let frame_size = serde_json::to_vec(&request).unwrap().len();
        let mut codec = Codec::new();
        codec.set_max_message_size(frame_size);

        let encoded = codec
            .encode_request(&request)
            .expect("the exact JSON frame-size boundary is admitted");
        assert_eq!(encoded.len(), frame_size + 1);

        codec.set_max_message_size(frame_size - 1);
        let error = codec
            .encode_request(&request)
            .expect_err("one byte below the JSON frame size must reject encoding");
        assert!(matches!(error, CodecError::MessageTooLarge(size) if size > frame_size - 1));
    }

    #[test]
    fn outbound_response_size_limit_accepts_exact_frame_and_rejects_one_byte_less() {
        let response = JsonRpcResponse::success(
            RequestId::Number(1),
            serde_json::json!({"payload": "bounded"}),
        );
        let frame_size = serde_json::to_vec(&response).unwrap().len();
        let mut codec = Codec::new();
        codec.set_max_message_size(frame_size);

        let encoded = codec
            .encode_response(&response)
            .expect("the exact JSON frame-size boundary is admitted");
        assert_eq!(encoded.len(), frame_size + 1);

        codec.set_max_message_size(frame_size - 1);
        let error = codec
            .encode_response(&response)
            .expect_err("one byte below the JSON frame size must reject encoding");
        assert!(matches!(error, CodecError::MessageTooLarge(size) if size > frame_size - 1));
    }

    #[test]
    fn outbound_encoding_rejects_directly_mutated_invalid_typed_messages() {
        let codec = Codec::new();
        let mut request = JsonRpcRequest::new("tools/call", None, 1_i64);
        request.jsonrpc = std::borrow::Cow::Borrowed("1.0");
        assert!(matches!(
            codec.encode_request(&request),
            Err(CodecError::Json(_))
        ));

        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            result: None,
            error: None,
            id: Some(RequestId::Number(1)),
        };
        assert!(matches!(
            codec.encode_response(&response),
            Err(CodecError::Json(_))
        ));
    }

    #[test]
    fn test_decode_notification_without_id() {
        let mut codec = Codec::new();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/test\"}\n";
        let messages = codec.decode(input).unwrap();
        assert_eq!(messages.len(), 1);
        if let JsonRpcMessage::Request(req) = &messages[0] {
            assert!(req.id.is_none());
            assert_eq!(req.method, "notifications/test");
        }
    }

    #[test]
    fn test_decode_rejects_duplicate_envelope_member() {
        let mut codec = Codec::new();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"first\",\"method\":\"second\",\"id\":1}\n";

        let error = codec.decode(input).unwrap_err();

        assert!(matches!(
            &error,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Request,
                request_id: Some(RequestId::Number(1)),
                ..
            }
        ));
        assert!(error.to_string().contains("duplicate JSON object member"));
    }

    #[test]
    fn duplicate_admission_uses_the_complete_envelope_shape() {
        let codec = Codec::new();
        let response = br#"{"jsonrpc":"2.0","jsonrpc":"2.0","result":null,"id":1}"#;

        let error = codec.decode_complete_message(response).unwrap_err();

        assert!(matches!(
            error,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Response,
                request_id: None,
                ..
            }
        ));
    }

    #[test]
    fn duplicate_id_is_never_retained_for_error_correlation() {
        let codec = Codec::new();
        let request = br#"{"jsonrpc":"2.0","method":"test","id":1,"id":2}"#;

        let error = codec.decode_complete_message(request).unwrap_err();

        assert!(matches!(
            &error,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Request,
                request_id: None,
                ..
            }
        ));
        assert!(error.request_id().is_none());
    }

    #[test]
    fn test_complete_decoders_share_strict_admission_policy() {
        let codec = Codec::new();
        let duplicate =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"first\",\"m\\u0065thod\":\"second\",\"id\":1}";

        let message_error = codec.decode_complete_message(duplicate).unwrap_err();
        let request_error = codec.decode_complete_request(duplicate).unwrap_err();

        assert!(
            message_error
                .to_string()
                .contains("duplicate JSON object member")
        );
        assert!(
            request_error
                .to_string()
                .contains("duplicate JSON object member")
        );
    }

    #[test]
    fn test_decode_rejects_escaped_duplicate_member_at_nested_depth() {
        let mut codec = Codec::new();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"test\",\"params\":{\"name\":1,\"\\u006eame\":2},\"id\":1}\n";

        let error = codec.decode(input).unwrap_err();

        assert_eq!(error.request_id(), Some(&RequestId::Number(1)));
        assert!(error.to_string().contains("duplicate JSON object member"));
    }

    #[test]
    fn test_decode_rejects_surrogate_escaped_duplicate_member() {
        let mut codec = Codec::new();
        let input = "{\"jsonrpc\":\"2.0\",\"method\":\"test\",\"params\":{\"😀\":1,\"\\ud83d\\ude00\":2},\"id\":1}\n";

        let error = codec.decode(input.as_bytes()).unwrap_err();

        assert!(error.to_string().contains("duplicate JSON object member"));
    }

    #[test]
    fn test_decode_allows_same_member_name_in_distinct_objects() {
        let mut codec = Codec::new();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"test\",\"params\":[{\"same\":1},{\"same\":2}],\"id\":1}\n";

        let messages = codec.decode(input).unwrap();

        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_json_admission_accepts_depth_limit_and_rejects_next_level() {
        let accepted_array_depth = MAX_JSON_NESTING_DEPTH - 1;
        let accepted = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"depth\",\"params\":{}0{},\"id\":1}}",
            "[".repeat(accepted_array_depth),
            "]".repeat(accepted_array_depth)
        );
        Codec::new()
            .admit_complete_frame(accepted.as_bytes(), None)
            .unwrap();

        let rejected_array_depth = MAX_JSON_NESTING_DEPTH;
        let mut rejected = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"depth\",\"params\":{}0{},\"id\":1}}",
            "[".repeat(rejected_array_depth),
            "]".repeat(rejected_array_depth)
        );
        rejected.push('\n');
        let mut codec = Codec::new();

        let error = codec.decode(rejected.as_bytes()).unwrap_err();

        assert!(error.to_string().contains("JSON nesting limit exceeded"));
    }

    #[test]
    fn test_decode_rejects_container_entry_limit_overflow() {
        let mut input = String::from("{\"jsonrpc\":\"2.0\",\"method\":\"entries\",\"params\":[");
        for index in 0..MAX_JSON_CONTAINER_ENTRIES {
            if index != 0 {
                input.push(',');
            }
            input.push('0');
        }
        input.push_str("],\"id\":1}\n");
        let mut codec = Codec::new();

        let error = codec.decode(input.as_bytes()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("JSON container-entry limit exceeded")
        );
    }

    #[test]
    fn test_decode_rejects_oversized_number_token_before_typed_decode() {
        let number = "9".repeat(MAX_JSON_NUMBER_BYTES + 1);
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"number\",\"params\":{{\"value\":{number}}},\"id\":1}}\n"
        );
        let mut codec = Codec::new();

        let error = codec.decode(input.as_bytes()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("JSON number-token limit exceeded")
        );
    }

    #[test]
    fn number_admission_stops_at_first_byte_over_token_budget() {
        let number = "9".repeat(MAX_JSON_NUMBER_BYTES * 2);
        let mut admission = JsonAdmission::new(&number, number.len());

        let error = admission.parse_number().unwrap_err();

        assert_eq!(error, JsonAdmissionError::NumberTooLong);
        assert_eq!(admission.position, MAX_JSON_NUMBER_BYTES + 1);
    }

    #[test]
    fn test_decode_rejects_aggregate_number_byte_overflow() {
        let number = "9".repeat(MAX_JSON_NUMBER_BYTES);
        let number_count = MAX_AGGREGATE_JSON_NUMBER_BYTES / MAX_JSON_NUMBER_BYTES + 1;
        let mut input = String::from("{\"jsonrpc\":\"2.0\",\"method\":\"numbers\",\"params\":[");
        for index in 0..number_count {
            if index != 0 {
                input.push(',');
            }
            input.push_str(&number);
        }
        input.push_str("],\"id\":1}\n");
        let mut codec = Codec::new();

        let error = codec.decode(input.as_bytes()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("aggregate JSON number-byte limit exceeded")
        );
    }

    #[test]
    fn test_decode_rejects_exponent_magnitude_overflow() {
        let mut codec = Codec::new();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"number\",\"params\":{\"value\":1e10001},\"id\":1}\n";

        let error = codec.decode(input).unwrap_err();

        assert!(error.to_string().contains("JSON exponent limit exceeded"));
    }

    #[test]
    fn test_decode_rejects_invalid_utf8_before_typed_decode() {
        let mut input = b"{\"jsonrpc\":\"2.0\",\"method\":\"".to_vec();
        input.push(0xff);
        input.extend_from_slice(b"\",\"id\":1}\n");
        let mut codec = Codec::new();

        let error = codec.decode(&input).unwrap_err();

        assert!(error.to_string().contains("invalid UTF-8 in JSON frame"));
    }

    #[test]
    fn closed_top_level_envelopes_reject_unknown_and_conflicting_members() {
        let codec = Codec::new();

        let unknown_request = codec
            .decode_complete_message(
                br#"{"jsonrpc":"2.0","method":"tools/list","id":"req-1","extension":true}"#,
            )
            .unwrap_err();
        assert!(matches!(
            unknown_request,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Request,
                request_id: Some(RequestId::String(ref id)),
                ..
            } if id == "req-1"
        ));

        let unknown_response = codec
            .decode_complete_message(br#"{"jsonrpc":"2.0","result":null,"id":1,"extension":true}"#)
            .unwrap_err();
        assert!(matches!(
            unknown_response,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Response,
                request_id: None,
                ..
            }
        ));

        let conflicting = codec
            .decode_complete_message(
                br#"{"jsonrpc":"2.0","method":"tools/list","result":null,"id":1}"#,
            )
            .unwrap_err();
        assert!(matches!(
            conflicting,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Response,
                request_id: None,
                ..
            }
        ));
    }

    #[test]
    fn invalid_request_retains_only_a_safe_readable_id() {
        let codec = Codec::new();
        let missing_method = br#"{"jsonrpc":"2.0","id":7}"#;

        let error = codec.decode_complete_message(missing_method).unwrap_err();

        assert_eq!(error.request_id(), Some(&RequestId::Number(7)));
        assert!(matches!(
            error,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Request,
                ..
            }
        ));
    }

    #[test]
    fn syntax_request_and_response_failures_remain_distinct() {
        let codec = Codec::new();
        assert!(matches!(
            codec.decode_complete_message(br#"{"jsonrpc":"2.0""#),
            Err(CodecError::Json(_))
        ));

        let invalid_request = codec.decode_complete_message(br"[]").unwrap_err();
        assert!(matches!(
            invalid_request,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Request,
                ..
            }
        ));

        let invalid_response = codec
            .decode_complete_message(
                br#"{"jsonrpc":"2.0","result":null,"error":{"code":-32603,"message":"failure"},"id":1}"#,
            )
            .unwrap_err();
        assert!(matches!(
            invalid_response,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Response,
                request_id: None,
                ..
            }
        ));
    }

    #[test]
    fn raw_string_id_limit_counts_received_escape_spelling() {
        let codec = Codec::new();
        let exact_id = format!("\"{}aa\"", "\\u0061".repeat(42));
        assert_eq!(exact_id.len(), MAX_JSONRPC_STRING_ID_ENCODED_BYTES);
        let exact = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"tools/list\",\"id\":{exact_id}}}");
        let message = codec
            .decode_complete_message(exact.as_bytes())
            .expect("a 256-byte raw string ID is allowed");
        let JsonRpcMessage::Request(request) = message else {
            panic!("expected request");
        };
        assert_eq!(request.id, Some(RequestId::String("a".repeat(44))));

        let overlong_id = format!("\"{}aaa\"", "\\u0061".repeat(42));
        assert_eq!(overlong_id.len(), MAX_JSONRPC_STRING_ID_ENCODED_BYTES + 1);
        let overlong =
            format!("{{\"jsonrpc\":\"2.0\",\"method\":\"tools/list\",\"id\":{overlong_id}}}");
        let error = codec
            .decode_complete_message(overlong.as_bytes())
            .unwrap_err();
        assert!(matches!(
            &error,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Request,
                request_id: None,
                ..
            }
        ));
        assert!(error.to_string().contains("raw encoded byte limit"));
    }

    #[test]
    fn request_only_decoder_never_reclassifies_a_response_as_a_request() {
        let codec = Codec::new();
        let response = br#"{"jsonrpc":"2.0","result":null,"id":1}"#;

        let error = codec.decode_complete_request(response).unwrap_err();

        assert!(matches!(
            error,
            CodecError::InvalidMessage {
                kind: InvalidMessageKind::Response,
                request_id: None,
                ..
            }
        ));
    }
}
