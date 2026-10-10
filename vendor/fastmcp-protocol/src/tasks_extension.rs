//! Exact wire types for the `io.modelcontextprotocol/tasks` extension.
//!
//! These types deliberately model the final Tasks vocabulary independently of
//! the legacy task runtime. They admit only decoded task identifiers within
//! the protocol's 1 KiB limit; enforcement of the larger pre-unescape raw
//! JSON-token limit remains at the raw-ingress security boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::common_types::{JsonInteger, OpenMetadata};
use crate::jsonrpc::{JSONRPC_VERSION, RequestId};
use crate::messages::{
    FinalCallToolResult, FinalEmbeddedInputKind, FinalEmbeddedInputRequest,
    FinalEmbeddedInputResponse, SubscriptionFilter,
};
use crate::protocol_version::FINAL_PROTOCOL_VERSION;

const SERDE_JSON_RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";

struct WireValue(String);

impl WireValue {
    fn into_json(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for WireValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct WireValueVisitor;

        impl<'de> serde::de::Visitor<'de> for WireValueVisitor {
            type Value = WireValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("any valid JSON value")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let Some(key) = map.next_key::<std::borrow::Cow<'_, str>>()? else {
                    return Err(A::Error::custom("raw JSON value is missing its source"));
                };
                if key != SERDE_JSON_RAW_VALUE_TOKEN {
                    return Err(A::Error::custom("invalid raw JSON value source"));
                }
                let source = map.next_value::<std::borrow::Cow<'_, str>>()?;
                Ok(WireValue(source.into_owned()))
            }

            fn visit_newtype_struct<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
            where
                D2: Deserializer<'de>,
            {
                let value = Value::deserialize(deserializer)?;
                serde_json::to_string(&value)
                    .map(WireValue)
                    .map_err(D2::Error::custom)
            }
        }

        deserializer.deserialize_newtype_struct(SERDE_JSON_RAW_VALUE_TOKEN, WireValueVisitor)
    }
}

/// The negotiated Tasks extension identifier.
pub const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";
/// Exact task-get method name.
pub const TASK_GET: &str = "tasks/get";
/// Exact task-update method name.
pub const TASK_UPDATE: &str = "tasks/update";
/// Exact task-cancel method name.
pub const TASK_CANCEL: &str = "tasks/cancel";
/// Exact task-status notification method name.
pub const TASK_STATUS_NOTIFICATION: &str = "notifications/tasks";
/// Legacy task-link metadata is forbidden inside a Task's inlined result.
pub const RELATED_TASK_META_KEY: &str = "io.modelcontextprotocol/related-task";
/// Maximum decoded UTF-8 bytes in a task ID.
pub const MAX_TASK_ID_BYTES: usize = 1024;
/// Maximum entries retained in either task input map.
pub const MAX_TASK_INPUT_MAP_ENTRIES: usize = 128;
/// Maximum task identifiers retained in one subscription filter fragment.
pub const MAX_TASK_SUBSCRIPTION_IDS: usize = 128;
/// Tasks-owned key composed into a final core subscription filter.
pub const TASK_SUBSCRIPTION_IDS_KEY: &str = "taskIds";

/// Composes the Tasks `taskIds` fragment into a final core subscription filter.
///
/// The core filter remains schema-open and extension-neutral. This function is
/// the sole typed attachment point that assigns Tasks semantics to its
/// `taskIds` member. Request order and duplicates are retained exactly; stream
/// matching applies exact decoded-ID set semantics later.
pub fn set_task_subscription_ids(
    filter: &mut SubscriptionFilter,
    task_ids: Vec<TaskId>,
) -> Result<(), TaskWireError> {
    validate_task_subscription_id_count(task_ids.len())?;
    if filter.additional.contains_key(TASK_SUBSCRIPTION_IDS_KEY) {
        return Err(TaskWireError::Invalid("taskIds"));
    }
    let encoded = serde_json::to_value(task_ids).map_err(|_| TaskWireError::Invalid("taskIds"))?;
    filter
        .additional
        .insert(TASK_SUBSCRIPTION_IDS_KEY.to_owned(), encoded);
    Ok(())
}

/// Decodes the Tasks fragment from a final core subscription filter.
///
/// Absence means that the subscription requests no Task events. An empty array
/// is retained as a present, empty selection and likewise matches no events.
pub fn task_subscription_ids(
    filter: &SubscriptionFilter,
) -> Result<Option<Vec<TaskId>>, TaskWireError> {
    let Some(value) = filter.additional.get(TASK_SUBSCRIPTION_IDS_KEY) else {
        return Ok(None);
    };
    let task_ids: Vec<TaskId> =
        serde_json::from_value(value.clone()).map_err(|_| TaskWireError::Invalid("taskIds"))?;
    validate_task_subscription_id_count(task_ids.len())?;
    Ok(Some(task_ids))
}

fn validate_task_subscription_id_count(count: usize) -> Result<(), TaskWireError> {
    if count > MAX_TASK_SUBSCRIPTION_IDS {
        return Err(TaskWireError::Invalid("taskIds"));
    }
    Ok(())
}

/// A decoded opaque task identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    /// Validates and retains an opaque task identifier byte-for-byte.
    pub fn parse(value: impl Into<String>) -> Result<Self, TaskWireError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_TASK_ID_BYTES {
            return Err(TaskWireError::Invalid("taskId"));
        }
        if value.chars().any(char::is_control) {
            return Err(TaskWireError::Invalid("taskId"));
        }
        Ok(Self(value))
    }

    /// Returns the exact decoded identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|value| Self::parse(value).map_err(serde::de::Error::custom))
    }
}

/// One exact reason a Tasks wire value was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskWireError {
    /// A field was absent, malformed, out of range, or inconsistent.
    Invalid(&'static str),
    /// An embedded input response did not match its request descriptor.
    InputResponseKind,
}

impl fmt::Display for TaskWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid {field}"),
            Self::InputResponseKind => {
                formatter.write_str("task input response kind does not match request")
            }
        }
    }
}

impl std::error::Error for TaskWireError {}

/// A canonical positive integer millisecond duration.
///
/// The composed Tasks profile accepts only positive mathematical JSON
/// integers that fit the local `u64` millisecond domain. Equivalent integral
/// input spellings are emitted as canonical integer text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDuration(String);

/// Failure while converting a final task duration for a bounded local runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskDurationConversionError {
    /// The wire value cannot be represented as a positive `u64` millisecond
    /// duration.
    RuntimeOutOfRange,
}

impl fmt::Display for TaskDurationConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("task duration cannot be represented as a positive u64 millisecond duration")
    }
}

impl std::error::Error for TaskDurationConversionError {}

impl TaskDuration {
    /// Admits and canonicalizes a positive JSON mathematical integer.
    pub fn from_json_integer(value: JsonInteger) -> Result<Self, TaskWireError> {
        let millis = task_duration_runtime_millis(value.as_str())
            .map_err(|_| TaskWireError::Invalid("duration"))?;
        if millis == 0 {
            return Err(TaskWireError::Invalid("duration"));
        }
        Ok(Self(millis.to_string()))
    }

    /// Returns the canonical positive-integer wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Converts this wire value into the bounded local millisecond domain.
    ///
    /// This is intentionally checked: the final Tasks schema has no maximum,
    /// while local duration APIs accept at most `u64` milliseconds.
    pub fn try_as_millis(&self) -> Result<u64, TaskDurationConversionError> {
        self.0
            .parse()
            .map_err(|_| TaskDurationConversionError::RuntimeOutOfRange)
    }
}

impl Serialize for TaskDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = self.try_as_millis().map_err(serde::ser::Error::custom)?;
        serializer.serialize_u64(millis)
    }
}

