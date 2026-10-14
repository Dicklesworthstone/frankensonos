//! Bounded MCP HTTP header values and schema-derived parameter mirrors.
//!
//! A compiled plan is syntax, not authority. It neither approves disclosure of
//! a parameter to intermediaries nor authorizes a tool. Local registration must
//! separately review every projected path for non-sensitive exposure. Servers
//! must select a plan through their already-authorized operation, never perform
//! an unauthenticated schema lookup with this module. General input validation
//! remains separate: in particular, an annotated null is omitted even when the
//! input schema will subsequently reject it.
//!
//! These helpers do not mutate arguments, follow schema references, retry a
//! request, or attach credentials. HTTP adapters retain responsibility for
//! ordinary header/framing limits and for emitting a HeaderMismatch response.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::schema::{AdmittedSchema, ValidationResult, MAX_SCHEMA_ADMISSION_NODES, MAX_SCHEMA_VALIDATION_DEPTH};

/// Maximum bindings in one tool's HTTP projection plan.
pub const MAX_PARAMETER_HEADERS: usize = 64;
/// Maximum bytes in one complete `Mcp-Param-*` field name.
pub const MAX_PARAMETER_HEADER_NAME_BYTES: usize = 128;
/// Maximum cumulative UTF-8 bytes in all retained property paths.
pub const MAX_PARAMETER_HEADER_PATH_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 bytes in one decoded header value.
pub const MAX_MCP_HEADER_VALUE_BYTES: usize = 8 * 1024;
/// Includes the sentinel and Base64 expansion of the maximum decoded value.
pub const MAX_MCP_ENCODED_HEADER_VALUE_BYTES: usize = 4 * MAX_MCP_HEADER_VALUE_BYTES.div_ceil(3)
    + SENTINEL_PREFIX.len() + SENTINEL_SUFFIX.len();
/// Maximum complete field-name/value bytes produced by one projection.
pub const MAX_PARAMETER_HEADER_BLOCK_BYTES: usize = 64 * 1024;
/// Numeric mirrors use the exact JavaScript-safe integer range, not f64.
pub const MAX_PARAMETER_HEADER_INTEGER: i64 = 9_007_199_254_740_991;
/// Aggregate source-string bytes retained by annotation-aware schema admission.
pub const MAX_TOOL_HEADER_SCHEMA_BYTES: usize = 1024 * 1024;

const PREFIX: &str = "Mcp-Param-";
const SENTINEL_PREFIX: &str = "=?base64?";
const SENTINEL_SUFFIX: &str = "?=";
const MAX_INTEGER_LEXEME_BYTES: usize = 1024;

/// Fixed diagnostics never retain a schema, property name, argument or header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpHeaderError {
    InvalidSchema,
    InvalidAnnotation,
    UnreachableAnnotation,
    DuplicateAnnotation,
    InvalidParameter,
    InvalidHeaderValue,
    HeaderMismatch,
    LimitExceeded,
}

impl fmt::Display for McpHeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSchema => "invalid MCP tool header schema",
            Self::InvalidAnnotation => "invalid MCP parameter-header annotation",
            Self::UnreachableAnnotation => "MCP parameter-header annotation is not statically reachable",
            Self::DuplicateAnnotation => "duplicate MCP parameter-header annotation",
            Self::InvalidParameter => "MCP parameter cannot be represented by its header type",
            Self::InvalidHeaderValue => "invalid MCP encoded header value",
            Self::HeaderMismatch => "MCP parameter header does not match its body value",
            Self::LimitExceeded => "MCP parameter-header bound exceeded",
        })
    }
}
impl std::error::Error for McpHeaderError {}

/// An annotation-aware schema whose validation vocabulary was admitted by the
/// shared Draft 2020-12 engine. That engine deliberately rejects unknown
/// keywords. Only `x-mcp-header` at schema locations is separated from its
/// validation copy; every other admission rule remains in force. The exact
/// original schema, including annotations and literal examples, is retained.
/// This is not registration-time exposure approval or operation authorization.
pub struct AdmittedToolHeaderSchema {
    source: Value,
    validation: AdmittedSchema,
    plan: ToolParameterHeaderPlan,
}
impl AdmittedToolHeaderSchema {
    pub fn admit(source: Value) -> Result<Self, McpHeaderError> {
        bound_schema_source(&source)?;
        let mut validation = source.clone();
        separate_header_annotations(&mut validation);
        let validation = crate::schema::admit_final_schema(validation)
            .map_err(|_| McpHeaderError::InvalidSchema)?;
        let mut compiler = Compiler { bindings: Vec::new(), nodes: 0, path_bytes: 0 };
        compiler.visit(&source, &mut Vec::new(), true, 0)?;
        Ok(Self { source, validation, plan: ToolParameterHeaderPlan { bindings: compiler.bindings } })
    }
    pub fn schema(&self) -> &Value { &self.source }
    pub fn header_plan(&self) -> &ToolParameterHeaderPlan { &self.plan }
    pub fn validate(&self, arguments: &Value) -> ValidationResult { self.validation.validate(arguments) }
}

/// Admits a tool's final input schema for local registration. A schema within
/// the Draft 2020-12 vocabulary is admitted unchanged. Otherwise its
/// `x-mcp-header` annotations are separated from the validation copy, every
/// annotation rule is enforced, and that copy is returned. Any other
/// unsupported keyword, or an invalid annotation, still refuses the schema.
pub fn admit_final_tool_input_schema(source: Value) -> Result<AdmittedSchema, McpHeaderError> {
    match crate::schema::admit_final_schema(source.clone()) {
        Ok(schema) => Ok(schema),
        Err(_) => AdmittedToolHeaderSchema::admit(source).map(|schema| schema.validation),
    }
}
impl fmt::Debug for AdmittedToolHeaderSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmittedToolHeaderSchema").field("plan", &self.plan).finish_non_exhaustive()
    }
}

fn bound_schema_source(source: &Value) -> Result<(), McpHeaderError> {
    let mut pending = vec![(source, 0_usize)];
    let mut nodes = 0_usize;
    let mut bytes = 0_usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > MAX_SCHEMA_ADMISSION_NODES || depth > MAX_SCHEMA_VALIDATION_DEPTH {
            return Err(McpHeaderError::LimitExceeded);
        }
        match value {
            Value::Object(object) => {
                if object.len() > MAX_SCHEMA_ADMISSION_NODES - nodes - pending.len() {
                    return Err(McpHeaderError::LimitExceeded);
                }
                for (key, child) in object {
                    bytes = bytes.checked_add(key.len()).ok_or(McpHeaderError::LimitExceeded)?;
                    pending.push((child, depth + 1));
                }
            }
            Value::Array(array) => {
                if array.len() > MAX_SCHEMA_ADMISSION_NODES - nodes - pending.len() {
                    return Err(McpHeaderError::LimitExceeded);
                }
                pending.extend(array.iter().map(|child| (child, depth + 1)));
            }
            Value::String(value) => {
                bytes = bytes.checked_add(value.len()).ok_or(McpHeaderError::LimitExceeded)?;
            }
            Value::Number(number) => {
                struct NumberBytes(usize);
                impl fmt::Write for NumberBytes {
                    fn write_str(&mut self, value: &str) -> fmt::Result {
                        self.0 = self.0.checked_add(value.len()).ok_or(fmt::Error)?;
                        if self.0 > MAX_TOOL_HEADER_SCHEMA_BYTES { return Err(fmt::Error); }
                        Ok(())
                    }
                }
                let mut measured = NumberBytes(bytes);
                fmt::write(&mut measured, format_args!("{number}")).map_err(|_| McpHeaderError::LimitExceeded)?;
                bytes = measured.0;
            }
            Value::Bool(_) | Value::Null => {}
        }
        if bytes > MAX_TOOL_HEADER_SCHEMA_BYTES { return Err(McpHeaderError::LimitExceeded); }
    }
    Ok(())
}