impl<'de> Deserialize<'de> for TaskDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        JsonInteger::deserialize(deserializer)
            .and_then(|value| Self::from_json_integer(value).map_err(serde::de::Error::custom))
    }
}

fn task_duration_runtime_millis(lexeme: &str) -> Result<u64, TaskDurationConversionError> {
    if lexeme.starts_with('-') {
        return Err(TaskDurationConversionError::RuntimeOutOfRange);
    }
    let (mantissa, exponent) = match lexeme.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent),
        None => (lexeme, "0"),
    };
    let exponent = exponent
        .parse::<i128>()
        .map_err(|_| TaskDurationConversionError::RuntimeOutOfRange)?;
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let fraction_len = i128::try_from(fraction.len())
        .map_err(|_| TaskDurationConversionError::RuntimeOutOfRange)?;
    let decimal_shift = exponent
        .checked_sub(fraction_len)
        .ok_or(TaskDurationConversionError::RuntimeOutOfRange)?;
    let integer_digits = if decimal_shift >= 0 {
        let zeroes = usize::try_from(decimal_shift)
            .map_err(|_| TaskDurationConversionError::RuntimeOutOfRange)?;
        let length = digits
            .len()
            .checked_add(zeroes)
            .ok_or(TaskDurationConversionError::RuntimeOutOfRange)?;
        if length > 20 {
            return Err(TaskDurationConversionError::RuntimeOutOfRange);
        }
        format!("{digits}{}", "0".repeat(zeroes))
    } else {
        let trimmed = usize::try_from(decimal_shift.unsigned_abs())
            .map_err(|_| TaskDurationConversionError::RuntimeOutOfRange)?;
        let Some(length) = digits.len().checked_sub(trimmed) else {
            return Err(TaskDurationConversionError::RuntimeOutOfRange);
        };
        if !digits[length..].bytes().all(|byte| byte == b'0') {
            return Err(TaskDurationConversionError::RuntimeOutOfRange);
        }
        digits[..length].to_owned()
    };
    let normalized = integer_digits.trim_start_matches('0');
    if normalized.is_empty() {
        return Ok(0);
    }
    if normalized.len() > 20 {
        return Err(TaskDurationConversionError::RuntimeOutOfRange);
    }
    normalized
        .parse()
        .map_err(|_| TaskDurationConversionError::RuntimeOutOfRange)
}

/// A strict final task timestamp retained in canonical final spelling.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TaskTimestamp(String);

impl TaskTimestamp {
    /// Parses an exact final timestamp, preserving only its canonical UTC form.
    pub fn parse(value: impl Into<String>) -> Result<Self, TaskWireError> {
        let value = value.into();
        if value.len() > 64
            || !value.is_ascii()
            || value.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
        {
            return Err(TaskWireError::Invalid("timestamp"));
        }
        let bytes = value.as_bytes();
        if bytes.len() < 20
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
        {
            return Err(TaskWireError::Invalid("timestamp"));
        }
        let year = decimal(&bytes[0..4])?;
        let month = decimal(&bytes[5..7])?;
        let day = decimal(&bytes[8..10])?;
        let hour = decimal(&bytes[11..13])?;
        let minute = decimal(&bytes[14..16])?;
        let second = decimal(&bytes[17..19])?;
        if year == 0
            || month == 0
            || month > 12
            || day == 0
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(TaskWireError::Invalid("timestamp"));
        }
        let zone_index = if bytes.get(19) == Some(&b'.') {
            let mut index = 20;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if index == 20 || index - 20 > 9 {
                return Err(TaskWireError::Invalid("timestamp"));
            }
            index
        } else {
            19
        };
        if bytes.get(zone_index) == Some(&b'Z') && bytes.len() == zone_index + 1 {
            Ok(Self(value))
        } else if matches!(bytes.get(zone_index), Some(b'+' | b'-'))
            && bytes.len() == zone_index + 6
            && bytes[zone_index + 3] == b':'
        {
            let offset_hour = decimal(&bytes[zone_index + 1..zone_index + 3])?;
            let offset_minute = decimal(&bytes[zone_index + 4..zone_index + 6])?;
            if offset_hour > 23
                || offset_minute > 59
                || (bytes[zone_index] == b'-' && offset_hour == 0 && offset_minute == 0)
            {
                return Err(TaskWireError::Invalid("timestamp"));
            }
            Ok(Self(value))
        } else {
            Err(TaskWireError::Invalid("timestamp"))
        }
    }

    /// Returns the exact admitted timestamp spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TaskTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|value| Self::parse(value).map_err(serde::de::Error::custom))
    }
}

fn decimal(bytes: &[u8]) -> Result<u32, TaskWireError> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return Err(TaskWireError::Invalid("timestamp"));
    }
    bytes
        .iter()
        .try_fold(0_u32, |number, byte| {
            number
                .checked_mul(10)
                .and_then(|number| number.checked_add(u32::from(*byte - b'0')))
        })
        .ok_or(TaskWireError::Invalid("timestamp"))
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// Exact task lifecycle status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

/// Typed descriptor map carried by an input-required task.
pub type TaskInputRequests = BTreeMap<String, FinalEmbeddedInputRequest>;
/// Typed result map carried by a task update.
pub type TaskInputResponses = BTreeMap<String, FinalEmbeddedInputResponse>;

/// Correlates input responses with the exact embedded descriptors that requested them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskInputLedger(BTreeMap<String, FinalEmbeddedInputKind>);

impl TaskInputLedger {
    /// Records every request kind in an input-required task.
    pub fn from_requests(requests: &TaskInputRequests) -> Result<Self, TaskWireError> {
        if requests.len() > MAX_TASK_INPUT_MAP_ENTRIES {
            return Err(TaskWireError::Invalid("inputRequests"));
        }
        Ok(Self(
            requests
                .iter()
                .map(|(key, request)| (key.clone(), request.response_kind()))
                .collect(),
        ))
    }
    /// Requires each supplied response for an outstanding key to match its
    /// request descriptor.
    ///
    /// Task updates may acknowledge a strict subset of outstanding requests;
    /// the task remains `input_required` until all required input arrives.
    /// Unknown or already-satisfied keys are intentionally ignored by this
    /// shape check; the state machine filters them before mutating its ledger.
    pub fn validate_responses(&self, responses: &TaskInputResponses) -> Result<(), TaskWireError> {
        if responses.len() > MAX_TASK_INPUT_MAP_ENTRIES {
            return Err(TaskWireError::Invalid("inputResponses"));
        }
        for (key, response) in responses {
            let Some(kind) = self.0.get(key) else {
                continue;
            };
            if !response.matches_kind(*kind) {
                return Err(TaskWireError::InputResponseKind);
            }
        }
        Ok(())
    }
}

/// Open-preserving final inner error used only by failed Tasks.
#[derive(Clone, Debug, PartialEq)]
pub struct FinalTaskError {
    pub code: JsonInteger,
    pub message: String,
    pub data: Option<Value>,
    pub additional: BTreeMap<String, Value>,
}

impl Serialize for FinalTaskError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut members = Map::new();
        members.insert(
            "code".to_owned(),
            serde_json::to_value(&self.code).map_err(serde::ser::Error::custom)?,
        );
        members.insert("message".to_owned(), Value::String(self.message.clone()));
        if let Some(data) = &self.data {
            members.insert("data".to_owned(), data.clone());
        }
        for (key, value) in &self.additional {
            if members.insert(key.clone(), value.clone()).is_some() {
                return Err(serde::ser::Error::custom(
                    "task error additional member collides",
                ));
            }
        }
        Value::Object(members).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FinalTaskError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Value::Object(mut members) = Value::deserialize(deserializer)? else {
            return Err(D::Error::custom("task error must be an object"));
        };
        let code = serde_json::from_value(
            members
                .remove("code")
                .ok_or_else(|| D::Error::custom("task error requires code"))?,
        )
        .map_err(D::Error::custom)?;
        let message = serde_json::from_value(
            members
                .remove("message")
                .ok_or_else(|| D::Error::custom("task error requires message"))?,
        )
        .map_err(D::Error::custom)?;
        Ok(Self {
            code,
            message,
            data: members.remove("data"),
            additional: members.into_iter().collect(),
        })
    }
}

/// The final nested `tools/call` result required by a completed Task.
#[derive(Clone, Debug)]
pub struct FinalTaskCallToolResult {
    pub result: FinalCallToolResult,
    pub meta: Option<OpenMetadata>,
    pub additional: BTreeMap<String, Value>,
    /// Exact nested result object retained after peer admission. This is
    /// intentionally private: any public-field mutation falls back to the
    /// canonical local form instead of replaying stale peer state.
    exact_wire: Option<String>,
}

impl FinalTaskCallToolResult {
    fn decode_exact_wire(wire: &str) -> Result<Self, TaskWireError> {
        let Value::Object(mut members) =
            serde_json::from_str(wire).map_err(|_| TaskWireError::Invalid("result"))?
        else {
            return Err(TaskWireError::Invalid("result"));
        };
        if members.contains_key("resultType") {
            return Err(TaskWireError::Invalid("result"));
        }
        let meta: Option<OpenMetadata> = members
            .remove("_meta")
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| TaskWireError::Invalid("result"))?;
        if meta
            .as_ref()
            .is_some_and(|meta| meta.entries().contains_key(RELATED_TASK_META_KEY))
        {
            return Err(TaskWireError::Invalid("result"));
        }
        let mut result_members = Map::new();
        for key in ["content", "isError", "structuredContent"] {
            if let Some(value) = members.remove(key) {
                result_members.insert(key.to_owned(), value);
            }
        }
        let result = serde_json::from_value(Value::Object(result_members))
            .map_err(|_| TaskWireError::Invalid("result"))?;
        Ok(Self {
            result,
            meta,
            additional: members.into_iter().collect(),
            exact_wire: Some(wire.to_owned()),
        })
    }

    fn canonical_value(&self) -> Result<Value, TaskWireError> {
        let Value::Object(mut members) =
            serde_json::to_value(&self.result).map_err(|_| TaskWireError::Invalid("result"))?
        else {
            return Err(TaskWireError::Invalid("result"));
        };
        if let Some(meta) = &self.meta {
            if meta.entries().contains_key(RELATED_TASK_META_KEY) {
                return Err(TaskWireError::Invalid("result"));
            }
            members.insert(
                "_meta".to_owned(),
                serde_json::to_value(meta).map_err(|_| TaskWireError::Invalid("result"))?,
            );
        }
        for (key, value) in &self.additional {
            if key == TASK_MISSING_RESULT_TYPE_DIAGNOSTIC {
                continue;
            }
            if members.insert(key.clone(), value.clone()).is_some() {
                return Err(TaskWireError::Invalid("result"));
            }
        }
        Ok(Value::Object(members))
    }
}

fn json_values_equal_ignoring_object_order(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left_value)| {
                    right.get(key).is_some_and(|right_value| {
                        json_values_equal_ignoring_object_order(left_value, right_value)
                    })
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_values_equal_ignoring_object_order(left, right))
        }
        _ => left == right,
    }
}

impl Serialize for FinalTaskCallToolResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let canonical = self.canonical_value().map_err(serde::ser::Error::custom)?;
        if let Some(exact_wire) = &self.exact_wire {
            if serde_json::from_str::<Value>(exact_wire)
                .ok()
                .as_ref()
                .is_some_and(|exact| json_values_equal_ignoring_object_order(exact, &canonical))
            {
                return RawValue::from_string(exact_wire.clone())
                    .map_err(serde::ser::Error::custom)?
                    .serialize(serializer);
            }
        }
        canonical.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FinalTaskCallToolResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireValue::deserialize(deserializer)?.into_json();
        Self::decode_exact_wire(&wire).map_err(D::Error::custom)
    }
}

/// Shared final task fields.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskBase {
    #[serde(rename = "taskId")]
    pub task_id: TaskId,
    pub status: TaskStatus,
    #[serde(
        rename = "statusMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub status_message: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: TaskTimestamp,
    #[serde(rename = "lastUpdatedAt")]
    pub last_updated_at: TaskTimestamp,
    #[serde(rename = "ttlMs")]
    pub ttl_ms: Option<TaskDuration>,
    #[serde(
        rename = "pollIntervalMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub poll_interval_ms: Option<TaskDuration>,
}

/// Presence state for a required, nullable `ttlMs` member.
///
/// Serde routes an absent member through `Default`, while a present JSON null
/// invokes `deserialize_option`. Keeping those states separate makes omission,
/// unlimited TTL, and a finite TTL unambiguous at the wire boundary.
enum RequiredTaskTtl {
    /// The `ttlMs` member was absent.
    Omitted,
    /// The `ttlMs` member was present with JSON null.
    Unlimited,
    /// The `ttlMs` member was present with a schema-valid JSON number.
    Limited(TaskDuration),
}

impl Default for RequiredTaskTtl {
    fn default() -> Self {
        Self::Omitted
    }
}

impl<'de> Deserialize<'de> for RequiredTaskTtl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<TaskDuration>::deserialize(deserializer).map(|value| match value {
            Some(duration) => Self::Limited(duration),
            None => Self::Unlimited,
        })
    }
}

struct OptionalTaskField<T>(Option<T>);

impl<T> Default for OptionalTaskField<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<'de, T> Deserialize<'de> for OptionalTaskField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskBaseWire {
    #[serde(rename = "taskId")]
    task_id: TaskId,
    status: TaskStatus,
    #[serde(rename = "statusMessage", default)]
    status_message: OptionalTaskField<String>,
    #[serde(rename = "createdAt")]
    created_at: TaskTimestamp,
    #[serde(rename = "lastUpdatedAt")]
    last_updated_at: TaskTimestamp,
    #[serde(rename = "ttlMs", default)]
    ttl_ms: RequiredTaskTtl,
    #[serde(rename = "pollIntervalMs", default)]
    poll_interval_ms: OptionalTaskField<TaskDuration>,
}

impl<'de> Deserialize<'de> for TaskBase {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TaskBaseWire::deserialize(deserializer)?;
        Ok(Self {
            task_id: wire.task_id,
            status: wire.status,
            status_message: wire.status_message.0,
            created_at: wire.created_at,
            last_updated_at: wire.last_updated_at,
            ttl_ms: match wire.ttl_ms {
                RequiredTaskTtl::Omitted => {
                    return Err(D::Error::custom(
                        "task requires an explicit ttlMs member; use null for unlimited",
                    ));
                }
                RequiredTaskTtl::Unlimited => None,
                RequiredTaskTtl::Limited(duration) => Some(duration),
            },
            poll_interval_ms: wire.poll_interval_ms.0,
        })
    }
}

/// Status-discriminated final Task payload.
#[derive(Clone, Debug)]
pub enum Task {
    Working(TaskBase),
    InputRequired {
        base: TaskBase,
        input_requests: TaskInputRequests,
    },
    Completed {
        base: TaskBase,
        result: FinalTaskCallToolResult,
    },
    Failed {
        base: TaskBase,
        error: FinalTaskError,
    },
    Cancelled(TaskBase),
}

impl Task {
    /// Returns common fields.
    #[must_use]
    pub fn base(&self) -> &TaskBase {
        match self {
            Self::Working(base) | Self::Cancelled(base) => base,
            Self::InputRequired { base, .. }
            | Self::Completed { base, .. }
            | Self::Failed { base, .. } => base,
        }
    }
}