// The source was depth/node/byte bounded before cloning or recursive descent.
// Instance-valued keywords are intentionally not visited or rewritten.
fn separate_header_annotations(schema: &mut Value) {
    let Some(schema) = schema.as_object_mut() else { return; };
    schema.remove("x-mcp-header");
    for (keyword, value) in schema {
        match keyword.as_str() {
            "properties" | "$defs" | "patternProperties" | "dependentSchemas" => {
                if let Some(children) = value.as_object_mut() {
                    for child in children.values_mut() { separate_header_annotations(child); }
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                if let Some(children) = value.as_array_mut() {
                    for child in children { separate_header_annotations(child); }
                }
            }
            "items" | "contains" | "additionalProperties" | "unevaluatedProperties"
            | "unevaluatedItems" | "propertyNames" | "not" | "if" | "then" | "else" | "contentSchema" => {
                separate_header_annotations(value);
            }
            _ => {}
        }
    }
}

/// Encode a `Mcp-Name` or parameter value without trimming or changing it.
/// Sentinel-looking literals are wrapped too, so a receiver decodes only once.
pub fn encode_mcp_header_value(value: &str) -> Result<String, McpHeaderError> {
    if value.len() > MAX_MCP_HEADER_VALUE_BYTES { return Err(McpHeaderError::LimitExceeded); }
    let plain = value.bytes().all(is_field_value_byte)
        && !value.starts_with([' ', '\t']) && !value.ends_with([' ', '\t'])
        && !is_sentinel(value);
    if plain { return Ok(value.to_owned()); }
    Ok(format!("{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}", STANDARD.encode(value.as_bytes())))
}

/// Admit raw field-value bytes before string conversion, remove only HTTP OWS,
/// and decode an exact lowercase sentinel once. Decoded Unicode, whitespace and
/// control characters remain data and must never be re-emitted as raw headers.
/// This function does not decode protocol-version or method headers.
pub fn decode_mcp_header_value(value: &[u8]) -> Result<String, McpHeaderError> {
    if value.len() > MAX_MCP_ENCODED_HEADER_VALUE_BYTES { return Err(McpHeaderError::LimitExceeded); }
    if !value.iter().copied().all(is_field_value_byte) { return Err(McpHeaderError::InvalidHeaderValue); }
    // Every byte is now ASCII. No Unicode whitespace normalization is allowed.
    let value = std::str::from_utf8(value).map_err(|_| McpHeaderError::InvalidHeaderValue)?;
    let value = value.trim_matches([' ', '\t']);
    if is_sentinel(value) {
        // Prefix and suffix may overlap in malformed input (`=?base64?=`).
        // Remove them sequentially rather than constructing an unchecked range.
        // Keep is_sentinel's broad recognition: the encoder must still wrap
        // malformed sentinel-looking literals so they round-trip as data.
        let encoded = value.strip_prefix(SENTINEL_PREFIX)
            .and_then(|payload| payload.strip_suffix(SENTINEL_SUFFIX))
            .ok_or(McpHeaderError::InvalidHeaderValue)?;
        let decoded = STANDARD.decode(encoded).map_err(|_| McpHeaderError::InvalidHeaderValue)?;
        if decoded.len() > MAX_MCP_HEADER_VALUE_BYTES { return Err(McpHeaderError::LimitExceeded); }
        String::from_utf8(decoded).map_err(|_| McpHeaderError::InvalidHeaderValue)
    } else if value.len() > MAX_MCP_HEADER_VALUE_BYTES {
        Err(McpHeaderError::LimitExceeded)
    } else {
        Ok(value.to_owned())
    }
}

fn is_sentinel(value: &str) -> bool {
    value.starts_with(SENTINEL_PREFIX) && value.ends_with(SENTINEL_SUFFIX)
}
fn is_field_value_byte(byte: u8) -> bool { matches!(byte, b'\t' | b' '..=b'~') }
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterHeaderType { String, Integer, Boolean }

/// One exact property path. A slash, tilde, dot or numeric-looking member name
/// is an ordinary object key, not a JSON Pointer, dotted path or array index.
#[derive(Clone, PartialEq, Eq)]
pub struct ParameterHeaderBinding {
    path: Vec<String>,
    name: String,
    kind: ParameterHeaderType,
}
impl ParameterHeaderBinding {
    pub fn property_path(&self) -> &[String] { &self.path }
    pub fn header_name(&self) -> &str { &self.name }
    pub fn parameter_type(&self) -> ParameterHeaderType { self.kind }
}
impl fmt::Debug for ParameterHeaderBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParameterHeaderBinding").field("path_depth", &self.path.len())
            .field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// Immutable syntax plan compiled only after Draft 2020-12 schema admission.
/// Invalid annotations reject the entire plan. The HTTP catalog owner can then
/// omit that tool without discarding valid siblings. No reference is fetched.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolParameterHeaderPlan { bindings: Vec<ParameterHeaderBinding> }
impl fmt::Debug for ToolParameterHeaderPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolParameterHeaderPlan").field("binding_count", &self.bindings.len()).finish()
    }
}
impl ToolParameterHeaderPlan {
    /// Compile a schema already admitted by the generic schema engine. Use
    /// `AdmittedToolHeaderSchema::admit` for a source containing `x-mcp-header`.
    pub fn compile(schema: &AdmittedSchema) -> Result<Self, McpHeaderError> {
        let mut compiler = Compiler { bindings: Vec::new(), nodes: 0, path_bytes: 0 };
        compiler.visit(schema.schema(), &mut Vec::new(), true, 0)?;
        Ok(Self { bindings: compiler.bindings })
    }

    pub fn bindings(&self) -> &[ParameterHeaderBinding] { &self.bindings }

    /// Produce all required fields atomically, leaving the complete original
    /// arguments untouched. Absent and null values omit their fields; a wrong
    /// primitive or unsafe integer fails the whole projection, never a subset.
    /// The invocation owner must approve disclosure before sending these fields.
    pub fn project(&self, arguments: Option<&Value>) -> Result<ProjectedParameterHeaders, McpHeaderError> {
        if arguments.is_some_and(|value| !value.is_object()) { return Err(McpHeaderError::InvalidParameter); }
        let mut fields = Vec::new();
        let mut bytes = 0_usize;
        for binding in &self.bindings {
            let Some(value) = value_at(arguments, &binding.path).filter(|value| !value.is_null()) else { continue; };
            let encoded = encode_mcp_header_value(&primitive_value(binding.kind, value)?)?;
            bytes = bytes.checked_add(binding.name.len()).and_then(|n| n.checked_add(encoded.len()))
                .ok_or(McpHeaderError::LimitExceeded)?;
            if bytes > MAX_PARAMETER_HEADER_BLOCK_BYTES { return Err(McpHeaderError::LimitExceeded); }
            fields.push((binding.name.clone(), encoded));
        }
        Ok(ProjectedParameterHeaders { fields })
    }