impl Serialize for Task {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        serialize_task_members(&mut map, self)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireValue::deserialize(deserializer)?.into_json();
        decode_task_wire(&wire).map_err(D::Error::custom)
    }
}

/// Decodes a standalone Task from its admitted source. Completed Tasks keep
/// the exact nested `tools/call` result so peer member order and numeric
/// lexemes are not reconstructed through `serde_json::Value`.
fn decode_task_wire(wire: &str) -> Result<Task, TaskWireError> {
    let Value::Object(members) =
        serde_json::from_str(wire).map_err(|_| TaskWireError::Invalid("task"))?
    else {
        return Err(TaskWireError::Invalid("task"));
    };
    decode_task_members(wire, members)
}

fn decode_task_members(wire: &str, mut members: Map<String, Value>) -> Result<Task, TaskWireError> {
    let status: TaskStatus = serde_json::from_value(
        members
            .get("status")
            .cloned()
            .ok_or(TaskWireError::Invalid("status"))?,
    )
    .map_err(|_| TaskWireError::Invalid("status"))?;
    let special = match status {
        TaskStatus::InputRequired => Some(("inputRequests", members.remove("inputRequests"))),
        TaskStatus::Completed => Some(("result", members.remove("result"))),
        TaskStatus::Failed => Some(("error", members.remove("error"))),
        TaskStatus::Working | TaskStatus::Cancelled => None,
    };
    if matches!(status, TaskStatus::Working | TaskStatus::Cancelled)
        && (members.contains_key("inputRequests")
            || members.contains_key("result")
            || members.contains_key("error"))
    {
        return Err(TaskWireError::Invalid("task"));
    }
    let base: TaskBase = serde_json::from_value(Value::Object(members))
        .map_err(|_| TaskWireError::Invalid("task"))?;
    match (status, special) {
        (TaskStatus::Working, _) => Ok(Task::Working(base)),
        (TaskStatus::Cancelled, _) => Ok(Task::Cancelled(base)),
        (TaskStatus::InputRequired, Some((_, Some(value)))) => {
            let input_requests = serde_json::from_value(value)
                .map_err(|_| TaskWireError::Invalid("inputRequests"))?;
            TaskInputLedger::from_requests(&input_requests)?;
            Ok(Task::InputRequired {
                base,
                input_requests,
            })
        }
        (TaskStatus::Completed, Some((_, Some(_)))) => {
            let result =
                FinalTaskCallToolResult::decode_exact_wire(raw_completed_task_result(wire)?.get())?;
            Ok(Task::Completed { base, result })
        }
        (TaskStatus::Failed, Some((_, Some(value)))) => {
            let error =
                serde_json::from_value(value).map_err(|_| TaskWireError::Invalid("error"))?;
            Ok(Task::Failed { base, error })
        }
        _ => Err(TaskWireError::Invalid("task")),
    }
}

fn serialize_task_members<M>(map: &mut M, task: &Task) -> Result<(), M::Error>
where
    M: SerializeMap,
{
    let base = task.base();
    map.serialize_entry("taskId", &base.task_id)?;
    map.serialize_entry("status", &base.status)?;
    if let Some(status_message) = &base.status_message {
        map.serialize_entry("statusMessage", status_message)?;
    }
    map.serialize_entry("createdAt", &base.created_at)?;
    map.serialize_entry("lastUpdatedAt", &base.last_updated_at)?;
    map.serialize_entry("ttlMs", &base.ttl_ms)?;
    if let Some(poll_interval_ms) = &base.poll_interval_ms {
        map.serialize_entry("pollIntervalMs", poll_interval_ms)?;
    }
    match task {
        Task::InputRequired { input_requests, .. } => {
            map.serialize_entry("inputRequests", input_requests)?;
        }
        Task::Completed { result, .. } => map.serialize_entry("result", result)?,
        Task::Failed { error, .. } => map.serialize_entry("error", error)?,
        Task::Working(_) | Task::Cancelled(_) => {}
    }
    Ok(())
}

/// Borrows the raw nested result member directly from an admitted Task wire
/// object. `RawValue` retains the exact peer substring, including escaped
/// object-member and string spellings, until the completed-result codec owns
/// a stable copy for later lossless re-emission.
fn raw_completed_task_result(wire: &str) -> Result<&RawValue, TaskWireError> {
    #[derive(Deserialize)]
    struct RawTaskResult<'a> {
        #[serde(borrow)]
        result: Option<&'a RawValue>,
    }

    serde_json::from_str::<RawTaskResult<'_>>(wire)
        .map_err(|_| TaskWireError::Invalid("result"))?
        .result
        .ok_or(TaskWireError::Invalid("result"))
}

/// Required final request parameters shared by all task methods.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRequestMeta {
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
}

impl TaskRequestMeta {
    fn validate(&self) -> Result<(), TaskWireError> {
        if self
            .meta
            .protocol_version()
            .map_err(|_| TaskWireError::Invalid("_meta"))?
            != Some(FINAL_PROTOCOL_VERSION)
            || self
                .meta
                .client_capabilities()
                .map_err(|_| TaskWireError::Invalid("_meta"))?
                .is_none()
        {
            return Err(TaskWireError::Invalid("_meta"));
        }
        Ok(())
    }
}

/// Parameters for `tasks/get`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetTaskParams {
    #[serde(flatten)]
    pub request: TaskRequestMeta,
    #[serde(rename = "taskId")]
    pub task_id: TaskId,
}
/// Parameters for `tasks/cancel`.
pub type CancelTaskParams = GetTaskParams;
/// Parameters for `tasks/update`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTaskParams {
    #[serde(flatten)]
    pub request: TaskRequestMeta,
    #[serde(rename = "taskId")]
    pub task_id: TaskId,
    #[serde(rename = "inputResponses")]
    pub input_responses: TaskInputResponses,
}

/// JSON-RPC task request wire value. Its `method` is validated by [`TaskMethodRequest::decode`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskMethodRequest<P> {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    pub params: P,
}

impl<P> TaskMethodRequest<P> {
    /// Creates an exact JSON-RPC request with this method's fixed wire name.
    #[must_use]
    pub fn new(id: RequestId, method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            method: method.into(),
            params,
        }
    }
}

impl TaskMethodRequest<GetTaskParams> {
    /// Decodes and validates `tasks/get`.
    pub fn decode(value: Value) -> Result<Self, TaskWireError> {
        let request: Self =
            serde_json::from_value(value).map_err(|_| TaskWireError::Invalid("tasks/get"))?;
        if request.jsonrpc != JSONRPC_VERSION || request.method != TASK_GET {
            return Err(TaskWireError::Invalid("tasks/get"));
        }
        request.params.request.validate()?;
        Ok(request)
    }
}
impl TaskMethodRequest<CancelTaskParams> {
    /// Decodes and validates `tasks/cancel`.
    pub fn decode_cancel(value: Value) -> Result<Self, TaskWireError> {
        let request: Self =
            serde_json::from_value(value).map_err(|_| TaskWireError::Invalid("tasks/cancel"))?;
        if request.jsonrpc != JSONRPC_VERSION || request.method != TASK_CANCEL {
            return Err(TaskWireError::Invalid("tasks/cancel"));
        }
        request.params.request.validate()?;
        Ok(request)
    }
}
impl TaskMethodRequest<UpdateTaskParams> {
    /// Decodes and validates `tasks/update` against its input ledger.
    pub fn decode_update(value: Value, ledger: &TaskInputLedger) -> Result<Self, TaskWireError> {
        let request: Self =
            serde_json::from_value(value).map_err(|_| TaskWireError::Invalid("tasks/update"))?;
        if request.jsonrpc != JSONRPC_VERSION || request.method != TASK_UPDATE {
            return Err(TaskWireError::Invalid("tasks/update"));
        }
        request.params.request.validate()?;
        ledger.validate_responses(&request.params.input_responses)?;
        Ok(request)
    }
}

/// The `tasks/create` result envelope.
#[derive(Clone, Debug)]
pub struct CreateTaskResult {
    pub task: Task,
    pub meta: Option<OpenMetadata>,
    pub additional: BTreeMap<String, Value>,
}

impl Serialize for CreateTaskResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut members = serializer.serialize_map(None)?;
        members.serialize_entry("resultType", "task")?;
        serialize_task_members(&mut members, &self.task)?;
        if let Some(meta) = &self.meta {
            members.serialize_entry("_meta", meta)?;
        }
        for (key, value) in &self.additional {
            if [
                "resultType",
                "taskId",
                "status",
                "statusMessage",
                "createdAt",
                "lastUpdatedAt",
                "ttlMs",
                "pollIntervalMs",
                "inputRequests",
                "result",
                "error",
                "_meta",
            ]
            .contains(&key.as_str())
            {
                return Err(serde::ser::Error::custom(
                    "task create additional member collides",
                ));
            }
            members.serialize_entry(key, value)?;
        }
        members.end()
    }
}
impl<'de> Deserialize<'de> for CreateTaskResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireValue::deserialize(deserializer)?.into_json();
        Self::decode_exact_wire(&wire).map_err(D::Error::custom)
    }
}

impl CreateTaskResult {
    /// Decodes a Tasks `tools/call` result from its admitted source JSON.
    ///
    /// The nested completed-task result retains its source member order and
    /// numeric lexemes. Callers that already retain response source must use
    /// this entry point instead of routing through `serde_json::Value`.
    pub(crate) fn decode_exact_wire(wire: &str) -> Result<Self, TaskWireError> {
        let Value::Object(mut members) =
            serde_json::from_str(wire).map_err(|_| TaskWireError::Invalid("task"))?
        else {
            return Err(TaskWireError::Invalid("task"));
        };
        if members.remove("resultType") != Some(Value::String("task".to_owned())) {
            return Err(TaskWireError::Invalid("resultType"));
        }
        let meta = members
            .remove("_meta")
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| TaskWireError::Invalid("_meta"))?;
        let task_fields: BTreeSet<&str> = [
            "taskId",
            "status",
            "statusMessage",
            "createdAt",
            "lastUpdatedAt",
            "ttlMs",
            "pollIntervalMs",
            "inputRequests",
            "result",
            "error",
        ]
        .into_iter()
        .collect();
        let task_members: Map<String, Value> = members
            .iter()
            .filter(|(key, _)| task_fields.contains(key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let task = decode_task_members(wire, task_members)?;
        members.retain(|key, _| !task_fields.contains(key.as_str()));
        Ok(Self {
            task,
            meta,
            additional: members.into_iter().collect(),
        })
    }
}

/// Final `complete` result envelope used by get, update, and cancel.
#[derive(Clone, Debug)]
pub struct CompleteTaskResult {
    pub task: Task,
    pub meta: Option<OpenMetadata>,
    pub additional: BTreeMap<String, Value>,
}

/// Bounded compatibility diagnostics for peer task result envelopes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskResultPeerDiagnostic {
    /// A final Tasks peer omitted `resultType`; it was admitted as `complete`
    /// for compatibility, while local emission remains explicit.
    MissingResultType,
}

const TASK_MISSING_RESULT_TYPE_DIAGNOSTIC: &str = "\0fastmcp/tasks/missing-result-type";

fn result_type_diagnostic(
    additional: &BTreeMap<String, Value>,
) -> Option<TaskResultPeerDiagnostic> {
    additional
        .contains_key(TASK_MISSING_RESULT_TYPE_DIAGNOSTIC)
        .then_some(TaskResultPeerDiagnostic::MissingResultType)
}

fn remove_complete_result_type(
    members: &mut Map<String, Value>,
) -> Result<Option<TaskResultPeerDiagnostic>, TaskWireError> {
    match members.remove("resultType") {
        None => Ok(Some(TaskResultPeerDiagnostic::MissingResultType)),
        Some(Value::String(value)) if value == "complete" => Ok(None),
        Some(_) => Err(TaskWireError::Invalid("resultType")),
    }
}

/// Exact empty `complete` acknowledgement used by `tasks/update` and
/// `tasks/cancel`.
#[derive(Clone, Debug, Default)]
pub struct EmptyTaskResult {
    /// Optional final result metadata.
    pub meta: Option<OpenMetadata>,
    /// Schema-open result siblings retained without authority.
    pub additional: BTreeMap<String, Value>,
}

impl Serialize for EmptyTaskResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut members = Map::new();
        members.insert(
            "resultType".to_owned(),
            Value::String("complete".to_owned()),
        );
        if let Some(meta) = &self.meta {
            members.insert(
                "_meta".to_owned(),
                serde_json::to_value(meta).map_err(serde::ser::Error::custom)?,
            );
        }
        for (key, value) in &self.additional {
            if key == TASK_MISSING_RESULT_TYPE_DIAGNOSTIC {
                continue;
            }
            if members.insert(key.clone(), value.clone()).is_some() {
                return Err(serde::ser::Error::custom(
                    "task acknowledgement additional member collides",
                ));
            }
        }
        Value::Object(members).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EmptyTaskResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Value::Object(mut members) = Value::deserialize(deserializer)? else {
            return Err(D::Error::custom("task acknowledgement must be an object"));
        };
        let diagnostic = remove_complete_result_type(&mut members).map_err(D::Error::custom)?;
        let meta = members
            .remove("_meta")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?;
        if diagnostic.is_some() {
            members.insert(
                TASK_MISSING_RESULT_TYPE_DIAGNOSTIC.to_owned(),
                Value::Bool(true),
            );
        }
        Ok(Self {
            meta,
            additional: members.into_iter().collect(),
        })
    }
}

/// `tasks/get`'s detailed `complete` result.
pub type GetTaskResult = CompleteTaskResult;
/// `tasks/update`'s empty `complete` acknowledgement.
pub type UpdateTaskResult = EmptyTaskResult;
/// `tasks/cancel`'s empty `complete` acknowledgement.
pub type CancelTaskResult = EmptyTaskResult;
impl Serialize for CompleteTaskResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut members = serializer.serialize_map(None)?;
        members.serialize_entry("resultType", "complete")?;
        serialize_task_members(&mut members, &self.task)?;
        if let Some(meta) = &self.meta {
            members.serialize_entry("_meta", meta)?;
        }
        for (key, value) in &self.additional {
            if key == TASK_MISSING_RESULT_TYPE_DIAGNOSTIC {
                continue;
            }
            if [
                "resultType",
                "taskId",
                "status",
                "statusMessage",
                "createdAt",
                "lastUpdatedAt",
                "ttlMs",
                "pollIntervalMs",
                "inputRequests",
                "result",
                "error",
                "_meta",
            ]
            .contains(&key.as_str())
            {
                return Err(serde::ser::Error::custom(
                    "task complete additional member collides",
                ));
            }
            members.serialize_entry(key, value)?;
        }
        members.end()
    }
}
impl<'de> Deserialize<'de> for CompleteTaskResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireValue::deserialize(deserializer)?.into_json();
        let Value::Object(mut members) = serde_json::from_str(&wire).map_err(D::Error::custom)?
        else {
            return Err(D::Error::custom("task result must be an object"));
        };
        let diagnostic = remove_complete_result_type(&mut members).map_err(D::Error::custom)?;
        let meta = members
            .remove("_meta")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?;
        let task_fields: BTreeSet<&str> = [
            "taskId",
            "status",
            "statusMessage",
            "createdAt",
            "lastUpdatedAt",
            "ttlMs",
            "pollIntervalMs",
            "inputRequests",
            "result",
            "error",
        ]
        .into_iter()
        .collect();
        let task_members: Map<String, Value> = members
            .iter()
            .filter(|(key, _)| task_fields.contains(key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let task = decode_task_members(&wire, task_members).map_err(D::Error::custom)?;
        members.retain(|key, _| !task_fields.contains(key.as_str()));
        if diagnostic.is_some() {
            members.insert(
                TASK_MISSING_RESULT_TYPE_DIAGNOSTIC.to_owned(),
                Value::Bool(true),
            );
        }
        Ok(Self {
            task,
            meta,
            additional: members.into_iter().collect(),
        })
    }
}