    /// Compare recognized mirrors only, after authentication AND named-operation
    /// authorization. Pass duplicate-preserving transport fields. Unknown fields
    /// do not create schema mappings; their syntax/framing limits remain with
    /// transport admission. This is not general tool argument validation.
    pub fn validate(&self, arguments: Option<&Value>, headers: &[(String, String)]) -> Result<(), McpHeaderError> {
        if arguments.is_some_and(|value| !value.is_object()) { return Err(McpHeaderError::InvalidParameter); }
        // Bound inspection independently from the transport's own header limit.
        if headers.len() > 1024 { return Err(McpHeaderError::LimitExceeded); }
        let mut total = 0_usize;
        for binding in &self.bindings {
            let mut values = headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case(&binding.name));
            let header = values.next();
            if values.next().is_some() { return Err(McpHeaderError::HeaderMismatch); }
            let value = value_at(arguments, &binding.path).filter(|value| !value.is_null());
            let (Some(value), Some((name, header))) = (value, header) else {
                if value.is_some() || header.is_some() { return Err(McpHeaderError::HeaderMismatch); }
                continue;
            };
            total = total.checked_add(name.len()).and_then(|n| n.checked_add(header.len()))
                .ok_or(McpHeaderError::LimitExceeded)?;
            if total > MAX_PARAMETER_HEADER_BLOCK_BYTES { return Err(McpHeaderError::LimitExceeded); }
            let decoded = decode_mcp_header_value(header.as_bytes())?;
            let expected = primitive_value(binding.kind, value)?;
            let equal = match binding.kind {
                ParameterHeaderType::Integer => safe_integer(&decoded).is_some_and(|value| value.to_string() == expected),
                _ => decoded == expected,
            };
            if !equal { return Err(McpHeaderError::HeaderMismatch); }
        }
        Ok(())
    }
}

/// Deliberately redacted in Debug: even a reviewed field can contain private
/// runtime data. Access to wire values is explicit; the original body is never
/// replaced by this projection.
#[derive(PartialEq, Eq)]
pub struct ProjectedParameterHeaders { fields: Vec<(String, String)> }
impl ProjectedParameterHeaders {
    pub fn fields(&self) -> &[(String, String)] { &self.fields }
    pub fn into_fields(self) -> Vec<(String, String)> { self.fields }
}
impl fmt::Debug for ProjectedParameterHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectedParameterHeaders").field("field_count", &self.fields.len()).finish()
    }
}

fn value_at<'a>(mut value: Option<&'a Value>, path: &[String]) -> Option<&'a Value> {
    for key in path { value = value?.as_object()?.get(key); }
    value
}
fn primitive_value(kind: ParameterHeaderType, value: &Value) -> Result<String, McpHeaderError> {
    match (kind, value) {
        (ParameterHeaderType::String, Value::String(value)) if value.len() <= MAX_MCP_HEADER_VALUE_BYTES => Ok(value.clone()),
        (ParameterHeaderType::String, Value::String(_)) => Err(McpHeaderError::LimitExceeded),
        (ParameterHeaderType::Boolean, Value::Bool(value)) => Ok(value.to_string()),
        (ParameterHeaderType::Integer, Value::Number(value)) => {
            struct Lexeme(String);
            impl fmt::Write for Lexeme {
                fn write_str(&mut self, value: &str) -> fmt::Result {
                    if value.len() > MAX_INTEGER_LEXEME_BYTES - self.0.len() { return Err(fmt::Error); }
                    self.0.push_str(value);
                    Ok(())
                }
            }
            let mut source = Lexeme(String::new());
            fmt::write(&mut source, format_args!("{value}")).map_err(|_| McpHeaderError::LimitExceeded)?;
            safe_integer(&source.0).map(|value| value.to_string()).ok_or(McpHeaderError::InvalidParameter)
        }
        _ => Err(McpHeaderError::InvalidParameter),
    }
}