impl CompleteTaskResult {
    /// Returns the bounded compatibility diagnostic attached during peer decode.
    #[must_use]
    pub fn peer_diagnostic(&self) -> Option<TaskResultPeerDiagnostic> {
        result_type_diagnostic(&self.additional)
    }
}

impl EmptyTaskResult {
    /// Returns the bounded compatibility diagnostic attached during peer decode.
    #[must_use]
    pub fn peer_diagnostic(&self) -> Option<TaskResultPeerDiagnostic> {
        result_type_diagnostic(&self.additional)
    }
}

/// Parameters carried by `notifications/tasks`.
#[derive(Clone, Debug)]
pub struct TaskStatusNotificationParams {
    pub task: Task,
    pub meta: Option<OpenMetadata>,
    pub additional: BTreeMap<String, Value>,
}
impl Serialize for TaskStatusNotificationParams {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut members = serializer.serialize_map(None)?;
        serialize_task_members(&mut members, &self.task)?;
        if let Some(meta) = &self.meta {
            members.serialize_entry("_meta", meta)?;
        }
        for (key, value) in &self.additional {
            if [
                "taskId",
                "status",
                "statusMessage",
                "createdAt",
                "lastUpdatedAt",
                "ttlMs",
                "pollIntervalMs",
                "inputRequests",
                "result",
                "error",
                "_meta",
            ]
            .contains(&key.as_str())
            {
                return Err(serde::ser::Error::custom(
                    "task notification additional member collides",
                ));
            }
            members.serialize_entry(key, value)?;
        }
        members.end()
    }
}
impl<'de> Deserialize<'de> for TaskStatusNotificationParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireValue::deserialize(deserializer)?.into_json();
        let Value::Object(mut members) = serde_json::from_str(&wire).map_err(D::Error::custom)?
        else {
            return Err(D::Error::custom(
                "task notification params must be an object",
            ));
        };
        let meta = members
            .remove("_meta")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?;
        let task_keys: BTreeSet<&str> = [
            "taskId",
            "status",
            "statusMessage",
            "createdAt",
            "lastUpdatedAt",
            "ttlMs",
            "pollIntervalMs",
            "inputRequests",
            "result",
            "error",
        ]
        .into_iter()
        .collect();
        let task_members: Map<String, Value> = members
            .iter()
            .filter(|(key, _)| task_keys.contains(key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let task = decode_task_members(&wire, task_members).map_err(D::Error::custom)?;
        members.retain(|key, _| !task_keys.contains(key.as_str()));
        Ok(Self {
            task,
            meta,
            additional: members.into_iter().collect(),
        })
    }
}

/// Exact `notifications/tasks` JSON-RPC notification.
#[derive(Clone, Debug)]
pub struct TaskStatusNotification {
    pub params: TaskStatusNotificationParams,
}

impl TaskStatusNotification {
    /// Creates an exact `notifications/tasks` notification.
    #[must_use]
    pub const fn new(params: TaskStatusNotificationParams) -> Self {
        Self { params }
    }
}

impl Serialize for TaskStatusNotification {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut members = Map::new();
        members.insert(
            "jsonrpc".to_owned(),
            Value::String(JSONRPC_VERSION.to_owned()),
        );
        members.insert(
            "method".to_owned(),
            Value::String(TASK_STATUS_NOTIFICATION.to_owned()),
        );
        members.insert(
            "params".to_owned(),
            serde_json::to_value(&self.params).map_err(serde::ser::Error::custom)?,
        );
        Value::Object(members).serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStatusNotificationWire {
    jsonrpc: String,
    method: String,
    params: TaskStatusNotificationParams,
}

impl<'de> Deserialize<'de> for TaskStatusNotification {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TaskStatusNotificationWire::deserialize(deserializer)?;
        if wire.jsonrpc != JSONRPC_VERSION || wire.method != TASK_STATUS_NOTIFICATION {
            return Err(D::Error::custom("invalid notifications/tasks notification"));
        }
        Ok(Self::new(wire.params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_subscription_fragment_preserves_order_duplicates_and_core_filter() {
        let mut filter = SubscriptionFilter {
            tools_list_changed: Some(true),
            ..SubscriptionFilter::default()
        };
        set_task_subscription_ids(
            &mut filter,
            vec![
                TaskId::parse("task-b").expect("task id"),
                TaskId::parse("task-a").expect("task id"),
                TaskId::parse("task-b").expect("task id"),
            ],
        )
        .expect("compose Tasks fragment");

        let decoded = task_subscription_ids(&filter)
            .expect("decode Tasks fragment")
            .expect("present Tasks fragment");
        assert_eq!(
            decoded.iter().map(TaskId::as_str).collect::<Vec<_>>(),
            ["task-b", "task-a", "task-b"]
        );
        assert_eq!(filter.tools_list_changed, Some(true));
    }

    #[test]
    fn malformed_task_subscription_fragment_rejects_without_mutation() {
        let filter = SubscriptionFilter {
            additional: BTreeMap::from([(
                TASK_SUBSCRIPTION_IDS_KEY.to_owned(),
                serde_json::json!("task-a"),
            )]),
            ..SubscriptionFilter::default()
        };
        let baseline = filter.clone();

        assert_eq!(
            task_subscription_ids(&filter),
            Err(TaskWireError::Invalid("taskIds"))
        );
        assert_eq!(filter.additional, baseline.additional);
        assert_eq!(filter.tools_list_changed, baseline.tools_list_changed);
    }

    #[test]
    fn task_subscription_fragment_rejects_collision_and_one_past_limit() {
        let mut collision = SubscriptionFilter {
            additional: BTreeMap::from([(
                TASK_SUBSCRIPTION_IDS_KEY.to_owned(),
                serde_json::json!([]),
            )]),
            ..SubscriptionFilter::default()
        };
        assert_eq!(
            set_task_subscription_ids(&mut collision, Vec::new()),
            Err(TaskWireError::Invalid("taskIds"))
        );

        let mut oversized = SubscriptionFilter::default();
        let ids = (0..=MAX_TASK_SUBSCRIPTION_IDS)
            .map(|index| TaskId::parse(format!("task-{index}")))
            .collect::<Result<Vec<_>, _>>()
            .expect("bounded identifiers");
        assert_eq!(
            set_task_subscription_ids(&mut oversized, ids),
            Err(TaskWireError::Invalid("taskIds"))
        );
        assert!(oversized.additional.is_empty());
    }

    fn base(status: TaskStatus) -> TaskBase {
        TaskBase {
            task_id: TaskId::parse("task-1").expect("id"),
            status,
            status_message: None,
            created_at: TaskTimestamp::parse("2026-07-28T12:00:00.000Z").expect("timestamp"),
            last_updated_at: TaskTimestamp::parse("2026-07-28T12:00:00.000Z").expect("timestamp"),
            ttl_ms: Some(
                TaskDuration::from_json_integer(JsonInteger::from(1_u64))
                    .expect("nonnegative test duration"),
            ),
            poll_interval_ms: None,
        }
    }

    #[test]
    fn task_duration_accepts_positive_integral_spellings_and_canonicalizes_output() {
        for input in ["1", "1.0", "1e0", "1.00e0", "18446744073709551615"] {
            let duration: TaskDuration =
                serde_json::from_str(input).expect("positive integral duration is admitted");
            assert!(duration.try_as_millis().is_ok());
            assert_eq!(
                serde_json::to_string(&duration).expect("admitted duration serializes"),
                duration
                    .try_as_millis()
                    .expect("admitted duration fits")
                    .to_string(),
                "accepted input {input:?} emits canonical integer text"
            );
        }
    }

    #[test]
    fn task_duration_rejects_zero_negative_fractional_and_out_of_range_without_mutation() {
        let accepted: TaskDuration =
            serde_json::from_str("1").expect("positive boundary duration is admitted");
        let accepted_wire = serde_json::to_string(&accepted).expect("accepted duration serializes");

        for input in ["0", "-1", "1.5", "18446744073709551616"] {
            assert!(
                serde_json::from_str::<TaskDuration>(input).is_err(),
                "changing only the duration value to {input:?} is rejected by the composed profile"
            );
            assert_eq!(
                serde_json::to_string(&accepted).expect("accepted duration remains serializable"),
                accepted_wire,
                "a rejected duration mutation cannot alter accepted task state"
            );
        }
    }

    #[test]
    fn tasks_wire_round_trip_preserves_open_error_and_ledger() {
        let requests = serde_json::from_value::<TaskInputRequests>(serde_json::json!({ "sample": { "method": "sampling/createMessage", "params": { "messages": [], "maxTokens": 16 } }, "roots": { "method": "roots/list" } })).expect("input descriptors");
        let ledger = TaskInputLedger::from_requests(&requests).expect("ledger");
        let responses = serde_json::from_value::<TaskInputResponses>(serde_json::json!({ "sample": { "role": "assistant", "model": "m", "content": { "type": "text", "text": "ok" } }, "roots": { "roots": [] } })).expect("input responses");
        ledger
            .validate_responses(&responses)
            .expect("matching responses");
        let arbitrary_precision_code: Value =
            serde_json::from_str("123456789012345678901234567890")
                .expect("arbitrary-precision JSON integer");
        let task = Task::Failed { base: base(TaskStatus::Failed), error: serde_json::from_value(serde_json::json!({ "code": arbitrary_precision_code, "message": "no", "x-peer": { "n": 1 } })).expect("open error") };
        let wire = serde_json::to_value(CreateTaskResult {
            task,
            meta: None,
            additional: BTreeMap::new(),
        })
        .expect("serialize");
        let wire_source = serde_json::to_string(&wire).expect("task result wire serializes");
        let decoded: CreateTaskResult =
            serde_json::from_str(&wire_source).expect("deserialize admitted task result source");
        assert_eq!(serde_json::to_value(decoded).expect("reserialize"), wire);

        let get: GetTaskResult = serde_json::from_value(serde_json::json!({
            "resultType": "complete",
            "taskId": "task-1",
            "status": "working",
            "createdAt": "2026-07-28T12:00:00.000Z",
            "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
            "ttlMs": 1.0
        }))
        .expect("complete get result");
        assert_eq!(
            serde_json::to_value(get).expect("get result serializes")["resultType"],
            "complete"
        );
        let acknowledgement: UpdateTaskResult =
            serde_json::from_value(serde_json::json!({ "resultType": "complete" }))
                .expect("empty update acknowledgement");
        assert_eq!(
            serde_json::to_value(acknowledgement).expect("acknowledgement serializes"),
            serde_json::json!({ "resultType": "complete" })
        );
        let get_request = TaskMethodRequest::<GetTaskParams>::decode(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/get",
            "params": {
                "taskId": "task-1",
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": { "io.modelcontextprotocol/tasks": {} }
                    }
                }
            }
        }))
        .expect("final task request");
        assert_eq!(get_request.method, TASK_GET);
    }

    #[test]
    fn tasks_rejects_one_wrong_response_kind_without_mutating_ledger() {
        let requests = serde_json::from_value::<TaskInputRequests>(
            serde_json::json!({ "roots": { "method": "roots/list" } }),
        )
        .expect("request");
        let ledger = TaskInputLedger::from_requests(&requests).expect("ledger");
        let before = requests.clone();
        let wrong = serde_json::from_value::<TaskInputResponses>(
            serde_json::json!({ "roots": { "action": "accept" } }),
        )
        .expect("well-formed response");
        assert_eq!(
            ledger.validate_responses(&wrong),
            Err(TaskWireError::InputResponseKind)
        );
        assert_eq!(
            serde_json::to_value(&requests).expect("before state"),
            serde_json::to_value(before).expect("after state"),
            "rejection must not alter task input state"
        );
    }

    #[test]
    fn tasks_ledger_ignores_unknown_response_keys_before_validating_outstanding_input() {
        let requests = serde_json::from_value::<TaskInputRequests>(
            serde_json::json!({ "roots": { "method": "roots/list" } }),
        )
        .expect("request");
        let ledger = TaskInputLedger::from_requests(&requests).expect("ledger");
        let responses = serde_json::from_value::<TaskInputResponses>(serde_json::json!({
            "roots": { "roots": [] },
            "already-satisfied": { "action": "accept" }
        }))
        .expect("well-formed mixed response map");

        ledger
            .validate_responses(&responses)
            .expect("an unknown key is ignored before a matching outstanding key is validated");
    }

    #[test]
    fn tasks_ledger_still_rejects_wrong_kind_for_outstanding_key_with_unknown_key_present() {
        let requests = serde_json::from_value::<TaskInputRequests>(
            serde_json::json!({ "roots": { "method": "roots/list" } }),
        )
        .expect("request");
        let ledger = TaskInputLedger::from_requests(&requests).expect("ledger");
        let responses = serde_json::from_value::<TaskInputResponses>(serde_json::json!({
            "roots": { "action": "accept" },
            "already-satisfied": { "roots": [] }
        }))
        .expect("well-formed mixed response map");

        assert_eq!(
            ledger.validate_responses(&responses),
            Err(TaskWireError::InputResponseKind),
            "changing only the outstanding response kind preserves strict validation"
        );
    }

    #[test]
    fn completed_task_uses_exact_nested_call_tool_result_wire() {
        let wire = r#"{"resultType":"complete","taskId":"task-1","status":"completed","createdAt":"2026-07-28T12:00:00.000Z","lastUpdatedAt":"2026-07-28T12:00:00.000Z","ttlMs":null,"result":{"x-first":1.20e+4,"content":[],"x-second":123456789012345678901234567890},"x-envelope":{"preserved":true}}"#;
        let decoded: GetTaskResult = serde_json::from_str(wire).expect("flattened completed task");
        let Task::Completed { result, .. } = &decoded.task else {
            panic!("fixture is a completed task");
        };
        let retained: Value = serde_json::from_str(
            result
                .exact_wire
                .as_deref()
                .expect("peer result retains exact source"),
        )
        .expect("retained result source remains valid JSON");
        let canonical = result.canonical_value().expect("canonical result value");
        assert!(
            json_values_equal_ignoring_object_order(&retained, &canonical),
            "retained and canonical result snapshots differ: retained={retained:?}; canonical={canonical:?}"
        );
        assert_eq!(
            serde_json::to_string(&decoded).expect("re-serialize exact nested result"),
            wire,
            "completed task nesting retains member order and number lexemes"
        );

        let standalone_wire = r#"{"taskId":"task-1","status":"completed","createdAt":"2026-07-28T12:00:00.000Z","lastUpdatedAt":"2026-07-28T12:00:00.000Z","ttlMs":null,"result":{"x-first":1.20e+4,"content":[],"x-second":123456789012345678901234567890}}"#;
        let standalone: Task =
            serde_json::from_str(standalone_wire).expect("standalone completed task");
        assert_eq!(
            serde_json::to_string(&standalone).expect("re-serialize standalone task"),
            standalone_wire,
            "the standalone Tasks ingress retains its nested result source as well"
        );

        let escaped_wire = r#"{"taskId":"task-1","status":"completed","createdAt":"2026-07-28T12:00:00.000Z","lastUpdatedAt":"2026-07-28T12:00:00.000Z","ttlMs":null,"result":{"escaped\/key":"quoted \u0022 value","content":[]}}"#;
        let mut escaped: Task = serde_json::from_str(escaped_wire).expect("escaped completed task");
        assert_eq!(
            serde_json::to_string(&escaped).expect("re-serialize escaped task"),
            escaped_wire,
            "raw Tasks ingress retains escaped member and string spellings"
        );
        let Task::Completed { result, .. } = &mut escaped else {
            panic!("escaped fixture is completed");
        };
        result.additional.insert(
            "escaped/key".to_owned(),
            Value::String("changed".to_owned()),
        );
        let changed = serde_json::to_string(&escaped).expect("mutated task re-serializes");
        assert_ne!(changed, escaped_wire);
        assert!(
            changed.contains(r#""escaped/key":"changed""#),
            "changing only the decoded additional value must select canonical fallback emission"
        );
        assert!(
            !changed.contains(r"escaped\/key"),
            "fallback emission must not replay stale escaped peer source after mutation"
        );

        let with_result_type = wire.replace(
            "\"x-first\":1.20e+4",
            "\"resultType\":\"complete\",\"x-first\":1.20e+4",
        );
        assert!(serde_json::from_str::<GetTaskResult>(&with_result_type).is_err());

        let with_legacy_related_task = wire.replace(
            "\"content\":[]",
            "\"content\":[],\"_meta\":{\"io.modelcontextprotocol/related-task\":{\"taskId\":\"task-1\"}}",
        );
        assert!(serde_json::from_str::<GetTaskResult>(&with_legacy_related_task).is_err());
    }

    #[test]
    fn task_complete_result_type_compatibility_is_diagnosed_but_null_and_wrong_kinds_reject() {
        let get_without_type = r#"{"taskId":"task-1","status":"working","createdAt":"2026-07-28T12:00:00.000Z","lastUpdatedAt":"2026-07-28T12:00:00.000Z","ttlMs":null}"#;
        let get: GetTaskResult = serde_json::from_str(get_without_type)
            .expect("missing get resultType defaults complete");
        assert_eq!(
            get.peer_diagnostic(),
            Some(TaskResultPeerDiagnostic::MissingResultType)
        );
        assert_eq!(
            serde_json::to_value(&get).expect("local get serialization")["resultType"],
            "complete"
        );

        for input in [
            r#"{"resultType":null}"#,
            r#"{"resultType":false}"#,
            r#"{"resultType":"task"}"#,
        ] {
            assert!(serde_json::from_str::<GetTaskResult>(input).is_err());
            assert!(serde_json::from_str::<UpdateTaskResult>(input).is_err());
            assert!(serde_json::from_str::<CancelTaskResult>(input).is_err());
        }
        for result in [
            serde_json::from_str::<UpdateTaskResult>("{}")
                .expect("update omission defaults complete"),
            serde_json::from_str::<CancelTaskResult>("{}")
                .expect("cancel omission defaults complete"),
        ] {
            assert_eq!(
                result.peer_diagnostic(),
                Some(TaskResultPeerDiagnostic::MissingResultType)
            );
        }
    }

    #[test]
    fn task_base_requires_ttl_and_rejects_present_null_optional_fields() {
        let wire = serde_json::json!({
            "resultType": "complete",
            "taskId": "task-1",
            "status": "working",
            "createdAt": "2026-07-28T12:00:00.000Z",
            "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
            "ttlMs": null
        });
        let accepted: GetTaskResult =
            serde_json::from_value(wire.clone()).expect("unlimited TTL is valid");
        assert_eq!(
            serde_json::to_value(&accepted).expect("unlimited TTL serializes"),
            wire
        );

        let mut finite_ttl = wire.clone();
        finite_ttl["ttlMs"] = serde_json::json!(60_000);
        let finite: GetTaskResult =
            serde_json::from_value(finite_ttl.clone()).expect("finite TTL is valid");
        assert_eq!(
            serde_json::to_value(&finite).expect("finite TTL serializes"),
            finite_ttl,
            "a finite ttlMs remains distinct from the null unlimited-TTL state"
        );

        for (field, value) in [
            ("ttlMs", serde_json::json!(-1.5)),
            ("pollIntervalMs", serde_json::json!(-2.5)),
        ] {
            let mut signed_fractional_duration = finite_ttl.clone();
            signed_fractional_duration[field] = value;
            assert!(
                serde_json::from_value::<GetTaskResult>(signed_fractional_duration).is_err(),
                "changing only {field} to a signed fractional duration violates the positive-integer Tasks profile"
            );
            assert_eq!(
                serde_json::to_value(&finite).expect("finite TTL remains serializable"),
                finite_ttl,
                "a rejected {field} mutation cannot alter the accepted finite-TTL baseline"
            );
        }

        let mut non_number_ttl = finite_ttl.clone();
        non_number_ttl["ttlMs"] = Value::String("-1.5".to_owned());
        assert!(
            serde_json::from_value::<GetTaskResult>(non_number_ttl).is_err(),
            "changing only ttlMs from a number to a string violates its nullable-number schema"
        );

        let mut null_poll_interval = finite_ttl.clone();
        null_poll_interval["pollIntervalMs"] = Value::Null;
        assert!(
            serde_json::from_value::<GetTaskResult>(null_poll_interval).is_err(),
            "changing only pollIntervalMs from a number to null violates its number-only schema"
        );

        let mut missing_ttl = wire.clone();
        missing_ttl
            .as_object_mut()
            .expect("result object")
            .remove("ttlMs");
        assert!(serde_json::from_value::<GetTaskResult>(missing_ttl).is_err());
        assert_eq!(
            serde_json::to_value(&accepted).expect("valid nullable TTL remains serializable"),
            wire,
            "rejecting only ttlMs omission does not change the valid nullable-TTL baseline"
        );

        let mut null_poll_interval = wire.clone();
        null_poll_interval["pollIntervalMs"] = Value::Null;
        assert!(serde_json::from_value::<GetTaskResult>(null_poll_interval).is_err());

        let mut null_status_message = wire;
        null_status_message["statusMessage"] = Value::Null;
        assert!(serde_json::from_value::<GetTaskResult>(null_status_message).is_err());
    }

    #[test]
    fn task_status_notification_enforces_exact_method_vocabulary() {
        let wire = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/tasks",
            "params": {
                "taskId": "task-1",
                "status": "working",
                "createdAt": "2026-07-28T12:00:00.000Z",
                "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
                "ttlMs": null,
                "x-notification": { "preserved": true }
            }
        });
        let notification: TaskStatusNotification =
            serde_json::from_value(wire.clone()).expect("exact notification");
        assert_eq!(
            serde_json::to_value(notification).expect("notification serializes"),
            wire
        );

        let mut wrong_method = wire;
        wrong_method["method"] = serde_json::json!("notifications/tasks/status");
        assert!(serde_json::from_value::<TaskStatusNotification>(wrong_method).is_err());
    }
}