struct Compiler { bindings: Vec<ParameterHeaderBinding>, nodes: usize, path_bytes: usize }
impl Compiler {
    fn visit(&mut self, schema: &Value, path: &mut Vec<String>, reachable: bool, depth: usize) -> Result<(), McpHeaderError> {
        self.nodes += 1;
        if self.nodes > MAX_SCHEMA_ADMISSION_NODES || depth > MAX_SCHEMA_VALIDATION_DEPTH {
            return Err(McpHeaderError::LimitExceeded);
        }
        let Some(schema) = schema.as_object() else { return Ok(()); };
        if let Some(annotation) = schema.get("x-mcp-header") {
            if !reachable || path.is_empty() { return Err(McpHeaderError::UnreachableAnnotation); }
            let suffix = annotation.as_str().ok_or(McpHeaderError::InvalidAnnotation)?;
            if suffix.is_empty() || !suffix.bytes().all(is_tchar) { return Err(McpHeaderError::InvalidAnnotation); }
            if suffix.len() > MAX_PARAMETER_HEADER_NAME_BYTES - PREFIX.len() || self.bindings.len() >= MAX_PARAMETER_HEADERS {
                return Err(McpHeaderError::LimitExceeded);
            }
            let kind = match schema.get("type").and_then(Value::as_str) {
                Some("string") => ParameterHeaderType::String,
                Some("integer") => ParameterHeaderType::Integer,
                Some("boolean") => ParameterHeaderType::Boolean,
                _ => return Err(McpHeaderError::InvalidAnnotation),
            };
            let name = format!("{PREFIX}{suffix}");
            if self.bindings.iter().any(|binding| binding.name.eq_ignore_ascii_case(&name)) {
                return Err(McpHeaderError::DuplicateAnnotation);
            }
            self.path_bytes = path.iter().try_fold(self.path_bytes, |n, key| n.checked_add(key.len()))
                .ok_or(McpHeaderError::LimitExceeded)?;
            if self.path_bytes > MAX_PARAMETER_HEADER_PATH_BYTES { return Err(McpHeaderError::LimitExceeded); }
            self.bindings.push(ParameterHeaderBinding { path: path.clone(), name, kind });
        }
        // Walk schema locations, not literal example/default/enum/const data.
        // Even unused definitions must be checked: an annotation there is not
        // reachable solely through properties and invalidates the tool.
        for (keyword, value) in schema {
            match keyword.as_str() {
                "properties" => if let Some(properties) = value.as_object() {
                    for (name, child) in properties {
                        if name.len() > MAX_PARAMETER_HEADER_PATH_BYTES { return Err(McpHeaderError::LimitExceeded); }
                        path.push(name.clone());
                        let result = self.visit(child, path, reachable, depth + 1);
                        path.pop();
                        result?;
                    }
                },
                "$defs" | "definitions" | "patternProperties" | "dependentSchemas" | "dependencies" => {
                    if let Some(children) = value.as_object() {
                        for child in children.values() { self.visit(child, path, false, depth + 1)?; }
                    }
                }
                "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                    if let Some(children) = value.as_array() {
                        for child in children { self.visit(child, path, false, depth + 1)?; }
                    }
                }
                "items" | "contains" | "additionalProperties" | "unevaluatedProperties"
                | "unevaluatedItems" | "propertyNames" | "not" | "if" | "then" | "else" | "contentSchema" => {
                    self.visit(value, path, false, depth + 1)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// Parse a bounded JSON number mathematically, without f64 rounding. Decimal
// fractions and exponent notation are accepted only when their exact value is
// an integer in the safe range. Header values use the same grammar as the body.
fn safe_integer(source: &str) -> Option<i64> {
    if source.is_empty() || source.len() > MAX_INTEGER_LEXEME_BYTES { return None; }
    let bytes = source.as_bytes();
    let negative = bytes[0] == b'-';
    let mut position = usize::from(negative);
    let mut digits = Vec::with_capacity(bytes.len());
    let first = *bytes.get(position)?;
    if first == b'0' {
        digits.push(0);
        position += 1;
        if bytes.get(position).is_some_and(u8::is_ascii_digit) { return None; }
    } else if (b'1'..=b'9').contains(&first) {
        while let Some(byte) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
            digits.push(*byte - b'0');
            position += 1;
        }
    } else { return None; }
    let mut fraction = 0_i32;
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        while let Some(byte) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
            digits.push(*byte - b'0');
            fraction += 1;
            position += 1;
        }
        if fraction == 0 { return None; }
    }
    let mut exponent = 0_i32;
    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        position += 1;
        let subtract = bytes.get(position) == Some(&b'-');
        if matches!(bytes.get(position), Some(b'+' | b'-')) { position += 1; }
        let start = position;
        while let Some(byte) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
            exponent = exponent.checked_mul(10)?.checked_add(i32::from(*byte - b'0'))?;
            position += 1;
        }
        if position == start { return None; }
        if subtract { exponent = exponent.checked_neg()?; }
    }
    if position != bytes.len() { return None; }
    let Some(first) = digits.iter().position(|digit| *digit != 0) else { return Some(0); };
    let mut digits = &digits[first..];
    let scale = exponent.checked_sub(fraction)?;
    if scale < 0 {
        let remove = usize::try_from(scale.checked_neg()?).ok()?;
        if remove >= digits.len() || digits[digits.len() - remove..].iter().any(|digit| *digit != 0) { return None; }
        digits = &digits[..digits.len() - remove];
    }
    let zeros = usize::try_from(scale.max(0)).ok()?;
    if digits.len().checked_add(zeros)? > 16 { return None; }
    let mut value = 0_i64;
    for digit in digits { value = value.checked_mul(10)?.checked_add(i64::from(*digit))?; }
    for _ in 0..zeros { value = value.checked_mul(10)?; }
    if value > MAX_PARAMETER_HEADER_INTEGER { return None; }
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(schema: Value) -> Result<ToolParameterHeaderPlan, McpHeaderError> {
        AdmittedToolHeaderSchema::admit(schema).map(|schema| schema.plan)
    }
    fn scalar(kind: &str) -> ToolParameterHeaderPlan {
        plan(json!({"type":"object", "properties":{"value":{"type":kind,"x-mcp-header":"Value"}}})).unwrap()
    }

    #[test]
    fn header_values_round_trip_without_injection_or_double_decoding() {
        for value in ["", "us-west1", "inside \t space", "Hello, 世界", " padded ", "line1\nline2", "\0", "=?base64?bGl0ZXJhbA==?="] {
            let encoded = encode_mcp_header_value(value).unwrap();
            assert!(encoded.bytes().all(is_field_value_byte));
            assert_eq!(decode_mcp_header_value(encoded.as_bytes()).unwrap(), value);
        }
        assert_eq!(encode_mcp_header_value(" padded ").unwrap(), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(decode_mcp_header_value(b" \t=?base64?IHBhZGRlZCA=?=\t ").unwrap(), " padded ");
        assert_eq!(decode_mcp_header_value(b"=?BASE64?YWJj?=").unwrap(), "=?BASE64?YWJj?=");
    }

    #[test]
    fn overlapping_sentinel_is_rejected_without_panicking_or_reinterpreting_it() {
        for raw in [b"=?base64?=".as_slice(), b" \t=?base64?=\t "] {
            assert_eq!(decode_mcp_header_value(raw), Err(McpHeaderError::InvalidHeaderValue));
        }
        // The empty encoding has two distinct question marks, not an overlap.
        assert_eq!(decode_mcp_header_value(b"=?base64??="), Ok(String::new()));
        for literal in ["=?base64?=", "=?base64??=", "=?base64?YQ==?="] {
            let encoded = encode_mcp_header_value(literal).unwrap();
            assert_ne!(encoded, literal, "sentinel-looking literals must be escaped");
            assert_eq!(decode_mcp_header_value(encoded.as_bytes()).unwrap(), literal);
        }
    }

    #[test]
    fn encoded_values_round_trip_at_every_base64_padding_boundary() {
        for length in [MAX_MCP_HEADER_VALUE_BYTES - 2, MAX_MCP_HEADER_VALUE_BYTES - 1,
            MAX_MCP_HEADER_VALUE_BYTES] {
            for value in ["\0".repeat(length), format!("{} ", "x".repeat(length - 1))] {
                let encoded = encode_mcp_header_value(&value).unwrap();
                assert_eq!(encoded.len(), SENTINEL_PREFIX.len() + 4 * length.div_ceil(3)
                    + SENTINEL_SUFFIX.len());
                assert!(encoded.len() <= MAX_MCP_ENCODED_HEADER_VALUE_BYTES);
                assert_eq!(decode_mcp_header_value(encoded.as_bytes()).unwrap(), value);
            }
        }
        // The byte immediately beyond the decoded limit can occupy the same
        // Base64 quantum as a valid maximum-length value. Check both bounds.
        let oversized = "\0".repeat(MAX_MCP_HEADER_VALUE_BYTES + 1);
        assert_eq!(encode_mcp_header_value(&oversized), Err(McpHeaderError::LimitExceeded));
        let wire = format!("{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}", STANDARD.encode(oversized));
        assert_eq!(wire.len(), MAX_MCP_ENCODED_HEADER_VALUE_BYTES);
        assert_eq!(decode_mcp_header_value(wire.as_bytes()), Err(McpHeaderError::LimitExceeded));
    }

    #[test]
    fn parameter_mirrors_reject_overlapping_sentinels_without_mutating_arguments() {
        let plan = scalar("string");
        let arguments = json!({"value":"=?base64?="});
        let before = arguments.clone();
        let projected = plan.project(Some(&arguments)).unwrap();
        plan.validate(Some(&arguments), projected.fields()).unwrap();
        assert_eq!(plan.validate(Some(&arguments), &[("Mcp-Param-Value".to_owned(),
            "=?base64?=".to_owned())]), Err(McpHeaderError::InvalidHeaderValue));
        assert_eq!(arguments, before);
    }

    #[test]
    fn maximum_encoded_parameter_projection_is_accepted_by_its_own_validator() {
        let plan = scalar("string");
        let arguments = json!({"value":"\0".repeat(MAX_MCP_HEADER_VALUE_BYTES)});
        let before = arguments.clone();
        let projected = plan.project(Some(&arguments)).unwrap();
        assert_eq!(projected.fields()[0].1.len(), MAX_MCP_ENCODED_HEADER_VALUE_BYTES);
        plan.validate(Some(&arguments), projected.fields()).unwrap();
        assert_eq!(arguments, before);
        let oversized = json!({"value":"\0".repeat(MAX_MCP_HEADER_VALUE_BYTES + 1)});
        assert_eq!(plan.project(Some(&oversized)), Err(McpHeaderError::LimitExceeded));
    }

    #[test]
    fn annotation_admission_preserves_schema_semantics_and_unknown_keyword_refusal() {
        let source = json!({"type":"object","properties":{
            "value":{"type":"integer","minimum":3,"x-mcp-header":"Value"}
        },"additionalProperties":false});
        let admitted = AdmittedToolHeaderSchema::admit(source.clone()).unwrap();
        assert_eq!(admitted.schema(), &source);
        admitted.validate(&json!({"value":3})).unwrap();
        assert!(admitted.validate(&json!({"value":2})).is_err());
        assert!(admitted.validate(&json!({"value":3,"other":true})).is_err());
        let mut invalid = source;
        invalid["properties"]["value"]["unknownValidationKeyword"] = json!(true);
        assert!(matches!(AdmittedToolHeaderSchema::admit(invalid), Err(McpHeaderError::InvalidSchema)));
        let large = json!({"type":"object","default":"x".repeat(MAX_TOOL_HEADER_SCHEMA_BYTES + 1)});
        assert!(matches!(AdmittedToolHeaderSchema::admit(large), Err(McpHeaderError::LimitExceeded)));
    }

    #[test]
    fn header_admission_rejects_raw_non_ascii_controls_and_malformed_sentinels() {
        for raw in [b"x\r\ny".as_slice(), b"\x80", b"\xff", "世界".as_bytes(), b"\x7f", b"=?base64?%%%?=", b"=?base64?/w==?=", b"=?base64?YR==?="] {
            assert!(decode_mcp_header_value(raw).is_err());
        }
        assert!(decode_mcp_header_value(b"=?base64?YQ==?=").is_ok());
        assert_eq!(encode_mcp_header_value(&"x".repeat(MAX_MCP_HEADER_VALUE_BYTES + 1)), Err(McpHeaderError::LimitExceeded));
        assert_eq!(decode_mcp_header_value(&vec![b'x'; MAX_MCP_ENCODED_HEADER_VALUE_BYTES + 1]), Err(McpHeaderError::LimitExceeded));
        let maximum = "界".repeat(MAX_MCP_HEADER_VALUE_BYTES / 3);
        assert_eq!(decode_mcp_header_value(encode_mcp_header_value(&maximum).unwrap().as_bytes()).unwrap(), maximum);
    }

    #[test]
    fn nested_paths_preserve_arguments_and_omit_missing_and_null_without_full_validation() {
        let schema = json!({"type":"object","required":["verbose"],"properties":{
            "verbose":{"type":"boolean","x-mcp-header":"Verbose"},
            "options":{"type":"object","properties":{"a/b~.0":{"type":"string","x-mcp-header":"Region"}}}
        }});
        let admitted = AdmittedToolHeaderSchema::admit(schema.clone()).unwrap();
        assert_eq!(admitted.schema(), &schema);
        let plan = admitted.header_plan();
        let arguments = json!({"verbose":null,"options":{"a/b~.0":"Hello, 世界"},"unrelated":"private"});
        let before = arguments.clone();
        assert!(admitted.validate(&arguments).is_err());
        let fields = plan.project(Some(&arguments)).unwrap();
        assert_eq!(fields.fields().len(), 1);
        assert_eq!(fields.fields()[0].0, "Mcp-Param-Region");
        plan.validate(Some(&arguments), fields.fields()).unwrap();
        assert_eq!(arguments, before);
        assert!(plan.project(Some(&json!({}))).unwrap().fields().is_empty());
        assert!(plan.project(None).unwrap().fields().is_empty());
    }

    #[test]
    fn schema_positions_not_instance_examples_determine_annotation_reachability() {
        let annotated = json!({"type":"string","x-mcp-header":"Value"});
        let property = json!({"type":"object","properties":{"value":annotated}});
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            let mut schema = json!({"type":"object"});
            schema[keyword] = json!([property]);
            assert_eq!(plan(schema), Err(McpHeaderError::UnreachableAnnotation));
        }
        for keyword in ["items", "contains", "additionalProperties", "unevaluatedProperties", "unevaluatedItems", "not", "if", "then", "else", "propertyNames", "contentSchema"] {
            let mut schema = json!({"type":"object"});
            schema[keyword] = property.clone();
            assert_eq!(plan(schema), Err(McpHeaderError::UnreachableAnnotation));
        }
        for keyword in ["$defs", "patternProperties", "dependentSchemas"] {
            let mut schema = json!({"type":"object"});
            schema[keyword] = json!({"hidden":property});
            assert_eq!(plan(schema), Err(McpHeaderError::UnreachableAnnotation));
        }
        let schema = json!({"type":"object", "default":property, "examples":[property], "const":property});
        assert!(plan(schema).unwrap().bindings().is_empty(), "literal data is not a schema annotation");
        assert_eq!(plan(annotated), Err(McpHeaderError::UnreachableAnnotation));
    }

    #[test]
    fn one_invalid_annotation_rejects_the_plan_without_salvaging_valid_bindings() {
        for bad in [json!(""), json!("bad name"), json!("x\r\ny"), json!("世界"), json!(true), json!(null)] {
            let schema = json!({"type":"object","properties":{
                "good":{"type":"string","x-mcp-header":"Good"},
                "bad":{"type":"string","x-mcp-header":bad}
            }});
            assert!(plan(schema).is_err());
        }
        for kind in [json!("number"), json!("array"), json!("object"), json!("null"), json!(["string", "null"])] {
            assert_eq!(plan(json!({"properties":{"v":{"type":kind,"x-mcp-header":"V"}}})), Err(McpHeaderError::InvalidAnnotation));
        }
        assert_eq!(plan(json!({"properties":{"v":{"x-mcp-header":"V"}}})), Err(McpHeaderError::InvalidAnnotation));
        assert_eq!(plan(json!({"properties":{"a":{"type":"string","x-mcp-header":"Region"},"b":{"type":"boolean","x-mcp-header":"rEGION"}}})), Err(McpHeaderError::DuplicateAnnotation));
        let mut properties = serde_json::Map::new();
        for index in 0..=MAX_PARAMETER_HEADERS {
            properties.insert(format!("v{index}"), json!({"type":"string","x-mcp-header":format!("H{index}")}));
        }
        assert_eq!(plan(json!({"type":"object","properties":properties})), Err(McpHeaderError::LimitExceeded));
    }

    #[test]
    fn exact_integers_accept_equivalent_lexemes_without_rounding_fractional_values() {
        for (lexeme, expected) in [("42",42), ("42.0",42), ("4.2e1",42), ("4200e-2",42), ("-0",0), ("0.000e100",0), ("9007199254740991",MAX_PARAMETER_HEADER_INTEGER), ("-9007199254740991",-MAX_PARAMETER_HEADER_INTEGER)] {
            assert_eq!(safe_integer(lexeme), Some(expected), "{lexeme}");
        }
        for lexeme in ["9007199254740992", "-9007199254740992", "9007199254740990.5", "42.00000000000000001", "0.1", "1e100", "1e-100", "01", "+1", "1.", "1e", "NaN", "1e999999999999999"] {
            assert_eq!(safe_integer(lexeme), None, "{lexeme}");
        }
        let plan = scalar("integer");
        let arguments: Value = serde_json::from_str("{\"value\":42.0}").unwrap();
        assert_eq!(plan.project(Some(&arguments)).unwrap().fields()[0].1, "42");
        plan.validate(Some(&arguments), &[("mcp-param-value".to_owned(), "4.2e1".to_owned())]).unwrap();
        assert!(plan.validate(Some(&arguments), &[("Mcp-Param-Value".to_owned(), "42.00000000000000001".to_owned())]).is_err());
        let arguments: Value = serde_json::from_str("{\"value\":9007199254740990.5}").unwrap();
        assert!(plan.project(Some(&arguments)).is_err());
    }

    #[test]
    fn recognized_mirrors_are_case_sensitive_values_and_case_insensitive_singletons() {
        let plan = scalar("string");
        let arguments = json!({"value":"PrivateCanary"});
        let valid = vec![("mCP-pARAM-vALUE".to_owned(), "PrivateCanary".to_owned())];
        plan.validate(Some(&arguments), &valid).unwrap();
        let mut duplicate = valid.clone();
        duplicate.push(("Mcp-Param-Value".to_owned(), "PrivateCanary".to_owned()));
        assert_eq!(plan.validate(Some(&arguments), &duplicate), Err(McpHeaderError::HeaderMismatch));
        assert_eq!(plan.validate(Some(&arguments), &[]), Err(McpHeaderError::HeaderMismatch));
        assert_eq!(plan.validate(Some(&json!({"value":null})), &valid), Err(McpHeaderError::HeaderMismatch));
        assert_eq!(plan.validate(Some(&arguments), &[("Mcp-Param-Value".to_owned(), "privatecanary".to_owned())]), Err(McpHeaderError::HeaderMismatch));
        let mut unknown = valid;
        unknown.push(("Mcp-Param-Unrecognized".to_owned(), "other".to_owned()));
        plan.validate(Some(&arguments), &unknown).unwrap();
        let projected = plan.project(Some(&arguments)).unwrap();
        assert!(!format!("{plan:?} {projected:?} {:?}", plan.bindings()).contains("PrivateCanary"));
    }

    #[test]
    fn wrong_types_and_aggregate_limits_never_return_partial_projection() {
        let plan = scalar("boolean");
        assert_eq!(plan.project(Some(&json!({"value":"true"}))), Err(McpHeaderError::InvalidParameter));
        assert_eq!(plan.project(Some(&json!([true]))), Err(McpHeaderError::InvalidParameter));
        assert_eq!(plan.project(Some(&json!({"value":true}))).unwrap().fields()[0].1, "true");
        let mut properties = serde_json::Map::new();
        let mut arguments = serde_json::Map::new();
        for index in 0..9 {
            properties.insert(format!("v{index}"), json!({"type":"string","x-mcp-header":format!("H{index}")}));
            arguments.insert(format!("v{index}"), Value::String("x".repeat(MAX_MCP_HEADER_VALUE_BYTES)));
        }
        let plan = self::plan(json!({"type":"object","properties":properties})).unwrap();
        assert_eq!(plan.project(Some(&Value::Object(arguments))), Err(McpHeaderError::LimitExceeded));
    }

    #[test]
    fn tool_input_schema_admission_keeps_plain_schemas_and_admits_annotations() {
        let plain = json!({"type":"object","properties":{"region":{"type":"string"}}});
        assert_eq!(admit_final_tool_input_schema(plain.clone()).unwrap().schema(), &plain);
        let annotated = json!({"type":"object","properties":{
            "region":{"type":"string","x-mcp-header":"Region"}
        }});
        let admitted = admit_final_tool_input_schema(annotated).unwrap();
        assert!(!admitted.schema().to_string().contains("x-mcp-header"));
        assert!(admitted.validate(&json!({"region":"eu-west"})).is_ok());
        assert!(admitted.validate(&json!({"region":7})).is_err());
    }

    #[test]
    fn tool_input_schema_admission_refuses_bad_annotations_and_other_keywords() {
        let nullable = json!({"type":"object","properties":{
            "region":{"type":["string","null"],"x-mcp-header":"Region"}
        }});
        assert!(admit_final_tool_input_schema(nullable).is_err());
        let unknown = json!({"type":"object","properties":{
            "region":{"type":"string","x-mcp-header":"Region","x-other":true}
        }});
        assert!(admit_final_tool_input_schema(unknown).is_err());
    }
}
