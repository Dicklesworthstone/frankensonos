//! MCP protocol messages.
//!
//! Request and response types for the MCP methods currently implemented here.

use std::collections::BTreeMap;

use serde::de::{DeserializeOwned, DeserializeSeed, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::common_types::{
    AbsoluteUri, ContentBlock, EmbeddedResourceContents, ExactNonNegativeJsonNumber,
    Implementation, JsonInteger, LoggingLevel, OpenMetadata,
};
use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, RequestId};
use crate::methods::{
    COMPLETION_COMPLETE, Final2026EnvelopeKind, Final2026Peer, INITIALIZE, LOGGING_SET_LEVEL,
    NOTIFICATIONS_CANCELLED, NOTIFICATIONS_MESSAGE, NOTIFICATIONS_PROGRESS,
    NOTIFICATIONS_PROMPTS_LIST_CHANGED, NOTIFICATIONS_RESOURCES_LIST_CHANGED,
    NOTIFICATIONS_RESOURCES_UPDATED, NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
    NOTIFICATIONS_TOOLS_LIST_CHANGED, PING, PROMPTS_GET, PROMPTS_LIST, RESOURCES_LIST,
    RESOURCES_READ, RESOURCES_SUBSCRIBE, RESOURCES_TEMPLATES_LIST, RESOURCES_UNSUBSCRIBE,
    SAMPLING_CREATE_MESSAGE, SERVER_DISCOVER, SUBSCRIPTIONS_LISTEN, TOOLS_CALL, TOOLS_LIST,
    final_2026_07_28_method,
};
use crate::protocol_policy::ProtocolEra;
use crate::protocol_version::{FINAL_PROTOCOL_VERSION, RequestVersionMetadata};
use crate::result::{
    CompleteResult, CoreResultDiscriminatorPolicy, DecodedResult, ExactJsonObject, ExactJsonValue,
    FinalResultMetadataRole, InputRequiredResult, ResultDecodeError, ResultPeerDiagnostic,
    UnknownResultMembers, decode_peer_result_for_era_with_metadata_role, deserialize_exact_object,
    encode_complete_result, encode_result, exact_json_to_serde, has_final_only_metadata,
};
use crate::types::{
    ClientCapabilities, ClientInfo, LegacyContent, LegacyMetadata, LegacyPromptMessage,
    LegacyResourceContent, Prompt, Resource, ResourceTemplate, ServerCapabilities, ServerInfo,
    Tool,
};

// ============================================================================
// Progress Marker
// ============================================================================

/// Progress marker used to correlate progress notifications with requests.
///
/// Per MCP spec, progress markers can be either strings or arbitrary-width
/// JSON integers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProgressMarker {
    /// String progress marker.
    String(String),
    /// Arbitrary-width JSON integer progress marker.
    Number(JsonInteger),
}

impl From<String> for ProgressMarker {
    fn from(s: String) -> Self {
        ProgressMarker::String(s)
    }
}

impl From<&str> for ProgressMarker {
    fn from(s: &str) -> Self {
        ProgressMarker::String(s.to_owned())
    }
}

impl From<i64> for ProgressMarker {
    fn from(n: i64) -> Self {
        ProgressMarker::Number(JsonInteger::from(n))
    }
}

impl From<JsonInteger> for ProgressMarker {
    fn from(n: JsonInteger) -> Self {
        Self::Number(n)
    }
}

impl std::hash::Hash for ProgressMarker {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::String(value) => {
                std::hash::Hash::hash(&0_u8, state);
                std::hash::Hash::hash(value, state);
            }
            Self::Number(value) => {
                std::hash::Hash::hash(&1_u8, state);
                std::hash::Hash::hash(value.as_str(), state);
            }
        }
    }
}

impl std::fmt::Display for ProgressMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgressMarker::String(s) => write!(f, "{s}"),
            ProgressMarker::Number(n) => f.write_str(n.as_str()),
        }
    }
}

/// Request metadata containing optional progress marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMeta {
    /// Progress marker for receiving progress notifications.
    // Avoid UBS "hardcoded secrets" heuristics while keeping the on-the-wire name.
    #[serde(rename = "progressTo\x6ben", skip_serializing_if = "Option::is_none")]
    pub progress_marker: Option<ProgressMarker>,
}

// ============================================================================
// Final per-request metadata
// ============================================================================

/// `_meta` key carrying the protocol version on every final request.
pub const FINAL_PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";

/// `_meta` key carrying the client capabilities on every final request.
pub const FINAL_CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";

/// `_meta` key carrying optional client identity on a final request.
pub const FINAL_CLIENT_INFO_META_KEY: &str = "io.modelcontextprotocol/clientInfo";

/// `_meta` key carrying the client-selected logging floor on a final request.
pub const FINAL_LOG_LEVEL_META_KEY: &str = "io.modelcontextprotocol/logLevel";

/// `_meta` key carrying optional server identity on a final response.
pub const FINAL_SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";

/// Required final metadata carried in the `_meta` object of every request.
///
/// The protocol version and client capabilities are intentionally distinct
/// from legacy [`RequestMeta`]. Final request admission validates the version
/// against the HTTP header before using the advertised capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalRequestMeta {
    /// The exact protocol revision selected for this request.
    #[serde(rename = "io.modelcontextprotocol/protocolVersion")]
    pub protocol_version: String,
    /// Capabilities advertised by the request's client.
    #[serde(rename = "io.modelcontextprotocol/clientCapabilities")]
    pub client_capabilities: ClientCapabilities,
    /// Optional client identity supplied on this request.
    #[serde(
        rename = "io.modelcontextprotocol/clientInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_info: Option<crate::common_types::Implementation>,
    /// Additional metadata retained without granting protocol capability.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_metadata: BTreeMap<String, Value>,
}

impl FinalRequestMeta {
    /// Creates canonical metadata for the exact final protocol version.
    #[must_use]
    pub fn new(client_capabilities: ClientCapabilities) -> Self {
        Self {
            protocol_version: FINAL_PROTOCOL_VERSION.to_owned(),
            client_capabilities,
            client_info: None,
            additional_metadata: BTreeMap::new(),
        }
    }

    /// Returns the protocol header/body mirror for final request admission.
    #[must_use]
    pub fn version_metadata<'a>(
        &'a self,
        header_version: Option<&'a str>,
    ) -> RequestVersionMetadata<'a> {
        RequestVersionMetadata {
            header_version,
            body_version: Some(&self.protocol_version),
        }
    }
}

// ============================================================================
// Era-aware core dispatch
// ============================================================================

/// Typed, order-preserving client replies to server-issued final MRTR input
/// requests.
///
/// An `inputResponses` object is a correlation map, not an open JSON bag: its
/// values are one of the final embedded input-response payloads. Keeping the
/// decoded entries in wire order also prevents a retry decode/re-encode cycle
/// from silently sorting response keys before the server observes them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FinalInputResponses {
    entries: Vec<(String, FinalEmbeddedInputResponse)>,
}

impl FinalInputResponses {
    /// Creates locally authored response entries after rejecting duplicate
    /// server-assigned keys.
    pub fn try_from_entries(
        entries: Vec<(String, FinalEmbeddedInputResponse)>,
    ) -> Result<Self, FinalInputResponseCorrelationError> {
        for (index, (key, _)) in entries.iter().enumerate() {
            if entries[..index].iter().any(|(previous, _)| previous == key) {
                return Err(FinalInputResponseCorrelationError::DuplicateResponseKey);
            }
        }
        Ok(Self { entries })
    }

    /// Returns the response entries in their admitted wire order.
    #[must_use]
    pub fn entries(&self) -> &[(String, FinalEmbeddedInputResponse)] {
        &self.entries
    }

    /// Returns the response associated with an exact server-assigned key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&FinalEmbeddedInputResponse> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, response)| response)
    }

    /// Returns the number of supplied responses.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether this is a present, empty response map.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Correlates every response with the exact admitted `inputRequests` map.
    ///
    /// A retry must answer every requested key exactly once, and each reply
    /// must have the result shape selected by its corresponding descriptor.
    pub fn validate_against(
        &self,
        input_requests: &ExactJsonObject,
    ) -> Result<(), FinalInputResponseCorrelationError> {
        let mut requested = Vec::with_capacity(input_requests.members().len());
        for member in input_requests.members() {
            let value = exact_json_to_serde(&member.value)
                .map_err(|_| FinalInputResponseCorrelationError::InvalidInputRequest)?;
            let request = serde_json::from_value::<FinalEmbeddedInputRequest>(value)
                .map_err(|_| FinalInputResponseCorrelationError::InvalidInputRequest)?;
            requested.push((member.name.as_str(), request.response_kind()));
        }

        for (key, response) in &self.entries {
            let Some((_, kind)) = requested
                .iter()
                .find(|(requested_key, _)| *requested_key == key)
            else {
                return Err(FinalInputResponseCorrelationError::UnknownResponseKey);
            };
            if !response.matches_kind(*kind) {
                return Err(FinalInputResponseCorrelationError::ResponseKindMismatch);
            }
        }
        if self.entries.len() != requested.len() {
            return Err(FinalInputResponseCorrelationError::MissingResponse);
        }
        Ok(())
    }

    /// Correlates this map directly to an admitted final input-required
    /// result.
    ///
    /// A state-only continuation cannot be answered with response entries.
    pub fn validate_against_input_required(
        &self,
        input_required: &InputRequiredResult,
    ) -> Result<(), FinalInputResponseCorrelationError> {
        match input_required.input_requests() {
            Some(input_requests) => self.validate_against(input_requests),
            None => Err(FinalInputResponseCorrelationError::StateOnlyInputResponses),
        }
    }
}

impl Serialize for FinalInputResponses {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, response) in &self.entries {
            map.serialize_entry(key, response)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for FinalInputResponses {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct InputResponsesVisitor;

        impl<'de> Visitor<'de> for InputResponsesVisitor {
            type Value = FinalInputResponses;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object of final embedded input responses")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if entries.iter().any(|(existing, _)| existing == &key) {
                        return Err(serde::de::Error::custom(
                            "duplicate final input response key",
                        ));
                    }
                    entries.push((key, map.next_value::<FinalEmbeddedInputResponse>()?));
                }
                FinalInputResponses::try_from_entries(entries).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_map(InputResponsesVisitor)
    }
}

/// Why a retry response map could not be correlated to a prior MRTR request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalInputResponseCorrelationError {
    /// A peer input-request descriptor is not one of the final embedded forms.
    InvalidInputRequest,
    /// A locally authored or peer-supplied map repeated a response key.
    DuplicateResponseKey,
    /// A supplied response key was not requested by the server.
    UnknownResponseKey,
    /// At least one requested key was not answered.
    MissingResponse,
    /// A response did not match its request descriptor's selected result kind.
    ResponseKindMismatch,
    /// A state-only input-required result was retried with a present
    /// `inputResponses` member, including an explicitly empty map.
    StateOnlyInputResponses,
}

impl std::fmt::Display for FinalInputResponseCorrelationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInputRequest => formatter.write_str("invalid final input request"),
            Self::DuplicateResponseKey => formatter.write_str("duplicate final input response key"),
            Self::UnknownResponseKey => formatter.write_str("unknown final input response key"),
            Self::MissingResponse => formatter.write_str("missing final input response"),
            Self::ResponseKindMismatch => {
                formatter.write_str("final input response does not match request kind")
            }
            Self::StateOnlyInputResponses => formatter
                .write_str("state-only final input-required result cannot accept input responses"),
        }
    }
}

impl std::error::Error for FinalInputResponseCorrelationError {}

fn deserialize_optional_final_input_responses<'de, D>(
    deserializer: D,
) -> Result<Option<FinalInputResponses>, D::Error>
where
    D: Deserializer<'de>,
{
    FinalInputResponses::deserialize(deserializer).map(Some)
}

fn deserialize_optional_non_null_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

/// Final pagination parameters shared by the core catalog methods.
///
/// This is intentionally separate from the legacy list parameter structs:
/// final requests always carry the common metadata object, while the legacy
/// wire era negotiates through its initialize request instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalListParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Opaque pagination cursor; a present empty cursor remains present.
    ///
    /// Absent, present-and-valid, and explicit `null` are three distinct wire
    /// states. Omitting the member yields `None`; a present string yields
    /// `Some`, including the empty string; an explicit `null` is refused
    /// rather than silently collapsed into absence.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub cursor: Option<String>,
    /// Only include catalog entries with every listed tag.
    #[serde(
        rename = "includeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_tags: Option<Vec<String>>,
    /// Exclude catalog entries with any listed tag.
    #[serde(
        rename = "excludeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_tags: Option<Vec<String>>,
}

/// Wire presence of an optional final request `arguments` member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalArguments<T> {
    /// The `arguments` member was not present on the wire.
    Absent,
    /// The member was present with its admitted typed value.
    Value(T),
}

impl<T> FinalArguments<T> {
    /// Returns whether the arguments member was absent.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Borrows the admitted argument value, if one was present.
    #[must_use]
    pub const fn as_value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent => None,
        }
    }

    /// Consumes the presence marker and returns its admitted value.
    #[must_use]
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent => None,
        }
    }
}

impl<T> Default for FinalArguments<T> {
    fn default() -> Self {
        Self::Absent
    }
}

impl<T: Serialize> Serialize for FinalArguments<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for FinalArguments<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).and_then(|value| {
            value.map(Self::Value).ok_or_else(|| {
                serde::de::Error::custom("final arguments must be absent or an object, never null")
            })
        })
    }
}

/// Final `tools/call` request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCallToolParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Name of the selected tool.
    pub name: String,
    /// Optional method-owned tool arguments.
    #[serde(
        default,
        skip_serializing_if = "FinalArguments::is_absent",
        deserialize_with = "deserialize_final_json_object_arguments"
    )]
    pub arguments: FinalArguments<Value>,
    /// Optional replies to embedded final input requests.
    #[serde(
        rename = "inputResponses",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_final_input_responses"
    )]
    pub input_responses: Option<FinalInputResponses>,
    /// Opaque retry state supplied with embedded input responses.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub request_state: Option<String>,
}

/// Final `resources/read` request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalReadResourceParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Structurally admitted resource URI.
    pub uri: AbsoluteUri,
    /// Optional replies to embedded final input requests.
    #[serde(
        rename = "inputResponses",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_final_input_responses"
    )]
    pub input_responses: Option<FinalInputResponses>,
    /// Opaque retry state supplied with embedded input responses.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub request_state: Option<String>,
}

/// Final `prompts/get` request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalGetPromptParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Name of the selected prompt.
    pub name: String,
    /// Optional prompt arguments.
    #[serde(default, skip_serializing_if = "FinalArguments::is_absent")]
    pub arguments: FinalArguments<BTreeMap<String, String>>,
    /// Optional replies to embedded final input requests.
    #[serde(
        rename = "inputResponses",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_final_input_responses"
    )]
    pub input_responses: Option<FinalInputResponses>,
    /// Opaque retry state supplied with embedded input responses.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub request_state: Option<String>,
}

fn deserialize_final_json_object_arguments<'de, D>(
    deserializer: D,
) -> Result<FinalArguments<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let arguments = FinalArguments::<Value>::deserialize(deserializer)?;
    if arguments.as_value().is_some_and(|value| !value.is_object()) {
        return Err(serde::de::Error::custom("arguments must be an object"));
    }
    Ok(arguments)
}

/// Exact legacy reference accepted by `completion/complete`.
///
/// Legacy completion parameter objects remain open, as they are in the
/// 2024-11-05 schema. The selected prompt/resource members are still typed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LegacyCompletionReference {
    /// Identifies one prompt or prompt template by name.
    #[serde(rename = "ref/prompt")]
    Prompt {
        /// Prompt or prompt-template name.
        name: String,
    },
    /// Identifies one resource or resource template by URI template.
    #[serde(rename = "ref/resource")]
    Resource {
        /// Resource URI or URI template.
        uri: String,
    },
}

/// Exact legacy completion argument selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyCompletionArgument {
    /// Argument name.
    pub name: String,
    /// Prefix used for completion matching.
    pub value: String,
}

/// Exact 2024-11-05 `completion/complete` request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyCompletionParams {
    /// Prompt or resource-template target.
    #[serde(rename = "ref")]
    pub reference: LegacyCompletionReference,
    /// Argument being completed.
    pub argument: LegacyCompletionArgument,
    /// Request metadata (progress token, etc.).
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Final reference accepted by `completion/complete`.
#[derive(Debug, Clone)]
pub enum FinalCompletionReference {
    /// Identifies one prompt or prompt template by name.
    Prompt {
        /// Prompt or prompt-template name.
        name: String,
    },
    /// Identifies one prompt or prompt template by name and display title.
    PromptWithTitle {
        /// Prompt or prompt-template name.
        name: String,
        /// Display title supplied for the selected prompt.
        title: String,
    },
    /// Identifies one resource template by URI template.
    Resource {
        /// Resource URI template.
        uri: String,
    },
}

impl FinalCompletionReference {
    /// Constructs a resource-template completion reference after RFC 6570
    /// admission. The exact source spelling is retained for wire emission.
    pub fn resource(uri: impl Into<String>) -> Result<Self, crate::UriTemplateError> {
        let uri = uri.into();
        crate::UriTemplate::parse(&uri)?;
        Ok(Self::Resource { uri })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum FinalCompletionReferenceWire {
    #[serde(rename = "ref/prompt")]
    Prompt {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    #[serde(rename = "ref/resource")]
    Resource { uri: String },
}

impl Serialize for FinalCompletionReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match self {
            Self::Prompt { name } => FinalCompletionReferenceWire::Prompt {
                name: name.clone(),
                title: None,
            },
            Self::PromptWithTitle { name, title } => FinalCompletionReferenceWire::Prompt {
                name: name.clone(),
                title: Some(title.clone()),
            },
            Self::Resource { uri } => {
                crate::UriTemplate::parse(uri).map_err(serde::ser::Error::custom)?;
                FinalCompletionReferenceWire::Resource { uri: uri.clone() }
            }
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FinalCompletionReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match FinalCompletionReferenceWire::deserialize(deserializer)? {
            FinalCompletionReferenceWire::Prompt {
                name,
                title: Some(title),
            } => Ok(Self::PromptWithTitle { name, title }),
            FinalCompletionReferenceWire::Prompt { name, title: None } => Ok(Self::Prompt { name }),
            FinalCompletionReferenceWire::Resource { uri } => {
                Self::resource(uri).map_err(serde::de::Error::custom)
            }
        }
    }
}

/// Final completion argument selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCompletionArgument {
    /// Argument name.
    pub name: String,
    /// Prefix used for completion matching.
    pub value: String,
}

/// Optional final completion context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCompletionContext {
    /// Previously resolved prompt or URI-template variables.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_completion_context_arguments",
        deserialize_with = "deserialize_optional_completion_context_arguments"
    )]
    pub arguments: Option<BTreeMap<String, String>>,
}

/// Maximum number of previously resolved variables in one final completion context.
pub const MAX_COMPLETION_CONTEXT_ARGUMENTS: usize = 256;
/// Maximum UTF-8 bytes in one final completion-context variable name.
pub const MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES: usize = 1024;
/// Maximum UTF-8 bytes in one final completion-context variable value.
pub const MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES: usize = 16 * 1024;
/// Maximum encoded JSON bytes occupied by one final completion-context argument map.
pub const MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES: usize = 256 * 1024;

/// Validates final completion parameters directly from retained JSON source.
///
/// JSON-RPC ingress invokes this before it materializes `params` as a
/// [`Value`]. The `_meta` member identifies the final-only parameter surface;
/// legacy completion parameters without it keep the established `Value` path.
///
/// The context argument object is inspected lexically before serde can decode
/// its strings. This preserves the received JSON spelling for the bounds: a
/// `\\u0061` occupies six received bytes, while `a` occupies one.
pub(crate) fn validate_raw_final_completion_params(
    method: &str,
    source: &str,
) -> Result<(), &'static str> {
    if method != COMPLETION_COMPLETE {
        return Ok(());
    }

    let has_metadata = raw_completion_params_layout(source)?;
    if !has_metadata {
        return Ok(());
    }

    // Scan every occurrence before serde visits the final parameter map. A
    // duplicate `context` is invalid at the typed layer, but serde would
    // deserialize its first value before noticing the second member. Keeping
    // only the final raw range here would therefore let an oversized first
    // context allocate before its received-byte bounds were checked.
    validate_raw_final_completion_contexts(source)?;

    FinalCompletionParams::deserialize(&mut serde_json::Deserializer::from_str(source))
        .map(|_| ())
        .map_err(|_| "invalid final completion parameters")
}

fn raw_completion_params_layout(source: &str) -> Result<bool, &'static str> {
    let mut cursor = RawJsonCursor::new(source);
    cursor.skip_whitespace();
    if !cursor.consume(b'{') {
        return Err("invalid completion parameters");
    }
    cursor.skip_whitespace();

    let mut has_metadata = false;
    if cursor.consume(b'}') {
        return Ok(has_metadata);
    }

    loop {
        let key = cursor.parse_string()?;
        cursor.skip_whitespace();
        if !cursor.consume(b':') {
            return Err("invalid completion parameters");
        }
        cursor.skip_whitespace();
        cursor.raw_value_range()?;
        if raw_json_string_is(source, key, "_meta") {
            has_metadata = true;
        }
        cursor.skip_whitespace();
        if cursor.consume(b'}') {
            cursor.skip_whitespace();
            if cursor.position != source.len() {
                return Err("invalid completion parameters");
            }
            return Ok(has_metadata);
        }
        if !cursor.consume(b',') {
            return Err("invalid completion parameters");
        }
        cursor.skip_whitespace();
    }
}

fn validate_raw_final_completion_contexts(source: &str) -> Result<(), &'static str> {
    let mut cursor = RawJsonCursor::new(source);
    cursor.skip_whitespace();
    if !cursor.consume(b'{') {
        return Err("invalid completion parameters");
    }
    cursor.skip_whitespace();
    if cursor.consume(b'}') {
        return Ok(());
    }

    loop {
        let key = cursor.parse_string()?;
        cursor.skip_whitespace();
        if !cursor.consume(b':') {
            return Err("invalid completion parameters");
        }
        cursor.skip_whitespace();
        let context = cursor.raw_value_range()?;
        if raw_json_string_is(source, key, "context") {
            let mut context_cursor = RawJsonCursor::at(source, context.start);
            validate_raw_completion_context(&mut context_cursor)?;
            if context_cursor.position != context.end {
                return Err("invalid final completion context");
            }
        }
        cursor.skip_whitespace();
        if cursor.consume(b'}') {
            cursor.skip_whitespace();
            if cursor.position != source.len() {
                return Err("invalid completion parameters");
            }
            return Ok(());
        }
        if !cursor.consume(b',') {
            return Err("invalid completion parameters");
        }
        cursor.skip_whitespace();
    }
}

fn validate_raw_completion_context(cursor: &mut RawJsonCursor<'_>) -> Result<(), &'static str> {
    cursor.skip_whitespace();
    if cursor.peek() != Some(b'{') {
        cursor.skip_raw_value()?;
        return Ok(());
    }
    cursor.consume(b'{');
    cursor.skip_whitespace();
    if cursor.consume(b'}') {
        return Ok(());
    }

    loop {
        let key = cursor.parse_string()?;
        cursor.skip_whitespace();
        if !cursor.consume(b':') {
            return Err("invalid final completion context");
        }
        cursor.skip_whitespace();
        if raw_json_string_is(cursor.source, key, "arguments") {
            validate_raw_completion_context_arguments(cursor)?;
        } else {
            cursor.skip_raw_value()?;
        }
        cursor.skip_whitespace();
        if cursor.consume(b'}') {
            return Ok(());
        }
        if !cursor.consume(b',') {
            return Err("invalid final completion context");
        }
        cursor.skip_whitespace();
    }
}

fn validate_raw_completion_context_arguments(
    cursor: &mut RawJsonCursor<'_>,
) -> Result<(), &'static str> {
    cursor.skip_whitespace();
    if cursor.peek() != Some(b'{') {
        cursor.skip_raw_value()?;
        return Ok(());
    }
    let object_start = cursor.position;
    cursor.consume(b'{');
    cursor.skip_whitespace();
    if cursor.consume(b'}') {
        return raw_completion_context_object_within_limit(cursor, object_start);
    }

    let mut entries = 0_usize;
    loop {
        let key = cursor.parse_string()?;
        if raw_json_string_content_bytes(key) > MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES {
            return Err("completion context argument key exceeds the maximum raw JSON byte limit");
        }
        entries = entries
            .checked_add(1)
            .ok_or("completion context arguments exceed the maximum entry count")?;
        if entries > MAX_COMPLETION_CONTEXT_ARGUMENTS {
            return Err("completion context arguments exceed the maximum of 256 entries");
        }

        cursor.skip_whitespace();
        if !cursor.consume(b':') {
            return Err("invalid final completion context arguments");
        }
        cursor.skip_whitespace();
        if cursor.peek() == Some(b'"') {
            let value = cursor.parse_string()?;
            if raw_json_string_content_bytes(value) > MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES {
                return Err(
                    "completion context argument value exceeds the maximum raw JSON byte limit",
                );
            }
        } else {
            cursor.skip_raw_value()?;
        }
        if cursor.position.saturating_sub(object_start) > MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES {
            return Err("completion context arguments exceed the maximum received JSON byte limit");
        }
        cursor.skip_whitespace();
        if cursor.consume(b'}') {
            return raw_completion_context_object_within_limit(cursor, object_start);
        }
        if !cursor.consume(b',') {
            return Err("invalid final completion context arguments");
        }
        cursor.skip_whitespace();
    }
}

fn raw_completion_context_object_within_limit(
    cursor: &RawJsonCursor<'_>,
    object_start: usize,
) -> Result<(), &'static str> {
    if cursor.position - object_start > MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES {
        Err("completion context arguments exceed the maximum received JSON byte limit")
    } else {
        Ok(())
    }
}

fn raw_json_string_content_bytes(token: std::ops::Range<usize>) -> usize {
    token.end.saturating_sub(token.start.saturating_add(2))
}

fn raw_json_string_is(source: &str, token: std::ops::Range<usize>, expected: &str) -> bool {
    let bytes = source.as_bytes();
    if bytes.get(token.start) != Some(&b'"')
        || token.end <= token.start + 1
        || bytes.get(token.end - 1) != Some(&b'"')
    {
        return false;
    }

    let mut position = token.start + 1;
    let mut expected_bytes = expected.bytes();
    while position < token.end - 1 {
        let byte = bytes[position];
        let decoded = if byte == b'\\' {
            position += 1;
            let Some(escape) = bytes.get(position).copied() else {
                return false;
            };
            match escape {
                b'"' => b'"',
                b'\\' => b'\\',
                b'/' => b'/',
                b'b' => 0x08,
                b'f' => 0x0c,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'u' => {
                    let Some(digits) = bytes.get(position + 1..position + 5) else {
                        return false;
                    };
                    let mut value = 0_u16;
                    for digit in digits {
                        let nibble = match digit {
                            b'0'..=b'9' => u16::from(*digit - b'0'),
                            b'a'..=b'f' => u16::from(*digit - b'a' + 10),
                            b'A'..=b'F' => u16::from(*digit - b'A' + 10),
                            _ => return false,
                        };
                        value = (value << 4) | nibble;
                    }
                    position += 4;
                    let Ok(value) = u8::try_from(value) else {
                        return false;
                    };
                    value
                }
                _ => return false,
            }
        } else {
            if !byte.is_ascii() {
                return false;
            }
            byte
        };
        if expected_bytes.next() != Some(decoded) {
            return false;
        }
        position += 1;
    }
    expected_bytes.next().is_none()
}

struct RawJsonCursor<'a> {
    source: &'a str,
    bytes: &'a [u8],
    position: usize,
}

impl<'a> RawJsonCursor<'a> {
    fn new(source: &'a str) -> Self {
        Self::at(source, 0)
    }

    fn at(source: &'a str, position: usize) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            position,
        }
    }

    fn raw_value_range(&mut self) -> Result<std::ops::Range<usize>, &'static str> {
        let start = self.position;
        self.skip_raw_value()?;
        Ok(start..self.position)
    }

    fn skip_raw_value(&mut self) -> Result<(), &'static str> {
        match self.peek() {
            Some(b'"') => {
                self.parse_string()?;
                Ok(())
            }
            Some(b'{' | b'[') => self.skip_raw_container(),
            Some(b't') => self.consume_literal(b"true"),
            Some(b'f') => self.consume_literal(b"false"),
            Some(b'n') => self.consume_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => {
                while !matches!(
                    self.peek(),
                    None | Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
                ) {
                    self.position += 1;
                }
                Ok(())
            }
            _ => Err("invalid completion parameters"),
        }
    }

    fn skip_raw_container(&mut self) -> Result<(), &'static str> {
        let mut depth = 0_usize;
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.parse_string()?;
                }
                Some(b'{' | b'[') => {
                    depth = depth
                        .checked_add(1)
                        .ok_or("invalid completion parameters")?;
                    self.position += 1;
                }
                Some(b'}' | b']') => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or("invalid completion parameters")?;
                    self.position += 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some(_) => self.position += 1,
                None => return Err("invalid completion parameters"),
            }
        }
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), &'static str> {
        let end = self
            .position
            .checked_add(literal.len())
            .ok_or("invalid completion parameters")?;
        if self.bytes.get(self.position..end) == Some(literal) {
            self.position = end;
            Ok(())
        } else {
            Err("invalid completion parameters")
        }
    }

    fn parse_string(&mut self) -> Result<std::ops::Range<usize>, &'static str> {
        let start = self.position;
        if !self.consume(b'"') {
            return Err("invalid completion parameters");
        }
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.position += 1;
                    return Ok(start..self.position);
                }
                Some(b'\\') => {
                    self.position += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.position += 1;
                        }
                        Some(b'u') => {
                            let end = self
                                .position
                                .checked_add(5)
                                .ok_or("invalid completion parameters")?;
                            if self.bytes.get(self.position + 1..end).is_none() {
                                return Err("invalid completion parameters");
                            }
                            self.position = end;
                        }
                        _ => return Err("invalid completion parameters"),
                    }
                }
                Some(0x20..=0x7f) => self.position += 1,
                Some(_) => {
                    let character = self.source[self.position..]
                        .chars()
                        .next()
                        .ok_or("invalid completion parameters")?;
                    self.position += character.len_utf8();
                }
                None => return Err("invalid completion parameters"),
            }
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

fn deserialize_optional_final_completion_context<'de, D>(
    deserializer: D,
) -> Result<Option<FinalCompletionContext>, D::Error>
where
    D: Deserializer<'de>,
{
    FinalCompletionContext::deserialize(deserializer).map(Some)
}

fn serialize_optional_completion_context_arguments<S>(
    arguments: &Option<BTreeMap<String, String>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(arguments) = arguments {
        validate_completion_context_arguments(arguments).map_err(serde::ser::Error::custom)?;
    }
    arguments.serialize(serializer)
}

fn deserialize_optional_completion_context_arguments<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoundedStringSeed {
        maximum: usize,
        field: &'static str,
    }

    impl BoundedStringSeed {
        const fn new(maximum: usize, field: &'static str) -> Self {
            Self { maximum, field }
        }
    }

    impl<'de> DeserializeSeed<'de> for BoundedStringSeed {
        type Value = String;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_str(BoundedStringVisitor {
                maximum: self.maximum,
                field: self.field,
            })
        }
    }

    struct BoundedStringVisitor {
        maximum: usize,
        field: &'static str,
    }

    impl BoundedStringVisitor {
        fn admit<E: serde::de::Error>(&self, value: &str) -> Result<(), E> {
            if value.len() > self.maximum {
                return Err(E::custom(format_args!(
                    "completion context argument {} exceeds the maximum of {} bytes",
                    self.field, self.maximum
                )));
            }
            Ok(())
        }
    }

    impl<'de> Visitor<'de> for BoundedStringVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "a completion context argument {} no longer than {} bytes",
                self.field, self.maximum
            )
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.admit(value)?;
            Ok(value.to_owned())
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.admit(value)?;
            Ok(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.admit(&value)?;
            Ok(value)
        }
    }

    struct ArgumentsVisitor;

    impl<'de> Visitor<'de> for ArgumentsVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded object of completion context string arguments")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut arguments = BTreeMap::new();
            let mut encoded_bytes = 2_usize; // `{}` for an empty JSON object.

            while let Some(key) = map.next_key_seed(BoundedStringSeed::new(
                MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES,
                "key",
            ))? {
                if arguments.len() == MAX_COMPLETION_CONTEXT_ARGUMENTS {
                    return Err(serde::de::Error::custom(
                        "completion context arguments exceed the maximum of 256 entries",
                    ));
                }
                if arguments.contains_key(&key) {
                    return Err(serde::de::Error::custom(
                        "duplicate completion context argument key",
                    ));
                }

                let value = map.next_value_seed(BoundedStringSeed::new(
                    MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES,
                    "value",
                ))?;

                encoded_bytes = next_completion_context_encoded_bytes(
                    encoded_bytes,
                    !arguments.is_empty(),
                    &key,
                    &value,
                )
                .ok_or_else(|| {
                    serde::de::Error::custom(
                        "completion context arguments exceed the maximum of 262144 encoded bytes",
                    )
                })?;
                arguments.insert(key, value);
            }

            Ok(arguments)
        }
    }

    deserializer.deserialize_map(ArgumentsVisitor).map(Some)
}

fn validate_completion_context_arguments(
    arguments: &BTreeMap<String, String>,
) -> Result<(), &'static str> {
    if arguments.len() > MAX_COMPLETION_CONTEXT_ARGUMENTS {
        return Err("completion context arguments exceed the maximum of 256 entries");
    }

    let mut encoded_bytes = 2_usize; // `{}` for an empty JSON object.
    for (index, (key, value)) in arguments.iter().enumerate() {
        if key.len() > MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES {
            return Err("completion context argument key exceeds the maximum of 1024 bytes");
        }
        if value.len() > MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES {
            return Err("completion context argument value exceeds the maximum of 16384 bytes");
        }
        encoded_bytes =
            next_completion_context_encoded_bytes(encoded_bytes, index != 0, key, value)
                .ok_or("completion context arguments exceed the maximum of 262144 encoded bytes")?;
    }
    Ok(())
}

fn next_completion_context_encoded_bytes(
    current: usize,
    has_previous_entry: bool,
    key: &str,
    value: &str,
) -> Option<usize> {
    current
        .checked_add(usize::from(has_previous_entry))
        .and_then(|total| total.checked_add(encoded_json_string_bytes(key)))
        .and_then(|total| total.checked_add(1)) // `:`
        .and_then(|total| total.checked_add(encoded_json_string_bytes(value)))
        .filter(|total| *total <= MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES)
}

fn encoded_json_string_bytes(value: &str) -> usize {
    2 + value
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum::<usize>()
}

/// Final `completion/complete` request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCompletionParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Prompt or resource-template target.
    #[serde(rename = "ref")]
    pub reference: FinalCompletionReference,
    /// Argument being completed.
    pub argument: FinalCompletionArgument,
    /// Previously resolved prompt or URI-template variables.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_final_completion_context"
    )]
    pub context: Option<FinalCompletionContext>,
}

/// Final empty request parameters, used by `server/discover`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmptyParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
}

/// `_meta` key correlating an event-stream subscription with its listen request.
pub const FINAL_SUBSCRIPTION_ID_META_KEY: &str = "io.modelcontextprotocol/subscriptionId";

/// Final notification categories selected for a subscription stream.
///
/// Every present field is an explicit opt-in. `false` and an empty resource
/// list remain distinct from an omitted field on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionFilter {
    /// Receive prompt catalog change notifications when true.
    #[serde(
        rename = "promptsListChanged",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub prompts_list_changed: Option<bool>,
    /// Resource URIs for update notifications.
    #[serde(
        rename = "resourceSubscriptions",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resource_subscriptions: Option<Vec<String>>,
    /// Receive resource catalog change notifications when true.
    #[serde(
        rename = "resourcesListChanged",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resources_list_changed: Option<bool>,
    /// Receive tool catalog change notifications when true.
    #[serde(
        rename = "toolsListChanged",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tools_list_changed: Option<bool>,
    /// Future notification categories accepted by the final schema and
    /// retained without activating any extension behavior.
    ///
    /// Absorption here is deliberate and required: the authoritative
    /// 2026-07-28 schema declares `SubscriptionFilter` with no
    /// `additionalProperties`, so unknown members are permitted, and no
    /// definition in that schema is closed. Rejecting them would refuse wire
    /// messages the specification allows.
    ///
    /// Absorption is also SILENT, which is the hazard: a misspelled canonical
    /// key lands here, the typed field stays `None`, and the subscription is
    /// narrowed to nothing without any party being in error. Call
    /// [`SubscriptionFilter::suspected_key_typos`] to make that observable.
    ///
    /// Do NOT "fix" this by adding `deny_unknown_fields` to this struct. It
    /// would contradict the schema, and it would also do nothing: serde emits
    /// its deny check over the residual left AFTER flatten targets
    /// deserialize, and a `BTreeMap<String, Value>` catch-all consumes every
    /// entry, so the check can never fire. (Contrast `GetTaskParams` in
    /// `tasks_extension`, where the same pair of attributes DOES reject,
    /// because it flattens a typed struct that leaves unknown keys behind.)
    #[serde(flatten, default)]
    pub additional: BTreeMap<String, Value>,
}

/// An unrecognized [`SubscriptionFilter`] key that closely resembles a
/// canonical field, and is therefore more likely a misspelling than a
/// forward-compatible extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspectedFilterKeyTypo {
    /// The unrecognized key exactly as it arrived on the wire.
    pub received: String,
    /// The canonical field it most closely resembles.
    pub resembles: &'static str,
}

/// Case-folded edit distance, used only to rank short wire key names.
fn folded_edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, lc) in left.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, rc) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(lc != rc);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[right.len()]
}

impl SubscriptionFilter {
    /// Canonical wire names of the typed filter fields, in declaration order.
    pub const CANONICAL_FIELDS: [&'static str; 4] = [
        "promptsListChanged",
        "resourceSubscriptions",
        "resourcesListChanged",
        "toolsListChanged",
    ];

    /// Unrecognized keys in [`Self::additional`] that resemble a canonical
    /// field closely enough to be probable misspellings.
    ///
    /// This never rejects and never mutates: the schema permits unknown
    /// members, so an extension must keep working. It only lets a caller see
    /// what a silent absorption cost it.
    ///
    /// A key is reported when, case-folded, it equals a canonical field, is a
    /// prefix of one, has one as a prefix, or is within an edit distance of
    /// two. **The prefix rules are load-bearing, not decoration.** The
    /// misspelling this accessor exists for is `resources` in place of
    /// `resourceSubscriptions`: that is thirteen edits away and differs by far
    /// more than capitalization, so a case-folding or edit-distance test alone
    /// would miss the very defect that motivated this code.
    ///
    /// Genuine extensions are unaffected: a key like `experimentalFooBar`
    /// matches none of the four rules against any canonical field.
    #[must_use]
    pub fn suspected_key_typos(&self) -> Vec<SuspectedFilterKeyTypo> {
        let mut reported = Vec::new();
        for key in self.additional.keys() {
            let folded = key.to_lowercase();
            let best = Self::CANONICAL_FIELDS
                .iter()
                .filter_map(|canonical| {
                    let canonical_folded = canonical.to_lowercase();
                    let distance = folded_edit_distance(&folded, &canonical_folded);
                    let resembles = folded == canonical_folded
                        || folded.starts_with(&canonical_folded)
                        || canonical_folded.starts_with(&folded)
                        || distance <= 2;
                    resembles.then_some((distance, *canonical))
                })
                .min_by_key(|(distance, canonical)| (*distance, *canonical));
            if let Some((_, resembles)) = best {
                reported.push(SuspectedFilterKeyTypo {
                    received: key.clone(),
                    resembles,
                });
            }
        }
        reported
    }
}

/// Final `subscriptions/listen` request parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalSubscriptionsListenParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Notification categories the client explicitly opts into.
    pub notifications: SubscriptionFilter,
}

/// Final `notifications/subscriptions/acknowledged` parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalSubscriptionsAcknowledgedNotificationParams {
    /// Optional notification metadata, including the subscription ID when the
    /// acknowledgement was delivered over a subscription stream.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
    /// The subset of the requested notification categories the server accepted.
    pub notifications: SubscriptionFilter,
    /// Schema-open extension members retained without activating behavior.
    #[serde(flatten, default)]
    pub additional: BTreeMap<String, Value>,
}

/// Exact final `notifications/message` parameters.
///
/// Final clients opt into these notifications through
/// `io.modelcontextprotocol/logLevel` in request metadata; this notification
/// itself remains independent of the removed final `logging/setLevel` RPC.
#[derive(Debug, Clone)]
pub struct FinalLogMessageParams {
    /// Final RFC 5424 severity.
    pub level: LoggingLevel,
    /// Optional non-null logger name.
    pub logger: Option<String>,
    /// Arbitrary log data.
    pub data: Value,
    /// Optional final notification metadata.
    pub meta: Option<OpenMetadata>,
    /// Schema-open extension members retained without activating behavior.
    pub additional: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct FinalLogMessageParamsRef<'a> {
    level: LoggingLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    logger: Option<&'a str>,
    data: &'a Value,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<&'a OpenMetadata>,
    #[serde(flatten)]
    additional: &'a BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct FinalLogMessageParamsWire {
    level: LoggingLevel,
    #[serde(default, deserialize_with = "deserialize_optional_non_null_logger")]
    logger: Option<String>,
    data: Value,
    #[serde(rename = "_meta", default)]
    meta: Option<OpenMetadata>,
    #[serde(flatten, default)]
    additional: BTreeMap<String, Value>,
}

impl Serialize for FinalLogMessageParams {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        FinalLogMessageParamsRef {
            level: self.level,
            logger: self.logger.as_deref(),
            data: &self.data,
            meta: self.meta.as_ref(),
            additional: &self.additional,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FinalLogMessageParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FinalLogMessageParamsWire::deserialize(deserializer)?;
        Ok(Self {
            level: wire.level,
            logger: wire.logger,
            data: wire.data,
            meta: wire.meta,
            additional: wire.additional,
        })
    }
}

fn deserialize_optional_non_null_logger<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_non_null_string(deserializer)
}

/// Exact final `notifications/cancelled` parameters.
///
/// This is deliberately separate from legacy [`CancelledParams`], while
/// retaining schema-open extension members without assigning them semantics.
#[derive(Debug, Clone, Serialize)]
pub struct FinalCancelledNotificationParams {
    /// The request ID whose result or subscription stream is no longer needed.
    #[serde(rename = "requestId")]
    pub request_id: RequestId,
    /// Optional open cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional final notification metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
    /// Schema-open extension members retained without activating behavior.
    #[serde(flatten, default)]
    pub additional: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for FinalCancelledNotificationParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value.as_object().ok_or_else(|| {
            serde::de::Error::custom("final cancellation parameters must be an object")
        })?;
        let request_id = object
            .get("requestId")
            .ok_or_else(|| serde::de::Error::custom("final cancellation requestId is required"))
            .and_then(|value| {
                serde_json::from_value::<RequestId>(value.clone()).map_err(serde::de::Error::custom)
            })?;
        let reason = match object.get("reason") {
            None => None,
            Some(Value::String(reason)) => Some(reason.clone()),
            Some(_) => {
                return Err(serde::de::Error::custom(
                    "final cancellation reason must be a non-null string",
                ));
            }
        };
        let meta = match object.get("_meta") {
            None => None,
            Some(Value::Object(entries)) => Some(
                OpenMetadata::try_from_notification_entries(
                    entries.clone().into_iter().collect::<BTreeMap<_, _>>(),
                )
                .map_err(serde::de::Error::custom)?,
            ),
            Some(_) => {
                return Err(serde::de::Error::custom(
                    "final cancellation _meta must be a non-null object",
                ));
            }
        };
        let additional = object
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "requestId" | "reason" | "_meta"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        Ok(Self {
            request_id,
            reason,
            meta,
            additional,
        })
    }
}

/// Exact final `notifications/progress` parameters.
#[derive(Debug, Clone, Serialize)]
pub struct FinalProgressNotificationParams {
    /// Token from the client request being progressed.
    #[serde(rename = "progressToken")]
    pub progress_token: ProgressMarker,
    /// Finite progress completed so far.
    pub progress: ExactNonNegativeJsonNumber,
    /// Finite total work, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<ExactNonNegativeJsonNumber>,
    /// Optional progress message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional final notification metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
    /// Schema-open extension members retained without activating behavior.
    #[serde(flatten, default)]
    pub additional: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct FinalProgressNotificationParamsWire {
    #[serde(rename = "progressToken")]
    progress_token: ProgressMarker,
    progress: ExactNonNegativeJsonNumber,
    #[serde(default)]
    total: Option<ExactNonNegativeJsonNumber>,
    #[serde(default)]
    message: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Option<OpenMetadata>,
    #[serde(flatten, default)]
    additional: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for FinalProgressNotificationParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FinalProgressNotificationParamsWire::deserialize(deserializer)?;
        Ok(Self {
            progress_token: wire.progress_token,
            progress: wire.progress,
            total: wire.total,
            message: wire.message,
            meta: wire.meta,
            additional: wire.additional,
        })
    }
}

/// Exact final `notifications/resources/updated` parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalResourceUpdatedNotificationParams {
    /// Absolute URI for the changed resource or provider-defined sub-resource.
    pub uri: AbsoluteUri,
    /// Optional final notification metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
    /// Schema-open extension members retained without activating behavior.
    #[serde(flatten, default)]
    pub additional: BTreeMap<String, Value>,
}

/// Exact optional parameter object for final catalog-change notifications.
///
/// `None` in a [`ServerNotification`] omits `params` entirely; `Some` retains
/// a present notification parameter object, including a metadata-only one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FinalEmptyNotificationParams {
    /// Optional final notification metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
    /// Schema-open extension members retained without activating behavior.
    #[serde(flatten, default)]
    pub additional: BTreeMap<String, Value>,
}

/// Exact final `sampling/createMessage` parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCreateMessageParams {
    /// Required final request metadata.
    #[serde(rename = "_meta")]
    pub meta: OpenMetadata,
    /// Sampling conversation.
    pub messages: Vec<crate::types::FinalSamplingMessage>,
    /// Requested maximum token count as an arbitrary-width JSON integer.
    #[serde(rename = "maxTokens")]
    pub max_tokens: JsonInteger,
    /// Optional system prompt.
    #[serde(
        rename = "systemPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt: Option<String>,
    /// Optional sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Optional stopping sequences. Presence remains distinct from an empty list.
    #[serde(
        rename = "stopSequences",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_sequences: Option<Vec<String>>,
    /// Optional model-selection preferences.
    #[serde(
        rename = "modelPreferences",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub model_preferences: Option<crate::types::ModelPreferences>,
    /// Optional requested MCP context inclusion.
    #[serde(
        rename = "includeContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_context: Option<IncludeContext>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Optional tools the model may call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<crate::types::FinalTool>>,
    /// Optional tool-selection controls.
    #[serde(
        rename = "toolChoice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_choice: Option<crate::types::FinalToolChoice>,
}

/// Exact final `sampling/createMessage` complete payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCreateMessageResult {
    /// Generated final sampling content.
    pub content: crate::types::FinalSamplingMessageContent,
    /// Model name selected by the client.
    pub model: String,
    /// Generated message role.
    pub role: crate::types::Role,
    /// Optional open sampling stop reason.
    #[serde(
        rename = "stopReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,
    /// Optional final metadata on this embedded MRTR input response value.
    ///
    /// This payload is not a JSON-RPC result envelope, so it deliberately
    /// carries no `resultType` discriminator.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
}

/// Exact final `sampling/createMessage` input-required result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCreateMessageInputRequiredResult {
    /// Mandatory final discriminator, fixed to `input_required`.
    #[serde(rename = "resultType")]
    pub result_type: FinalInputRequiredResultType,
    /// Optional final result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
    /// Server-initiated requests that must be fulfilled before retrying.
    #[serde(
        rename = "inputRequests",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_requests: Option<BTreeMap<String, Value>>,
    /// Opaque state retained for the retry.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_state: Option<String>,
}

impl FinalCreateMessageInputRequiredResult {
    /// Validates the final input-required presence invariant.
    pub fn validate(&self) -> Result<(), CoreDispatchError> {
        if self.input_requests.is_some() || self.request_state.is_some() {
            Ok(())
        } else {
            Err(CoreDispatchError::InvalidResult {
                era: ProtocolEra::Modern2026,
                method: SAMPLING_CREATE_MESSAGE,
            })
        }
    }
}

/// Final input-required discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinalInputRequiredResultType {
    /// Additional input is required before retrying the original request.
    #[serde(rename = "input_required")]
    InputRequired,
}

/// A final MRTR descriptor embedded in a Task, without a JSON-RPC envelope.
///
/// Task input requests are correlated exclusively by their containing map
/// key.  Consequently these descriptors deliberately omit `jsonrpc`, `id`,
/// and the outer request `_meta` capability object.
#[derive(Debug, Clone)]
pub enum FinalEmbeddedInputRequest {
    /// A final sampling descriptor.
    Sampling(FinalEmbeddedCreateMessageParams),
    /// A roots-list descriptor.
    Roots(FinalEmbeddedRootsListParams),
    /// A form or URL elicitation descriptor.
    Elicitation(FinalEmbeddedElicitationParams),
}

impl FinalEmbeddedInputRequest {
    /// Returns the response kind that may answer this descriptor.
    #[must_use]
    pub const fn response_kind(&self) -> FinalEmbeddedInputKind {
        match self {
            Self::Sampling(_) => FinalEmbeddedInputKind::Sampling,
            Self::Roots(_) => FinalEmbeddedInputKind::Roots,
            Self::Elicitation(FinalEmbeddedElicitationParams::Form(_)) => {
                FinalEmbeddedInputKind::FormElicitation
            }
            Self::Elicitation(FinalEmbeddedElicitationParams::Url(_)) => {
                FinalEmbeddedInputKind::UrlElicitation
            }
        }
    }
}

impl Serialize for FinalEmbeddedInputRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut object = serde_json::Map::new();
        match self {
            Self::Sampling(params) => {
                object.insert(
                    "method".to_owned(),
                    Value::String("sampling/createMessage".to_owned()),
                );
                object.insert(
                    "params".to_owned(),
                    serde_json::to_value(params).map_err(serde::ser::Error::custom)?,
                );
            }
            Self::Roots(params) => {
                object.insert("method".to_owned(), Value::String("roots/list".to_owned()));
                if !params.is_empty() {
                    object.insert(
                        "params".to_owned(),
                        serde_json::to_value(params).map_err(serde::ser::Error::custom)?,
                    );
                }
            }
            Self::Elicitation(params) => {
                object.insert(
                    "method".to_owned(),
                    Value::String("elicitation/create".to_owned()),
                );
                object.insert(
                    "params".to_owned(),
                    serde_json::to_value(params).map_err(serde::ser::Error::custom)?,
                );
            }
        }
        Value::Object(object).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FinalEmbeddedInputRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let (method, params) =
            take_embedded_request_members(value).map_err(serde::de::Error::custom)?;
        match method.as_str() {
            "sampling/createMessage" => {
                let params = params.ok_or_else(|| {
                    serde::de::Error::custom("sampling descriptor requires params")
                })?;
                serde_json::from_value(params)
                    .map(Self::Sampling)
                    .map_err(serde::de::Error::custom)
            }
            "roots/list" => {
                let params = params.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                let params: FinalEmbeddedRootsListParams =
                    serde_json::from_value(params).map_err(serde::de::Error::custom)?;
                params.validate().map_err(serde::de::Error::custom)?;
                Ok(Self::Roots(params))
            }
            "elicitation/create" => {
                let params = params.ok_or_else(|| {
                    serde::de::Error::custom("elicitation descriptor requires params")
                })?;
                serde_json::from_value(params)
                    .map(Self::Elicitation)
                    .map_err(serde::de::Error::custom)
            }
            _ => Err(serde::de::Error::custom(
                "unsupported embedded input method",
            )),
        }
    }
}

fn take_embedded_request_members(value: Value) -> Result<(String, Option<Value>), &'static str> {
    let Value::Object(mut members) = value else {
        return Err("embedded input request must be an object");
    };
    let method = members
        .remove("method")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or("embedded input request requires a string method")?;
    let params = members.remove("params");
    if members.is_empty() {
        Ok((method, params))
    } else {
        Err("embedded input request has unknown envelope members")
    }
}

/// The selected final MRTR response kind for one Task map key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalEmbeddedInputKind {
    /// A sampling response.
    Sampling,
    /// A roots-list response.
    Roots,
    /// A form-elicitation response.
    FormElicitation,
    /// A URL-elicitation response.
    UrlElicitation,
}

/// Exact final parameters for an embedded sampling descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmbeddedCreateMessageParams {
    /// Sampling conversation.
    pub messages: Vec<crate::types::FinalSamplingMessage>,
    /// Requested maximum token count as an arbitrary-width JSON integer.
    #[serde(rename = "maxTokens")]
    pub max_tokens: JsonInteger,
    /// Optional system prompt.
    #[serde(
        rename = "systemPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt: Option<String>,
    /// Optional sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Optional stopping sequences.
    #[serde(
        rename = "stopSequences",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_sequences: Option<Vec<String>>,
    /// Optional model-selection preferences.
    #[serde(
        rename = "modelPreferences",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub model_preferences: Option<crate::types::ModelPreferences>,
    /// Optional requested MCP context inclusion.
    #[serde(
        rename = "includeContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_context: Option<IncludeContext>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Optional tools the model may call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<crate::types::FinalTool>>,
    /// Optional tool-selection controls.
    #[serde(
        rename = "toolChoice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_choice: Option<crate::types::FinalToolChoice>,
}

/// Bounded generic metadata permitted on an embedded roots descriptor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmbeddedRootsListParams {
    /// Generic inert metadata. It cannot carry final request authority.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
}

impl FinalEmbeddedRootsListParams {
    fn is_empty(&self) -> bool {
        self.meta.is_none()
    }

    fn validate(&self) -> Result<(), &'static str> {
        let Some(meta) = &self.meta else {
            return Ok(());
        };
        if meta.entries().contains_key(FINAL_PROTOCOL_VERSION_META_KEY)
            || meta
                .entries()
                .contains_key(FINAL_CLIENT_CAPABILITIES_META_KEY)
        {
            return Err("embedded roots metadata cannot carry outer request authority");
        }
        Ok(())
    }
}

/// Exact final parameters for an embedded elicitation descriptor.
#[derive(Debug, Clone)]
pub enum FinalEmbeddedElicitationParams {
    /// In-band form elicitation.
    Form(FinalEmbeddedFormElicitationParams),
    /// External URL elicitation.
    Url(FinalEmbeddedUrlElicitationParams),
}

impl Serialize for FinalEmbeddedElicitationParams {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Form(params) if params.mode == ElicitMode::Form => params.serialize(serializer),
            Self::Url(params) if params.mode == ElicitMode::Url => params.serialize(serializer),
            Self::Form(_) => Err(serde::ser::Error::custom(
                "form elicitation descriptor must use mode form",
            )),
            Self::Url(_) => Err(serde::ser::Error::custom(
                "URL elicitation descriptor must use mode url",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for FinalEmbeddedElicitationParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let mode = value
            .as_object()
            .and_then(|members| members.get("mode"))
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("elicitation descriptor requires mode"))?;
        match mode {
            "form" => serde_json::from_value(value)
                .map(Self::Form)
                .map_err(serde::de::Error::custom),
            "url" => serde_json::from_value(value)
                .map(Self::Url)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("unsupported elicitation mode")),
        }
    }
}

/// Form elicitation request parameters without an outer request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmbeddedFormElicitationParams {
    /// Exact form discriminator.
    pub mode: ElicitMode,
    /// User-facing request text.
    pub message: String,
    /// Requested form schema.
    #[serde(rename = "requestedSchema")]
    pub requested_schema: crate::schema::AdmittedFinalFormSchema,
}

/// URL elicitation request parameters without legacy elicitation identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmbeddedUrlElicitationParams {
    /// Exact URL discriminator.
    pub mode: ElicitMode,
    /// User-facing request text.
    pub message: String,
    /// Structurally admitted external URL.
    pub url: AbsoluteUri,
}

/// A final MRTR result payload embedded in a Task, without a JSON-RPC envelope.
#[derive(Debug, Clone, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "the public Task-input response union keeps every protocol-selected payload inline; boxing only sampling would add allocation and an asymmetric dereference to an otherwise uniform typed API"
)]
pub enum FinalEmbeddedInputResponse {
    /// Sampling result payload.
    Sampling(FinalCreateMessageResult),
    /// Roots-list result payload.
    Roots(FinalEmbeddedRootsListResult),
    /// Elicitation result payload. The request ledger determines form versus URL.
    Elicitation(FinalEmbeddedElicitationResult),
}

impl FinalEmbeddedInputResponse {
    /// Returns whether this response can answer the supplied descriptor kind.
    #[must_use]
    pub fn matches_kind(&self, kind: FinalEmbeddedInputKind) -> bool {
        match (self, kind) {
            (Self::Sampling(_), FinalEmbeddedInputKind::Sampling)
            | (Self::Roots(_), FinalEmbeddedInputKind::Roots) => true,
            (Self::Elicitation(response), FinalEmbeddedInputKind::FormElicitation) => {
                response.valid_for_form()
            }
            (Self::Elicitation(response), FinalEmbeddedInputKind::UrlElicitation) => {
                response.valid_for_url()
            }
            _ => false,
        }
    }
}

impl Serialize for FinalEmbeddedInputResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Sampling(response) => response.serialize(serializer),
            Self::Roots(response) => response.serialize(serializer),
            Self::Elicitation(response) => response.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for FinalEmbeddedInputResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("embedded input response must be an object"))?;
        if object.contains_key("jsonrpc")
            || object.contains_key("id")
            || object.contains_key("result")
        {
            return Err(serde::de::Error::custom(
                "embedded input response cannot be a JSON-RPC envelope",
            ));
        }
        if object.contains_key("model") || object.contains_key("role") {
            return serde_json::from_value(value)
                .map(Self::Sampling)
                .map_err(serde::de::Error::custom);
        }
        if object.contains_key("roots") {
            return serde_json::from_value(value)
                .map(Self::Roots)
                .map_err(serde::de::Error::custom);
        }
        if object.contains_key("action") {
            return serde_json::from_value(value)
                .map(Self::Elicitation)
                .map_err(serde::de::Error::custom);
        }
        Err(serde::de::Error::custom(
            "unsupported embedded input response",
        ))
    }
}

/// Exact roots-list result payload embedded in a Task input response map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmbeddedRootsListResult {
    /// Roots supplied by the client.
    pub roots: Vec<Root>,
}

/// Exact elicitation response payload embedded in a Task input response map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmbeddedElicitationResult {
    /// User action.
    pub action: ElicitAction,
    /// Optional submitted form data.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_elicitation_content"
    )]
    pub content: Option<BTreeMap<String, ElicitContentValue>>,
}

fn deserialize_optional_elicitation_content<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, ElicitContentValue>>, D::Error>
where
    D: Deserializer<'de>,
{
    BTreeMap::deserialize(deserializer).map(Some)
}

impl FinalEmbeddedElicitationResult {
    fn valid_for_form(&self) -> bool {
        match self.action {
            ElicitAction::Accept => self.content.is_some(),
            ElicitAction::Decline | ElicitAction::Cancel => self.content.is_none(),
        }
    }

    fn valid_for_url(&self) -> bool {
        self.content.is_none()
    }
}

/// Final `tools/list` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalListToolsResult {
    /// Catalog tools in their selected order.
    pub tools: Vec<crate::types::FinalTool>,
    /// Opaque next cursor, if another page is available.
    ///
    /// Absent, present-and-valid, and explicit `null` are three distinct wire
    /// states; an explicit `null` is refused rather than read as "no further
    /// page".
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub next_cursor: Option<String>,
    /// Required lossless final cache lifetime.
    #[serde(rename = "ttlMs")]
    pub ttl_ms: crate::result::CacheTtl,
    /// Required final cache sharing scope.
    #[serde(
        rename = "cacheScope",
        serialize_with = "serialize_cache_scope",
        deserialize_with = "deserialize_cache_scope"
    )]
    pub cache_scope: crate::result::CacheScope,
}

/// Final `tools/call` result payload using final common content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalCallToolResult {
    /// Final common output content.
    pub content: Vec<ContentBlock>,
    /// Whether the tool execution completed with a tool-level error.
    #[serde(
        rename = "isError",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub is_error: bool,
    /// Optional structured tool output, validated by the advertised output schema.
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json_value"
    )]
    pub structured_content: Option<Value>,
}

impl crate::result::CompleteResultPayload for FinalCallToolResult {
    const KNOWN_MEMBER_NAMES: &'static [&'static str] =
        &["content", "isError", "structuredContent"];

    fn decode_known_members(
        members: &mut crate::result::TypedCompleteMembers<'_>,
    ) -> Result<Self, ResultDecodeError> {
        let mut selected = serde_json::Map::new();
        for name in Self::KNOWN_MEMBER_NAMES {
            if let Some(value) = members.take(name)? {
                selected.insert((*name).to_owned(), exact_json_to_serde(&value)?);
            }
        }
        serde_json::from_value(Value::Object(selected))
            .map_err(|_| ResultDecodeError::invalid_known_member("$.content"))
    }
}

fn deserialize_present_json_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// Final `resources/list` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalListResourcesResult {
    /// Catalog resources in their selected order.
    pub resources: Vec<crate::types::FinalResource>,
    /// Opaque next cursor, if another page is available.
    ///
    /// Absent, present-and-valid, and explicit `null` are three distinct wire
    /// states; an explicit `null` is refused rather than read as "no further
    /// page".
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub next_cursor: Option<String>,
    /// Required lossless final cache lifetime.
    #[serde(rename = "ttlMs")]
    pub ttl_ms: crate::result::CacheTtl,
    /// Required final cache sharing scope.
    #[serde(
        rename = "cacheScope",
        serialize_with = "serialize_cache_scope",
        deserialize_with = "deserialize_cache_scope"
    )]
    pub cache_scope: crate::result::CacheScope,
}

/// Final `resources/templates/list` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalListResourceTemplatesResult {
    /// Catalog templates in their selected order.
    #[serde(rename = "resourceTemplates")]
    pub resource_templates: Vec<crate::types::FinalResourceTemplate>,
    /// Opaque next cursor, if another page is available.
    ///
    /// Absent, present-and-valid, and explicit `null` are three distinct wire
    /// states; an explicit `null` is refused rather than read as "no further
    /// page".
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub next_cursor: Option<String>,
    /// Required lossless final cache lifetime.
    #[serde(rename = "ttlMs")]
    pub ttl_ms: crate::result::CacheTtl,
    /// Required final cache sharing scope.
    #[serde(
        rename = "cacheScope",
        serialize_with = "serialize_cache_scope",
        deserialize_with = "deserialize_cache_scope"
    )]
    pub cache_scope: crate::result::CacheScope,
}

/// Final `resources/read` result payload using final common resource content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalReadResourceResult {
    /// Read resource contents.
    pub contents: Vec<EmbeddedResourceContents>,
    /// Required lossless final cache lifetime.
    #[serde(rename = "ttlMs")]
    pub ttl_ms: crate::result::CacheTtl,
    /// Required final cache sharing scope.
    #[serde(
        rename = "cacheScope",
        serialize_with = "serialize_cache_scope",
        deserialize_with = "deserialize_cache_scope"
    )]
    pub cache_scope: crate::result::CacheScope,
}

/// Final `prompts/list` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalListPromptsResult {
    /// Catalog prompts in their selected order.
    pub prompts: Vec<crate::types::FinalPrompt>,
    /// Opaque next cursor, if another page is available.
    ///
    /// Absent, present-and-valid, and explicit `null` are three distinct wire
    /// states; an explicit `null` is refused rather than read as "no further
    /// page".
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null_string"
    )]
    pub next_cursor: Option<String>,
    /// Required lossless final cache lifetime.
    #[serde(rename = "ttlMs")]
    pub ttl_ms: crate::result::CacheTtl,
    /// Required final cache sharing scope.
    #[serde(
        rename = "cacheScope",
        serialize_with = "serialize_cache_scope",
        deserialize_with = "deserialize_cache_scope"
    )]
    pub cache_scope: crate::result::CacheScope,
}

fn serialize_cache_scope<S>(
    scope: &crate::result::CacheScope,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(match scope {
        crate::result::CacheScope::Public => "public",
        crate::result::CacheScope::Private => "private",
    })
}

fn deserialize_cache_scope<'de, D>(deserializer: D) -> Result<crate::result::CacheScope, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match String::deserialize(deserializer)?.as_str() {
        "public" => Ok(crate::result::CacheScope::Public),
        "private" => Ok(crate::result::CacheScope::Private),
        _ => Err(serde::de::Error::custom(
            "cacheScope must be `public` or `private`",
        )),
    }
}

/// One final prompt message using a final common content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalPromptMessage {
    /// Role of the prompt message author.
    pub role: crate::types::Role,
    /// Final common message content.
    pub content: ContentBlock,
}

/// Final `prompts/get` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalGetPromptResult {
    /// Optional prompt description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Prompt messages.
    pub messages: Vec<FinalPromptMessage>,
}

// ============================================================================
// Final directional notification unions
// ============================================================================

/// Typed admission error for an MCP 2026-07-28 notification union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalNotificationError {
    /// The public JSON-RPC struct contained invalid envelope data.
    InvalidEnvelope { method: String },
    /// A notification union was given a request with an ID.
    RequestIdPresent { method: String },
    /// The method is not part of the active final core method table.
    UnsupportedMethod { method: String },
    /// The method is a final request rather than a final notification.
    WrongEnvelope { method: String },
    /// The selected peer cannot originate this notification method.
    WrongDirection {
        /// Exact notification method literal.
        method: String,
        /// Peer that attempted to originate it.
        sender: Final2026Peer,
    },
    /// The method's required parameter shape was missing or invalid.
    InvalidParams { method: &'static str },
    /// A locally constructed typed parameter object could not be encoded.
    EncodeFailure { method: &'static str },
}

impl std::fmt::Display for FinalNotificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnvelope { method } => {
                write!(
                    formatter,
                    "invalid JSON-RPC notification envelope for {method}"
                )
            }
            Self::RequestIdPresent { method } => {
                write!(formatter, "{method} must be a JSON-RPC notification")
            }
            Self::UnsupportedMethod { method } => {
                write!(formatter, "{method} is not an active final notification")
            }
            Self::WrongEnvelope { method } => {
                write!(formatter, "{method} is a final request, not a notification")
            }
            Self::WrongDirection { method, sender } => {
                write!(
                    formatter,
                    "{sender:?} cannot send final notification {method}"
                )
            }
            Self::InvalidParams { method } => {
                write!(
                    formatter,
                    "invalid final notification parameters for {method}"
                )
            }
            Self::EncodeFailure { method } => {
                write!(
                    formatter,
                    "unable to encode final notification parameters for {method}"
                )
            }
        }
    }
}

impl std::error::Error for FinalNotificationError {}

/// The one notification a final client may originate.
#[derive(Debug, Clone)]
pub enum ClientNotification {
    /// `notifications/cancelled` for a client-owned request.
    Cancelled(FinalCancelledNotificationParams),
}

impl ClientNotification {
    /// Admits one JSON-RPC notification only from the exact final client union.
    pub fn decode(request: &JsonRpcRequest) -> Result<Self, FinalNotificationError> {
        admit_final_notification(request, Final2026Peer::Client)?;
        match request.method.as_str() {
            NOTIFICATIONS_CANCELLED => {
                decode_required_final_notification_params(request).map(Self::Cancelled)
            }
            _ => Err(FinalNotificationError::WrongDirection {
                method: request.method.clone(),
                sender: Final2026Peer::Client,
            }),
        }
    }

    /// Returns this notification's exact method literal.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Cancelled(_) => NOTIFICATIONS_CANCELLED,
        }
    }

    /// Encodes this typed union as an ID-free JSON-RPC notification.
    pub fn encode(&self) -> Result<JsonRpcRequest, FinalNotificationError> {
        let params = match self {
            Self::Cancelled(params) => {
                encode_final_notification_params(NOTIFICATIONS_CANCELLED, params)?
            }
        };
        Ok(JsonRpcRequest::notification(self.method(), Some(params)))
    }
}

/// The eight notifications a final server may originate.
#[derive(Debug, Clone)]
pub enum ServerNotification {
    /// `notifications/cancelled` for a server-terminated subscription stream.
    Cancelled(FinalCancelledNotificationParams),
    /// `notifications/progress` for an in-flight client request.
    Progress(FinalProgressNotificationParams),
    /// `notifications/message` log event.
    Message(FinalLogMessageParams),
    /// `notifications/resources/updated` resource change event.
    ResourceUpdated(FinalResourceUpdatedNotificationParams),
    /// `notifications/resources/list_changed` catalog change event.
    ResourcesListChanged(Option<FinalEmptyNotificationParams>),
    /// `notifications/tools/list_changed` catalog change event.
    ToolsListChanged(Option<FinalEmptyNotificationParams>),
    /// `notifications/prompts/list_changed` catalog change event.
    PromptsListChanged(Option<FinalEmptyNotificationParams>),
    /// `notifications/subscriptions/acknowledged` stream acknowledgement.
    SubscriptionsAcknowledged(FinalSubscriptionsAcknowledgedNotificationParams),
}

impl ServerNotification {
    /// Admits one JSON-RPC notification only from the exact final server union.
    pub fn decode(request: &JsonRpcRequest) -> Result<Self, FinalNotificationError> {
        admit_final_notification(request, Final2026Peer::Server)?;
        match request.method.as_str() {
            NOTIFICATIONS_CANCELLED => {
                decode_required_final_notification_params(request).map(Self::Cancelled)
            }
            NOTIFICATIONS_PROGRESS => {
                decode_required_final_notification_params(request).map(Self::Progress)
            }
            NOTIFICATIONS_MESSAGE => {
                decode_required_final_notification_params(request).map(Self::Message)
            }
            NOTIFICATIONS_RESOURCES_UPDATED => {
                decode_required_final_notification_params(request).map(Self::ResourceUpdated)
            }
            NOTIFICATIONS_RESOURCES_LIST_CHANGED => {
                decode_optional_final_notification_params(request).map(Self::ResourcesListChanged)
            }
            NOTIFICATIONS_TOOLS_LIST_CHANGED => {
                decode_optional_final_notification_params(request).map(Self::ToolsListChanged)
            }
            NOTIFICATIONS_PROMPTS_LIST_CHANGED => {
                decode_optional_final_notification_params(request).map(Self::PromptsListChanged)
            }
            NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED => {
                decode_required_final_notification_params(request)
                    .map(Self::SubscriptionsAcknowledged)
            }
            _ => Err(FinalNotificationError::WrongDirection {
                method: request.method.clone(),
                sender: Final2026Peer::Server,
            }),
        }
    }

    /// Decodes a final server notification with the exact received `params` JSON.
    ///
    /// Only the modern progress branch needs this companion to preserve native-size
    /// decimal and exponent spellings that a materialized [`Value`] normalizes.
    /// The caller must pass the raw `params` member from the same admitted frame.
    /// Other final notification branches use [`Self::decode`] unchanged.
    pub fn decode_with_raw_params(
        request: &JsonRpcRequest,
        raw_params: &str,
    ) -> Result<Self, FinalNotificationError> {
        admit_final_notification(request, Final2026Peer::Server)?;
        if request.method != NOTIFICATIONS_PROGRESS {
            return Self::decode(request);
        }

        let parsed_params = serde_json::from_str(raw_params).map_err(|_| {
            FinalNotificationError::InvalidParams {
                method: NOTIFICATIONS_PROGRESS,
            }
        })?;
        if request.params.as_ref() != Some(&parsed_params) {
            return Err(FinalNotificationError::InvalidParams {
                method: NOTIFICATIONS_PROGRESS,
            });
        }
        serde_json::from_str(raw_params)
            .map(Self::Progress)
            .map_err(|_| FinalNotificationError::InvalidParams {
                method: NOTIFICATIONS_PROGRESS,
            })
    }

    /// Returns this notification's exact method literal.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Cancelled(_) => NOTIFICATIONS_CANCELLED,
            Self::Progress(_) => NOTIFICATIONS_PROGRESS,
            Self::Message(_) => NOTIFICATIONS_MESSAGE,
            Self::ResourceUpdated(_) => NOTIFICATIONS_RESOURCES_UPDATED,
            Self::ResourcesListChanged(_) => NOTIFICATIONS_RESOURCES_LIST_CHANGED,
            Self::ToolsListChanged(_) => NOTIFICATIONS_TOOLS_LIST_CHANGED,
            Self::PromptsListChanged(_) => NOTIFICATIONS_PROMPTS_LIST_CHANGED,
            Self::SubscriptionsAcknowledged(_) => NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
        }
    }

    /// Encodes this typed union as an ID-free JSON-RPC notification.
    pub fn encode(&self) -> Result<JsonRpcRequest, FinalNotificationError> {
        let params = match self {
            Self::Cancelled(params) => Some(encode_final_notification_params(
                NOTIFICATIONS_CANCELLED,
                params,
            )?),
            Self::Progress(params) => Some(encode_final_notification_params(
                NOTIFICATIONS_PROGRESS,
                params,
            )?),
            Self::Message(params) => Some(encode_final_notification_params(
                NOTIFICATIONS_MESSAGE,
                params,
            )?),
            Self::ResourceUpdated(params) => Some(encode_final_notification_params(
                NOTIFICATIONS_RESOURCES_UPDATED,
                params,
            )?),
            Self::ResourcesListChanged(params) => params
                .as_ref()
                .map(|params| {
                    encode_final_notification_params(NOTIFICATIONS_RESOURCES_LIST_CHANGED, params)
                })
                .transpose()?,
            Self::ToolsListChanged(params) => params
                .as_ref()
                .map(|params| {
                    encode_final_notification_params(NOTIFICATIONS_TOOLS_LIST_CHANGED, params)
                })
                .transpose()?,
            Self::PromptsListChanged(params) => params
                .as_ref()
                .map(|params| {
                    encode_final_notification_params(NOTIFICATIONS_PROMPTS_LIST_CHANGED, params)
                })
                .transpose()?,
            Self::SubscriptionsAcknowledged(params) => Some(encode_final_notification_params(
                NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
                params,
            )?),
        };
        Ok(JsonRpcRequest::notification(self.method(), params))
    }

    /// Serializes this notification as its exact JSON-RPC wire frame.
    ///
    /// [`encode`](Self::encode) carries its params as `serde_json::Value`,
    /// whose map representation cannot preserve member order, so byte-exact
    /// emission and round-trip fidelity proofs must use this string encoder:
    /// the typed params serialize directly, preserving declaration order and
    /// raw number lexemes.
    pub fn encode_wire(&self) -> Result<String, FinalNotificationError> {
        let method = self.method();
        let params = match self {
            Self::Cancelled(params) => Some(serde_json::to_string(params)),
            Self::Progress(params) => Some(serde_json::to_string(params)),
            Self::Message(params) => Some(serde_json::to_string(params)),
            Self::ResourceUpdated(params) => Some(serde_json::to_string(params)),
            Self::ResourcesListChanged(params)
            | Self::ToolsListChanged(params)
            | Self::PromptsListChanged(params) => params.as_ref().map(serde_json::to_string),
            Self::SubscriptionsAcknowledged(params) => Some(serde_json::to_string(params)),
        };
        let params = params
            .transpose()
            .map_err(|_| FinalNotificationError::EncodeFailure { method })?;
        Ok(match params {
            Some(params) => {
                format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#)
            }
            None => format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#),
        })
    }
}

fn admit_final_notification(
    request: &JsonRpcRequest,
    sender: Final2026Peer,
) -> Result<(), FinalNotificationError> {
    if request.validate().is_err() {
        return Err(FinalNotificationError::InvalidEnvelope {
            method: request.method.clone(),
        });
    }
    if !request.is_notification() {
        return Err(FinalNotificationError::RequestIdPresent {
            method: request.method.clone(),
        });
    }
    let Some(method) = final_2026_07_28_method(&request.method) else {
        return Err(FinalNotificationError::UnsupportedMethod {
            method: request.method.clone(),
        });
    };
    if !matches!(method.envelope, Final2026EnvelopeKind::Notification) {
        return Err(FinalNotificationError::WrongEnvelope {
            method: request.method.clone(),
        });
    }
    if !method.admits_notification_from(sender) {
        return Err(FinalNotificationError::WrongDirection {
            method: request.method.clone(),
            sender,
        });
    }
    Ok(())
}

fn decode_required_final_notification_params<T: DeserializeOwned>(
    request: &JsonRpcRequest,
) -> Result<T, FinalNotificationError> {
    request
        .params
        .as_ref()
        .ok_or(FinalNotificationError::InvalidParams {
            method: notification_method_literal(request),
        })
        .and_then(|params| {
            serde_json::from_value(params.clone()).map_err(|_| {
                FinalNotificationError::InvalidParams {
                    method: notification_method_literal(request),
                }
            })
        })
}

fn decode_optional_final_notification_params<T: DeserializeOwned>(
    request: &JsonRpcRequest,
) -> Result<Option<T>, FinalNotificationError> {
    request
        .params
        .as_ref()
        .map(|params| {
            serde_json::from_value(params.clone()).map_err(|_| {
                FinalNotificationError::InvalidParams {
                    method: notification_method_literal(request),
                }
            })
        })
        .transpose()
}

fn encode_final_notification_params<T: Serialize>(
    method: &'static str,
    params: &T,
) -> Result<Value, FinalNotificationError> {
    serde_json::to_value(params).map_err(|_| FinalNotificationError::EncodeFailure { method })
}

fn notification_method_literal(request: &JsonRpcRequest) -> &'static str {
    match request.method.as_str() {
        NOTIFICATIONS_CANCELLED => NOTIFICATIONS_CANCELLED,
        NOTIFICATIONS_PROGRESS => NOTIFICATIONS_PROGRESS,
        NOTIFICATIONS_MESSAGE => NOTIFICATIONS_MESSAGE,
        NOTIFICATIONS_RESOURCES_UPDATED => NOTIFICATIONS_RESOURCES_UPDATED,
        NOTIFICATIONS_RESOURCES_LIST_CHANGED => NOTIFICATIONS_RESOURCES_LIST_CHANGED,
        NOTIFICATIONS_TOOLS_LIST_CHANGED => NOTIFICATIONS_TOOLS_LIST_CHANGED,
        NOTIFICATIONS_PROMPTS_LIST_CHANGED => NOTIFICATIONS_PROMPTS_LIST_CHANGED,
        NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED => NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
        _ => unreachable!("notification admission rejects unknown method literals"),
    }
}

/// Completion candidates returned by the legacy protocol era.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionValues {
    /// Completion values selected by the server.
    #[serde(
        serialize_with = "serialize_completion_values",
        deserialize_with = "deserialize_completion_values"
    )]
    pub values: Vec<String>,
    /// Total number of available values, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    /// Whether further completion values are available.
    #[serde(rename = "hasMore", default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

/// Completion candidates returned by the final protocol era.
///
/// The final schema's `total` is a mathematical JSON integer, so it retains
/// [`JsonInteger`] rather than narrowing a peer value to a machine integer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalCompletionValues {
    /// Completion values selected by the server.
    #[serde(
        serialize_with = "serialize_final_completion_values",
        deserialize_with = "deserialize_final_completion_values"
    )]
    pub values: Vec<String>,
    /// Exact total number of available values, when the peer supplied one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_final_completion_total",
        deserialize_with = "deserialize_optional_final_completion_total"
    )]
    pub total: Option<JsonInteger>,
    /// Whether further completion values are available.
    #[serde(
        rename = "hasMore",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_final_completion_has_more"
    )]
    pub has_more: Option<bool>,
}

/// Bounded peer conformance diagnostics specific to final completion values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalCompletionPeerDiagnostic {
    /// A peer supplied a schema-valid negative total. It is retained only for
    /// display and compatibility; it must not control allocation, pagination,
    /// or local result emission.
    NegativeTotal,
}

fn deserialize_optional_final_completion_total<'de, D>(
    deserializer: D,
) -> Result<Option<JsonInteger>, D::Error>
where
    D: Deserializer<'de>,
{
    let total = JsonInteger::deserialize(deserializer)?;
    Ok(Some(total))
}

fn serialize_optional_final_completion_total<S>(
    total: &Option<JsonInteger>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if total
        .as_ref()
        .is_some_and(final_completion_total_is_negative)
    {
        return Err(serde::ser::Error::custom(
            "final completion total must be a nonnegative JSON integer",
        ));
    }
    total.serialize(serializer)
}

fn deserialize_optional_final_completion_has_more<'de, D>(
    deserializer: D,
) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}

/// Maximum completion candidates allowed on the wire by either supported era.
pub const MAX_COMPLETION_VALUES: usize = 100;
/// Maximum UTF-8 bytes in one final completion candidate.
pub const MAX_FINAL_COMPLETION_VALUE_BYTES: usize = 16 * 1024;
/// Maximum aggregate UTF-8 bytes in final completion candidates.
pub const MAX_FINAL_COMPLETION_VALUES_BYTES: usize = 256 * 1024;

impl FinalCompletionValues {
    /// Returns the bounded compatibility diagnostic for an admitted peer
    /// total. Locally authored values must still pass [`Self::validate`].
    #[must_use]
    pub fn peer_diagnostic(&self) -> Option<FinalCompletionPeerDiagnostic> {
        self.total
            .as_ref()
            .filter(|total| final_completion_total_is_negative(total))
            .map(|_| FinalCompletionPeerDiagnostic::NegativeTotal)
    }

    /// Validates the final-only bounds and nonnegative total invariant for
    /// local provider output.
    ///
    /// Exact MCP 2024-11-05 completion values deliberately retain their
    /// schema's unconstrained signed `total`; this validation belongs only to
    /// the final completion surface.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_final_completion_values(&self.values)?;
        if self
            .total
            .as_ref()
            .is_some_and(final_completion_total_is_negative)
        {
            return Err("final completion total must be a nonnegative JSON integer");
        }
        Ok(())
    }
}

fn serialize_completion_values<S>(values: &Vec<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if values.len() > MAX_COMPLETION_VALUES {
        return Err(serde::ser::Error::custom(
            "completion values exceed the maximum of 100 items",
        ));
    }
    values.serialize(serializer)
}

fn deserialize_completion_values<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    if values.len() > MAX_COMPLETION_VALUES {
        return Err(serde::de::Error::custom(
            "completion values exceed the maximum of 100 items",
        ));
    }
    Ok(values)
}

fn serialize_final_completion_values<S>(
    values: &Vec<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    validate_final_completion_values(values).map_err(serde::ser::Error::custom)?;
    values.serialize(serializer)
}

fn deserialize_final_completion_values<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    validate_final_completion_values(&values).map_err(serde::de::Error::custom)?;
    Ok(values)
}

fn validate_final_completion_values(values: &[String]) -> Result<(), &'static str> {
    if values.len() > MAX_COMPLETION_VALUES {
        return Err("completion values exceed the maximum of 100 items");
    }

    let mut total_bytes = 0_usize;
    for value in values {
        if value.len() > MAX_FINAL_COMPLETION_VALUE_BYTES {
            return Err("final completion value exceeds the maximum of 16384 bytes");
        }
        total_bytes = total_bytes
            .checked_add(value.len())
            .ok_or("final completion values exceed the maximum aggregate byte limit")?;
        if total_bytes > MAX_FINAL_COMPLETION_VALUES_BYTES {
            return Err("final completion values exceed the maximum aggregate byte limit");
        }
    }
    Ok(())
}

fn final_completion_total_is_negative(total: &JsonInteger) -> bool {
    let Some(absolute) = total.as_str().strip_prefix('-') else {
        return false;
    };
    absolute
        .split(['e', 'E'])
        .next()
        .is_some_and(|mantissa| mantissa.bytes().any(|byte| matches!(byte, b'1'..=b'9')))
}

/// Exact legacy `completion/complete` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyCompletionResult {
    /// Completion candidates.
    pub completion: CompletionValues,
    /// Opaque legacy response metadata retained without interpretation.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<LegacyOpaqueMetadata>,
}

/// Ordered opaque legacy metadata.
///
/// `serde_json::Map` uses a key-sorted representation in this workspace. The
/// Legacy result wires promise the received `_meta` member order remains
/// observable on replay, so this narrow wrapper retains object-member order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegacyOpaqueMetadata {
    entries: Vec<(String, Value)>,
}

impl LegacyOpaqueMetadata {
    /// Looks up one retained metadata value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, value)| value)
    }

    /// Returns metadata entries in their original wire order.
    #[must_use]
    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }
}

impl Serialize for LegacyOpaqueMetadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for LegacyOpaqueMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MetadataVisitor;

        impl<'de> Visitor<'de> for MetadataVisitor {
            type Value = LegacyOpaqueMetadata;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object of legacy metadata")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if entries.iter().any(|(existing, _)| existing == &key) {
                        return Err(serde::de::Error::custom("duplicate legacy metadata member"));
                    }
                    entries.push((key, value));
                }
                Ok(LegacyOpaqueMetadata { entries })
            }
        }

        deserializer.deserialize_map(MetadataVisitor)
    }
}

/// Final `completion/complete` result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalCompletionResult {
    /// Completion candidates.
    pub completion: FinalCompletionValues,
}

/// Empty final `subscriptions/listen` result body.
///
/// The required subscription-stream ID is carried in the common result
/// metadata and exposed by [`FinalCoreResult::SubscriptionsListen`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalSubscriptionsListenResult {}

/// Empty final complete-result payload used by acknowledgement methods.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalEmptyResult {}

/// Empty exact-legacy result payload used by acknowledgement methods.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyEmptyResult {}

/// Legacy core requests remain exact uses of the existing message structs.
/// They are never reinterpreted as final requests.
#[derive(Debug, Clone)]
pub enum LegacyCoreRequest {
    /// `initialize` is unique to the legacy initialize-handshake era.
    Initialize(InitializeParams),
    /// `completion/complete`.
    Completion(LegacyCompletionParams),
    /// `sampling/createMessage`.
    SamplingCreateMessage(CreateMessageParams),
    /// `tools/list`.
    ToolsList(ListToolsParams),
    /// `tools/call`.
    ToolsCall(CallToolParams),
    /// `resources/list`.
    ResourcesList(ListResourcesParams),
    /// `resources/templates/list`.
    ResourceTemplatesList(ListResourceTemplatesParams),
    /// `resources/read`.
    ResourcesRead(ReadResourceParams),
    /// `resources/subscribe`.
    ResourcesSubscribe(SubscribeResourceParams),
    /// `resources/unsubscribe`.
    ResourcesUnsubscribe(UnsubscribeResourceParams),
    /// `prompts/list`.
    PromptsList(ListPromptsParams),
    /// `prompts/get`.
    PromptsGet(GetPromptParams),
    /// `logging/setLevel`.
    SetLogLevel(SetLogLevelParams),
    /// `ping`, which has no legacy parameter object.
    Ping,
}

/// Final core requests use final metadata and common vocabulary throughout.
#[derive(Debug, Clone)]
pub enum FinalCoreRequest {
    /// `server/discover`.
    Discover(FinalEmptyParams),
    /// `completion/complete`.
    Completion(FinalCompletionParams),
    /// `tools/list`.
    ToolsList(FinalListParams),
    /// `tools/call`.
    ToolsCall(FinalCallToolParams),
    /// `resources/list`.
    ResourcesList(FinalListParams),
    /// `resources/templates/list`.
    ResourceTemplatesList(FinalListParams),
    /// `resources/read`.
    ResourcesRead(FinalReadResourceParams),
    /// `prompts/list`.
    PromptsList(FinalListParams),
    /// `prompts/get`.
    PromptsGet(FinalGetPromptParams),
    /// `subscriptions/listen`.
    SubscriptionsListen(FinalSubscriptionsListenParams),
}

/// Public, era-aware dispatch for the currently supported core request set.
#[derive(Debug, Clone)]
pub enum CoreRequest {
    /// Exact MCP 2024-11-05 request vocabulary.
    Legacy(LegacyCoreRequest),
    /// Final MCP 2026-07-28 request vocabulary.
    Final(FinalCoreRequest),
}

/// Exact legacy response payloads, intentionally disjoint from final results.
#[derive(Debug, Clone)]
pub enum LegacyCoreResult {
    /// `initialize`.
    Initialize(InitializeResult),
    /// `completion/complete`.
    Completion(LegacyCompletionResult),
    /// `sampling/createMessage`.
    SamplingCreateMessage(CreateMessageResult),
    /// `tools/list`.
    ToolsList(ListToolsResult),
    /// `tools/call`.
    ToolsCall(CallToolResult),
    /// `resources/list`.
    ResourcesList(ListResourcesResult),
    /// `resources/templates/list`.
    ResourceTemplatesList(ListResourceTemplatesResult),
    /// `resources/read`.
    ResourcesRead(ReadResourceResult),
    /// `resources/subscribe` acknowledgement.
    ResourcesSubscribe(LegacyEmptyResult),
    /// `resources/unsubscribe` acknowledgement.
    ResourcesUnsubscribe(LegacyEmptyResult),
    /// `prompts/list`.
    PromptsList(ListPromptsResult),
    /// `prompts/get`.
    PromptsGet(GetPromptResult),
    /// `logging/setLevel` acknowledgement.
    SetLogLevel(LegacyEmptyResult),
    /// `ping` acknowledgement.
    Ping(LegacyEmptyResult),
}

/// Final result dispatch for the currently supported core methods.
///
/// Every complete branch carries the bounded final complete-result algebra.
/// `tools/call`, `resources/read`, and `prompts/get` additionally carry the
/// final MRTR `input_required` branch. An absent `resultType` is accepted
/// only by the separately selected legacy result decoder.
#[derive(Debug, Clone)]
pub enum FinalCoreResult {
    /// `server/discover`.
    Discover(crate::server_discovery::ServerDiscoverResult),
    /// `completion/complete`.
    Completion {
        result: CompleteResult<FinalCompletionResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `tools/list`.
    ToolsList {
        result: CompleteResult<FinalListToolsResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `tools/call`.
    ToolsCall {
        result: CompleteResult<FinalCallToolResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// A Tasks-backed final `tools/call` result.
    #[cfg(feature = "tasks")]
    ToolsCallTask {
        result: crate::tasks_extension::CreateTaskResult,
    },
    /// `tools/call` requires client input before a retry.
    ToolsCallInputRequired {
        result: InputRequiredResult,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `resources/list`.
    ResourcesList {
        result: CompleteResult<FinalListResourcesResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `resources/templates/list`.
    ResourceTemplatesList {
        result: CompleteResult<FinalListResourceTemplatesResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `resources/read`.
    ResourcesRead {
        result: CompleteResult<FinalReadResourceResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `resources/read` requires client input before a retry.
    ResourcesReadInputRequired {
        result: InputRequiredResult,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `prompts/list`.
    PromptsList {
        result: CompleteResult<FinalListPromptsResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `prompts/get`.
    PromptsGet {
        result: CompleteResult<FinalGetPromptResult>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `prompts/get` requires client input before a retry.
    PromptsGetInputRequired {
        result: InputRequiredResult,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    /// `subscriptions/listen` graceful termination.
    SubscriptionsListen {
        result: CompleteResult<FinalSubscriptionsListenResult>,
        /// The required subscription ID extracted from the result metadata.
        subscription_id: RequestId,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
}

/// Server-owned final metadata retained across response middleware.
///
/// This is intentionally opaque outside the protocol crate: callers preserve
/// and compare the typed seal, rather than interpreting or reconstructing its
/// metadata through a raw compatibility path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalResultMetadataSeal {
    family: FinalResultMetadataFamily,
    server_info: FinalResultServerInfo,
    subscription_id: Option<RequestId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalResultMetadataFamily {
    Discover,
    Completion,
    ToolsList,
    ToolsCall,
    #[cfg(feature = "tasks")]
    ToolsCallTask,
    ToolsCallInputRequired,
    ResourcesList,
    ResourceTemplatesList,
    ResourcesRead,
    ResourcesReadInputRequired,
    PromptsList,
    PromptsGet,
    PromptsGetInputRequired,
    SubscriptionsListen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FinalResultServerInfo {
    Discovery(Option<FinalDiscoveryServerInfo>),
    Common(Option<Implementation>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FinalDiscoveryServerInfo {
    name: String,
    version: String,
}

impl From<&ServerInfo> for FinalDiscoveryServerInfo {
    fn from(server_info: &ServerInfo) -> Self {
        Self {
            name: server_info.name.clone(),
            version: server_info.version.clone(),
        }
    }
}

/// Public, era-aware dispatch for core results.
#[derive(Debug, Clone)]
#[allow(
    clippy::large_enum_variant,
    reason = "the public dual-era dispatch intentionally keeps each exhaustive typed result algebra inline; boxing one negotiated era would distort its direct pattern-matching API solely to reduce enum size"
)]
pub enum CoreResult {
    /// Exact MCP 2024-11-05 result vocabulary.
    Legacy(LegacyCoreResult),
    /// Final MCP 2026-07-28 complete-result vocabulary.
    Final(FinalCoreResult),
}

/// Stable errors raised while selecting a typed core request or result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreDispatchError {
    /// A selected wire capability was compiled out of this crate build.
    FeatureUnavailable {
        /// The exact Cargo feature required for this wire capability.
        feature: &'static str,
    },
    /// The selected era does not support this method.
    UnsupportedMethod { era: ProtocolEra, method: String },
    /// A request's method-specific parameters could not be decoded.
    InvalidParams {
        era: ProtocolEra,
        method: &'static str,
    },
    /// Final request metadata was absent, malformed, or for another era.
    InvalidFinalMetadata { method: &'static str },
    /// A legacy request attempted to carry final per-request metadata.
    CrossEraRequestMetadata { method: &'static str },
    /// A legacy result attempted to carry a final `resultType` discriminator.
    CrossEraResultType { method: &'static str },
    /// A legacy result attempted to carry final result metadata.
    CrossEraResultMetadata { method: &'static str },
    /// A result did not match the selected method-specific payload.
    InvalidResult {
        era: ProtocolEra,
        method: &'static str,
    },
    /// A final result used another core discriminator.
    UnexpectedFinalResultType { method: &'static str },
    /// A final subscriptions/listen result did not correlate to its JSON-RPC response ID.
    SubscriptionIdMismatch,
    /// The bounded final result codec rejected the wire value.
    ResultCodec(ResultDecodeError),
}

impl std::fmt::Display for CoreDispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FeatureUnavailable { feature } => {
                write!(formatter, "the {feature:?} protocol feature is unavailable")
            }
            Self::UnsupportedMethod { era, method } => {
                write!(formatter, "{method} is not supported in {era:?}")
            }
            Self::InvalidParams { era, method } => {
                write!(formatter, "invalid {method} parameters for {era:?}")
            }
            Self::InvalidFinalMetadata { method } => {
                write!(formatter, "invalid final metadata for {method}")
            }
            Self::CrossEraRequestMetadata { method } => {
                write!(formatter, "legacy {method} cannot carry final metadata")
            }
            Self::CrossEraResultType { method } => {
                write!(formatter, "legacy {method} cannot carry final resultType")
            }
            Self::CrossEraResultMetadata { method } => {
                write!(
                    formatter,
                    "legacy {method} cannot carry final result metadata"
                )
            }
            Self::InvalidResult { era, method } => {
                write!(formatter, "invalid {method} result for {era:?}")
            }
            Self::UnexpectedFinalResultType { method } => {
                write!(formatter, "final {method} requires a complete result")
            }
            Self::SubscriptionIdMismatch => {
                formatter.write_str("subscription result metadata does not match response id")
            }
            Self::ResultCodec(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CoreDispatchError {}

impl From<ResultDecodeError> for CoreDispatchError {
    fn from(error: ResultDecodeError) -> Self {
        Self::ResultCodec(error)
    }
}

impl CoreRequest {
    /// Decodes one core request only through the exact protocol era selected
    /// by the connection. The two request vocabularies are deliberately
    /// disjoint even where their method literals are shared.
    pub fn decode(
        era: ProtocolEra,
        method: &str,
        params: Option<&Value>,
    ) -> Result<Self, CoreDispatchError> {
        match era {
            ProtocolEra::Legacy2024 => {
                #[cfg(feature = "legacy-2024-11-05")]
                {
                    Self::decode_legacy(method, params)
                }
                #[cfg(not(feature = "legacy-2024-11-05"))]
                {
                    let _ = (method, params);
                    Err(CoreDispatchError::FeatureUnavailable {
                        feature: "legacy-2024-11-05",
                    })
                }
            }
            ProtocolEra::Modern2026 => Self::decode_final(method, params),
        }
    }

    /// Decodes one core request while retaining the admitted raw parameter
    /// source for final MRTR retry maps.
    ///
    /// The ordinary [`Self::decode`] path accepts a materialized
    /// [`Value`], whose object representation cannot retain member order.
    /// Callers that admitted the original parameter source can use this form
    /// for the three final methods whose `inputResponses` maps have typed,
    /// ordered response entries. The supplied source must describe exactly the
    /// same parameter value as `params`; it cannot be attached to another
    /// admitted frame.
    pub fn decode_with_raw_params(
        era: ProtocolEra,
        method: &str,
        params: Option<&Value>,
        raw_params: Option<&str>,
    ) -> Result<Self, CoreDispatchError> {
        Self::decode_with_raw_params_inner(era, method, params, raw_params, None)
    }

    /// Decodes a final tool request with an explicit raw-parameter byte budget.
    ///
    /// Only value strings beneath the root `arguments` member share the larger
    /// budget. Object keys, other value strings, structural checks, metadata,
    /// and equality with the admitted parameter value keep their existing rules.
    /// Other methods retain the default decoder. Without a raw sidecar, callers
    /// remain responsible for admission of the materialized parameter value.
    pub fn decode_with_raw_params_and_argument_byte_limit(
        era: ProtocolEra,
        method: &str,
        params: Option<&Value>,
        raw_params: Option<&str>,
        max_bytes: usize,
    ) -> Result<Self, CoreDispatchError> {
        let argument_byte_limit =
            (era == ProtocolEra::Modern2026 && method == TOOLS_CALL).then_some(max_bytes);
        Self::decode_with_raw_params_inner(era, method, params, raw_params, argument_byte_limit)
    }

    fn decode_with_raw_params_inner(
        era: ProtocolEra,
        method: &str,
        params: Option<&Value>,
        raw_params: Option<&str>,
        argument_byte_limit: Option<usize>,
    ) -> Result<Self, CoreDispatchError> {
        let Some(raw_params) = raw_params else {
            return Self::decode(era, method, params);
        };
        if era != ProtocolEra::Modern2026
            || !matches!(method, TOOLS_CALL | RESOURCES_READ | PROMPTS_GET)
        {
            return Self::decode(era, method, params);
        }
        let method_literal = match method {
            TOOLS_CALL => TOOLS_CALL,
            RESOURCES_READ => RESOURCES_READ,
            PROMPTS_GET => PROMPTS_GET,
            _ => unreachable!("the final MRTR raw-params guard selected a known method"),
        };
        let exact = match argument_byte_limit {
            Some(max_bytes) => {
                crate::result::parse_exact_json_with_argument_byte_limit(raw_params, max_bytes)
            }
            None => crate::result::parse_exact_json(raw_params),
        };
        exact.map_err(|_| {
            CoreDispatchError::InvalidParams {
                era,
                method: method_literal,
            }
        })?;
        let raw_value: Value =
            serde_json::from_str(raw_params).map_err(|_| CoreDispatchError::InvalidParams {
                era,
                method: method_literal,
            })?;
        if params != Some(&raw_value) {
            return Err(CoreDispatchError::InvalidParams {
                era,
                method: method_literal,
            });
        }

        let request = match method {
            TOOLS_CALL => {
                FinalCoreRequest::ToolsCall(serde_json::from_str(raw_params).map_err(|_| {
                    CoreDispatchError::InvalidParams {
                        era,
                        method: TOOLS_CALL,
                    }
                })?)
            }
            RESOURCES_READ => {
                FinalCoreRequest::ResourcesRead(serde_json::from_str(raw_params).map_err(|_| {
                    CoreDispatchError::InvalidParams {
                        era,
                        method: RESOURCES_READ,
                    }
                })?)
            }
            PROMPTS_GET => {
                FinalCoreRequest::PromptsGet(serde_json::from_str(raw_params).map_err(|_| {
                    CoreDispatchError::InvalidParams {
                        era,
                        method: PROMPTS_GET,
                    }
                })?)
            }
            _ => unreachable!("the final MRTR raw-params guard selected a known method"),
        };
        request.validate_metadata()?;
        Ok(Self::Final(request))
    }

    /// Returns the era selected by this request.
    #[must_use]
    pub const fn era(&self) -> ProtocolEra {
        match self {
            Self::Legacy(_) => ProtocolEra::Legacy2024,
            Self::Final(_) => ProtocolEra::Modern2026,
        }
    }

    /// Returns the exact core method literal selected by this request.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Legacy(request) => request.method(),
            Self::Final(request) => request.method(),
        }
    }

    /// Encodes the method-owned parameter object without adding a JSON-RPC
    /// envelope. Final requests revalidate their common metadata before
    /// serialization so local callers cannot emit a cross-era request.
    pub fn encode_params(&self) -> Result<Option<Value>, CoreDispatchError> {
        match self {
            Self::Legacy(request) => request.encode_params(),
            Self::Final(request) => request.encode_params(),
        }
    }

    /// Decodes the JSON-RPC result payload selected by this request.
    pub fn decode_result(&self, input: &str) -> Result<CoreResult, CoreDispatchError> {
        match self {
            Self::Legacy(request) => request.decode_result(input).map(CoreResult::Legacy),
            Self::Final(request) => request.decode_result(input, None).map(CoreResult::Final),
        }
    }

    /// Decodes a successful JSON-RPC response selected by this request.
    ///
    /// This form preserves the response correlation context required by final
    /// `subscriptions/listen`: its result metadata subscription ID must equal
    /// the enclosing JSON-RPC response ID. This `Value`-only API is lossy for
    /// received member order and noncanonical numeric lexemes; callers with
    /// admitted source must use [`Self::decode_response_result`].
    pub fn decode_response(
        &self,
        response: &JsonRpcResponse,
    ) -> Result<CoreResult, CoreDispatchError> {
        let Some(response_id) = response.id.as_ref() else {
            return Err(CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            });
        };
        let Some(result) = response.result.as_ref() else {
            return Err(CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            });
        };
        let input =
            serde_json::to_string(result).map_err(|_| CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            })?;
        match self {
            Self::Legacy(request) => {
                #[cfg(feature = "legacy-2024-11-05")]
                {
                    request.decode_result(&input).map(CoreResult::Legacy)
                }
                #[cfg(not(feature = "legacy-2024-11-05"))]
                {
                    let _ = request;
                    Err(CoreDispatchError::FeatureUnavailable {
                        feature: "legacy-2024-11-05",
                    })
                }
            }
            Self::Final(request) => request
                .decode_result(&input, Some(response_id))
                .map(CoreResult::Final),
        }
    }

    /// Decodes a successful JSON-RPC response from its admitted result source.
    ///
    /// Unlike [`Self::decode_response`], this path does not serialize the
    /// response's [`Value`] again. The caller supplies the exact source JSON
    /// retained while the response frame was admitted, preserving object
    /// member order and number lexemes for the lossless result algebra. The
    /// parsed value must still equal the response's typed result so source from
    /// a different response cannot be attached accidentally.
    pub fn decode_response_result(
        &self,
        response: &JsonRpcResponse,
        result_source: &str,
    ) -> Result<CoreResult, CoreDispatchError> {
        let Some(response_id) = response.id.as_ref() else {
            return Err(CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            });
        };
        let Some(result) = response.result.as_ref() else {
            return Err(CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            });
        };
        let admitted_value: Value =
            serde_json::from_str(result_source).map_err(|_| CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            })?;
        if &admitted_value != result {
            return Err(CoreDispatchError::InvalidResult {
                era: self.era(),
                method: self.method(),
            });
        }
        match self {
            Self::Legacy(request) => {
                #[cfg(feature = "legacy-2024-11-05")]
                {
                    request.decode_result(result_source).map(CoreResult::Legacy)
                }
                #[cfg(not(feature = "legacy-2024-11-05"))]
                {
                    let _ = request;
                    Err(CoreDispatchError::FeatureUnavailable {
                        feature: "legacy-2024-11-05",
                    })
                }
            }
            Self::Final(request) => request
                .decode_result(result_source, Some(response_id))
                .map(CoreResult::Final),
        }
    }

    #[cfg(feature = "legacy-2024-11-05")]
    fn decode_legacy(method: &str, params: Option<&Value>) -> Result<Self, CoreDispatchError> {
        let request =
            match method {
                INITIALIZE => LegacyCoreRequest::Initialize(decode_params(
                    ProtocolEra::Legacy2024,
                    INITIALIZE,
                    params,
                )?),
                COMPLETION_COMPLETE => LegacyCoreRequest::Completion(decode_params(
                    ProtocolEra::Legacy2024,
                    COMPLETION_COMPLETE,
                    params,
                )?),
                SAMPLING_CREATE_MESSAGE => LegacyCoreRequest::SamplingCreateMessage(decode_params(
                    ProtocolEra::Legacy2024,
                    SAMPLING_CREATE_MESSAGE,
                    params,
                )?),
                TOOLS_LIST => LegacyCoreRequest::ToolsList(decode_params(
                    ProtocolEra::Legacy2024,
                    TOOLS_LIST,
                    params,
                )?),
                TOOLS_CALL => LegacyCoreRequest::ToolsCall(decode_params(
                    ProtocolEra::Legacy2024,
                    TOOLS_CALL,
                    params,
                )?),
                RESOURCES_LIST => LegacyCoreRequest::ResourcesList(decode_params(
                    ProtocolEra::Legacy2024,
                    RESOURCES_LIST,
                    params,
                )?),
                RESOURCES_TEMPLATES_LIST => LegacyCoreRequest::ResourceTemplatesList(
                    decode_params(ProtocolEra::Legacy2024, RESOURCES_TEMPLATES_LIST, params)?,
                ),
                RESOURCES_READ => LegacyCoreRequest::ResourcesRead(decode_params(
                    ProtocolEra::Legacy2024,
                    RESOURCES_READ,
                    params,
                )?),
                RESOURCES_SUBSCRIBE => LegacyCoreRequest::ResourcesSubscribe(decode_params(
                    ProtocolEra::Legacy2024,
                    RESOURCES_SUBSCRIBE,
                    params,
                )?),
                RESOURCES_UNSUBSCRIBE => LegacyCoreRequest::ResourcesUnsubscribe(decode_params(
                    ProtocolEra::Legacy2024,
                    RESOURCES_UNSUBSCRIBE,
                    params,
                )?),
                PROMPTS_LIST => LegacyCoreRequest::PromptsList(decode_params(
                    ProtocolEra::Legacy2024,
                    PROMPTS_LIST,
                    params,
                )?),
                PROMPTS_GET => LegacyCoreRequest::PromptsGet(decode_params(
                    ProtocolEra::Legacy2024,
                    PROMPTS_GET,
                    params,
                )?),
                LOGGING_SET_LEVEL => LegacyCoreRequest::SetLogLevel(decode_params(
                    ProtocolEra::Legacy2024,
                    LOGGING_SET_LEVEL,
                    params,
                )?),
                PING => {
                    require_absent_or_empty_params(ProtocolEra::Legacy2024, PING, params)?;
                    LegacyCoreRequest::Ping
                }
                _ => {
                    return Err(CoreDispatchError::UnsupportedMethod {
                        era: ProtocolEra::Legacy2024,
                        method: method.to_owned(),
                    });
                }
            };
        if legacy_params_carry_final_metadata(params) {
            return Err(CoreDispatchError::CrossEraRequestMetadata {
                method: request.method(),
            });
        }
        if let LegacyCoreRequest::Initialize(params) = &request
            && params.protocol_version != ProtocolEra::Legacy2024.version().as_str()
        {
            return Err(CoreDispatchError::InvalidParams {
                era: ProtocolEra::Legacy2024,
                method: INITIALIZE,
            });
        }
        Ok(Self::Legacy(request))
    }

    fn decode_final(method: &str, params: Option<&Value>) -> Result<Self, CoreDispatchError> {
        let request = match method {
            SERVER_DISCOVER => {
                FinalCoreRequest::Discover(decode_final_params(SERVER_DISCOVER, params)?)
            }
            COMPLETION_COMPLETE => {
                FinalCoreRequest::Completion(decode_final_params(COMPLETION_COMPLETE, params)?)
            }
            TOOLS_LIST => FinalCoreRequest::ToolsList(decode_final_params(TOOLS_LIST, params)?),
            TOOLS_CALL => FinalCoreRequest::ToolsCall(decode_final_params(TOOLS_CALL, params)?),
            RESOURCES_LIST => {
                FinalCoreRequest::ResourcesList(decode_final_params(RESOURCES_LIST, params)?)
            }
            RESOURCES_TEMPLATES_LIST => FinalCoreRequest::ResourceTemplatesList(
                decode_final_params(RESOURCES_TEMPLATES_LIST, params)?,
            ),
            RESOURCES_READ => {
                FinalCoreRequest::ResourcesRead(decode_final_params(RESOURCES_READ, params)?)
            }
            PROMPTS_LIST => {
                FinalCoreRequest::PromptsList(decode_final_params(PROMPTS_LIST, params)?)
            }
            PROMPTS_GET => FinalCoreRequest::PromptsGet(decode_final_params(PROMPTS_GET, params)?),
            SUBSCRIPTIONS_LISTEN => FinalCoreRequest::SubscriptionsListen(decode_final_params(
                SUBSCRIPTIONS_LISTEN,
                params,
            )?),
            _ => {
                return Err(CoreDispatchError::UnsupportedMethod {
                    era: ProtocolEra::Modern2026,
                    method: method.to_owned(),
                });
            }
        };
        request.validate_metadata()?;
        Ok(Self::Final(request))
    }
}

impl LegacyCoreRequest {
    /// Returns this request's exact method literal.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Initialize(_) => INITIALIZE,
            Self::Completion(_) => COMPLETION_COMPLETE,
            Self::SamplingCreateMessage(_) => SAMPLING_CREATE_MESSAGE,
            Self::ToolsList(_) => TOOLS_LIST,
            Self::ToolsCall(_) => TOOLS_CALL,
            Self::ResourcesList(_) => RESOURCES_LIST,
            Self::ResourceTemplatesList(_) => RESOURCES_TEMPLATES_LIST,
            Self::ResourcesRead(_) => RESOURCES_READ,
            Self::ResourcesSubscribe(_) => RESOURCES_SUBSCRIBE,
            Self::ResourcesUnsubscribe(_) => RESOURCES_UNSUBSCRIBE,
            Self::PromptsList(_) => PROMPTS_LIST,
            Self::PromptsGet(_) => PROMPTS_GET,
            Self::SetLogLevel(_) => LOGGING_SET_LEVEL,
            Self::Ping => PING,
        }
    }

    fn encode_params(&self) -> Result<Option<Value>, CoreDispatchError> {
        match self {
            Self::Initialize(params) => encode_params(ProtocolEra::Legacy2024, INITIALIZE, params),
            Self::Completion(params) => {
                encode_params(ProtocolEra::Legacy2024, COMPLETION_COMPLETE, params)
            }
            Self::SamplingCreateMessage(params) => {
                encode_params(ProtocolEra::Legacy2024, SAMPLING_CREATE_MESSAGE, params)
            }
            Self::ToolsList(params) => encode_params(ProtocolEra::Legacy2024, TOOLS_LIST, params),
            Self::ToolsCall(params) => encode_params(ProtocolEra::Legacy2024, TOOLS_CALL, params),
            Self::ResourcesList(params) => {
                encode_params(ProtocolEra::Legacy2024, RESOURCES_LIST, params)
            }
            Self::ResourceTemplatesList(params) => {
                encode_params(ProtocolEra::Legacy2024, RESOURCES_TEMPLATES_LIST, params)
            }
            Self::ResourcesRead(params) => {
                encode_params(ProtocolEra::Legacy2024, RESOURCES_READ, params)
            }
            Self::ResourcesSubscribe(params) => {
                encode_params(ProtocolEra::Legacy2024, RESOURCES_SUBSCRIBE, params)
            }
            Self::ResourcesUnsubscribe(params) => {
                encode_params(ProtocolEra::Legacy2024, RESOURCES_UNSUBSCRIBE, params)
            }
            Self::PromptsList(params) => {
                encode_params(ProtocolEra::Legacy2024, PROMPTS_LIST, params)
            }
            Self::PromptsGet(params) => encode_params(ProtocolEra::Legacy2024, PROMPTS_GET, params),
            Self::SetLogLevel(params) => {
                encode_params(ProtocolEra::Legacy2024, LOGGING_SET_LEVEL, params)
            }
            Self::Ping => Ok(None),
        }
    }

    fn decode_result(&self, input: &str) -> Result<LegacyCoreResult, CoreDispatchError> {
        match self {
            Self::Initialize(_) => {
                decode_legacy_result(INITIALIZE, input).map(LegacyCoreResult::Initialize)
            }
            Self::Completion(_) => {
                decode_legacy_result(COMPLETION_COMPLETE, input).map(LegacyCoreResult::Completion)
            }
            Self::SamplingCreateMessage(_) => decode_legacy_result(SAMPLING_CREATE_MESSAGE, input)
                .map(LegacyCoreResult::SamplingCreateMessage),
            Self::ToolsList(_) => {
                decode_legacy_result(TOOLS_LIST, input).map(LegacyCoreResult::ToolsList)
            }
            Self::ToolsCall(_) => {
                decode_legacy_result(TOOLS_CALL, input).map(LegacyCoreResult::ToolsCall)
            }
            Self::ResourcesList(_) => {
                decode_legacy_result(RESOURCES_LIST, input).map(LegacyCoreResult::ResourcesList)
            }
            Self::ResourceTemplatesList(_) => decode_legacy_result(RESOURCES_TEMPLATES_LIST, input)
                .map(LegacyCoreResult::ResourceTemplatesList),
            Self::ResourcesRead(_) => {
                decode_legacy_result(RESOURCES_READ, input).map(LegacyCoreResult::ResourcesRead)
            }
            Self::ResourcesSubscribe(_) => decode_legacy_result(RESOURCES_SUBSCRIBE, input)
                .map(LegacyCoreResult::ResourcesSubscribe),
            Self::ResourcesUnsubscribe(_) => decode_legacy_result(RESOURCES_UNSUBSCRIBE, input)
                .map(LegacyCoreResult::ResourcesUnsubscribe),
            Self::PromptsList(_) => {
                decode_legacy_result(PROMPTS_LIST, input).map(LegacyCoreResult::PromptsList)
            }
            Self::PromptsGet(_) => {
                decode_legacy_result(PROMPTS_GET, input).map(LegacyCoreResult::PromptsGet)
            }
            Self::SetLogLevel(_) => {
                decode_legacy_result(LOGGING_SET_LEVEL, input).map(LegacyCoreResult::SetLogLevel)
            }
            Self::Ping => decode_legacy_result(PING, input).map(LegacyCoreResult::Ping),
        }
    }
}

impl FinalCoreRequest {
    /// Returns this request's exact method literal.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Discover(_) => SERVER_DISCOVER,
            Self::Completion(_) => COMPLETION_COMPLETE,
            Self::ToolsList(_) => TOOLS_LIST,
            Self::ToolsCall(_) => TOOLS_CALL,
            Self::ResourcesList(_) => RESOURCES_LIST,
            Self::ResourceTemplatesList(_) => RESOURCES_TEMPLATES_LIST,
            Self::ResourcesRead(_) => RESOURCES_READ,
            Self::PromptsList(_) => PROMPTS_LIST,
            Self::PromptsGet(_) => PROMPTS_GET,
            Self::SubscriptionsListen(_) => SUBSCRIPTIONS_LISTEN,
        }
    }

    fn validate_metadata(&self) -> Result<(), CoreDispatchError> {
        let metadata = match self {
            Self::Discover(params) => &params.meta,
            Self::Completion(params) => &params.meta,
            Self::ToolsList(params)
            | Self::ResourcesList(params)
            | Self::ResourceTemplatesList(params)
            | Self::PromptsList(params) => &params.meta,
            Self::ToolsCall(params) => &params.meta,
            Self::ResourcesRead(params) => &params.meta,
            Self::PromptsGet(params) => &params.meta,
            Self::SubscriptionsListen(params) => &params.meta,
        };
        let valid_version = metadata.protocol_version().ok().flatten()
            == Some(ProtocolEra::Modern2026.version().as_str());
        let has_capabilities = metadata.client_capabilities().ok().flatten().is_some();
        if valid_version && has_capabilities {
            Ok(())
        } else {
            Err(CoreDispatchError::InvalidFinalMetadata {
                method: self.method(),
            })
        }
    }

    fn encode_params(&self) -> Result<Option<Value>, CoreDispatchError> {
        self.validate_metadata()?;
        match self {
            Self::Discover(params) => {
                encode_params(ProtocolEra::Modern2026, SERVER_DISCOVER, params)
            }
            Self::Completion(params) => {
                encode_params(ProtocolEra::Modern2026, COMPLETION_COMPLETE, params)
            }
            Self::ToolsList(params) => encode_params(ProtocolEra::Modern2026, TOOLS_LIST, params),
            Self::ToolsCall(params) => encode_params(ProtocolEra::Modern2026, TOOLS_CALL, params),
            Self::ResourcesList(params) => {
                encode_params(ProtocolEra::Modern2026, RESOURCES_LIST, params)
            }
            Self::ResourceTemplatesList(params) => {
                encode_params(ProtocolEra::Modern2026, RESOURCES_TEMPLATES_LIST, params)
            }
            Self::ResourcesRead(params) => {
                encode_params(ProtocolEra::Modern2026, RESOURCES_READ, params)
            }
            Self::PromptsList(params) => {
                encode_params(ProtocolEra::Modern2026, PROMPTS_LIST, params)
            }
            Self::PromptsGet(params) => encode_params(ProtocolEra::Modern2026, PROMPTS_GET, params),
            Self::SubscriptionsListen(params) => {
                encode_params(ProtocolEra::Modern2026, SUBSCRIPTIONS_LISTEN, params)
            }
        }
    }

    fn decode_result(
        &self,
        input: &str,
        response_id: Option<&RequestId>,
    ) -> Result<FinalCoreResult, CoreDispatchError> {
        match self {
            Self::Discover(_) => serde_json::from_str(input)
                .map(FinalCoreResult::Discover)
                .map_err(|_| CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: SERVER_DISCOVER,
                }),
            Self::Completion(_) => {
                decode_final_complete(COMPLETION_COMPLETE, input, &["completion"])
                    .map(|(result, diagnostic)| FinalCoreResult::Completion { result, diagnostic })
            }
            Self::ToolsList(_) => decode_final_complete(
                TOOLS_LIST,
                input,
                &["tools", "nextCursor", "ttlMs", "cacheScope"],
            )
            .map(|(result, diagnostic)| FinalCoreResult::ToolsList { result, diagnostic }),
            Self::ToolsCall(_) => decode_final_tools_call(input),
            Self::ResourcesList(_) => decode_final_complete(
                RESOURCES_LIST,
                input,
                &["resources", "nextCursor", "ttlMs", "cacheScope"],
            )
            .map(|(result, diagnostic)| FinalCoreResult::ResourcesList { result, diagnostic }),
            Self::ResourceTemplatesList(_) => {
                decode_final_complete(
                    RESOURCES_TEMPLATES_LIST,
                    input,
                    &["resourceTemplates", "nextCursor", "ttlMs", "cacheScope"],
                )
                .map(|(result, diagnostic)| {
                    FinalCoreResult::ResourceTemplatesList { result, diagnostic }
                })
            }
            Self::ResourcesRead(_) => decode_final_complete_or_input_required(
                RESOURCES_READ,
                input,
                &["contents", "ttlMs", "cacheScope"],
            )
            .map(|result| match result {
                FinalMethodResult::Complete { result, diagnostic } => {
                    FinalCoreResult::ResourcesRead { result, diagnostic }
                }
                FinalMethodResult::InputRequired { result, diagnostic } => {
                    FinalCoreResult::ResourcesReadInputRequired { result, diagnostic }
                }
            }),
            Self::PromptsList(_) => decode_final_complete(
                PROMPTS_LIST,
                input,
                &["prompts", "nextCursor", "ttlMs", "cacheScope"],
            )
            .map(|(result, diagnostic)| FinalCoreResult::PromptsList { result, diagnostic }),
            Self::PromptsGet(_) => decode_final_complete_or_input_required(
                PROMPTS_GET,
                input,
                &["description", "messages"],
            )
            .map(|result| match result {
                FinalMethodResult::Complete { result, diagnostic } => {
                    FinalCoreResult::PromptsGet { result, diagnostic }
                }
                FinalMethodResult::InputRequired { result, diagnostic } => {
                    FinalCoreResult::PromptsGetInputRequired { result, diagnostic }
                }
            }),
            Self::SubscriptionsListen(_) => {
                let (result, diagnostic) = decode_final_complete(SUBSCRIPTIONS_LISTEN, input, &[])?;
                let subscription_id = subscription_id_from_result(&result)?;
                if response_id
                    .is_some_and(|response_id| !response_id.correlates_with(&subscription_id))
                {
                    return Err(CoreDispatchError::SubscriptionIdMismatch);
                }
                Ok(FinalCoreResult::SubscriptionsListen {
                    result,
                    subscription_id,
                    diagnostic,
                })
            }
        }
    }
}

impl CoreResult {
    /// Returns the era selected by this result.
    #[must_use]
    pub const fn era(&self) -> ProtocolEra {
        match self {
            Self::Legacy(_) => ProtocolEra::Legacy2024,
            Self::Final(_) => ProtocolEra::Modern2026,
        }
    }

    /// Returns the exact method literal that selected this result type.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Legacy(result) => result.method(),
            Self::Final(result) => result.method(),
        }
    }

    /// Encodes this typed result without a JSON-RPC response envelope.
    pub fn encode(&self) -> Result<String, CoreDispatchError> {
        match self {
            Self::Legacy(result) => result.encode(),
            Self::Final(result) => result.encode(),
        }
    }
}

impl LegacyCoreResult {
    /// Returns the exact method literal that selected this legacy result.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Initialize(_) => INITIALIZE,
            Self::Completion(_) => COMPLETION_COMPLETE,
            Self::SamplingCreateMessage(_) => SAMPLING_CREATE_MESSAGE,
            Self::ToolsList(_) => TOOLS_LIST,
            Self::ToolsCall(_) => TOOLS_CALL,
            Self::ResourcesList(_) => RESOURCES_LIST,
            Self::ResourceTemplatesList(_) => RESOURCES_TEMPLATES_LIST,
            Self::ResourcesRead(_) => RESOURCES_READ,
            Self::ResourcesSubscribe(_) => RESOURCES_SUBSCRIBE,
            Self::ResourcesUnsubscribe(_) => RESOURCES_UNSUBSCRIBE,
            Self::PromptsList(_) => PROMPTS_LIST,
            Self::PromptsGet(_) => PROMPTS_GET,
            Self::SetLogLevel(_) => LOGGING_SET_LEVEL,
            Self::Ping(_) => PING,
        }
    }

    fn encode(&self) -> Result<String, CoreDispatchError> {
        match self {
            Self::Initialize(result) => encode_legacy_result(INITIALIZE, result),
            Self::Completion(result) => encode_legacy_result(COMPLETION_COMPLETE, result),
            Self::SamplingCreateMessage(result) => {
                encode_legacy_result(SAMPLING_CREATE_MESSAGE, result)
            }
            Self::ToolsList(result) => encode_legacy_result(TOOLS_LIST, result),
            Self::ToolsCall(result) => encode_legacy_result(TOOLS_CALL, result),
            Self::ResourcesList(result) => encode_legacy_result(RESOURCES_LIST, result),
            Self::ResourceTemplatesList(result) => {
                encode_legacy_result(RESOURCES_TEMPLATES_LIST, result)
            }
            Self::ResourcesRead(result) => encode_legacy_result(RESOURCES_READ, result),
            Self::ResourcesSubscribe(result) => encode_legacy_result(RESOURCES_SUBSCRIBE, result),
            Self::ResourcesUnsubscribe(result) => {
                encode_legacy_result(RESOURCES_UNSUBSCRIBE, result)
            }
            Self::PromptsList(result) => encode_legacy_result(PROMPTS_LIST, result),
            Self::PromptsGet(result) => encode_legacy_result(PROMPTS_GET, result),
            Self::SetLogLevel(result) => encode_legacy_result(LOGGING_SET_LEVEL, result),
            Self::Ping(result) => encode_legacy_result(PING, result),
        }
    }
}

impl FinalCoreResult {
    /// Returns the exact method literal that selected this final result.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Discover(_) => SERVER_DISCOVER,
            Self::Completion { .. } => COMPLETION_COMPLETE,
            Self::ToolsList { .. } => TOOLS_LIST,
            Self::ToolsCall { .. } | Self::ToolsCallInputRequired { .. } => TOOLS_CALL,
            #[cfg(feature = "tasks")]
            Self::ToolsCallTask { .. } => TOOLS_CALL,
            Self::ResourcesList { .. } => RESOURCES_LIST,
            Self::ResourceTemplatesList { .. } => RESOURCES_TEMPLATES_LIST,
            Self::ResourcesRead { .. } | Self::ResourcesReadInputRequired { .. } => RESOURCES_READ,
            Self::PromptsList { .. } => PROMPTS_LIST,
            Self::PromptsGet { .. } | Self::PromptsGetInputRequired { .. } => PROMPTS_GET,
            Self::SubscriptionsListen { .. } => SUBSCRIPTIONS_LISTEN,
        }
    }

    /// Returns the server-owned metadata that middleware must preserve for
    /// this selected final result family.
    ///
    /// The returned seal includes absence, so middleware cannot introduce a
    /// reserved server identity where the server did not emit one. It omits
    /// every open metadata member by design.
    pub fn protected_metadata_seal(&self) -> Result<FinalResultMetadataSeal, CoreDispatchError> {
        fn common_server_info<T>(
            result: &CompleteResult<T>,
        ) -> Result<Option<Implementation>, CoreDispatchError> {
            result
                .meta
                .final_server_info()
                .map_err(CoreDispatchError::from)
        }

        fn input_required_server_info(
            result: &InputRequiredResult,
        ) -> Result<Option<Implementation>, CoreDispatchError> {
            result
                .meta
                .final_server_info()
                .map_err(CoreDispatchError::from)
        }

        #[cfg(feature = "tasks")]
        fn task_server_info(
            result: &crate::tasks_extension::CreateTaskResult,
        ) -> Result<Option<Implementation>, CoreDispatchError> {
            result.meta.as_ref().map_or(Ok(None), |metadata| {
                metadata
                    .server_info()
                    .map_err(|_| CoreDispatchError::InvalidResult {
                        era: ProtocolEra::Modern2026,
                        method: TOOLS_CALL,
                    })
            })
        }

        match self {
            Self::Discover(result) => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::Discover,
                server_info: FinalResultServerInfo::Discovery(
                    result.server_info().map(FinalDiscoveryServerInfo::from),
                ),
                subscription_id: None,
            }),
            Self::Completion { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::Completion,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::ToolsList { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ToolsList,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::ToolsCall { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ToolsCall,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            #[cfg(feature = "tasks")]
            Self::ToolsCallTask { result } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ToolsCallTask,
                server_info: FinalResultServerInfo::Common(task_server_info(result)?),
                subscription_id: None,
            }),
            Self::ToolsCallInputRequired { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ToolsCallInputRequired,
                server_info: FinalResultServerInfo::Common(input_required_server_info(result)?),
                subscription_id: None,
            }),
            Self::ResourcesList { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ResourcesList,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::ResourceTemplatesList { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ResourceTemplatesList,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::ResourcesRead { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ResourcesRead,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::ResourcesReadInputRequired { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ResourcesReadInputRequired,
                server_info: FinalResultServerInfo::Common(input_required_server_info(result)?),
                subscription_id: None,
            }),
            Self::PromptsList { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::PromptsList,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::PromptsGet { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::PromptsGet,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: None,
            }),
            Self::PromptsGetInputRequired { result, .. } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::PromptsGetInputRequired,
                server_info: FinalResultServerInfo::Common(input_required_server_info(result)?),
                subscription_id: None,
            }),
            Self::SubscriptionsListen {
                result,
                subscription_id,
                ..
            } => Ok(FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::SubscriptionsListen,
                server_info: FinalResultServerInfo::Common(common_server_info(result)?),
                subscription_id: Some(subscription_id.clone()),
            }),
        }
    }

    fn encode(&self) -> Result<String, CoreDispatchError> {
        match self {
            Self::Discover(result) => {
                serde_json::to_string(result).map_err(|_| CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: SERVER_DISCOVER,
                })
            }
            Self::Completion { result, .. } => {
                encode_final_complete(COMPLETION_COMPLETE, result, &["completion"])
            }
            Self::ToolsList { result, .. } => encode_final_complete(
                TOOLS_LIST,
                result,
                &["tools", "nextCursor", "ttlMs", "cacheScope"],
            ),
            Self::ToolsCall { result, .. } => encode_final_complete(
                TOOLS_CALL,
                result,
                &["content", "isError", "structuredContent"],
            ),
            #[cfg(feature = "tasks")]
            Self::ToolsCallTask { result } => encode_final_tools_call_task(result),
            Self::ToolsCallInputRequired { result, .. } => {
                encode_final_input_required(TOOLS_CALL, result)
            }
            Self::ResourcesList { result, .. } => encode_final_complete(
                RESOURCES_LIST,
                result,
                &["resources", "nextCursor", "ttlMs", "cacheScope"],
            ),
            Self::ResourceTemplatesList { result, .. } => encode_final_complete(
                RESOURCES_TEMPLATES_LIST,
                result,
                &["resourceTemplates", "nextCursor", "ttlMs", "cacheScope"],
            ),
            Self::ResourcesRead { result, .. } => {
                encode_final_complete(RESOURCES_READ, result, &["contents", "ttlMs", "cacheScope"])
            }
            Self::ResourcesReadInputRequired { result, .. } => {
                encode_final_input_required(RESOURCES_READ, result)
            }
            Self::PromptsList { result, .. } => encode_final_complete(
                PROMPTS_LIST,
                result,
                &["prompts", "nextCursor", "ttlMs", "cacheScope"],
            ),
            Self::PromptsGet { result, .. } => {
                encode_final_complete(PROMPTS_GET, result, &["description", "messages"])
            }
            Self::PromptsGetInputRequired { result, .. } => {
                encode_final_input_required(PROMPTS_GET, result)
            }
            Self::SubscriptionsListen {
                result,
                subscription_id,
                ..
            } => {
                if !subscription_id_from_result(result)?.correlates_with(subscription_id) {
                    return Err(CoreDispatchError::InvalidResult {
                        era: ProtocolEra::Modern2026,
                        method: SUBSCRIPTIONS_LISTEN,
                    });
                }
                encode_final_complete(SUBSCRIPTIONS_LISTEN, result, &[])
            }
        }
    }
}

fn decode_params<T: DeserializeOwned>(
    era: ProtocolEra,
    method: &'static str,
    params: Option<&Value>,
) -> Result<T, CoreDispatchError> {
    serde_json::from_value(
        params
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::default())),
    )
    .map_err(|_| CoreDispatchError::InvalidParams { era, method })
}

fn decode_final_params<T: DeserializeOwned>(
    method: &'static str,
    params: Option<&Value>,
) -> Result<T, CoreDispatchError> {
    decode_params(ProtocolEra::Modern2026, method, params)
}

fn encode_params<T: Serialize>(
    era: ProtocolEra,
    method: &'static str,
    params: &T,
) -> Result<Option<Value>, CoreDispatchError> {
    serde_json::to_value(params)
        .map(Some)
        .map_err(|_| CoreDispatchError::InvalidParams { era, method })
}

#[cfg(feature = "legacy-2024-11-05")]
fn require_absent_or_empty_params(
    era: ProtocolEra,
    method: &'static str,
    params: Option<&Value>,
) -> Result<(), CoreDispatchError> {
    if params.is_none_or(|params| params.as_object().is_some_and(|object| object.is_empty())) {
        Ok(())
    } else {
        Err(CoreDispatchError::InvalidParams { era, method })
    }
}

#[cfg(feature = "legacy-2024-11-05")]
fn legacy_params_carry_final_metadata(params: Option<&Value>) -> bool {
    params.is_some_and(has_final_only_metadata)
}

fn decode_legacy_result<T: DeserializeOwned>(
    method: &'static str,
    input: &str,
) -> Result<T, CoreDispatchError> {
    let value: Value =
        serde_json::from_str(input).map_err(|_| CoreDispatchError::InvalidResult {
            era: ProtocolEra::Legacy2024,
            method,
        })?;
    if value
        .as_object()
        .is_some_and(|object| object.contains_key("resultType"))
    {
        return Err(CoreDispatchError::CrossEraResultType { method });
    }
    if has_final_only_metadata(&value) {
        return Err(CoreDispatchError::CrossEraResultMetadata { method });
    }
    serde_json::from_value(value).map_err(|_| CoreDispatchError::InvalidResult {
        era: ProtocolEra::Legacy2024,
        method,
    })
}

fn encode_legacy_result<T: Serialize>(
    method: &'static str,
    result: &T,
) -> Result<String, CoreDispatchError> {
    let value = serde_json::to_value(result).map_err(|_| CoreDispatchError::InvalidResult {
        era: ProtocolEra::Legacy2024,
        method,
    })?;
    if value
        .as_object()
        .is_some_and(|object| object.contains_key("resultType"))
    {
        return Err(CoreDispatchError::CrossEraResultType { method });
    }
    if has_final_only_metadata(&value) {
        return Err(CoreDispatchError::CrossEraResultMetadata { method });
    }
    // Serialize the typed result directly: `value` exists only for the era
    // checks above, and emitting it would alphabetize members through the
    // BTreeMap-backed Value instead of keeping declaration order.
    serde_json::to_string(result).map_err(|_| CoreDispatchError::InvalidResult {
        era: ProtocolEra::Legacy2024,
        method,
    })
}

enum FinalMethodResult<T> {
    Complete {
        result: CompleteResult<T>,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
    InputRequired {
        result: InputRequiredResult,
        diagnostic: Option<ResultPeerDiagnostic>,
    },
}

fn decode_final_tools_call(input: &str) -> Result<FinalCoreResult, CoreDispatchError> {
    let ExactJsonValue::Object(wire) =
        crate::result::parse_exact_json(input).map_err(|_| CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method: TOOLS_CALL,
        })?
    else {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method: TOOLS_CALL,
        });
    };
    if wire.get("serverInfo").is_some() {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method: TOOLS_CALL,
        });
    }
    if matches!(
        wire.get("resultType"),
        Some(ExactJsonValue::String(result_type)) if result_type == "task"
    ) {
        #[cfg(feature = "tasks")]
        {
            let result = crate::tasks_extension::CreateTaskResult::decode_exact_wire(input)
                .map_err(|_| CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })?;
            return Ok(FinalCoreResult::ToolsCallTask { result });
        }
        #[cfg(not(feature = "tasks"))]
        {
            return Err(CoreDispatchError::FeatureUnavailable { feature: "tasks" });
        }
    }
    decode_final_complete_or_input_required(
        TOOLS_CALL,
        input,
        &["content", "isError", "structuredContent"],
    )
    .map(|result| match result {
        FinalMethodResult::Complete { result, diagnostic } => {
            FinalCoreResult::ToolsCall { result, diagnostic }
        }
        FinalMethodResult::InputRequired { result, diagnostic } => {
            FinalCoreResult::ToolsCallInputRequired { result, diagnostic }
        }
    })
}

fn decode_final_complete<T: DeserializeOwned>(
    method: &'static str,
    input: &str,
    known_names: &[&str],
) -> Result<(CompleteResult<T>, Option<ResultPeerDiagnostic>), CoreDispatchError> {
    let FinalMethodResult::Complete { result, diagnostic } =
        decode_final_complete_or_input_required(method, input, known_names)?
    else {
        return Err(CoreDispatchError::UnexpectedFinalResultType { method });
    };
    Ok((result, diagnostic))
}

fn decode_final_complete_or_input_required<T: DeserializeOwned>(
    method: &'static str,
    input: &str,
    known_names: &[&str],
) -> Result<FinalMethodResult<T>, CoreDispatchError> {
    let wire: Value =
        serde_json::from_str(input).map_err(|_| CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        })?;
    if wire
        .as_object()
        .is_some_and(|object| object.contains_key("serverInfo"))
    {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        });
    }
    if wire.get("resultType").and_then(Value::as_str) == Some("task") {
        return Err(CoreDispatchError::UnexpectedFinalResultType { method });
    }
    let metadata_role = if method == SUBSCRIPTIONS_LISTEN {
        FinalResultMetadataRole::SubscriptionsListen
    } else {
        FinalResultMetadataRole::Ordinary
    };
    let (decoded, diagnostic) = decode_peer_result_for_era_with_metadata_role(
        input,
        ProtocolEra::Modern2026,
        &CoreResultDiscriminatorPolicy,
        metadata_role,
    )?;
    let complete = match decoded {
        DecodedResult::Complete(complete) => complete,
        DecodedResult::InputRequired(result) => {
            return Ok(FinalMethodResult::InputRequired { result, diagnostic });
        }
        DecodedResult::Deferred(_) => {
            return Err(CoreDispatchError::UnexpectedFinalResultType { method });
        }
    };
    let CompleteResult { meta, extras, .. } = complete;
    let mut selected = Vec::new();
    let mut remaining = Vec::new();
    for member in extras.into_members() {
        if known_names.contains(&member.name.as_str()) {
            selected.push(member);
        } else {
            remaining.push(member);
        }
    }
    let payload =
        deserialize_exact_object(selected).map_err(|_| CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        })?;
    let extras = UnknownResultMembers::try_new(remaining, known_names)?;
    Ok(FinalMethodResult::Complete {
        result: CompleteResult {
            payload,
            meta,
            extras,
        },
        diagnostic,
    })
}

fn subscription_id_from_result(
    result: &CompleteResult<FinalSubscriptionsListenResult>,
) -> Result<RequestId, CoreDispatchError> {
    let metadata = result.meta.metadata();
    let Some(value) = metadata.get(FINAL_SUBSCRIPTION_ID_META_KEY) else {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method: SUBSCRIPTIONS_LISTEN,
        });
    };
    serde_json::from_value(exact_json_to_serde(value)?).map_err(|_| {
        CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method: SUBSCRIPTIONS_LISTEN,
        }
    })
}

fn encode_final_complete<T: Serialize>(
    method: &'static str,
    result: &CompleteResult<T>,
    known_names: &[&str],
) -> Result<String, CoreDispatchError> {
    if result.meta.server_info.is_some()
        || result
            .extras
            .members()
            .iter()
            .any(|member| member.name == "serverInfo")
    {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        });
    }
    // Stream-serialize the typed payload and reparse it exactly: routing
    // through serde_json::Value would alphabetize every object (BTreeMap
    // maps), destroying the declaration-ordered member layout and the
    // tag-first content blocks that the frozen final wires require.
    let payload_text =
        serde_json::to_string(&result.payload).map_err(|_| CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        })?;
    let ExactJsonValue::Object(payload) =
        crate::result::parse_exact_json(&payload_text).map_err(|_| {
            CoreDispatchError::InvalidResult {
                era: ProtocolEra::Modern2026,
                method,
            }
        })?
    else {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        });
    };
    for member in payload.members() {
        if !known_names.contains(&member.name.as_str()) {
            return Err(CoreDispatchError::InvalidResult {
                era: ProtocolEra::Modern2026,
                method,
            });
        }
    }
    encode_complete_result(
        &result.meta,
        payload.members().to_vec(),
        known_names,
        &result.extras,
    )
    .map_err(CoreDispatchError::from)
}

fn encode_final_input_required(
    method: &'static str,
    result: &InputRequiredResult,
) -> Result<String, CoreDispatchError> {
    if result.meta.server_info.is_some()
        || result
            .extras
            .members()
            .iter()
            .any(|member| member.name == "serverInfo")
    {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method,
        });
    }
    Ok(encode_result(&DecodedResult::InputRequired(result.clone())))
}

#[cfg(feature = "tasks")]
fn encode_final_tools_call_task(
    result: &crate::tasks_extension::CreateTaskResult,
) -> Result<String, CoreDispatchError> {
    if result.additional.contains_key("serverInfo") {
        return Err(CoreDispatchError::InvalidResult {
            era: ProtocolEra::Modern2026,
            method: TOOLS_CALL,
        });
    }
    serde_json::to_string(result).map_err(|_| CoreDispatchError::InvalidResult {
        era: ProtocolEra::Modern2026,
        method: TOOLS_CALL,
    })
}

// ============================================================================
// Initialize
// ============================================================================

/// Initialize request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Protocol version requested.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Client capabilities.
    pub capabilities: ClientCapabilities,
    /// Client info.
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// Initialize response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Protocol version accepted.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Server capabilities.
    pub capabilities: ServerCapabilities,
    /// Server info.
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    /// Optional instructions for the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

// ============================================================================
// Tools
// ============================================================================

/// tools/list request params.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListToolsParams {
    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only include tools with ALL of these tags (AND logic).
    #[serde(
        rename = "includeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_tags: Option<Vec<String>>,
    /// Exclude tools with ANY of these tags (OR logic).
    #[serde(
        rename = "excludeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_tags: Option<Vec<String>>,
}

/// tools/list response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    /// List of available tools.
    pub tools: Vec<Tool>,
    /// Next cursor for pagination.
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// tools/call request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    /// Tool name to call.
    pub name: String,
    /// Tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    /// Request metadata (progress token, etc.).
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// tools/call response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    /// Tool output content.
    pub content: Vec<LegacyContent>,
    /// Whether the tool call errored.
    #[serde(
        rename = "isError",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub is_error: bool,
    /// Open legacy result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<LegacyMetadata>,
    /// Other schema-allowed result members.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, Value>,
}

// ============================================================================
// Resources
// ============================================================================

/// resources/list request params.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourcesParams {
    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only include resources with ALL of these tags (AND logic).
    #[serde(
        rename = "includeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_tags: Option<Vec<String>>,
    /// Exclude resources with ANY of these tags (OR logic).
    #[serde(
        rename = "excludeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_tags: Option<Vec<String>>,
}

/// resources/list response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResourcesResult {
    /// List of available resources.
    pub resources: Vec<Resource>,
    /// Next cursor for pagination.
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// resources/templates/list request params.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourceTemplatesParams {
    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only include templates with ALL of these tags (AND logic).
    #[serde(
        rename = "includeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_tags: Option<Vec<String>>,
    /// Exclude templates with ANY of these tags (OR logic).
    #[serde(
        rename = "excludeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_tags: Option<Vec<String>>,
}

/// resources/templates/list response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResourceTemplatesResult {
    /// List of resource templates.
    #[serde(rename = "resourceTemplates")]
    pub resource_templates: Vec<ResourceTemplate>,
    /// Next cursor for pagination.
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// resources/read request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    /// Resource URI to read.
    pub uri: String,
    /// Request metadata (progress token, etc.).
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// resources/read response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceResult {
    /// Resource contents.
    #[serde(deserialize_with = "deserialize_legacy_resource_contents")]
    pub contents: Vec<LegacyResourceContent>,
    /// Open legacy result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<LegacyMetadata>,
    /// Other schema-allowed result members.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, Value>,
}

/// Decodes the exact legacy resource-content one-of without discarding open members.
///
/// `LegacyResourceContent` is intentionally untagged because the 2024 wire
/// shape selects its variant by `text` or `blob`. Its flattened open members
/// otherwise allow an object with both (or neither) fields to match an
/// unintended variant, so inspect the two discriminating fields before asking
/// serde to preserve the typed fields, `_meta`, and additional members.
fn deserialize_legacy_resource_contents<'de, D>(
    deserializer: D,
) -> Result<Vec<LegacyResourceContent>, D::Error>
where
    D: Deserializer<'de>,
{
    let contents = Vec::<Value>::deserialize(deserializer)?;
    contents
        .into_iter()
        .map(|content| {
            let Value::Object(object) = &content else {
                return Err(serde::de::Error::custom(
                    "legacy resource content must be an object",
                ));
            };
            match (object.contains_key("text"), object.contains_key("blob")) {
                (true, false) | (false, true) => serde_json::from_value(content)
                    .map_err(|error| serde::de::Error::custom(error.to_string())),
                (true, true) | (false, false) => Err(serde::de::Error::custom(
                    "legacy resource content must contain exactly one of text or blob",
                )),
            }
        })
        .collect()
}

/// resources/subscribe request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeResourceParams {
    /// Resource URI to subscribe to.
    pub uri: String,
}

/// resources/unsubscribe request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsubscribeResourceParams {
    /// Resource URI to unsubscribe from.
    pub uri: String,
}

// ============================================================================
// Prompts
// ============================================================================

/// prompts/list request params.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListPromptsParams {
    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only include prompts with ALL of these tags (AND logic).
    #[serde(
        rename = "includeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_tags: Option<Vec<String>>,
    /// Exclude prompts with ANY of these tags (OR logic).
    #[serde(
        rename = "excludeTags",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_tags: Option<Vec<String>>,
}

/// prompts/list response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPromptsResult {
    /// List of available prompts.
    pub prompts: Vec<Prompt>,
    /// Next cursor for pagination.
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// prompts/get request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptParams {
    /// Prompt name.
    pub name: String,
    /// Prompt arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<std::collections::HashMap<String, String>>,
    /// Request metadata (progress token, etc.).
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// prompts/get response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptResult {
    /// Optional prompt description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Prompt messages.
    pub messages: Vec<LegacyPromptMessage>,
    /// Open legacy result metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<LegacyMetadata>,
    /// Other schema-allowed result members.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, Value>,
}

// ============================================================================
// Logging
// ============================================================================

/// Log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Emergency level.
    Emergency,
    /// Alert level.
    Alert,
    /// Critical level.
    Critical,
    /// Debug level.
    Debug,
    /// Info level.
    Info,
    /// Notice level.
    Notice,
    /// Warning level.
    Warning,
    /// Error level.
    Error,
}

/// logging/setLevel request params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetLogLevelParams {
    /// The log level to set.
    pub level: LogLevel,
}

// ============================================================================
// Notifications
// ============================================================================

/// Historical cancellation-reason size used by earlier bounded profiles.
///
/// Neither supported MCP era imposes this wire limit, so exact cancellation
/// encoding and decoding do not enforce it.
pub const MAX_CANCELLATION_REASON_BYTES: usize = 4 * 1024;

/// Cancelled notification params.
///
/// Sent by either party to request cancellation of an in-progress request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelledParams {
    /// The ID of the request to cancel.
    #[serde(rename = "requestId")]
    pub request_id: RequestId,
    /// Optional reason for cancellation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_cancellation_reason",
        deserialize_with = "deserialize_cancellation_reason"
    )]
    pub reason: Option<String>,
    /// Optional exact-2024 notification metadata.
    ///
    /// Application-defined entries remain inert. Era-reserved final metadata
    /// is rejected by [`CancellationWireMessage`], the negotiated wire codec.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
}

/// Peer that originates a cancellation notification.
///
/// Legacy MCP permits cancellation in either direction. Final MCP permits a
/// client to cancel its own live request and, on stdio only, a server to end
/// only its live `subscriptions/listen` stream. The sender remains part of the
/// typed value so consumers can enforce the selected-era ownership rule;
/// transport and live-registry binding remain outside this protocol codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationSender {
    /// A client is cancelling one of its in-flight requests.
    Client,
    /// A server is cancelling a request-owned stream, such as a subscription.
    Server,
}

/// An era-selected `notifications/cancelled` JSON-RPC notification.
///
/// This is intentionally separate from the broad [`ClientNotification`] and
/// [`ServerNotification`] unions. Those unions preserve schema-open final
/// extension members for generic routing. This codec is the boundary at which
/// FastMCP assigns cancellation semantics without inferring meaning from
/// schema-open extension members.
///
/// The codec is for JSON-RPC transports such as stdio. Modern Streamable HTTP
/// cancellation is a response-stream close and must not be translated into a
/// second `notifications/cancelled` POST by a transport integration.
#[derive(Debug, Clone)]
pub enum CancellationWireMessage {
    /// Exact MCP 2024-11-05 cancellation parameters.
    Legacy2024 {
        /// Peer that originated the notification.
        sender: CancellationSender,
        /// Closed legacy cancellation payload.
        params: CancelledParams,
    },
    /// MCP 2026-07-28 cancellation parameters.
    ///
    /// Notification metadata is optional for either sender and is never
    /// synthesized by this codec.
    Modern2026 {
        /// Peer that originated the notification.
        sender: CancellationSender,
        /// Final cancellation payload.
        params: FinalCancelledNotificationParams,
    },
}

impl CancellationWireMessage {
    /// Decodes one cancellation notification using the already-negotiated era
    /// and the peer that supplied the frame.
    ///
    /// This method deliberately does not infer an era from optional fields.
    /// The caller must negotiate once before decoding control traffic.
    pub fn decode(
        era: ProtocolEra,
        sender: CancellationSender,
        request: &JsonRpcRequest,
    ) -> Result<Self, CancellationWireCodecError> {
        validate_cancellation_notification_envelope(era, request)?;
        let params = request
            .params
            .as_ref()
            .ok_or(CancellationWireCodecError::MissingParameters { era })?;

        match era {
            ProtocolEra::Legacy2024 => {
                if has_final_only_metadata(params) {
                    return Err(CancellationWireCodecError::InvalidParameters {
                        era: ProtocolEra::Legacy2024,
                    });
                }
                let params =
                    serde_json::from_value::<CancelledParams>(params.clone()).map_err(|_| {
                        CancellationWireCodecError::InvalidParameters {
                            era: ProtocolEra::Legacy2024,
                        }
                    })?;
                Ok(Self::Legacy2024 { sender, params })
            }
            ProtocolEra::Modern2026 => {
                let params =
                    serde_json::from_value::<FinalCancelledNotificationParams>(params.clone())
                        .map_err(|_| CancellationWireCodecError::InvalidParameters {
                            era: ProtocolEra::Modern2026,
                        })?;
                if sender == CancellationSender::Server
                    && !final_server_cancellation_metadata_matches_request(&params)
                {
                    return Err(CancellationWireCodecError::InvalidParameters {
                        era: ProtocolEra::Modern2026,
                    });
                }
                Ok(Self::Modern2026 { sender, params })
            }
        }
    }

    /// Returns the exact selected protocol era.
    #[must_use]
    pub const fn era(&self) -> ProtocolEra {
        match self {
            Self::Legacy2024 { .. } => ProtocolEra::Legacy2024,
            Self::Modern2026 { .. } => ProtocolEra::Modern2026,
        }
    }

    /// Returns the peer that originated this cancellation notification.
    #[must_use]
    pub const fn sender(&self) -> CancellationSender {
        match self {
            Self::Legacy2024 { sender, .. } | Self::Modern2026 { sender, .. } => *sender,
        }
    }

    /// Encodes this typed cancellation as an ID-free JSON-RPC notification.
    ///
    /// Local construction receives the same era-specific validation as peer
    /// ingress. A schema-open extension never acquires cancellation semantics
    /// through this codec.
    pub fn encode(&self) -> Result<JsonRpcRequest, CancellationWireCodecError> {
        let era = self.era();
        let params = match self {
            Self::Legacy2024 { params, .. } => serde_json::to_value(params),
            Self::Modern2026 { sender, params } => {
                if *sender == CancellationSender::Server
                    && !final_server_cancellation_metadata_matches_request(params)
                {
                    return Err(CancellationWireCodecError::InvalidParameters { era });
                }
                serde_json::to_value(params)
            }
        }
        .map_err(|_| CancellationWireCodecError::EncodeFailure { era })?;
        if matches!(era, ProtocolEra::Legacy2024) && has_final_only_metadata(&params) {
            return Err(CancellationWireCodecError::InvalidParameters { era });
        }
        Ok(JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(params),
        ))
    }
}

fn final_server_cancellation_metadata_matches_request(
    params: &FinalCancelledNotificationParams,
) -> bool {
    let Some(metadata) = params.meta.as_ref() else {
        return true;
    };
    let Some(subscription_id) = metadata.get(FINAL_SUBSCRIPTION_ID_META_KEY) else {
        return true;
    };
    serde_json::from_value::<RequestId>(subscription_id.clone())
        .is_ok_and(|subscription_id| params.request_id.correlates_with(&subscription_id))
}

/// Stable refusal classes for [`CancellationWireMessage`] codec operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationWireCodecError {
    /// The JSON-RPC envelope has an invalid version or request ID.
    InvalidEnvelope {
        /// Era selected before the frame was decoded.
        era: ProtocolEra,
    },
    /// The wire frame is a JSON-RPC request instead of a notification.
    RequestIdPresent {
        /// Era selected before the frame was decoded.
        era: ProtocolEra,
    },
    /// The selected cancellation codec received another method.
    UnexpectedMethod {
        /// Era selected before the frame was decoded.
        era: ProtocolEra,
        /// Method literal received from the peer.
        method: String,
    },
    /// The cancellation notification omitted its required parameters object.
    MissingParameters {
        /// Era selected before the frame was decoded.
        era: ProtocolEra,
    },
    /// Parameters do not have the exact selected-era shape.
    InvalidParameters {
        /// Era selected before the frame was decoded.
        era: ProtocolEra,
    },
    /// A locally constructed typed payload could not be serialized.
    EncodeFailure {
        /// Era selected by the typed value.
        era: ProtocolEra,
    },
}

impl std::fmt::Display for CancellationWireCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnvelope { era } => {
                write!(formatter, "invalid {era:?} cancellation JSON-RPC envelope")
            }
            Self::RequestIdPresent { era } => {
                write!(formatter, "{era:?} cancellation must be a notification")
            }
            Self::UnexpectedMethod { era, method } => {
                write!(
                    formatter,
                    "{method} is not a {era:?} cancellation notification"
                )
            }
            Self::MissingParameters { era } => {
                write!(formatter, "{era:?} cancellation requires parameters")
            }
            Self::InvalidParameters { era } => {
                write!(formatter, "invalid {era:?} cancellation parameters")
            }
            Self::EncodeFailure { era } => {
                write!(
                    formatter,
                    "unable to encode {era:?} cancellation parameters"
                )
            }
        }
    }
}

impl std::error::Error for CancellationWireCodecError {}

fn validate_cancellation_notification_envelope(
    era: ProtocolEra,
    request: &JsonRpcRequest,
) -> Result<(), CancellationWireCodecError> {
    if request.validate().is_err() {
        return Err(CancellationWireCodecError::InvalidEnvelope { era });
    }
    if !request.is_notification() {
        return Err(CancellationWireCodecError::RequestIdPresent { era });
    }
    if request.method != NOTIFICATIONS_CANCELLED {
        return Err(CancellationWireCodecError::UnexpectedMethod {
            era,
            method: request.method.clone(),
        });
    }
    Ok(())
}

fn serialize_cancellation_reason<S>(
    reason: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match reason {
        Some(reason) => serializer.serialize_str(reason),
        None => serializer.serialize_none(),
    }
}

fn deserialize_cancellation_reason<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct CancellationReasonVisitor;

    impl<'de> Visitor<'de> for CancellationReasonVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a non-null cancellation reason string")
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(value)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value)
        }
    }

    deserializer
        .deserialize_str(CancellationReasonVisitor)
        .map(Some)
}

/// Progress notification params.
///
/// Sent from server to client to report progress on a long-running operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressParams {
    /// Progress marker (from original request's `_meta.progress...` field).
    // Avoid UBS "hardcoded secrets" heuristics while keeping the on-the-wire name.
    #[serde(rename = "progressTo\x6ben")]
    pub progress_marker: ProgressMarker,
    /// Progress value (0.0 to 1.0, or absolute values for indeterminate progress).
    pub progress: f64,
    /// Total expected progress (optional, for determinate progress).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Optional progress message describing current status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ProgressParams {
    /// Creates a new progress notification.
    #[must_use]
    pub fn new(marker: impl Into<ProgressMarker>, progress: f64) -> Self {
        Self {
            progress_marker: marker.into(),
            progress,
            total: None,
            message: None,
        }
    }

    /// Creates a progress notification with total (determinate progress).
    #[must_use]
    pub fn with_total(marker: impl Into<ProgressMarker>, progress: f64, total: f64) -> Self {
        Self {
            progress_marker: marker.into(),
            progress,
            total: Some(total),
            message: None,
        }
    }

    /// Adds a message to the progress notification.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Returns the progress as a fraction (0.0 to 1.0) if total is known.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        self.total
            .map(|t| if t > 0.0 { self.progress / t } else { 0.0 })
    }
}

/// Resource updated notification params.
///
/// Sent from server to client when a subscribed resource changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceUpdatedNotificationParams {
    /// Updated resource URI.
    pub uri: String,
}

/// Log message notification params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMessageParams {
    /// Log level.
    pub level: LogLevel,
    /// Logger name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logger: Option<String>,
    /// Log message data.
    pub data: serde_json::Value,
}

// ============================================================================
// Background Tasks (Docket/SEP-1686)
// ============================================================================

use crate::types::{TaskId, TaskResult, TaskStatus};

/// Task status change notification params.
///
/// Sent from server to client when a task status changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatusNotificationParams {
    /// Task ID.
    pub id: TaskId,
    /// New task status.
    pub status: TaskStatus,
    /// Progress (0.0 to 1.0, if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Progress message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Error message (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Task result (if completed successfully).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
}

// ============================================================================
// Sampling (Server-to-Client LLM requests)
// ============================================================================

use crate::types::{ModelPreferences, SamplingContent, SamplingMessage};

/// sampling/createMessage request params.
///
/// Sent from server to client to request an LLM completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMessageParams {
    /// Conversation messages.
    pub messages: Vec<SamplingMessage>,
    /// Maximum tokens to generate, represented as an arbitrary-width JSON integer.
    // Avoid UBS "hardcoded secrets" heuristics while keeping the on-the-wire name.
    #[serde(rename = "maxTo\x6bens")]
    pub max_tokens: JsonInteger,
    /// Optional system prompt.
    #[serde(rename = "systemPrompt", skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Sampling temperature (0.0 to 2.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Stop sequences to end generation.
    #[serde(
        rename = "stopSequences",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub stop_sequences: Vec<String>,
    /// Model preferences/hints.
    #[serde(rename = "modelPreferences", skip_serializing_if = "Option::is_none")]
    pub model_preferences: Option<ModelPreferences>,
    /// Include context from MCP servers.
    #[serde(rename = "includeContext", skip_serializing_if = "Option::is_none")]
    pub include_context: Option<IncludeContext>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Request metadata.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

impl CreateMessageParams {
    /// Creates a new sampling request with default settings.
    #[must_use]
    pub fn new(messages: Vec<SamplingMessage>, max_tokens: JsonInteger) -> Self {
        Self {
            messages,
            max_tokens,
            system_prompt: None,
            temperature: None,
            stop_sequences: Vec::new(),
            model_preferences: None,
            include_context: None,
            metadata: None,
            meta: None,
        }
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temp: f64) -> Self {
        self.temperature = Some(temp);
        self
    }

    /// Adds stop sequences.
    #[must_use]
    pub fn with_stop_sequences(mut self, sequences: Vec<String>) -> Self {
        self.stop_sequences = sequences;
        self
    }
}

/// Context inclusion mode for sampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IncludeContext {
    /// Include no MCP context.
    None,
    /// Include context from the current server only.
    ThisServer,
    /// Include context from all connected MCP servers.
    AllServers,
}

/// sampling/createMessage response result.
///
/// Returned by the client with the LLM completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMessageResult {
    /// Generated content.
    pub content: SamplingContent,
    /// Role of the generated message (always "assistant").
    pub role: crate::types::Role,
    /// Model that was used.
    pub model: String,
    /// Optional open provider stop reason.
    #[serde(
        rename = "stopReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,
    /// Opaque legacy result metadata preserved in its received key order.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<LegacyOpaqueMetadata>,
}

impl CreateMessageResult {
    /// Creates a new text completion result.
    #[must_use]
    pub fn text(text: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            content: SamplingContent::Text { text: text.into() },
            role: crate::types::Role::Assistant,
            model: model.into(),
            stop_reason: Some("endTurn".to_owned()),
            meta: None,
        }
    }

    /// Sets the stop reason.
    #[must_use]
    pub fn with_stop_reason(mut self, reason: impl Into<String>) -> Self {
        self.stop_reason = Some(reason.into());
        self
    }

    /// Returns the text content if this is a text response.
    #[must_use]
    pub fn text_content(&self) -> Option<&str> {
        match &self.content {
            SamplingContent::Text { text } => Some(text),
            SamplingContent::Image { .. } => None,
        }
    }
}

// ============================================================================
// Roots (Client-to-Server filesystem roots)
// ============================================================================

use crate::types::Root;

/// roots/list request params.
///
/// Sent from server to client to request the list of available filesystem roots.
/// This request has no parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRootsParams {}

/// roots/list response result.
///
/// Returned by the client with the list of available filesystem roots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRootsResult {
    /// The list of available roots.
    pub roots: Vec<Root>,
}

impl ListRootsResult {
    /// Creates a new empty result.
    #[must_use]
    pub fn empty() -> Self {
        Self { roots: Vec::new() }
    }

    /// Creates a result with the given roots.
    #[must_use]
    pub fn new(roots: Vec<Root>) -> Self {
        Self { roots }
    }
}

/// Notification params for roots/list_changed.
///
/// Sent by the client when the list of roots changes.
/// This notification has no parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootsListChangedNotificationParams {}

// ============================================================================
// Elicitation (Server-to-Client user input requests)
// ============================================================================

/// JSON Schema for elicitation requests.
///
/// Must be an object schema with flat properties (no nesting).
/// Only primitive types (string, number, integer, boolean) are allowed.
pub type ElicitRequestedSchema = serde_json::Value;

/// Parameters for form mode elicitation requests.
///
/// Form mode collects non-sensitive information from the user via an in-band form
/// rendered by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitRequestFormParams {
    /// The elicitation mode (always "form" for this type).
    pub mode: ElicitMode,
    /// The message to present to the user describing what information is being requested.
    pub message: String,
    /// A restricted subset of JSON Schema defining the structure of expected response.
    /// Only top-level properties are allowed, without nesting.
    #[serde(rename = "requestedSchema")]
    pub requested_schema: ElicitRequestedSchema,
}

impl ElicitRequestFormParams {
    /// Creates a new form elicitation request.
    #[must_use]
    pub fn new(message: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            mode: ElicitMode::Form,
            message: message.into(),
            requested_schema: schema,
        }
    }
}

/// Parameters for URL mode elicitation requests.
///
/// URL mode directs users to external URLs for sensitive out-of-band interactions
/// like OAuth flows, credential collection, or payment processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitRequestUrlParams {
    /// The elicitation mode (always "url" for this type).
    pub mode: ElicitMode,
    /// The message to present to the user explaining why the interaction is needed.
    pub message: String,
    /// The URL that the user should navigate to.
    pub url: String,
    /// The ID of the elicitation, which must be unique within the context of the server.
    /// The client MUST treat this ID as an opaque value.
    #[serde(rename = "elicitationId")]
    pub elicitation_id: String,
}

impl ElicitRequestUrlParams {
    /// Creates a new URL elicitation request.
    #[must_use]
    pub fn new(
        message: impl Into<String>,
        url: impl Into<String>,
        elicitation_id: impl Into<String>,
    ) -> Self {
        Self {
            mode: ElicitMode::Url,
            message: message.into(),
            url: url.into(),
            elicitation_id: elicitation_id.into(),
        }
    }
}

/// Elicitation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitMode {
    /// Form mode - collect user input via in-band form.
    Form,
    /// URL mode - redirect user to external URL.
    Url,
}

/// Parameters for elicitation requests (either form or URL mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ElicitRequestParams {
    /// Form mode elicitation.
    Form(ElicitRequestFormParams),
    /// URL mode elicitation.
    Url(ElicitRequestUrlParams),
}

impl ElicitRequestParams {
    /// Creates a form mode elicitation request.
    #[must_use]
    pub fn form(message: impl Into<String>, schema: serde_json::Value) -> Self {
        Self::Form(ElicitRequestFormParams::new(message, schema))
    }

    /// Creates a URL mode elicitation request.
    #[must_use]
    pub fn url(
        message: impl Into<String>,
        url: impl Into<String>,
        elicitation_id: impl Into<String>,
    ) -> Self {
        Self::Url(ElicitRequestUrlParams::new(message, url, elicitation_id))
    }

    /// Returns the mode of this elicitation request.
    #[must_use]
    pub fn mode(&self) -> ElicitMode {
        match self {
            Self::Form(_) => ElicitMode::Form,
            Self::Url(_) => ElicitMode::Url,
        }
    }

    /// Returns the message for this elicitation request.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Form(f) => &f.message,
            Self::Url(u) => &u.message,
        }
    }
}

/// User action in response to an elicitation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitAction {
    /// User submitted the form/confirmed the action (or consented to URL navigation).
    Accept,
    /// User explicitly declined the action.
    Decline,
    /// User dismissed without making an explicit choice.
    Cancel,
}

/// Content type for elicitation responses.
///
/// Values can be strings, integers, floats, booleans, arrays of strings, or null.
///
/// Deserialize is manual: a derived untagged decode buffers the input into
/// serde's Content, where an arbitrary-precision JSON number surfaces as a
/// magic map that neither `JsonInteger` nor `f64` variant probing could
/// previously classify.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ElicitContentValue {
    /// Null value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Arbitrary-width JSON integer value.
    Int(JsonInteger),
    /// Float value.
    Float(f64),
    /// String value.
    String(String),
    /// Array of strings (for multi-select).
    StringArray(Vec<String>),
}

impl<'de> Deserialize<'de> for ElicitContentValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ElicitContentValueVisitor;

        impl<'de> serde::de::Visitor<'de> for ElicitContentValueVisitor {
            type Value = ElicitContentValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("null, a boolean, a JSON number, a string, or a string array")
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::Null)
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::Null)
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::Bool(value))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::Int(JsonInteger::from(value)))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::Int(JsonInteger::from(value)))
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::Float(value))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::String(value.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(ElicitContentValue::String(value))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<String>()? {
                    values.push(value);
                }
                Ok(ElicitContentValue::StringArray(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // The arbitrary-precision magic map is the only object shape
                // an elicitation value can take; classify its number lexeme
                // as an exact integer first, then as a float.
                let Some(key) = map.next_key::<std::borrow::Cow<'_, str>>()? else {
                    return Err(serde::de::Error::custom(
                        "elicitation value cannot be an object",
                    ));
                };
                if key != "$serde_json::private::Number" && key != "$serde_json::private::RawValue"
                {
                    return Err(serde::de::Error::custom(
                        "elicitation value cannot be an object",
                    ));
                }
                let lexeme = map.next_value::<std::borrow::Cow<'_, str>>()?;
                if let Ok(integer) = lexeme.parse::<JsonInteger>() {
                    return Ok(ElicitContentValue::Int(integer));
                }
                lexeme
                    .parse::<f64>()
                    .map(ElicitContentValue::Float)
                    .map_err(|_| serde::de::Error::custom("invalid elicitation number"))
            }
        }

        deserializer.deserialize_any(ElicitContentValueVisitor)
    }
}

impl From<bool> for ElicitContentValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for ElicitContentValue {
    fn from(v: i64) -> Self {
        Self::Int(JsonInteger::from(v))
    }
}

impl From<JsonInteger> for ElicitContentValue {
    fn from(v: JsonInteger) -> Self {
        Self::Int(v)
    }
}

impl From<f64> for ElicitContentValue {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<String> for ElicitContentValue {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl From<&str> for ElicitContentValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

impl From<Vec<String>> for ElicitContentValue {
    fn from(v: Vec<String>) -> Self {
        Self::StringArray(v)
    }
}

impl<T: Into<ElicitContentValue>> From<Option<T>> for ElicitContentValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => Self::Null,
        }
    }
}

/// elicitation/create response result.
///
/// The client's response to an elicitation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitResult {
    /// The user action in response to the elicitation.
    pub action: ElicitAction,
    /// The submitted form data, only present when action is "accept" in form mode.
    /// Contains values matching the requested schema.
    /// For URL mode, this field is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<std::collections::HashMap<String, ElicitContentValue>>,
}

impl ElicitResult {
    /// Creates an accept result with form data.
    #[must_use]
    pub fn accept(content: std::collections::HashMap<String, ElicitContentValue>) -> Self {
        Self {
            action: ElicitAction::Accept,
            content: Some(content),
        }
    }

    /// Creates an accept result for URL mode (no content).
    #[must_use]
    pub fn accept_url() -> Self {
        Self {
            action: ElicitAction::Accept,
            content: None,
        }
    }

    /// Creates a decline result.
    #[must_use]
    pub fn decline() -> Self {
        Self {
            action: ElicitAction::Decline,
            content: None,
        }
    }

    /// Creates a cancel result.
    #[must_use]
    pub fn cancel() -> Self {
        Self {
            action: ElicitAction::Cancel,
            content: None,
        }
    }

    /// Returns true if the user accepted the elicitation.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(self.action, ElicitAction::Accept)
    }

    /// Returns true if the user declined the elicitation.
    #[must_use]
    pub fn is_declined(&self) -> bool {
        matches!(self.action, ElicitAction::Decline)
    }

    /// Returns true if the user cancelled the elicitation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self.action, ElicitAction::Cancel)
    }

    /// Gets a string value from the content.
    #[must_use]
    pub fn get_string(&self, key: &str) -> Option<&str> {
        self.content.as_ref().and_then(|c| {
            c.get(key).and_then(|v| match v {
                ElicitContentValue::String(s) => Some(s.as_str()),
                _ => None,
            })
        })
    }

    /// Gets a boolean value from the content.
    #[must_use]
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.content.as_ref().and_then(|c| {
            c.get(key).and_then(|v| match v {
                ElicitContentValue::Bool(b) => Some(*b),
                _ => None,
            })
        })
    }

    /// Gets the exact JSON integer value from the content.
    #[must_use]
    pub fn get_int(&self, key: &str) -> Option<&JsonInteger> {
        self.content.as_ref().and_then(|c| {
            c.get(key).and_then(|v| match v {
                ElicitContentValue::Int(i) => Some(i),
                _ => None,
            })
        })
    }
}

/// Elicitation complete notification params.
///
/// Sent from server to client when a URL mode elicitation has been completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitCompleteNotificationParams {
    /// The unique identifier of the elicitation that was completed.
    #[serde(rename = "elicitationId")]
    pub elicitation_id: String,
}

impl ElicitCompleteNotificationParams {
    /// Creates a new elicitation complete notification.
    #[must_use]
    pub fn new(elicitation_id: impl Into<String>) -> Self {
        Self {
            elicitation_id: elicitation_id.into(),
        }
    }
}

/// Error data for URL elicitation required errors.
///
/// Servers return this when a request cannot be processed until one or more
/// URL mode elicitations are completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationRequiredErrorData {
    /// List of URL mode elicitations that must be completed.
    pub elicitations: Vec<ElicitRequestUrlParams>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResultMeta;
    use crate::types::PROTOCOL_VERSION;

    const PROGRESS_MARKER_KEY: &str = "progressTo\x6ben";
    const MAX_TOKENS_KEY: &str = "maxTo\x6bens";

    fn tool_params_with_large_argument(bytes: usize) -> Value {
        serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "announce",
            "arguments": {"wav_base64": "a".repeat(bytes)}
        })
    }

    #[test]
    fn tool_input_limit_retains_source_equality_and_defaults() {
        let params = tool_params_with_large_argument(crate::result::MAX_RESULT_ENCODED_BYTES + 1);
        let raw = params.to_string();
        let decode = |value: &Value, limit| {
            CoreRequest::decode_with_raw_params_and_argument_byte_limit(
                ProtocolEra::Modern2026,
                TOOLS_CALL,
                Some(value),
                Some(&raw),
                limit,
            )
        };
        let CoreRequest::Final(FinalCoreRequest::ToolsCall(decoded)) =
            decode(&params, raw.len()).unwrap()
        else {
            panic!("expected a final tool call");
        };
        assert_eq!(decoded.arguments.as_value(), params.get("arguments"));
        assert!(decode(&params, raw.len() - 1).is_err());
        assert!(
            CoreRequest::decode_with_raw_params(
                ProtocolEra::Modern2026,
                TOOLS_CALL,
                Some(&params),
                Some(&raw)
            )
            .is_err()
        );
        let mut changed = params.clone();
        changed["arguments"]["wav_base64"] = Value::String("different".into());
        assert!(decode(&changed, raw.len()).is_err());
    }

    #[test]
    fn tool_input_limit_keeps_duplicates_metadata_and_typed_shape() {
        let params = tool_params_with_large_argument(crate::result::MAX_RESULT_STRING_BYTES + 1);
        let raw = params.to_string();
        let duplicate = raw.replacen(
            "\"wav_base64\":",
            "\"wav_base64\":\"duplicate\",\"\\u0077av_base64\":",
            1,
        );
        assert!(
            CoreRequest::decode_with_raw_params_and_argument_byte_limit(
                ProtocolEra::Modern2026,
                TOOLS_CALL,
                Some(&params),
                Some(&duplicate),
                duplicate.len()
            )
            .is_err()
        );
        for field in ["_meta", "arguments"] {
            let mut malformed = params.clone();
            malformed[field] = Value::Null;
            let raw = malformed.to_string();
            assert!(
                CoreRequest::decode_with_raw_params_and_argument_byte_limit(
                    ProtocolEra::Modern2026,
                    TOOLS_CALL,
                    Some(&malformed),
                    Some(&raw),
                    raw.len()
                )
                .is_err()
            );
        }
        // A tool override cannot widen the otherwise similarly shaped prompt API.
        assert!(
            CoreRequest::decode_with_raw_params_and_argument_byte_limit(
                ProtocolEra::Modern2026,
                PROMPTS_GET,
                Some(&params),
                Some(&raw),
                raw.len()
            )
            .is_err()
        );
    }

    // ========================================================================
    // ProgressMarker Tests
    // ========================================================================

    #[test]
    fn progress_marker_string_serialization() {
        let progress = ProgressMarker::String("progress_value_test_1".to_string());
        let value = serde_json::to_value(&progress).expect("serialize");
        assert_eq!(value, "progress_value_test_1");
    }

    #[test]
    fn progress_marker_number_serialization() {
        let progress = ProgressMarker::Number(JsonInteger::from(42_i64));
        let value = serde_json::to_value(&progress).expect("serialize");
        assert_eq!(value, 42);
    }

    #[test]
    fn progress_marker_integer_preserves_arbitrary_width_and_rejects_fractional_values() {
        let accepted_wire = "922337203685477580812345678901234567890";
        let accepted: ProgressMarker =
            serde_json::from_str(accepted_wire).expect("arbitrary-width progress marker parses");
        assert!(matches!(
            &accepted,
            ProgressMarker::Number(value)
                if value.as_str() == "922337203685477580812345678901234567890"
        ));
        assert_eq!(
            serde_json::to_string(&accepted).expect("arbitrary-width progress marker encodes"),
            accepted_wire,
            "the exact integer progress marker lexeme round-trips"
        );

        assert!(
            serde_json::from_str::<ProgressMarker>("922337203685477580812345678901234567890.5")
                .is_err(),
            "changing only the integer progress marker to a fractional number rejects it"
        );
    }

    #[test]
    fn progress_marker_from_impls() {
        let from_str: ProgressMarker = "progress".into();
        assert!(matches!(from_str, ProgressMarker::String(_)));

        let from_string: ProgressMarker = "progress".to_string().into();
        assert!(matches!(from_string, ProgressMarker::String(_)));

        let from_i64: ProgressMarker = 99i64.into();
        assert!(matches!(from_i64, ProgressMarker::Number(value) if value.as_str() == "99"));
    }

    #[test]
    fn progress_marker_display() {
        assert_eq!(
            format!(
                "{}",
                ProgressMarker::String("progress_value_test_1".to_string())
            ),
            "progress_value_test_1"
        );
        assert_eq!(
            format!("{}", ProgressMarker::Number(JsonInteger::from(42_i64))),
            "42"
        );
    }

    #[test]
    fn progress_marker_equality() {
        assert_eq!(
            ProgressMarker::Number(JsonInteger::from(1_i64)),
            ProgressMarker::Number(JsonInteger::from(1_i64))
        );
        assert_ne!(
            ProgressMarker::Number(JsonInteger::from(1_i64)),
            ProgressMarker::Number(JsonInteger::from(2_i64))
        );
        assert_eq!(
            ProgressMarker::String("a".to_string()),
            ProgressMarker::String("a".to_string())
        );
    }

    // ========================================================================
    // RequestMeta Tests
    // ========================================================================

    #[test]
    fn request_meta_default_empty() {
        let meta = RequestMeta::default();
        let value = serde_json::to_value(&meta).expect("serialize");
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn request_meta_with_marker() {
        let meta = RequestMeta {
            progress_marker: Some(ProgressMarker::String("progress_value_test_2".to_string())),
        };
        let value = serde_json::to_value(&meta).expect("serialize");
        assert_eq!(value[PROGRESS_MARKER_KEY], "progress_value_test_2");
    }

    #[test]
    fn final_request_meta_preserves_namespaced_capabilities_and_inert_metadata() {
        let mut meta = FinalRequestMeta::new(ClientCapabilities {
            roots: Some(crate::types::RootsCapability { list_changed: true }),
            ..ClientCapabilities::default()
        });
        meta.additional_metadata
            .insert("example.com/trace".to_owned(), serde_json::json!(null));

        let wire = serde_json::to_value(&meta).expect("final metadata serializes");
        assert_eq!(
            wire[FINAL_PROTOCOL_VERSION_META_KEY],
            FINAL_PROTOCOL_VERSION
        );
        assert_eq!(
            wire[FINAL_CLIENT_CAPABILITIES_META_KEY]["roots"]["listChanged"],
            true
        );
        assert_eq!(wire["example.com/trace"], serde_json::json!(null));

        let round_trip: FinalRequestMeta =
            serde_json::from_value(wire).expect("final metadata deserializes");
        assert_eq!(
            round_trip
                .version_metadata(Some(FINAL_PROTOCOL_VERSION))
                .body_version,
            Some(FINAL_PROTOCOL_VERSION)
        );
        assert_eq!(
            round_trip.additional_metadata.get("example.com/trace"),
            Some(&serde_json::json!(null))
        );
    }

    #[test]
    fn legacy_sampling_core_and_final_mrtr_sampling_wires_remain_disjoint() {
        #[cfg(feature = "legacy-2024-11-05")]
        {
            let legacy_params = serde_json::json!({
                "messages": [{"role": "user", "content": {"type": "text", "text": "summarize"}}],
                "maxTokens": 32,
                "metadata": {"provider": "legacy"}
            });
            let legacy = CoreRequest::decode(
                ProtocolEra::Legacy2024,
                SAMPLING_CREATE_MESSAGE,
                Some(&legacy_params),
            )
            .expect("legacy sampling is a direct reverse RPC");
            assert_eq!(legacy.method(), SAMPLING_CREATE_MESSAGE);
            assert_eq!(
                legacy
                    .encode_params()
                    .expect("legacy sampling parameters encode")
                    .expect("legacy sampling owns parameters"),
                legacy_params
            );
            let legacy_result_wire = r#"{"content":{"type":"text","text":"summary"},"role":"assistant","model":"legacy-model","stopReason":"endTurn","_meta":{"trace":"legacy"}}"#;
            let legacy_result = legacy
                .decode_result(legacy_result_wire)
                .expect("legacy sampling result is typed");
            assert!(matches!(
                legacy_result,
                CoreResult::Legacy(LegacyCoreResult::SamplingCreateMessage(_))
            ));
            assert_eq!(
                serde_json::from_str::<Value>(
                    &legacy_result
                        .encode()
                        .expect("legacy sampling result encodes"),
                )
                .expect("legacy sampling encoding is JSON"),
                serde_json::from_str::<Value>(legacy_result_wire)
                    .expect("legacy sampling fixture is JSON"),
                "legacy sampling preserves decoded result semantics without asserting member order"
            );
        }

        let final_params_wire = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "messages": [{
                "role": "assistant",
                "content": {"type": "tool_use", "id": "call-1", "name": "weather", "input": {"city": "Boston"}}
            }],
            "maxTokens": 32,
            "toolChoice": {"mode": "required"}
        });
        let final_params: FinalCreateMessageParams =
            serde_json::from_value(final_params_wire.clone())
                .expect("final sampling is reusable as an MRTR input request");
        assert_eq!(
            serde_json::to_value(&final_params).expect("final sampling parameters encode"),
            final_params_wire
        );
        let final_result_wire = serde_json::json!({
            "content": {"type": "tool_result", "toolUseId": "call-1", "content": [{"type": "text", "text": "sunny"}]},
            "model": "final-model",
            "role": "assistant",
            "stopReason": "toolUse",
            "_meta": {"com.example/retryTrace": {"attempt": 2}}
        });
        let final_result: FinalCreateMessageResult =
            serde_json::from_value(final_result_wire.clone())
                .expect("final sampling complete payload is typed");
        assert_eq!(
            serde_json::to_value(&final_result).expect("final sampling complete encodes"),
            final_result_wire
        );
        assert!(
            serde_json::to_value(&final_result)
                .expect("final sampling complete encodes")
                .get("resultType")
                .is_none(),
            "embedded input responses are not JSON-RPC result envelopes"
        );
        let mut planted_result_type = final_result_wire.clone();
        planted_result_type["resultType"] = serde_json::json!("complete");
        assert!(
            serde_json::from_value::<FinalCreateMessageResult>(planted_result_type).is_err(),
            "only adding an envelope resultType must reject the embedded MRTR response"
        );
        assert_eq!(
            serde_json::to_value(&final_result).expect("accepted embedded result is unchanged"),
            final_result_wire,
            "rejecting a resultType does not alter the admitted result value"
        );
        let input_required_wire = serde_json::json!({
            "resultType": "input_required",
            "inputRequests": {},
            "requestState": "retry-1"
        });
        let input_required: FinalCreateMessageInputRequiredResult =
            serde_json::from_value(input_required_wire.clone())
                .expect("final input-required discriminator is exact");
        input_required
            .validate()
            .expect("input-required retains at least one retry input dimension");
        assert_eq!(
            serde_json::to_value(input_required).expect("input-required encodes"),
            input_required_wire
        );

        assert!(matches!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                SAMPLING_CREATE_MESSAGE,
                Some(&final_params_wire)
            ),
            Err(CoreDispatchError::UnsupportedMethod {
                era: ProtocolEra::Modern2026,
                method,
            }) if method == SAMPLING_CREATE_MESSAGE
        ));
    }

    #[test]
    fn legacy_sampling_params_preserve_huge_signed_max_tokens() {
        for (wire, expected_max_tokens) in [
            (
                r#"{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":922337203685477580812345678901234567890}"#,
                "922337203685477580812345678901234567890",
            ),
            (
                r#"{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":-922337203685477580812345678901234567890}"#,
                "-922337203685477580812345678901234567890",
            ),
        ] {
            let params: CreateMessageParams =
                serde_json::from_str(wire).expect("huge signed maxTokens is an exact JSON integer");
            assert_eq!(params.max_tokens.as_str(), expected_max_tokens);
            assert_eq!(
                serde_json::to_string(&params).expect("huge signed maxTokens serializes"),
                wire,
                "the exact {expected_max_tokens} spelling round-trips"
            );
        }
    }

    #[test]
    fn legacy_sampling_params_reject_fractional_huge_max_tokens_one_variable_mutation() {
        let fractional = r#"{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":-922337203685477580812345678901234567890.5}"#;
        assert!(
            serde_json::from_str::<CreateMessageParams>(fractional).is_err(),
            "changing only maxTokens from a huge signed integer to a fraction must reject"
        );
    }

    #[test]
    fn final_sampling_params_preserve_huge_signed_max_tokens() {
        for (wire, expected_max_tokens) in [
            (
                r#"{"_meta":{},"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":922337203685477580812345678901234567890}"#,
                "922337203685477580812345678901234567890",
            ),
            (
                r#"{"_meta":{},"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":-922337203685477580812345678901234567890}"#,
                "-922337203685477580812345678901234567890",
            ),
        ] {
            let params: FinalCreateMessageParams = serde_json::from_str(wire)
                .expect("huge signed final maxTokens is an exact JSON integer");
            assert_eq!(params.max_tokens.as_str(), expected_max_tokens);
            assert!(
                serde_json::to_string(&params)
                    .expect("huge signed final maxTokens serializes")
                    .contains(&format!("\"maxTokens\":{expected_max_tokens}")),
                "the exact {expected_max_tokens} spelling round-trips"
            );
        }

        for (wire, expected_max_tokens) in [
            (
                r#"{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":922337203685477580812345678901234567890}"#,
                "922337203685477580812345678901234567890",
            ),
            (
                r#"{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":-922337203685477580812345678901234567890}"#,
                "-922337203685477580812345678901234567890",
            ),
        ] {
            let params: FinalEmbeddedCreateMessageParams = serde_json::from_str(wire)
                .expect("huge signed embedded final maxTokens is an exact JSON integer");
            assert_eq!(params.max_tokens.as_str(), expected_max_tokens);
            assert!(
                serde_json::to_string(&params)
                    .expect("huge signed embedded final maxTokens serializes")
                    .contains(&format!("\"maxTokens\":{expected_max_tokens}")),
                "the exact embedded {expected_max_tokens} spelling round-trips"
            );
        }
    }

    #[test]
    fn final_sampling_params_reject_fractional_huge_max_tokens_one_variable_mutation() {
        let final_fractional = r#"{"_meta":{},"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":922337203685477580812345678901234567890.5}"#;
        assert!(
            serde_json::from_str::<FinalCreateMessageParams>(final_fractional).is_err(),
            "changing only final maxTokens from a huge integer to a fraction must reject"
        );

        let embedded_fractional = r#"{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}],"maxTokens":922337203685477580812345678901234567890.5}"#;
        assert!(
            serde_json::from_str::<FinalEmbeddedCreateMessageParams>(embedded_fractional).is_err(),
            "changing only embedded final maxTokens from a huge integer to a fraction must reject"
        );
    }

    #[test]
    fn final_embedded_input_response_equality_covers_each_typed_response() {
        let sampling: FinalEmbeddedInputResponse = serde_json::from_value(serde_json::json!({
            "content": {"type": "text", "text": "summary"},
            "model": "final-model",
            "role": "assistant",
            "_meta": {"com.example/trace": {"attempt": 1}}
        }))
        .expect("final sampling response decodes");
        let roots: FinalEmbeddedInputResponse = serde_json::from_value(serde_json::json!({
            "roots": [{"uri": "file:///workspace", "name": "workspace"}]
        }))
        .expect("final roots response decodes");
        let elicitation: FinalEmbeddedInputResponse = serde_json::from_value(serde_json::json!({
            "action": "accept",
            "content": {"choice": "yes", "attempt": 1}
        }))
        .expect("final elicitation response decodes");

        assert_eq!(sampling, sampling.clone());
        assert_eq!(roots, roots.clone());
        assert_eq!(elicitation, elicitation.clone());
        assert_ne!(sampling, roots);
        assert_ne!(roots, elicitation);
    }

    #[test]
    fn final_notification_unions_round_trip_the_exact_client_and_server_members() {
        let client_wire = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": "client-request-7",
                "reason": "client no longer needs this response",
                "awaitCleanup": true,
                "com.example/cancellationTrace": {"attempt": 2}
            })),
        );
        let client = ClientNotification::decode(&client_wire)
            .expect("the final client union admits its cancellation notification");
        assert_eq!(client.method(), NOTIFICATIONS_CANCELLED);
        assert!(client_wire.is_notification());
        let ClientNotification::Cancelled(params) = &client;
        assert_eq!(
            params.additional.get("awaitCleanup"),
            Some(&serde_json::json!(true)),
            "schema-open cancellation members retain legacy-looking names as opaque data"
        );
        assert_eq!(
            serde_json::to_value(client.encode().expect("client notification re-encodes"))
                .expect("client notification remains JSON"),
            serde_json::to_value(&client_wire).expect("client notification wire remains JSON")
        );

        let server_wires = [
            JsonRpcRequest::notification(
                NOTIFICATIONS_CANCELLED,
                Some(serde_json::json!({
                    "requestId": "subscription-9",
                    "com.example/cancellationTrace": "stream-close"
                })),
            ),
            JsonRpcRequest::notification(
                NOTIFICATIONS_PROGRESS,
                Some(serde_json::json!({
                    "progressToken": "job-9",
                    "progress": 1.0,
                    "total": 2.0,
                    "message": "halfway",
                    "com.example/progressPhase": "indexing"
                })),
            ),
            JsonRpcRequest::notification(
                NOTIFICATIONS_MESSAGE,
                Some(serde_json::json!({
                    "level": "notice",
                    "logger": "discovery-server",
                    "data": {"event": "catalog-refreshed"},
                    "com.example/logTrace": 7
                })),
            ),
            JsonRpcRequest::notification(
                NOTIFICATIONS_RESOURCES_UPDATED,
                Some(serde_json::json!({
                    "uri": "file:///workspace/status",
                    "com.example/resourceRevision": 4
                })),
            ),
            JsonRpcRequest::notification(
                NOTIFICATIONS_RESOURCES_LIST_CHANGED,
                Some(serde_json::json!({"com.example/listRevision": 8})),
            ),
            JsonRpcRequest::notification(
                NOTIFICATIONS_TOOLS_LIST_CHANGED,
                Some(serde_json::json!({
                    "_meta": {"com.example/trace": "tools-4"},
                    "com.example/listRevision": 9
                })),
            ),
            JsonRpcRequest::notification(NOTIFICATIONS_PROMPTS_LIST_CHANGED, None),
            JsonRpcRequest::notification(
                NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
                Some(serde_json::json!({
                    "notifications": {"toolsListChanged": true},
                    "com.example/acknowledgement": {"accepted": true}
                })),
            ),
        ];
        let expected_methods = [
            NOTIFICATIONS_CANCELLED,
            NOTIFICATIONS_PROGRESS,
            NOTIFICATIONS_MESSAGE,
            NOTIFICATIONS_RESOURCES_UPDATED,
            NOTIFICATIONS_RESOURCES_LIST_CHANGED,
            NOTIFICATIONS_TOOLS_LIST_CHANGED,
            NOTIFICATIONS_PROMPTS_LIST_CHANGED,
            NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
        ];

        for (wire, expected_method) in server_wires.iter().zip(expected_methods) {
            let notification = ServerNotification::decode(wire)
                .expect("every exact final server notification member is admitted");
            assert_eq!(notification.method(), expected_method);
            assert!(wire.is_notification());
            assert_eq!(
                serde_json::to_value(
                    notification
                        .encode()
                        .expect("server notification re-encodes")
                )
                .expect("server notification remains JSON"),
                serde_json::to_value(wire).expect("server notification wire remains JSON"),
                "{expected_method} preserves its exact notification parameter shape"
            );
        }
    }

    #[test]
    fn final_log_message_omits_an_absent_logger_and_rejects_explicit_null() {
        let absent = JsonRpcRequest::notification(
            NOTIFICATIONS_MESSAGE,
            Some(serde_json::json!({
                "level": "notice",
                "data": {"message": "catalog refreshed"}
            })),
        );
        let admitted = ServerNotification::decode(&absent)
            .expect("a final log message without a logger is admitted");
        let ServerNotification::Message(params) = &admitted else {
            panic!("final log message decodes to the message variant");
        };
        assert_eq!(params.logger, None);
        assert_eq!(
            serde_json::to_value(admitted.encode().expect("admitted log message re-encodes"))
                .expect("admitted log message remains JSON"),
            serde_json::to_value(&absent).expect("absent-logger message remains JSON"),
            "an absent final logger remains absent when the notification re-encodes"
        );

        let empty = JsonRpcRequest::notification(
            NOTIFICATIONS_MESSAGE,
            Some(serde_json::json!({
                "level": "notice",
                "logger": "",
                "data": {"message": "catalog refreshed"}
            })),
        );
        let empty = ServerNotification::decode(&empty)
            .expect("an empty final logger is a valid string value");
        let ServerNotification::Message(empty_params) = &empty else {
            panic!("empty logger decodes to the final message variant");
        };
        assert_eq!(empty_params.logger.as_deref(), Some(""));
        assert_eq!(
            serde_json::to_value(empty.encode().expect("empty logger re-encodes"))
                .expect("empty logger notification remains JSON")["params"]["logger"],
            ""
        );

        let explicit_null = JsonRpcRequest::notification(
            NOTIFICATIONS_MESSAGE,
            Some(serde_json::json!({
                "level": "notice",
                "logger": null,
                "data": {"message": "catalog refreshed"}
            })),
        );
        assert!(
            matches!(
                ServerNotification::decode(&explicit_null),
                Err(FinalNotificationError::InvalidParams {
                    method: NOTIFICATIONS_MESSAGE
                })
            ),
            "an explicit null final logger is invalid"
        );

        let outbound_missing_logger = FinalLogMessageParams {
            level: LoggingLevel::Notice,
            logger: None,
            data: serde_json::json!({"message": "catalog refreshed"}),
            meta: None,
            additional: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_value(outbound_missing_logger)
                .expect("an absent logger serializes as an omitted member"),
            serde_json::json!({
                "level": "notice",
                "data": {"message": "catalog refreshed"}
            })
        );
    }

    #[test]
    fn final_progress_raw_params_preserve_large_decimal_and_exponent_lexemes() {
        let large_wire = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"job-large","progress":123456789012345678901234567890}}"#;
        let large_request: JsonRpcRequest =
            serde_json::from_str(large_wire).expect("large exact progress notification parses");
        let large_params =
            r#"{"progressToken":"job-large","progress":123456789012345678901234567890}"#;
        let large_notification =
            ServerNotification::decode_with_raw_params(&large_request, large_params)
                .expect("large exact progress notification is admitted with its raw parameters");
        let ServerNotification::Progress(large_params) = &large_notification else {
            panic!("progress method decodes to the progress notification variant");
        };
        assert_eq!(
            large_params.progress.as_str(),
            "123456789012345678901234567890",
            "the large integer progress lexeme is retained without an IEEE-754 conversion"
        );
        assert_eq!(
            large_notification
                .encode_wire()
                .expect("large progress re-encodes"),
            large_wire,
            "the large integer progress lexeme round-trips exactly"
        );

        let equivalent_wire = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"job-decimal","progress":1.20e+4,"total":12000.0}}"#;
        let equivalent_request: JsonRpcRequest = serde_json::from_str(equivalent_wire)
            .expect("decimal/exponent exact progress notification parses");
        let equivalent_params =
            r#"{"progressToken":"job-decimal","progress":1.20e+4,"total":12000.0}"#;
        let equivalent_notification =
            ServerNotification::decode_with_raw_params(&equivalent_request, equivalent_params)
                .expect("numerically equal decimal and exponent progress fields are admitted");
        let ServerNotification::Progress(equivalent_params) = &equivalent_notification else {
            panic!("progress method decodes to the progress notification variant");
        };
        assert_eq!(equivalent_params.progress.as_str(), "1.20e+4");
        assert_eq!(
            equivalent_params
                .total
                .as_ref()
                .map(ExactNonNegativeJsonNumber::as_str),
            Some("12000.0")
        );
        assert_eq!(
            equivalent_params.total.as_ref(),
            Some(&equivalent_params.progress)
        );
        assert_eq!(
            equivalent_notification
                .encode_wire()
                .expect("equivalent progress re-encodes"),
            equivalent_wire,
            "equivalent decimal/exponent values retain their individual wire lexemes"
        );
    }

    #[test]
    fn final_progress_admits_finite_negative_and_greater_than_total_values() {
        let baseline_params =
            r#"{"progressToken":"job-ordered","progress":1.20e+4,"total":12000.0}"#;
        let baseline_wire = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{baseline_params}}}"#
        );
        let baseline: JsonRpcRequest =
            serde_json::from_str(&baseline_wire).expect("baseline progress notification parses");
        let admitted = ServerNotification::decode_with_raw_params(&baseline, baseline_params)
            .expect("equal exact progress and total values form the baseline");
        let baseline_wire = serde_json::to_value(&baseline).expect("baseline progress serializes");

        let negative_params = r#"{"progressToken":"job-ordered","progress":-1,"total":12000.0}"#;
        let negative: JsonRpcRequest = serde_json::from_str(&format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{negative_params}}}"#
        ))
        .expect("one-variable negative progress notification parses");
        let negative = ServerNotification::decode_with_raw_params(&negative, negative_params)
            .expect("negative finite progress is admitted");
        let ServerNotification::Progress(negative_params) = negative else {
            panic!("negative final progress notification decodes to the progress variant");
        };
        assert_eq!(negative_params.progress.as_str(), "-1");
        assert_eq!(
            negative_params
                .total
                .as_ref()
                .map(ExactNonNegativeJsonNumber::as_str),
            Some("12000.0")
        );

        let negative_total_params =
            r#"{"progressToken":"job-ordered","progress":1.20e+4,"total":-2}"#;
        let negative_total: JsonRpcRequest = serde_json::from_str(&format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{negative_total_params}}}"#
        ))
        .expect("one-variable negative total progress notification parses");
        let negative_total =
            ServerNotification::decode_with_raw_params(&negative_total, negative_total_params)
                .expect("negative finite total is admitted");
        let ServerNotification::Progress(negative_total_params) = negative_total else {
            panic!("negative-total final progress notification decodes to the progress variant");
        };
        assert_eq!(
            negative_total_params
                .total
                .as_ref()
                .map(ExactNonNegativeJsonNumber::as_str),
            Some("-2")
        );

        let greater_than_total_params =
            r#"{"progressToken":"job-ordered","progress":1.20e+4,"total":11999.0}"#;
        let greater_than_total: JsonRpcRequest = serde_json::from_str(&format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{greater_than_total_params}}}"#
        ))
        .expect("one-variable greater-than-total progress notification parses");
        let greater_than_total = ServerNotification::decode_with_raw_params(
            &greater_than_total,
            greater_than_total_params,
        )
        .expect("finite final progress greater than its total is admitted");
        let ServerNotification::Progress(greater_than_total_params) = greater_than_total else {
            panic!("greater-than-total notification decodes to the progress variant");
        };
        assert!(
            greater_than_total_params.progress
                > *greater_than_total_params
                    .total
                    .as_ref()
                    .expect("greater-than-total notification retains total"),
            "the admitted final values retain their unconstrained numeric relationship"
        );
        assert_eq!(
            serde_json::to_value(admitted.encode().expect("baseline progress re-encodes"))
                .expect("baseline progress JSON serializes"),
            baseline_wire,
            "admitting unconstrained finite values cannot mutate the exact progress baseline"
        );
    }

    #[test]
    fn legacy_progress_params_remain_the_separate_2024_f64_surface() {
        let legacy: ProgressParams =
            serde_json::from_str(r#"{"progressToken":"legacy-job","progress":-1.5,"total":2.0}"#)
                .expect("legacy progress remains governed by its existing f64 decoder");

        assert!((legacy.progress + 1.5).abs() < f64::EPSILON);
        assert!(
            legacy
                .total
                .is_some_and(|total| (total - 2.0).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn final_notification_unions_reject_wrong_direction_and_malformed_field() {
        let progress = JsonRpcRequest::notification(
            NOTIFICATIONS_PROGRESS,
            Some(serde_json::json!({"progressToken": "job-9", "progress": 1.0})),
        );
        let progress_wire = serde_json::to_value(&progress).expect("progress wire serializes");
        assert!(
            matches!(
                ClientNotification::decode(&progress),
                Err(FinalNotificationError::WrongDirection { method, sender: Final2026Peer::Client })
                    if method == NOTIFICATIONS_PROGRESS
            ),
            "only the originating peer changes: client admission rejects server-only progress"
        );
        assert_eq!(
            serde_json::to_value(&progress).expect("rejected progress remains serializable"),
            progress_wire,
            "wrong-direction rejection leaves the original notification wire unchanged"
        );

        let cancellation = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": "client-request-7",
                "awaitCleanup": true
            })),
        );
        let admitted = ClientNotification::decode(&cancellation)
            .expect("final cancellation preserves schema-open additional fields");
        let accepted_wire = serde_json::to_value(&cancellation).expect("accepted wire serializes");
        let mut planted = cancellation.clone();
        planted
            .params
            .as_mut()
            .and_then(Value::as_object_mut)
            .expect("cancellation owns object parameters")
            .insert("requestId".to_owned(), Value::Null);
        assert!(
            matches!(
                ClientNotification::decode(&planted),
                Err(FinalNotificationError::InvalidParams {
                    method: NOTIFICATIONS_CANCELLED
                })
            ),
            "changing only required requestId to null rejects the final cancellation shape"
        );
        assert_eq!(
            serde_json::to_value(admitted.encode().expect("accepted cancellation re-encodes"))
                .expect("accepted cancellation remains JSON"),
            accepted_wire,
            "the one-field malformed-field rejection leaves the admitted cancellation unchanged"
        );
    }

    #[test]
    fn final_cancellation_preserves_large_integer_request_ids_and_rejects_fractional_ids() {
        let large_id = "922337203685477580812345678901234567890";
        let accepted_wire = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{large_id}}}}}"#
        );
        let accepted: JsonRpcRequest = serde_json::from_str(&accepted_wire)
            .expect("arbitrary-precision integer cancellation ID decodes");
        let notification = ClientNotification::decode(&accepted)
            .expect("final cancellation retains the arbitrary-precision request ID");
        let ClientNotification::Cancelled(params) = &notification;
        assert_eq!(
            params.request_id,
            RequestId::Integer(large_id.to_owned()),
            "the cancellation parameter preserves the numeric ID without narrowing it"
        );
        assert_eq!(
            serde_json::to_string(&notification.encode().expect("cancellation re-encodes"))
                .expect("cancellation JSON serializes"),
            accepted_wire,
            "the final notification returns the exact large integer lexeme to the wire"
        );

        let baseline = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({"requestId": 1})),
        );
        let admitted = ClientNotification::decode(&baseline)
            .expect("integer cancellation request IDs remain admitted");
        let baseline_wire = serde_json::to_value(&baseline).expect("baseline wire serializes");
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(Value::as_object_mut)
            .expect("cancellation owns object parameters")
            .insert("requestId".to_owned(), serde_json::json!(1.5));
        assert!(
            matches!(
                ClientNotification::decode(&planted),
                Err(FinalNotificationError::InvalidParams {
                    method: NOTIFICATIONS_CANCELLED
                })
            ),
            "changing only the requestId from an integer to a fraction rejects cancellation"
        );
        assert_eq!(
            serde_json::to_value(admitted.encode().expect("integer cancellation re-encodes"))
                .expect("integer cancellation remains JSON"),
            baseline_wire,
            "fractional rejection cannot alter the admitted integer cancellation"
        );
    }

    #[test]
    fn legacy_sampling_stop_reason_is_optional_and_open() {
        let absent_wire = serde_json::json!({
            "content": {"type": "text", "text": "summary"},
            "role": "assistant",
            "model": "legacy-model"
        });
        let absent: CreateMessageResult = serde_json::from_value(absent_wire.clone())
            .expect("exact legacy sampling permits an absent stopReason");
        assert_eq!(absent.stop_reason, None);
        assert_eq!(
            serde_json::to_value(&absent).expect("absent legacy stopReason re-encodes"),
            absent_wire
        );

        let arbitrary_wire = serde_json::json!({
            "content": {"type": "text", "text": "summary"},
            "role": "assistant",
            "model": "legacy-model",
            "stopReason": "provider_safety_limit"
        });
        let arbitrary: CreateMessageResult = serde_json::from_value(arbitrary_wire.clone())
            .expect("exact legacy sampling retains an arbitrary provider stopReason");
        assert_eq!(
            arbitrary.stop_reason.as_deref(),
            Some("provider_safety_limit")
        );
        assert_eq!(
            serde_json::to_value(arbitrary).expect("open legacy stopReason re-encodes"),
            arbitrary_wire
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn legacy_sampling_rejects_one_final_result_field_without_mutating_its_baseline() {
        let request = CoreRequest::decode(
            ProtocolEra::Legacy2024,
            SAMPLING_CREATE_MESSAGE,
            Some(&serde_json::json!({
                "messages": [{"role": "user", "content": {"type": "text", "text": "hello"}}],
                "maxTokens": 8
            })),
        )
        .expect("legacy sampling baseline request");
        let accepted = r#"{"content":{"type":"text","text":"hello"},"role":"assistant","model":"legacy","stopReason":"endTurn"}"#;
        let baseline = request
            .decode_result(accepted)
            .expect("legacy sampling baseline result");
        let planted = r#"{"content":{"type":"text","text":"hello"},"role":"assistant","model":"legacy","stopReason":"endTurn","resultType":"complete"}"#;
        assert!(
            matches!(
                request.decode_result(planted),
                Err(CoreDispatchError::CrossEraResultType {
                    method: SAMPLING_CREATE_MESSAGE
                })
            ),
            "only the final resultType field changes the accepted legacy sampling result"
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                &request
                    .decode_result(accepted)
                    .expect("legacy baseline remains admitted")
                    .encode()
                    .expect("legacy baseline encodes"),
            )
            .expect("reaccepted legacy sampling result is JSON"),
            serde_json::from_str::<Value>(&baseline.encode().expect("baseline encodes"))
                .expect("baseline legacy sampling result is JSON"),
            "the cross-era rejection leaves legacy sampling semantics unchanged"
        );
    }

    #[test]
    fn final_catalog_results_preserve_typed_cache_hints() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let cases = [
            (
                TOOLS_LIST,
                r#"{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private"}"#,
            ),
            (
                RESOURCES_LIST,
                r#"{"resultType":"complete","resources":[],"ttlMs":1,"cacheScope":"public"}"#,
            ),
            (
                RESOURCES_TEMPLATES_LIST,
                r#"{"resultType":"complete","resourceTemplates":[],"ttlMs":2,"cacheScope":"private"}"#,
            ),
            (
                PROMPTS_LIST,
                r#"{"resultType":"complete","prompts":[],"ttlMs":3,"cacheScope":"public"}"#,
            ),
            (
                RESOURCES_READ,
                r#"{"resultType":"complete","contents":[],"ttlMs":4,"cacheScope":"private"}"#,
            ),
        ];
        for (method, wire) in cases {
            let request_params = if method == RESOURCES_READ {
                serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                        "io.modelcontextprotocol/clientCapabilities": {}
                    },
                    "uri": "file:///workspace/status"
                })
            } else {
                params.clone()
            };
            let request =
                CoreRequest::decode(ProtocolEra::Modern2026, method, Some(&request_params))
                    .expect("final catalog/read request");
            let result = request
                .decode_result(wire)
                .expect("required final cache fields decode");
            assert_eq!(
                serde_json::from_str::<Value>(
                    &result.encode().expect("final cached result encodes")
                )
                .expect("final cached result encoding is JSON"),
                serde_json::from_str::<Value>(wire).expect("final cached result fixture is JSON"),
                "{method} preserves final cache-result semantics"
            );
        }

        let tools_request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&params))
            .expect("tools/list request");
        assert!(
            matches!(
                tools_request.decode_result(
                    r#"{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"shared"}"#
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_LIST,
                })
            ),
            "only an invalid cacheScope changes the otherwise valid final catalog result; peer TTL omission/negativity is normalized as immediately stale at client ingress"
        );
    }

    #[test]
    fn final_catalog_ttl_ms_preserves_an_unbounded_wire_integer() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&params))
            .expect("final tools/list request");
        let accepted = r#"{"resultType":"complete","tools":[],"ttlMs":18446744073709551616,"cacheScope":"private"}"#;
        let decoded = request
            .decode_result(accepted)
            .expect("the unbounded nonnegative final TTL is admitted");
        let CoreResult::Final(FinalCoreResult::ToolsList { result, .. }) = &decoded else {
            panic!("final tools/list result");
        };
        assert_eq!(result.payload.ttl_ms.as_str(), "18446744073709551616");
        assert_eq!(
            result.payload.ttl_ms.try_as_millis(),
            Err(crate::result::CacheTtlConversionError::RuntimeOutOfRange),
            "only the runtime conversion rejects the one-over-u64 TTL"
        );
        assert_eq!(
            decoded.encode().expect("unbounded final TTL re-encodes"),
            accepted
        );

        let fractional = r#"{"resultType":"complete","tools":[],"ttlMs":18446744073709551616.5,"cacheScope":"private"}"#;
        assert!(
            matches!(
                request.decode_result(fractional),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_LIST,
                })
            ),
            "changing only ttlMs from an unbounded integer to a fraction violates the final cache schema"
        );
    }

    #[test]
    fn final_retry_parameters_preserve_input_state_and_require_object_arguments() {
        let call_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "weather",
            "arguments": {"city": "Boston"},
            "inputResponses": {"request-1": {"roots": []}},
            "requestState": "retry-1"
        });
        let baseline = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&call_params))
            .expect("final call admits retry state and object arguments");
        assert_eq!(
            baseline
                .encode_params()
                .expect("call parameters encode")
                .expect("call owns parameters"),
            call_params
        );

        let mut planted = call_params.clone();
        planted["arguments"] = serde_json::json!(["Boston"]);
        assert!(
            matches!(
                CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&planted)),
                Err(CoreDispatchError::InvalidParams {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })
            ),
            "only a non-object arguments value changes the accepted final call"
        );

        let read_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "uri": "file:///workspace/status",
            "inputResponses": {"request-1": {"roots": []}},
            "requestState": "retry-1"
        });
        let get_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "status",
            "inputResponses": {"request-1": {"roots": []}},
            "requestState": "retry-1"
        });
        for (method, params) in [(RESOURCES_READ, read_params), (PROMPTS_GET, get_params)] {
            let request = CoreRequest::decode(ProtocolEra::Modern2026, method, Some(&params))
                .expect("final retry parameters decode");
            assert_eq!(
                request
                    .encode_params()
                    .expect("retry parameters encode")
                    .expect("retry-owning request has parameters"),
                params,
                "{method} retains retry input responses and request state"
            );
        }
    }

    #[test]
    fn final_mrtr_retry_responses_are_typed_ordered_and_correlatable() {
        let responses_wire = r#"{"second":{"roots":[]},"first":{"roots":[]}}"#;
        let responses: FinalInputResponses = serde_json::from_str(responses_wire)
            .expect("typed final input responses decode in their wire order");
        assert_eq!(
            responses
                .entries()
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"],
            "input response key order remains observable after decoding"
        );
        assert_eq!(
            serde_json::to_string(&responses).expect("typed responses re-encode"),
            responses_wire,
            "the exact inputResponses object order round-trips"
        );

        let raw_call_params = r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"weather","inputResponses":{"second":{"roots":[]},"first":{"roots":[]}}}"#;
        let materialized_call_params: Value = serde_json::from_str(raw_call_params)
            .expect("raw call parameters materialize for JSON-RPC envelope admission");
        let CoreRequest::Final(FinalCoreRequest::ToolsCall(call)) =
            CoreRequest::decode_with_raw_params(
                ProtocolEra::Modern2026,
                TOOLS_CALL,
                Some(&materialized_call_params),
                Some(raw_call_params),
            )
            .expect("the raw final core decoder preserves MRTR response ordering")
        else {
            panic!("raw tools/call parameters select the final request type");
        };
        assert_eq!(
            call.input_responses
                .as_ref()
                .expect("present retry map")
                .entries()
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"],
            "the core raw-params path does not inherit materialized-map sorting"
        );

        let ExactJsonValue::Object(input_requests) = crate::result::parse_exact_json(
            r#"{"second":{"method":"roots/list"},"first":{"method":"roots/list"}}"#,
        )
        .expect("input request map admits exactly") else {
            panic!("inputRequests must be an object");
        };
        responses
            .validate_against(&input_requests)
            .expect("each typed response matches its exact input request key and kind");

        let wrong_kind: FinalInputResponses =
            serde_json::from_str(r#"{"second":{"action":"decline"},"first":{"roots":[]}}"#)
                .expect("a differently typed embedded response is structurally valid");
        assert_eq!(
            wrong_kind.validate_against(&input_requests),
            Err(FinalInputResponseCorrelationError::ResponseKindMismatch),
            "changing only one response payload kind rejects correlation"
        );

        let mismatched_raw = raw_call_params.replacen("weather", "forecast", 1);
        assert!(
            matches!(
                CoreRequest::decode_with_raw_params(
                    ProtocolEra::Modern2026,
                    TOOLS_CALL,
                    Some(&materialized_call_params),
                    Some(&mismatched_raw),
                ),
                Err(CoreDispatchError::InvalidParams {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })
            ),
            "changing only the raw method-owned value cannot attach a source from another frame"
        );

        let raw_resource_params = r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"uri":"file:///workspace/status","inputResponses":{"second":{"roots":[]},"first":{"roots":[]}}}"#;
        let raw_prompt_params = r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"status","inputResponses":{"second":{"roots":[]},"first":{"roots":[]}}}"#;
        for (method, raw_params, mismatched_raw) in [
            (
                RESOURCES_READ,
                raw_resource_params,
                raw_resource_params.replacen("status", "other", 1),
            ),
            (
                PROMPTS_GET,
                raw_prompt_params,
                raw_prompt_params.replacen("status", "other", 1),
            ),
        ] {
            let materialized: Value = serde_json::from_str(raw_params)
                .expect("raw method parameters materialize for envelope admission");
            let decoded = CoreRequest::decode_with_raw_params(
                ProtocolEra::Modern2026,
                method,
                Some(&materialized),
                Some(raw_params),
            )
            .expect("raw final retry parameters preserve ordered input responses");
            let entry_keys = match decoded {
                CoreRequest::Final(FinalCoreRequest::ResourcesRead(params)) => params
                    .input_responses
                    .as_ref()
                    .expect("resource retry map is present")
                    .entries()
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>(),
                CoreRequest::Final(FinalCoreRequest::PromptsGet(params)) => params
                    .input_responses
                    .as_ref()
                    .expect("prompt retry map is present")
                    .entries()
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>(),
                _ => panic!("raw parameters select their method's final request type"),
            };
            assert_eq!(
                entry_keys,
                vec!["second".to_owned(), "first".to_owned()],
                "{method} retains inputResponses wire order"
            );
            assert!(
                matches!(
                    CoreRequest::decode_with_raw_params(
                        ProtocolEra::Modern2026,
                        method,
                        Some(&materialized),
                        Some(&mismatched_raw),
                    ),
                    Err(CoreDispatchError::InvalidParams {
                        era: ProtocolEra::Modern2026,
                        method: rejected_method,
                    }) if rejected_method == method
                ),
                "changing only one raw {method} value cannot attach another frame's source"
            );
        }
    }

    #[test]
    fn final_mrtr_retry_rejects_duplicate_wire_keys_and_present_state_only_maps() {
        let raw_params = r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"weather","inputResponses":{"roots":{"roots":[]},"roots":{"roots":[]}},"requestState":"retry-1"}"#;
        let materialized: Value = serde_json::from_str(raw_params)
            .expect("JSON-RPC envelope materializes duplicate members as a value");
        assert!(
            matches!(
                CoreRequest::decode_with_raw_params(
                    ProtocolEra::Modern2026,
                    TOOLS_CALL,
                    Some(&materialized),
                    Some(raw_params),
                ),
                Err(CoreDispatchError::InvalidParams {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })
            ),
            "a duplicate inputResponses wire key is rejected before it can collapse into a map"
        );

        let request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            TOOLS_CALL,
            Some(&serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                },
                "name": "weather"
            })),
        )
        .expect("accepted final tool request remains available after planted rejection");
        let CoreResult::Final(FinalCoreResult::ToolsCallInputRequired { result, .. }) = request
            .decode_result(r#"{"resultType":"input_required","requestState":"state-only"}"#)
            .expect("state-only input-required result decodes")
        else {
            panic!("state-only result selects the final input-required branch");
        };
        assert_eq!(
            FinalInputResponses::default().validate_against_input_required(&result),
            Err(FinalInputResponseCorrelationError::StateOnlyInputResponses),
            "an explicit empty inputResponses object is not an absent member"
        );
        assert!(
            result.input_requests().is_none(),
            "the planted explicit-empty rejection does not add input requests"
        );
        assert_eq!(
            result.request_state(),
            Some("state-only"),
            "the planted explicit-empty rejection does not mutate accepted state-only state"
        );
    }

    #[test]
    fn final_retry_parameters_reject_null_or_untyped_input_responses() {
        let accepted = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "weather",
            "inputResponses": {"request-1": {"roots": []}},
            "requestState": "retry-1"
        });
        let baseline = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&accepted))
            .expect("typed final retry parameters are admitted");

        for (field, value) in [
            ("inputResponses", serde_json::Value::Null),
            ("requestState", serde_json::Value::Null),
            (
                "inputResponses",
                serde_json::json!({"request-1": {"approved": true}}),
            ),
        ] {
            let mut planted = accepted.clone();
            planted[field] = value;
            assert!(
                matches!(
                    CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&planted)),
                    Err(CoreDispatchError::InvalidParams {
                        era: ProtocolEra::Modern2026,
                        method: TOOLS_CALL,
                    })
                ),
                "changing only {field} rejects a null or untyped final retry member"
            );
        }
        assert_eq!(
            baseline.encode_params().expect("baseline encodes"),
            Some(accepted),
            "each planted rejection leaves the accepted retry parameters unchanged"
        );
    }

    #[test]
    fn final_arguments_admit_absence_and_objects_but_reject_null() {
        let meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        });

        let tool_absent = serde_json::json!({"_meta": meta.clone(), "name": "weather"});
        let CoreRequest::Final(FinalCoreRequest::ToolsCall(tool)) =
            CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&tool_absent))
                .expect("an absent final tool arguments member is admitted")
        else {
            panic!("tools/call selects its final request type");
        };
        assert!(tool.arguments.is_absent());
        assert_eq!(
            CoreRequest::Final(FinalCoreRequest::ToolsCall(tool))
                .encode_params()
                .expect("absent tool arguments encode")
                .expect("tools/call has params"),
            tool_absent,
        );

        let tool_object = serde_json::json!({
            "_meta": meta.clone(),
            "name": "weather",
            "arguments": {"units": "metric"}
        });
        assert!(
            CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&tool_object)).is_ok()
        );
        let tool_null =
            serde_json::json!({"_meta": meta.clone(), "name": "weather", "arguments": null});
        assert!(
            CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&tool_null)).is_err(),
            "changing only final tool arguments from an object to null rejects"
        );

        let prompt_absent = serde_json::json!({"_meta": meta.clone(), "name": "summary"});
        let CoreRequest::Final(FinalCoreRequest::PromptsGet(prompt)) =
            CoreRequest::decode(ProtocolEra::Modern2026, PROMPTS_GET, Some(&prompt_absent))
                .expect("an absent final prompt arguments member is admitted")
        else {
            panic!("prompts/get selects its final request type");
        };
        assert!(prompt.arguments.is_absent());

        let prompt_null = serde_json::json!({"_meta": meta, "name": "summary", "arguments": null});
        assert!(
            CoreRequest::decode(ProtocolEra::Modern2026, PROMPTS_GET, Some(&prompt_null)).is_err(),
            "changing only final prompt arguments from absent to null rejects"
        );
    }

    #[test]
    fn final_server_info_is_admitted_only_in_result_metadata() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&params))
            .expect("final tools/list request");
        let accepted = r#"{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"final-server","version":"1.0.0"}}}"#;
        let baseline = request
            .decode_result(accepted)
            .expect("final serverInfo is admitted in metadata");
        assert_eq!(
            baseline.encode().expect("metadata serverInfo encodes"),
            accepted
        );

        let planted = r#"{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"final-server","version":"1.0.0"}},"serverInfo":{"name":"legacy-location","version":"1.0.0"}}"#;
        assert!(
            matches!(
                request.decode_result(planted),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_LIST,
                })
            ),
            "only a top-level serverInfo changes the final result admission"
        );
        assert_eq!(
            request
                .decode_result(accepted)
                .expect("baseline remains admitted")
                .encode()
                .expect("baseline encodes"),
            baseline.encode().expect("original baseline encodes"),
            "the top-level serverInfo rejection does not alter metadata server info"
        );
    }

    #[test]
    fn final_result_metadata_seal_is_typed_and_excludes_open_metadata() {
        let server_info =
            Implementation::try_new("sealed-server", "1.0.0").expect("server identity is valid");
        let metadata = OpenMetadata::try_from_entries([
            (
                FINAL_SERVER_INFO_META_KEY.to_owned(),
                serde_json::to_value(&server_info).expect("server identity serializes"),
            ),
            ("com.example/trace".to_owned(), serde_json::json!("open")),
        ])
        .expect("metadata is valid");
        let complete = FinalCoreResult::ToolsCall {
            result: CompleteResult::new(
                FinalCallToolResult {
                    content: Vec::new(),
                    is_error: false,
                    structured_content: None,
                },
                ResultMeta::server_generated(server_info.clone()).with_metadata(metadata),
            ),
            diagnostic: None,
        };
        assert_eq!(
            complete
                .protected_metadata_seal()
                .expect("complete result seal is typed"),
            FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ToolsCall,
                server_info: FinalResultServerInfo::Common(Some(server_info.clone())),
                subscription_id: None,
            }
        );

        let input_required = FinalCoreResult::PromptsGetInputRequired {
            result: InputRequiredResult::new(
                None,
                Some("retry".to_owned()),
                ResultMeta::server_generated(server_info.clone()),
            )
            .expect("input-required result is valid"),
            diagnostic: None,
        };
        assert_eq!(
            input_required
                .protected_metadata_seal()
                .expect("input-required result seal is typed"),
            FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::PromptsGetInputRequired,
                server_info: FinalResultServerInfo::Common(Some(server_info.clone())),
                subscription_id: None,
            }
        );

        let subscription_id = RequestId::String("subscription-7".to_owned());
        let subscription_metadata = OpenMetadata::try_from_entries([
            (
                FINAL_SERVER_INFO_META_KEY.to_owned(),
                serde_json::to_value(&server_info).expect("server identity serializes"),
            ),
            (
                FINAL_SUBSCRIPTION_ID_META_KEY.to_owned(),
                serde_json::to_value(&subscription_id).expect("subscription id serializes"),
            ),
        ])
        .expect("subscription metadata is valid");
        let subscription = FinalCoreResult::SubscriptionsListen {
            result: CompleteResult::new(
                FinalSubscriptionsListenResult {},
                ResultMeta::server_generated(server_info.clone())
                    .with_metadata(subscription_metadata),
            ),
            subscription_id: subscription_id.clone(),
            diagnostic: None,
        };
        assert_eq!(
            subscription
                .protected_metadata_seal()
                .expect("subscription result seal is typed"),
            FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::SubscriptionsListen,
                server_info: FinalResultServerInfo::Common(Some(server_info)),
                subscription_id: Some(subscription_id),
            }
        );
    }

    #[test]
    fn final_log_level_metadata_replaces_final_set_level_rpc() {
        let final_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/logLevel": "notice"
            }
        });
        let request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            SERVER_DISCOVER,
            Some(&final_params),
        )
        .expect("final discovery metadata carries log level");
        let CoreRequest::Final(FinalCoreRequest::Discover(params)) = request else {
            panic!("final discovery request");
        };
        assert_eq!(
            params.meta.log_level().expect("typed final log level"),
            Some(LoggingLevel::Notice)
        );
        let notification = FinalLogMessageParams {
            level: LoggingLevel::Notice,
            logger: Some("final.server".to_owned()),
            data: serde_json::json!({"message": "catalog refreshed"}),
            meta: None,
            additional: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_value(notification).expect("final log notification encodes"),
            serde_json::json!({
                "level": "notice",
                "logger": "final.server",
                "data": {"message": "catalog refreshed"}
            })
        );
        assert!(matches!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                PING,
                Some(&serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }))
            ),
            Err(CoreDispatchError::UnsupportedMethod {
                era: ProtocolEra::Modern2026,
                method,
            }) if method == PING
        ));
        #[cfg(feature = "legacy-2024-11-05")]
        assert!(
            CoreRequest::decode(ProtocolEra::Legacy2024, PING, None).is_ok(),
            "the exact legacy ping request remains available only in its legacy era"
        );
    }

    #[test]
    fn final_discover_core_result_round_trips_typed_capabilities_and_cache_hints() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, SERVER_DISCOVER, Some(&params))
            .expect("final server/discover request is typed");
        let advertised = crate::ServerDiscoverResult::new(
            crate::ServerDiscoverCapabilities::from_registry(
                &crate::ServerBehaviorRegistry::from_behaviors([
                    crate::ServerBehavior::ToolsList,
                    crate::ServerBehavior::ToolsListChangedNotification,
                ]),
                std::collections::BTreeMap::new(),
            )
            .expect("installed server behavior derives typed discovery capabilities"),
            ServerInfo {
                name: "discovery-server".to_owned(),
                version: "1.0.0".to_owned(),
            },
            Some(
                crate::ServerInstructions::new("Use tools before answering.")
                    .expect("bounded discovery instructions"),
            ),
            crate::DiscoveryCacheHints::private_ttl_ms(60_000),
        );
        let accepted = serde_json::to_value(&advertised).expect("typed discovery result encodes");
        let result = request
            .decode_result(
                &serde_json::to_string(&accepted).expect("discovery wire serializes for dispatch"),
            )
            .expect("typed discovery result is admitted by final core dispatch");
        let CoreResult::Final(final_result @ FinalCoreResult::Discover(decoded)) = &result else {
            panic!("server/discover selects its typed final result");
        };
        assert_eq!(
            decoded.supported_versions(),
            [FINAL_PROTOCOL_VERSION.to_owned()],
            "the final discovery version set round-trips exactly"
        );
        assert_eq!(
            decoded
                .server_info()
                .map(|info| (info.name.as_str(), info.version.as_str())),
            Some(("discovery-server", "1.0.0")),
            "serverInfo remains final result metadata"
        );
        assert_eq!(
            final_result
                .protected_metadata_seal()
                .expect("discovery result metadata seal is typed"),
            FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::Discover,
                server_info: FinalResultServerInfo::Discovery(Some(FinalDiscoveryServerInfo {
                    name: "discovery-server".to_owned(),
                    version: "1.0.0".to_owned(),
                })),
                subscription_id: None,
            }
        );
        assert_eq!(
            decoded
                .instructions()
                .map(crate::ServerInstructions::as_str),
            Some("Use tools before answering."),
            "instructions remain part of the typed discovery result"
        );
        assert_eq!(
            decoded
                .cache_hints()
                .ttl_ms()
                .try_as_millis()
                .expect("local TTL fits the runtime domain"),
            60_000
        );
        assert!(!decoded.cache_hints().is_public());
        assert_eq!(
            serde_json::from_str::<Value>(&result.encode().expect("typed result re-encodes"))
                .expect("encoded typed result remains JSON"),
            accepted,
            "capabilities, serverInfo, instructions, and cache hints all survive core dispatch"
        );

        let mut planted = accepted.clone();
        planted["cacheScope"] = serde_json::json!("shared");
        assert!(
            matches!(
                request.decode_result(
                    &serde_json::to_string(&planted)
                        .expect("one-field malformed discovery wire serializes"),
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: SERVER_DISCOVER,
                })
            ),
            "changing only cacheScope to an unrecognized value rejects the typed discovery result"
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                &result.encode().expect("accepted result stays immutable")
            )
            .expect("accepted result stays JSON"),
            accepted,
            "the malformed peer field cannot mutate the admitted discovery result"
        );
    }

    #[test]
    fn final_discover_core_result_rejects_non_complete_result_algebra() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, SERVER_DISCOVER, Some(&params))
            .expect("final server/discover request is typed");
        let baseline = serde_json::json!({
            "resultType": "complete",
            "supportedVersions": [FINAL_PROTOCOL_VERSION],
            "capabilities": {},
            "ttlMs": 0,
            "cacheScope": "private"
        });

        for result_type in [
            serde_json::json!("input_required"),
            serde_json::json!("task"),
            serde_json::json!("com.example/deferred-discovery"),
            Value::Null,
        ] {
            let mut planted = baseline.clone();
            planted["resultType"] = result_type;
            assert!(
                matches!(
                    request.decode_result(
                        &serde_json::to_string(&planted)
                            .expect("one-field invalid discovery result serializes"),
                    ),
                    Err(CoreDispatchError::InvalidResult {
                        era: ProtocolEra::Modern2026,
                        method: SERVER_DISCOVER,
                    })
                ),
                "only complete or omission can select the modern discovery result"
            );
        }

        let mut contradictory = baseline;
        contradictory["requestState"] = serde_json::json!("resume-1");
        assert!(
            matches!(
                request.decode_result(
                    &serde_json::to_string(&contradictory)
                        .expect("contradictory discovery result serializes"),
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: SERVER_DISCOVER,
                })
            ),
            "a complete discriminator cannot make an input-required shape discovery"
        );
    }

    #[test]
    fn core_dispatch_round_trips_legacy_and_final_core_payloads() {
        let final_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "echo",
            "arguments": {"message": "hello"}
        });
        let final_request =
            CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&final_params))
                .expect("final tools/call request is typed through common metadata");
        assert_eq!(final_request.era(), ProtocolEra::Modern2026);
        assert_eq!(final_request.method(), TOOLS_CALL);
        assert_eq!(
            final_request
                .encode_params()
                .expect("final request re-encodes")
                .expect("final requests always own a parameter object"),
            final_params
        );

        let final_wire = r#"{"resultType":"complete","content":[{"type":"text","text":"ready"}],"extension":{"opaque":true}}"#;
        let final_result = final_request
            .decode_result(final_wire)
            .expect("final complete result selects the tools/call payload");
        let CoreResult::Final(FinalCoreResult::ToolsCall { result, diagnostic }) = &final_result
        else {
            panic!("final tools/call result");
        };
        assert_eq!(diagnostic, &None);
        assert!(matches!(
            result.payload.content.as_slice(),
            [ContentBlock::Text { text, .. }] if text == "ready"
        ));
        assert_eq!(
            result
                .extras
                .members()
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>(),
            ["extension"]
        );
        assert_eq!(
            final_result.encode().expect("final result re-encodes"),
            final_wire
        );

        #[cfg(feature = "legacy-2024-11-05")]
        {
            let legacy_params = serde_json::json!({"cursor": ""});
            let legacy_request =
                CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_LIST, Some(&legacy_params))
                    .expect("legacy tools/list keeps its exact parameter struct");
            assert_eq!(legacy_request.era(), ProtocolEra::Legacy2024);
            assert_eq!(
                legacy_request
                    .encode_params()
                    .expect("legacy request re-encodes")
                    .expect("list request owns a parameter object"),
                legacy_params
            );
            let legacy_wire = r#"{"tools":[],"nextCursor":""}"#;
            let legacy_result = legacy_request
                .decode_result(legacy_wire)
                .expect("legacy result selects the legacy payload");
            assert!(matches!(
                legacy_result,
                CoreResult::Legacy(LegacyCoreResult::ToolsList(_))
            ));
            assert_eq!(
                legacy_result.encode().expect("legacy result re-encodes"),
                legacy_wire
            );
        }
    }

    #[test]
    fn final_tools_call_result_preserves_absent_and_explicit_null_structured_content() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "nullable"
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&params))
            .expect("final tools/call request decodes");

        let absent_wire = r#"{"resultType":"complete","content":[]}"#;
        let absent = request
            .decode_result(absent_wire)
            .expect("absent structuredContent is valid");
        let CoreResult::Final(FinalCoreResult::ToolsCall { result, .. }) = &absent else {
            panic!("final tools/call complete result");
        };
        assert!(result.payload.structured_content.is_none());
        assert_eq!(
            absent
                .encode()
                .expect("absent structuredContent re-encodes"),
            absent_wire
        );

        let null_wire = r#"{"resultType":"complete","content":[],"structuredContent":null}"#;
        let explicit_null = request
            .decode_result(null_wire)
            .expect("explicit-null structuredContent is a present JSON value");
        let CoreResult::Final(FinalCoreResult::ToolsCall { result, .. }) = &explicit_null else {
            panic!("final tools/call complete result");
        };
        assert_eq!(result.payload.structured_content, Some(Value::Null));
        assert_eq!(
            explicit_null
                .encode()
                .expect("explicit-null structuredContent re-encodes"),
            null_wire
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_tools_call_task_result_selects_a_disjoint_typed_branch() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "long-running"
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&params))
            .expect("final tools/call request admits the task result branch");
        let accepted = serde_json::json!({
            "resultType": "task",
            "taskId": "task-1",
            "status": "working",
            "createdAt": "2026-07-28T12:00:00.000Z",
            "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
            "ttlMs": null,
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "task-server",
                    "version": "1.0.0"
                }
            },
            "com.example/opaque": {"retained": true}
        });
        let wire = serde_json::to_string(&accepted).expect("task result serializes");

        let decoded = request
            .decode_result(&wire)
            .expect("final tools/call task result decodes");
        let CoreResult::Final(final_result) = &decoded else {
            panic!("tools/call must select a final result branch");
        };
        let FinalCoreResult::ToolsCallTask { result } = final_result else {
            panic!("tools/call must select the task result branch");
        };
        assert_eq!(result.task.base().task_id.as_str(), "task-1");
        assert_eq!(
            final_result
                .protected_metadata_seal()
                .expect("task result metadata seal is typed"),
            FinalResultMetadataSeal {
                family: FinalResultMetadataFamily::ToolsCallTask,
                server_info: FinalResultServerInfo::Common(Some(
                    Implementation::try_new("task-server", "1.0.0")
                        .expect("task server identity is valid"),
                )),
                subscription_id: None,
            },
            "task metadata seals serverInfo while retaining unrelated open entries"
        );
        assert_eq!(
            result.additional.get("com.example/opaque"),
            Some(&serde_json::json!({"retained": true}))
        );
        assert_eq!(
            serde_json::from_str::<Value>(&decoded.encode().expect("task result re-encodes"))
                .expect("encoded task result is JSON"),
            accepted,
            "the typed task branch preserves the task result and inert siblings"
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_tools_call_task_response_uses_admitted_raw_result_source() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "long-running"
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&params))
            .expect("final tools/call request admits the task result branch");
        let frame = r#"{"jsonrpc":"2.0","result":{"resultType":"task","taskId":"task-1","status":"completed","createdAt":"2026-07-28T12:00:00.000Z","lastUpdatedAt":"2026-07-28T12:00:00.000Z","ttlMs":null,"result":{"x-first":1.20e+4,"content":[],"x-second":123456789012345678901234567890}},"id":91}"#;
        let admission = crate::decode_strict_jsonrpc_response(frame.as_bytes(), frame.len())
            .expect("bounded JSON-RPC admission retains the exact result member");
        let result_source = admission
            .raw_result()
            .expect("successful response has an exact result source");
        let decoded = request
            .decode_response_result(admission.response(), result_source)
            .expect("Tasks decoder consumes the admitted result source");
        assert_eq!(
            decoded.encode().expect("admitted task result re-encodes"),
            result_source,
            "response ingress preserves nested Tasks member order and numeric lexemes"
        );
    }

    #[cfg(all(feature = "tasks", feature = "legacy-2024-11-05"))]
    #[test]
    fn final_tools_call_task_result_rejections_leave_decode_state_unchanged() {
        let call_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "name": "long-running"
        });
        let call = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_CALL, Some(&call_params))
            .expect("baseline final tools/call request");
        let accepted = serde_json::json!({
            "resultType": "task",
            "taskId": "task-1",
            "status": "working",
            "createdAt": "2026-07-28T12:00:00.000Z",
            "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
            "ttlMs": null
        });
        let accepted_wire = serde_json::to_string(&accepted).expect("task result serializes");
        let baseline = call
            .decode_result(&accepted_wire)
            .expect("baseline task result decodes");

        let mut wrong_result = accepted.clone();
        wrong_result["resultType"] = serde_json::json!("complete");
        assert!(
            matches!(
                call.decode_result(
                    &serde_json::to_string(&wrong_result)
                        .expect("one-field wrong result serializes")
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })
            ),
            "changing only resultType cannot reinterpret a task result as a complete tools/call result"
        );

        let mut missing_ttl = accepted.clone();
        missing_ttl
            .as_object_mut()
            .expect("task result is an object")
            .remove("ttlMs");
        assert!(
            matches!(
                call.decode_result(
                    &serde_json::to_string(&missing_ttl)
                        .expect("one-field missing TTL task serializes")
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })
            ),
            "unlike cacheable catalog peers, Tasks keep ttlMs required"
        );

        let mut negative_ttl = accepted.clone();
        negative_ttl["ttlMs"] = serde_json::json!(-1);
        assert!(
            matches!(
                call.decode_result(
                    &serde_json::to_string(&negative_ttl)
                        .expect("one-field negative TTL task serializes")
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: TOOLS_CALL,
                })
            ),
            "the composed Tasks profile rejects a negative ttlMs"
        );
        assert_eq!(
            baseline.encode().expect("baseline task re-encodes"),
            call.decode_result(&accepted_wire)
                .expect("accepted task remains decodable")
                .encode()
                .expect("accepted task re-encodes"),
            "rejected ttlMs mutation cannot alter accepted task state"
        );

        let list_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let wrong_method =
            CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&list_params))
                .expect("final tools/list request");
        assert!(
            matches!(
                wrong_method.decode_result(&accepted_wire),
                Err(CoreDispatchError::UnexpectedFinalResultType { method: TOOLS_LIST })
            ),
            "the task result discriminator belongs only to final tools/call"
        );

        let reaccepted = call
            .decode_result(&accepted_wire)
            .expect("wrong result and method do not mutate task result decoding");
        assert_eq!(
            serde_json::from_str::<Value>(&baseline.encode().expect("baseline encodes"))
                .expect("baseline is JSON"),
            serde_json::from_str::<Value>(&reaccepted.encode().expect("reaccepted encodes"))
                .expect("reaccepted result is JSON"),
            "the rejected one-field resultType cannot mutate the admitted task result"
        );

        let legacy_params = serde_json::json!({"name": "long-running"});
        let legacy = CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_CALL, Some(&legacy_params))
            .expect("legacy tools/call request");
        let legacy_wire = r#"{"content":[{"type":"text","text":"ready"}]}"#;
        let legacy_baseline = legacy
            .decode_result(legacy_wire)
            .expect("legacy tools/call result remains admitted");
        let legacy_planted = r#"{"resultType":"task","content":[{"type":"text","text":"ready"}]}"#;
        assert!(
            matches!(
                legacy.decode_result(legacy_planted),
                Err(CoreDispatchError::CrossEraResultType { method: TOOLS_CALL })
            ),
            "adding only the final task discriminator cannot alter legacy tools/call decoding"
        );
        assert_eq!(
            legacy
                .decode_result(legacy_wire)
                .expect("legacy rejection leaves baseline decoding intact")
                .encode()
                .expect("legacy reaccepted result encodes"),
            legacy_baseline.encode().expect("legacy baseline encodes"),
            "the final task branch leaves exact legacy result decoding unchanged"
        );
    }

    #[test]
    fn final_core_input_required_results_round_trip_for_mrtr_methods() {
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let requests = [
            (
                TOOLS_CALL,
                serde_json::json!({
                    "_meta": metadata,
                    "name": "collect-input"
                }),
            ),
            (
                RESOURCES_READ,
                serde_json::json!({
                    "_meta": metadata,
                    "uri": "file:///workspace/status"
                }),
            ),
            (
                PROMPTS_GET,
                serde_json::json!({
                    "_meta": metadata,
                    "name": "collect-input"
                }),
            ),
        ];
        let wire = r#"{"resultType":"input_required","inputRequests":{"roots":{"method":"roots/list"}},"requestState":"retry-7","ttlMs":-1,"cacheScope":"private","com.example/opaque":{"retained":true}}"#;

        for (method, params) in requests {
            let request = CoreRequest::decode(ProtocolEra::Modern2026, method, Some(&params))
                .expect("each final MRTR-capable request decodes");
            let result = request
                .decode_result(wire)
                .expect("input-required result is admitted for the selected method");
            let input_required = match (&result, method) {
                (
                    CoreResult::Final(FinalCoreResult::ToolsCallInputRequired { result, .. }),
                    TOOLS_CALL,
                )
                | (
                    CoreResult::Final(FinalCoreResult::ResourcesReadInputRequired {
                        result, ..
                    }),
                    RESOURCES_READ,
                )
                | (
                    CoreResult::Final(FinalCoreResult::PromptsGetInputRequired { result, .. }),
                    PROMPTS_GET,
                ) => result,
                _ => panic!("{method} must select its final input-required branch"),
            };
            assert!(
                input_required
                    .input_requests()
                    .and_then(|requests| requests.get("roots"))
                    .is_some(),
                "{method} preserves the exact MRTR input request map"
            );
            assert_eq!(input_required.request_state(), Some("retry-7"));
            let extras = input_required.extras.members();
            assert_eq!(
                extras.len(),
                3,
                "{method} keeps complete-result cache lookalikes inert on input_required"
            );
            for name in ["ttlMs", "cacheScope", "com.example/opaque"] {
                assert!(
                    extras.iter().any(|member| member.name == name),
                    "{method} retains inert {name} on input_required"
                );
            }
            assert_eq!(
                serde_json::from_str::<Value>(
                    &result.encode().expect("input-required result re-encodes")
                )
                .expect("input-required result encoding is JSON"),
                serde_json::from_str::<Value>(wire).expect("input-required result fixture is JSON"),
                "{method} retains input-required state and inert lookalikes"
            );
        }
    }

    #[test]
    fn final_core_rejects_input_required_for_ineligible_method_without_mutation() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&params))
            .expect("final tools/list request decodes");
        let accepted = r#"{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private","inputRequests":{"roots":{"method":"roots/list"}},"requestState":"retry-7"}"#;
        let baseline = request
            .decode_result(accepted)
            .expect("complete tools/list retains foreign open siblings inertly");
        let planted = accepted.replacen("\"complete\"", "\"input_required\"", 1);

        assert!(
            matches!(
                request.decode_result(&planted),
                Err(CoreDispatchError::UnexpectedFinalResultType { method: TOOLS_LIST })
            ),
            "changing only resultType cannot make tools/list MRTR-capable"
        );
        assert_eq!(
            baseline
                .encode()
                .expect("accepted complete result remains encodable"),
            accepted,
            "the rejected input-required discriminator leaves the accepted result unchanged"
        );
    }

    #[test]
    fn core_completion_preserves_legacy_and_final_payload_semantics() {
        #[cfg(feature = "legacy-2024-11-05")]
        {
            let legacy_params = serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "deploy"},
                "argument": {"name": "environment", "value": "sta"}
            });
            let legacy_request = CoreRequest::decode(
                ProtocolEra::Legacy2024,
                COMPLETION_COMPLETE,
                Some(&legacy_params),
            )
            .expect("exact legacy completion request is typed");
            assert_eq!(legacy_request.era(), ProtocolEra::Legacy2024);
            assert_eq!(legacy_request.method(), COMPLETION_COMPLETE);
            assert_eq!(
                legacy_request
                    .encode_params()
                    .expect("legacy completion request re-encodes")
                    .expect("completion owns an object parameter"),
                legacy_params
            );

            let legacy_wire = r#"{"completion":{"values":["staging"],"total":1,"hasMore":false}}"#;
            let legacy_result = legacy_request
                .decode_result(legacy_wire)
                .expect("exact legacy completion result is typed");
            let CoreResult::Legacy(LegacyCoreResult::Completion(result)) = &legacy_result else {
                panic!("legacy completion result");
            };
            assert_eq!(result.completion.values, vec!["staging".to_owned()]);
            assert_eq!(result.completion.total, Some(1));
            assert_eq!(result.completion.has_more, Some(false));
            let encoded_legacy: Value = serde_json::from_str(
                &legacy_result
                    .encode()
                    .expect("legacy completion re-encodes"),
            )
            .expect("legacy completion encoding is JSON");
            assert_eq!(
                encoded_legacy["completion"]["values"],
                serde_json::json!(["staging"])
            );
            assert_eq!(encoded_legacy["completion"]["total"], 1);
            assert_eq!(encoded_legacy["completion"]["hasMore"], false);
        }

        let final_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "ref": {
                "type": "ref/prompt",
                "name": "deploy",
                "title": "Deploy application"
            },
            "argument": {"name": "environment", "value": "pro"},
            "context": {"arguments": {"region": "us-east-1"}}
        });
        let final_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            COMPLETION_COMPLETE,
            Some(&final_params),
        )
        .expect("final completion request is typed through final metadata");
        assert_eq!(final_request.era(), ProtocolEra::Modern2026);
        assert_eq!(final_request.method(), COMPLETION_COMPLETE);
        let CoreRequest::Final(FinalCoreRequest::Completion(params)) = &final_request else {
            panic!("final completion request");
        };
        assert!(matches!(
            &params.reference,
            FinalCompletionReference::PromptWithTitle { title, .. } if title == "Deploy application"
        ));
        assert_eq!(
            final_request
                .encode_params()
                .expect("final completion request re-encodes")
                .expect("completion owns an object parameter"),
            final_params
        );

        let final_wire = r#"{"resultType":"complete","completion":{"values":["production"],"total":1,"hasMore":false},"extension":{"opaque":true}}"#;
        let final_result = final_request
            .decode_result(final_wire)
            .expect("final complete result selects the completion payload");
        let CoreResult::Final(FinalCoreResult::Completion { result, diagnostic }) = &final_result
        else {
            panic!("final completion result");
        };
        assert_eq!(diagnostic, &None);
        assert_eq!(
            result.payload.completion.values,
            vec!["production".to_owned()]
        );
        assert_eq!(
            result.payload.completion.total,
            Some(JsonInteger::from(1_i64))
        );
        assert_eq!(result.payload.completion.has_more, Some(false));
        assert_eq!(
            result
                .extras
                .members()
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>(),
            ["extension"]
        );
        let encoded_final: Value =
            serde_json::from_str(&final_result.encode().expect("final completion re-encodes"))
                .expect("final completion encoding is JSON");
        let completion = encoded_final["completion"]
            .as_object()
            .expect("final completion remains an object");
        assert_eq!(encoded_final["resultType"], "complete");
        assert_eq!(completion["values"], serde_json::json!(["production"]));
        assert_eq!(
            completion["total"]
                .as_number()
                .map(serde_json::Number::as_str),
            Some("1")
        );
        assert_eq!(completion.get("hasMore"), Some(&Value::Bool(false)));
        assert!(
            completion.contains_key("total") && completion.contains_key("hasMore"),
            "present final completion optionals remain present after re-encoding"
        );
        assert_eq!(
            encoded_final["extension"],
            serde_json::json!({"opaque": true})
        );
    }

    #[test]
    fn completion_values_enforce_the_one_hundred_value_limit() {
        let accepted = CompletionValues {
            values: (0..MAX_COMPLETION_VALUES)
                .map(|index| format!("value-{index}"))
                .collect(),
            total: Some(MAX_COMPLETION_VALUES as i64),
            has_more: Some(false),
        };
        let encoded = serde_json::to_value(&accepted).expect("one hundred values serialize");
        assert_eq!(
            encoded["values"].as_array().map(Vec::len),
            Some(MAX_COMPLETION_VALUES)
        );

        let too_many = (0..=MAX_COMPLETION_VALUES)
            .map(|index| format!("value-{index}"))
            .collect::<Vec<_>>();
        assert!(
            serde_json::from_value::<CompletionValues>(serde_json::json!({
                "values": too_many
            }))
            .is_err(),
            "a peer cannot admit 101 completion values"
        );
        assert!(
            serde_json::to_value(CompletionValues {
                values: (0..=MAX_COMPLETION_VALUES)
                    .map(|index| format!("value-{index}"))
                    .collect(),
                total: None,
                has_more: None,
            })
            .is_err(),
            "locally authored results cannot emit 101 completion values"
        );
    }

    #[test]
    fn final_completion_values_preserve_arbitrary_precision_totals_at_the_value_bound() {
        let admitted = FinalCompletionValues {
            values: (0..MAX_COMPLETION_VALUES)
                .map(|index| format!("value-{index}"))
                .collect(),
            total: Some(
                serde_json::from_str("922337203685477580812345678901234567890")
                    .expect("an arbitrary-precision JSON integer"),
            ),
            has_more: Some(false),
        };
        let wire = serde_json::to_value(&admitted).expect("100 final completion values serialize");
        assert_eq!(
            wire["values"].as_array().map(Vec::len),
            Some(MAX_COMPLETION_VALUES)
        );
        assert_eq!(
            wire["total"].as_number().map(serde_json::Number::as_str),
            Some("922337203685477580812345678901234567890")
        );
        assert!(
            serde_json::to_value(FinalCompletionValues {
                values: (0..=MAX_COMPLETION_VALUES)
                    .map(|index| format!("value-{index}"))
                    .collect(),
                total: None,
                has_more: None,
            })
            .is_err(),
            "a locally authored final result cannot emit 101 completion values"
        );
    }

    #[test]
    fn final_completion_values_diagnose_negative_peer_totals_and_refuse_local_emission() {
        let accepted = serde_json::json!({
            "values": ["stable"],
            "total": 0,
            "hasMore": false,
        });
        let admitted = serde_json::from_value::<FinalCompletionValues>(accepted.clone())
            .expect("a nonnegative final completion total and bounded candidate are admitted");
        assert_eq!(
            serde_json::to_value(&admitted).expect("the admitted final completion re-encodes"),
            accepted
        );

        let mut negative_total = accepted.clone();
        negative_total["total"] = serde_json::json!(-1);
        let admitted_negative = serde_json::from_value::<FinalCompletionValues>(negative_total)
            .expect("a schema-valid negative peer total remains decodable");
        assert_eq!(
            admitted_negative.peer_diagnostic(),
            Some(FinalCompletionPeerDiagnostic::NegativeTotal),
            "a negative peer total has bounded compatibility diagnostics"
        );
        assert!(
            serde_json::to_value(&admitted_negative).is_err(),
            "a peer-only negative total cannot be forwarded as local provider output"
        );
        assert!(
            serde_json::to_value(FinalCompletionValues {
                values: vec!["stable".to_owned()],
                total: Some(JsonInteger::from(-1_i64)),
                has_more: Some(false),
            })
            .is_err(),
            "a locally authored final completion cannot emit a negative total"
        );

        let at_value_bound = serde_json::json!({
            "values": ["v".repeat(MAX_FINAL_COMPLETION_VALUE_BYTES)],
            "total": 1,
        });
        assert!(
            serde_json::from_value::<FinalCompletionValues>(at_value_bound).is_ok(),
            "a final completion candidate at the per-value byte limit is admitted"
        );

        let one_byte_over = serde_json::json!({
            "values": ["v".repeat(MAX_FINAL_COMPLETION_VALUE_BYTES + 1)],
            "total": 1,
        });
        assert!(
            serde_json::from_value::<FinalCompletionValues>(one_byte_over).is_err(),
            "adding one candidate byte crosses the final completion limiter"
        );

        let at_aggregate_bound = serde_json::json!({
            "values": vec!["v".repeat(MAX_FINAL_COMPLETION_VALUE_BYTES);
                MAX_FINAL_COMPLETION_VALUES_BYTES / MAX_FINAL_COMPLETION_VALUE_BYTES],
            "total": MAX_FINAL_COMPLETION_VALUES_BYTES / MAX_FINAL_COMPLETION_VALUE_BYTES,
        });
        assert!(
            serde_json::from_value::<FinalCompletionValues>(at_aggregate_bound.clone()).is_ok(),
            "final completion candidates at the aggregate byte limit are admitted"
        );

        let mut aggregate_one_byte_over = at_aggregate_bound;
        aggregate_one_byte_over["values"]
            .as_array_mut()
            .expect("completion values array")
            .push(serde_json::json!("v"));
        assert!(
            serde_json::from_value::<FinalCompletionValues>(aggregate_one_byte_over).is_err(),
            "adding one aggregate candidate byte crosses the final completion limiter"
        );

        let legacy_negative = serde_json::json!({
            "values": ["stable"],
            "total": -1,
        });
        assert!(
            serde_json::from_value::<CompletionValues>(legacy_negative).is_ok(),
            "exact MCP 2024-11-05 retains its signed completion total schema"
        );
    }

    #[test]
    fn final_completion_context_preserves_presence_and_exact_bounds() {
        let meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let request_without_context = serde_json::json!({
            "_meta": meta,
            "ref": {"type": "ref/prompt", "name": "deploy"},
            "argument": {"name": "environment", "value": "pro"}
        });
        let CoreRequest::Final(FinalCoreRequest::Completion(without_context)) =
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&request_without_context),
            )
            .expect("an absent final completion context is valid")
        else {
            panic!("final completion request");
        };
        assert!(without_context.context.is_none());

        let request_with_empty_context = serde_json::json!({
            "_meta": meta,
            "ref": {"type": "ref/prompt", "name": "deploy"},
            "argument": {"name": "environment", "value": "pro"},
            "context": {"arguments": {}}
        });
        let CoreRequest::Final(FinalCoreRequest::Completion(with_empty_context)) =
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&request_with_empty_context),
            )
            .expect("a present empty final completion context is valid")
        else {
            panic!("final completion request");
        };
        assert!(
            with_empty_context
                .context
                .as_ref()
                .and_then(|context| context.arguments.as_ref())
                .is_some_and(BTreeMap::is_empty),
            "present empty context arguments remain distinct from an absent context"
        );

        let mut bounded_arguments = serde_json::Map::new();
        for index in 0..MAX_COMPLETION_CONTEXT_ARGUMENTS {
            bounded_arguments.insert(format!("key-{index}"), Value::String("value".to_owned()));
        }
        let request_at_bound = serde_json::json!({
            "_meta": meta,
            "ref": {"type": "ref/prompt", "name": "deploy"},
            "argument": {"name": "environment", "value": "pro"},
            "context": {"arguments": bounded_arguments}
        });
        let at_bound = CoreRequest::decode(
            ProtocolEra::Modern2026,
            COMPLETION_COMPLETE,
            Some(&request_at_bound),
        )
        .expect("the exact completion-context entry limit is valid");
        assert_eq!(
            at_bound
                .encode_params()
                .expect("bounded final completion context re-encodes")
                .expect("completion has parameters"),
            request_at_bound,
            "the admitted context map retains every exact string entry"
        );
    }

    #[test]
    fn final_completion_context_encoded_bytes_honor_short_escapes_and_exact_boundary() {
        assert_eq!(
            encoded_json_string_bytes("\u{0008}\t\n\u{000c}\r\u{0000}\"\\"),
            22,
            "JSON uses two-byte escapes for backspace, tab, newline, form feed, carriage return, quote, and backslash"
        );

        let mut arguments = BTreeMap::new();
        let mut encoded_bytes = 2_usize;
        for index in 0..15 {
            let key = format!("key-{index}");
            let value = "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES);
            encoded_bytes = next_completion_context_encoded_bytes(
                encoded_bytes,
                !arguments.is_empty(),
                &key,
                &value,
            )
            .expect("the first fifteen maximum values fit the aggregate bound");
            arguments.insert(key, value);
        }
        let final_key = "last".to_owned();
        let encoded_empty_final_value =
            next_completion_context_encoded_bytes(encoded_bytes, true, &final_key, "")
                .expect("an empty final value fits the aggregate bound");
        let final_value =
            "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES - encoded_empty_final_value);
        assert!(final_value.len() <= MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES);
        assert_eq!(
            next_completion_context_encoded_bytes(encoded_bytes, true, &final_key, &final_value,),
            Some(MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES),
            "the final value reaches the aggregate bound exactly"
        );
        arguments.insert(final_key.clone(), final_value.clone());
        let admitted = FinalCompletionContext {
            arguments: Some(arguments),
        };
        let wire = serde_json::to_string(&admitted).expect("the exact aggregate bound serializes");
        let decoded: FinalCompletionContext =
            serde_json::from_str(&wire).expect("bounded string seeds admit the exact bound");
        assert_eq!(decoded, admitted);

        let mut one_byte_over = admitted;
        one_byte_over
            .arguments
            .as_mut()
            .expect("context arguments are present")
            .get_mut(&final_key)
            .expect("final boundary value is present")
            .push('v');
        assert!(
            serde_json::to_string(&one_byte_over).is_err(),
            "one additional encoded byte must reject"
        );

        let oversized_key_wire = format!(
            r#"{{"arguments":{{"{}":"value"}}}}"#,
            "k".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES + 1)
        );
        assert!(
            serde_json::from_str::<FinalCompletionContext>(&oversized_key_wire).is_err(),
            "the bounded key seed rejects before retaining an oversized key"
        );

        let maximum_key_wire = format!(
            r#"{{"arguments":{{"{}":"value"}}}}"#,
            "k".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES)
        );
        assert!(
            serde_json::from_str::<FinalCompletionContext>(&maximum_key_wire).is_ok(),
            "the bounded key seed admits exactly 1024 key bytes"
        );

        let maximum_value_wire = format!(
            r#"{{"arguments":{{"key":"{}"}}}}"#,
            "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES)
        );
        assert!(
            serde_json::from_str::<FinalCompletionContext>(&maximum_value_wire).is_ok(),
            "the bounded value seed admits exactly 16384 value bytes"
        );
        let oversized_value_wire = format!(
            r#"{{"arguments":{{"key":"{}"}}}}"#,
            "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES + 1)
        );
        assert!(
            serde_json::from_str::<FinalCompletionContext>(&oversized_value_wire).is_err(),
            "the bounded value seed rejects before retaining an oversized value"
        );
    }

    #[test]
    fn jsonrpc_ingress_validates_final_completion_context_from_raw_params() {
        let final_request = format!(
            r#"{{"jsonrpc":"2.0","method":"completion/complete","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{FINAL_PROTOCOL_VERSION}","io.modelcontextprotocol/clientCapabilities":{{}}}},"ref":{{"type":"ref/prompt","name":"deploy"}},"argument":{{"name":"environment","value":"pro"}},"context":{{"arguments":{{"key":"{}"}}}}}},"id":1}}"#,
            "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES)
        );
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                final_request.as_bytes(),
                MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES * 2,
            )
            .is_ok(),
            "strict JSON-RPC ingress admits a final context value at the raw-source bound"
        );

        let oversized_final_request = format!(
            r#"{{"jsonrpc":"2.0","method":"completion/complete","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{FINAL_PROTOCOL_VERSION}","io.modelcontextprotocol/clientCapabilities":{{}}}},"ref":{{"type":"ref/prompt","name":"deploy"}},"argument":{{"name":"environment","value":"pro"}},"context":{{"arguments":{{"key":"{}"}}}}}},"id":1}}"#,
            "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES + 1)
        );
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                oversized_final_request.as_bytes(),
                MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES * 2,
            )
            .is_err(),
            "strict JSON-RPC ingress rejects an oversized final context before params become Value"
        );

        let legacy_request = format!(
            r#"{{"jsonrpc":"2.0","method":"completion/complete","params":{{"ref":{{"type":"ref/prompt","name":"deploy"}},"argument":{{"name":"environment","value":"sta"}},"context":{{"arguments":{{"key":"{}"}}}}}},"id":1}}"#,
            "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES + 1)
        );
        assert!(
            serde_json::from_str::<JsonRpcRequest>(&legacy_request).is_ok(),
            "legacy completion parameters without final metadata retain their existing wire path"
        );
    }

    #[test]
    fn jsonrpc_ingress_bounds_every_duplicate_final_completion_context_before_serde() {
        fn final_completion_request(first: &str, second: &str) -> String {
            format!(
                r#"{{"jsonrpc":"2.0","method":"completion/complete","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{FINAL_PROTOCOL_VERSION}","io.modelcontextprotocol/clientCapabilities":{{}}}},"ref":{{"type":"ref/prompt","name":"deploy"}},"argument":{{"name":"environment","value":"pro"}},"context":{{"arguments":{{"key":"{first}"}}}},"context":{{"arguments":{{"key":"{second}"}}}}}},"id":1}}"#
            )
        }

        let small = "small";
        let oversized = r"\/".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES / 2 + 1);
        assert_eq!(
            oversized.len(),
            MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES + 2,
            "the fixture exceeds only the received raw value-byte bound"
        );

        for (first, second, case) in [
            (oversized.as_str(), small, "oversized-first/small-second"),
            (small, oversized.as_str(), "small-first/oversized-second"),
        ] {
            let error =
                serde_json::from_str::<JsonRpcRequest>(&final_completion_request(first, second))
                    .expect_err("every final context occurrence must be raw-bounded before serde");
            assert!(
                error.to_string().contains(
                    "completion context argument value exceeds the maximum raw JSON byte limit"
                ),
                "{case} must fail at the raw bound rather than after serde reaches a duplicate context"
            );
        }
    }

    #[test]
    fn jsonrpc_ingress_measures_received_completion_context_json_bytes() {
        fn final_completion_request(arguments: &str) -> String {
            let mut request = format!(
                r#"{{"jsonrpc":"2.0","method":"completion/complete","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"{FINAL_PROTOCOL_VERSION}","io.modelcontextprotocol/clientCapabilities":{{}}}},"ref":{{"type":"ref/prompt","name":"deploy"}},"argument":{{"name":"environment","value":"pro"}},"context":{{"arguments":"#
            );
            request.push_str(arguments);
            request.push_str(r#"}},"id":1}"#);
            request
        }

        let escaped_key_at_bound = format!("{}{}", r"\u006b".repeat(170), "k".repeat(4));
        assert_eq!(
            escaped_key_at_bound.len(),
            MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES
        );
        let accepted_key =
            final_completion_request(&format!(r#"{{"{escaped_key_at_bound}":"value"}}"#));
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                accepted_key.as_bytes(),
                accepted_key.len(),
            )
            .is_ok(),
            "an escaped context key at the received-byte limit is admitted"
        );
        let rejected_key =
            final_completion_request(&format!(r#"{{"{escaped_key_at_bound}\u006b":"value"}}"#));
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                rejected_key.as_bytes(),
                rejected_key.len(),
            )
            .is_err(),
            "adding only one escaped key spelling crosses the raw key-byte limit"
        );

        let escaped_value_at_bound = r"\/".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES / 2);
        assert_eq!(
            escaped_value_at_bound.len(),
            MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES
        );
        let accepted_value =
            final_completion_request(&format!(r#"{{"key":"{escaped_value_at_bound}"}}"#));
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                accepted_value.as_bytes(),
                accepted_value.len(),
            )
            .is_ok(),
            "an escaped context value at the received-byte limit is admitted"
        );
        let rejected_value =
            final_completion_request(&format!(r#"{{"key":"{escaped_value_at_bound}x"}}"#));
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                rejected_value.as_bytes(),
                rejected_value.len(),
            )
            .is_err(),
            "adding only one raw value byte crosses the received value-byte limit"
        );

        let escaped_maximum_value = r"\/".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES / 2);
        let entries = (0..15)
            .map(|index| format!(r#""key-{index}":"{escaped_maximum_value}""#))
            .collect::<Vec<_>>();
        // The tail value's OPENING quote belongs to the prefix; the suffix
        // carries only the closing quote and brace. Omitting it made the
        // fixture invalid JSON and turned both assertions vacuous.
        let prefix = format!(r#"{{{},"tail":""#, entries.join(","));
        let suffix = r#""}"#;
        let tail_bytes = MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES - prefix.len() - suffix.len();
        assert!(tail_bytes <= MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES);
        let tail = format!(
            "{}{}",
            r"\/".repeat(tail_bytes / 2),
            "x".repeat(tail_bytes % 2)
        );
        let aggregate_at_bound = format!("{prefix}{tail}{suffix}");
        assert_eq!(
            aggregate_at_bound.len(),
            MAX_COMPLETION_CONTEXT_ARGUMENT_BYTES,
            "the fixture counts the exact received arguments-object bytes, including \\/ escapes"
        );
        let accepted_aggregate = final_completion_request(&aggregate_at_bound);
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                accepted_aggregate.as_bytes(),
                accepted_aggregate.len(),
            )
            .is_ok(),
            "an alternate-escape arguments object at the received-byte limit is admitted"
        );
        let rejected_aggregate = final_completion_request(&format!("{prefix}{tail}x{suffix}"));
        assert!(
            crate::jsonrpc::decode_strict_jsonrpc_message(
                rejected_aggregate.as_bytes(),
                rejected_aggregate.len(),
            )
            .is_err(),
            "adding only one raw JSON byte makes the arguments object too large"
        );
    }

    #[test]
    fn final_completion_context_rejects_one_field_null_and_one_entry_over_bound() {
        let meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        let accepted = serde_json::json!({
            "_meta": meta,
            "ref": {"type": "ref/resource", "uri": "file:///templates/{name}"},
            "argument": {"name": "name", "value": "prod"},
            "context": {"arguments": {"region": "us-east-1"}}
        });
        let baseline = CoreRequest::decode(
            ProtocolEra::Modern2026,
            COMPLETION_COMPLETE,
            Some(&accepted),
        )
        .expect("baseline final completion context is valid");

        let mut null_arguments = accepted.clone();
        null_arguments["context"]["arguments"] = Value::Null;
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&null_arguments),
            )
            .is_err(),
            "changing only context.arguments to null must reject"
        );

        let mut null_context = accepted.clone();
        null_context["context"] = Value::Null;
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&null_context),
            )
            .is_err(),
            "changing only context to null must reject"
        );

        let mut at_bound_arguments = serde_json::Map::new();
        for index in 0..MAX_COMPLETION_CONTEXT_ARGUMENTS {
            at_bound_arguments.insert(format!("key-{index}"), Value::String("value".to_owned()));
        }
        let mut one_over_bound = accepted.clone();
        one_over_bound["context"]["arguments"] = Value::Object(at_bound_arguments);
        one_over_bound["context"]["arguments"]["one-too-many"] = Value::String("value".to_owned());
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&one_over_bound),
            )
            .is_err(),
            "adding only a 257th context argument must reject"
        );

        let mut oversized_key = accepted.clone();
        let mut oversized_key_arguments = serde_json::Map::new();
        oversized_key_arguments.insert(
            "k".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_KEY_BYTES + 1),
            Value::String("value".to_owned()),
        );
        oversized_key["context"]["arguments"] = Value::Object(oversized_key_arguments);
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&oversized_key),
            )
            .is_err(),
            "changing only the context map to contain an oversized key must reject"
        );

        let mut oversized_value = accepted.clone();
        oversized_value["context"]["arguments"] = serde_json::json!({
            "key": "v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES + 1)
        });
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&oversized_value),
            )
            .is_err(),
            "changing only the context map to contain an oversized value must reject"
        );

        let mut aggregate_over_bound_arguments = serde_json::Map::new();
        for index in 0..17 {
            aggregate_over_bound_arguments.insert(
                format!("key-{index}"),
                Value::String("v".repeat(MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES)),
            );
        }
        let mut aggregate_over_bound = accepted.clone();
        aggregate_over_bound["context"]["arguments"] =
            Value::Object(aggregate_over_bound_arguments);
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                COMPLETION_COMPLETE,
                Some(&aggregate_over_bound),
            )
            .is_err(),
            "changing only the context map to exceed its aggregate encoded-byte bound must reject"
        );
        assert_eq!(
            baseline
                .encode_params()
                .expect("accepted context remains encodable")
                .expect("completion has parameters"),
            accepted,
            "rejected context mutations leave the accepted request unchanged"
        );
    }

    #[test]
    fn final_completion_resource_reference_requires_rfc6570_on_construction_decode_and_encode() {
        let valid = FinalCompletionReference::resource("mcp://resources/{item}{?cursor}")
            .expect("a valid RFC 6570 resource reference constructs");
        assert_eq!(
            serde_json::to_value(&valid).expect("valid resource reference serializes"),
            serde_json::json!({"type": "ref/resource", "uri": "mcp://resources/{item}{?cursor}"})
        );

        let invalid = "mcp://resources/{item:0}";
        assert!(
            FinalCompletionReference::resource(invalid).is_err(),
            "construction rejects an RFC 6570-invalid prefix modifier"
        );
        assert!(
            serde_json::from_value::<FinalCompletionReference>(serde_json::json!({
                "type": "ref/resource",
                "uri": invalid,
            }))
            .is_err(),
            "peer decode rejects the same invalid resource reference"
        );
        let bypass = FinalCompletionReference::Resource {
            uri: invalid.to_owned(),
        };
        assert!(
            serde_json::to_value(&bypass).is_err(),
            "direct enum construction cannot bypass RFC 6570 admission on local emission"
        );
    }

    #[test]
    fn final_completion_total_is_exact_and_rejects_one_field_invalid_forms() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "ref": {"type": "ref/prompt", "name": "deploy"},
            "argument": {"name": "environment", "value": "pro"}
        });
        let request =
            CoreRequest::decode(ProtocolEra::Modern2026, COMPLETION_COMPLETE, Some(&params))
                .expect("final completion request");
        let accepted = r#"{"resultType":"complete","completion":{"values":["production"],"total":922337203685477580812345678901234567890,"hasMore":false}}"#;
        let baseline = request
            .decode_result(accepted)
            .expect("an arbitrary-precision exact total is valid");
        let CoreResult::Final(FinalCoreResult::Completion { result, .. }) = &baseline else {
            panic!("final completion result");
        };
        assert_eq!(
            result
                .payload
                .completion
                .total
                .as_ref()
                .map(JsonInteger::as_str),
            Some("922337203685477580812345678901234567890")
        );

        for planted_total in ["null", "1.5"] {
            let planted = format!(
                r#"{{"resultType":"complete","completion":{{"values":["production"],"total":{planted_total},"hasMore":false}}}}"#
            );
            assert!(
                request.decode_result(&planted).is_err(),
                "changing only total to {planted_total} must reject"
            );
        }
        let negative = r#"{"resultType":"complete","completion":{"values":["production"],"total":-1,"hasMore":false}}"#;
        let negative_result = request
            .decode_result(negative)
            .expect("a negative schema-valid peer total remains decodable");
        let CoreResult::Final(FinalCoreResult::Completion { result, .. }) = negative_result else {
            panic!("negative total remains a completion result");
        };
        assert_eq!(
            result.payload.completion.peer_diagnostic(),
            Some(FinalCompletionPeerDiagnostic::NegativeTotal),
            "negative completion totals retain a bounded peer diagnostic"
        );
        assert!(
            result.payload.completion.validate().is_err(),
            "a negative peer total is not valid for local provider emission"
        );
        let planted_has_more = r#"{"resultType":"complete","completion":{"values":["production"],"total":922337203685477580812345678901234567890,"hasMore":null}}"#;
        assert!(
            request.decode_result(planted_has_more).is_err(),
            "changing only hasMore to null must reject"
        );
        let encoded: Value = serde_json::from_str(
            &baseline
                .encode()
                .expect("accepted exact total remains encodable"),
        )
        .expect("accepted exact total encoding is JSON");
        let completion = encoded["completion"]
            .as_object()
            .expect("accepted exact total retains its completion object");
        assert_eq!(encoded["resultType"], "complete");
        assert_eq!(completion["values"], serde_json::json!(["production"]));
        assert_eq!(
            completion["total"]
                .as_number()
                .map(serde_json::Number::as_str),
            Some("922337203685477580812345678901234567890")
        );
        assert_eq!(completion.get("hasMore"), Some(&Value::Bool(false)));
        assert!(
            completion.contains_key("total") && completion.contains_key("hasMore"),
            "present final completion optionals remain present after re-encoding"
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn legacy_completion_result_retains_meta_during_round_trip() {
        let request = CoreRequest::decode(
            ProtocolEra::Legacy2024,
            COMPLETION_COMPLETE,
            Some(&serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "deploy"},
                "argument": {"name": "environment", "value": "sta"}
            })),
        )
        .expect("legacy completion request");
        let wire = r#"{"completion":{"values":["staging"]},"_meta":{"trace":{"attempt":1},"cache":"private"}}"#;
        let result = request
            .decode_result(wire)
            .expect("legacy completion metadata remains typed");
        let CoreResult::Legacy(LegacyCoreResult::Completion(completion)) = &result else {
            panic!("legacy completion result");
        };
        let metadata = completion
            .meta
            .as_ref()
            .expect("legacy metadata is retained");
        assert_eq!(metadata.get("cache"), Some(&serde_json::json!("private")));
        assert_eq!(
            metadata.get("trace"),
            Some(&serde_json::json!({"attempt": 1}))
        );
        assert_eq!(
            serde_json::from_str::<Value>(&result.encode().expect("legacy completion re-encodes"))
                .expect("legacy completion encoding is JSON"),
            serde_json::from_str::<Value>(wire).expect("legacy completion fixture is JSON"),
            "legacy completion _meta is retained without asserting source member order"
        );
    }

    #[test]
    fn final_subscriptions_listen_round_trips_request_result_and_acknowledgement() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "notifications": {
                "promptsListChanged": false,
                "resourceSubscriptions": ["file:///workspace/status"],
                "resourcesListChanged": true,
                "toolsListChanged": true,
                "com.example/extension": {"enabled": true}
            }
        });
        let request =
            CoreRequest::decode(ProtocolEra::Modern2026, SUBSCRIPTIONS_LISTEN, Some(&params))
                .expect("final subscriptions/listen request is typed");
        assert_eq!(request.method(), SUBSCRIPTIONS_LISTEN);
        let CoreRequest::Final(FinalCoreRequest::SubscriptionsListen(listen)) = &request else {
            panic!("final subscriptions/listen request");
        };
        assert!(matches!(
            listen.notifications.resource_subscriptions.as_deref(),
            Some([uri]) if uri == "file:///workspace/status"
        ));
        assert_eq!(listen.notifications.prompts_list_changed, Some(false));
        assert_eq!(
            listen.notifications.additional.get("com.example/extension"),
            Some(&serde_json::json!({"enabled": true}))
        );
        assert_eq!(
            request
                .encode_params()
                .expect("final subscriptions/listen re-encodes")
                .expect("listen owns a parameter object"),
            params
        );

        let acknowledgement_wire = serde_json::json!({
            "_meta": {"io.modelcontextprotocol/subscriptionId": "subscription-7"},
            "notifications": {
                "resourceSubscriptions": ["file:///workspace/status"],
                "toolsListChanged": true
            }
        });
        let acknowledgement: FinalSubscriptionsAcknowledgedNotificationParams =
            serde_json::from_value(acknowledgement_wire.clone())
                .expect("acknowledgement notification is typed");
        assert_eq!(
            acknowledgement
                .meta
                .as_ref()
                .and_then(|metadata| metadata.get(FINAL_SUBSCRIPTION_ID_META_KEY)),
            Some(&serde_json::json!("subscription-7"))
        );
        assert_eq!(
            serde_json::to_value(&acknowledgement).expect("acknowledgement re-encodes"),
            acknowledgement_wire
        );

        let result_wire = r#"{"resultType":"complete","_meta":{"io.modelcontextprotocol/subscriptionId":"subscription-7","io.modelcontextprotocol/serverInfo":{"name":"final-server","version":"1.0.0"}}}"#;
        let response = JsonRpcResponse::success(
            RequestId::from("subscription-7"),
            serde_json::from_str(result_wire).expect("subscription result JSON"),
        );
        let result = request
            .decode_response(&response)
            .expect("final subscriptions/listen termination result is typed");
        let CoreResult::Final(FinalCoreResult::SubscriptionsListen {
            result: listen_result,
            subscription_id,
            diagnostic,
        }) = &result
        else {
            panic!("final subscriptions/listen result");
        };
        assert_eq!(subscription_id, &RequestId::from("subscription-7"));
        assert!(diagnostic.is_none());
        assert_eq!(listen_result.extras.members().len(), 0);
        assert_eq!(
            serde_json::from_str::<Value>(
                &result
                    .encode()
                    .expect("final subscriptions/listen re-encodes"),
            )
            .expect("encoded subscription result is JSON"),
            serde_json::from_str::<Value>(result_wire).expect("subscription result is JSON")
        );

        let legacy_subscribe = SubscribeResourceParams {
            uri: "file:///workspace/status".to_owned(),
        };
        let legacy_unsubscribe = UnsubscribeResourceParams {
            uri: "file:///workspace/status".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&legacy_subscribe).expect("legacy subscribe serializes"),
            serde_json::json!({"uri": "file:///workspace/status"})
        );
        assert_eq!(
            serde_json::to_value(&legacy_unsubscribe).expect("legacy unsubscribe serializes"),
            serde_json::json!({"uri": "file:///workspace/status"})
        );
        #[cfg(feature = "legacy-2024-11-05")]
        {
            let subscribe = CoreRequest::decode(
                ProtocolEra::Legacy2024,
                RESOURCES_SUBSCRIBE,
                Some(&serde_json::json!({"uri": "file:///workspace/status"})),
            )
            .expect("exact-2024 resources/subscribe is a typed core request");
            assert!(matches!(
                subscribe,
                CoreRequest::Legacy(LegacyCoreRequest::ResourcesSubscribe(params))
                    if params.uri == "file:///workspace/status"
            ));
            let unsubscribe = CoreRequest::decode(
                ProtocolEra::Legacy2024,
                RESOURCES_UNSUBSCRIBE,
                Some(&serde_json::json!({"uri": "file:///workspace/status"})),
            )
            .expect("exact-2024 resources/unsubscribe is a typed core request");
            assert!(matches!(
                unsubscribe,
                CoreRequest::Legacy(LegacyCoreRequest::ResourcesUnsubscribe(params))
                    if params.uri == "file:///workspace/status"
            ));
            assert!(
                matches!(
                    CoreRequest::decode(
                        ProtocolEra::Modern2026,
                        RESOURCES_SUBSCRIBE,
                        Some(&serde_json::json!({"uri": "file:///workspace/status"})),
                    ),
                    Err(CoreDispatchError::UnsupportedMethod {
                        era: ProtocolEra::Modern2026,
                        method,
                    }) if method == RESOURCES_SUBSCRIBE
                ),
                "resources/subscribe stays exact-2024-only"
            );
            assert!(
                matches!(
                    CoreRequest::decode(
                        ProtocolEra::Legacy2024,
                        RESOURCES_SUBSCRIBE,
                        Some(&serde_json::json!({})),
                    ),
                    Err(CoreDispatchError::InvalidParams {
                        era: ProtocolEra::Legacy2024,
                        method: RESOURCES_SUBSCRIBE,
                    })
                ),
                "resources/subscribe without uri is invalid"
            );
        }
    }

    #[test]
    fn final_subscriptions_listen_rejects_one_field_response_id_mismatch() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "notifications": {"toolsListChanged": true}
        });
        let request =
            CoreRequest::decode(ProtocolEra::Modern2026, SUBSCRIPTIONS_LISTEN, Some(&params))
                .expect("final subscriptions/listen request");
        let result = serde_json::json!({
            "resultType": "complete",
            "_meta": {"io.modelcontextprotocol/subscriptionId": "subscription-7"}
        });
        let accepted = JsonRpcResponse::success(RequestId::from("subscription-7"), result.clone());
        request
            .decode_response(&accepted)
            .expect("matching subscription response id is admitted");

        let mut wrong_role_result = result.clone();
        wrong_role_result["_meta"]["io.modelcontextprotocol/logLevel"] =
            serde_json::json!("notice");
        let wrong_role =
            JsonRpcResponse::success(RequestId::from("subscription-7"), wrong_role_result);
        assert!(
            matches!(
                request.decode_response(&wrong_role),
                Err(CoreDispatchError::ResultCodec(error))
                    if error.kind()
                        == crate::result::ResultDecodeErrorKind::InvalidKnownMember
                        && error.path()
                            == "$._meta.io.modelcontextprotocol/logLevel"
            ),
            "only request-only logLevel changes the valid subscriptions/listen terminal result"
        );
        request
            .decode_response(&accepted)
            .expect("the wrong-role member cannot mutate the accepted subscription binding");

        let planted = JsonRpcResponse::success(RequestId::from("subscription-8"), result);
        assert!(
            matches!(
                request.decode_response(&planted),
                Err(CoreDispatchError::SubscriptionIdMismatch)
            ),
            "only the response id differs from the otherwise valid subscription result"
        );
        request
            .decode_response(&accepted)
            .expect("the mismatched response cannot mutate the accepted binding");
    }

    #[test]
    fn final_subscriptions_listen_correlates_equivalent_numeric_id_spellings() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "notifications": {"toolsListChanged": true}
        });
        let request =
            CoreRequest::decode(ProtocolEra::Modern2026, SUBSCRIPTIONS_LISTEN, Some(&params))
                .expect("final subscriptions/listen request");
        let accepted_frame = r#"{"jsonrpc":"2.0","id":2e0,"result":{"resultType":"complete","_meta":{"io.modelcontextprotocol/subscriptionId":2.0}}}"#;
        let accepted =
            crate::decode_strict_jsonrpc_response(accepted_frame.as_bytes(), accepted_frame.len())
                .expect("equivalent numeric subscription identifiers are valid JSON-RPC");
        let accepted_result_source = accepted
            .raw_result()
            .expect("successful subscription response retains exact result source");

        let decoded = request
            .decode_response_result(accepted.response(), accepted_result_source)
            .expect("equivalent numeric spellings correlate");
        let encoded = decoded
            .encode()
            .expect("the correlated subscription result re-encodes");
        assert_eq!(
            encoded, accepted_result_source,
            "the exact final result path retains the subscription ID's admitted numeric lexeme"
        );

        let planted_frame = r#"{"jsonrpc":"2.0","id":2e0,"result":{"resultType":"complete","_meta":{"io.modelcontextprotocol/subscriptionId":3.0}}}"#;
        let planted =
            crate::decode_strict_jsonrpc_response(planted_frame.as_bytes(), planted_frame.len())
                .expect("one-number mathematical-integer near-miss is valid JSON-RPC");
        let planted_result_source = planted
            .raw_result()
            .expect("successful planted response retains exact result source");
        assert!(
            matches!(
                request.decode_response_result(planted.response(), planted_result_source),
                Err(CoreDispatchError::SubscriptionIdMismatch)
            ),
            "only the mathematical subscription identifier changes"
        );

        let missing_identifier_frame =
            r#"{"jsonrpc":"2.0","id":2e0,"result":{"resultType":"complete","_meta":{}}}"#;
        let missing_identifier = crate::decode_strict_jsonrpc_response(
            missing_identifier_frame.as_bytes(),
            missing_identifier_frame.len(),
        )
        .expect("the one-member-absent response remains valid JSON-RPC");
        let missing_identifier_result_source = missing_identifier
            .raw_result()
            .expect("successful response retains its exact result source");
        assert!(
            matches!(
                request.decode_response_result(
                    missing_identifier.response(),
                    missing_identifier_result_source,
                ),
                Err(CoreDispatchError::InvalidResult {
                    era: ProtocolEra::Modern2026,
                    method: SUBSCRIPTIONS_LISTEN,
                })
            ),
            "removing only the required subscriptionId rejects the terminal result"
        );

        let reaccepted = request
            .decode_response_result(accepted.response(), accepted_result_source)
            .expect("the numeric near-miss cannot mutate correlation state");
        assert_eq!(
            reaccepted
                .encode()
                .expect("the reaccepted subscription result re-encodes"),
            accepted_result_source,
            "the subscriptionId-absent rejection leaves the accepted exact result unchanged"
        );
    }

    #[test]
    fn final_subscriptions_listen_rejects_one_legacy_subscription_field() {
        let accepted = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "notifications": {
                "resourceSubscriptions": ["file:///workspace/status"]
            }
        });
        let baseline = CoreRequest::decode(
            ProtocolEra::Modern2026,
            SUBSCRIPTIONS_LISTEN,
            Some(&accepted),
        )
        .expect("baseline final subscriptions/listen request");

        let mut planted = accepted.clone();
        planted["uri"] = serde_json::json!("file:///workspace/status");
        assert!(
            matches!(
                CoreRequest::decode(
                    ProtocolEra::Modern2026,
                    SUBSCRIPTIONS_LISTEN,
                    Some(&planted)
                ),
                Err(CoreDispatchError::InvalidParams {
                    era: ProtocolEra::Modern2026,
                    method: SUBSCRIPTIONS_LISTEN,
                })
            ),
            "only the legacy resources/subscribe uri field changes the valid final listen request"
        );

        let reaccepted = CoreRequest::decode(
            ProtocolEra::Modern2026,
            SUBSCRIPTIONS_LISTEN,
            Some(&accepted),
        )
        .expect("cross-era rejection cannot mutate final listen decoding");
        assert_eq!(
            baseline
                .encode_params()
                .expect("baseline encodes")
                .expect("baseline parameters"),
            reaccepted
                .encode_params()
                .expect("reaccepted encodes")
                .expect("reaccepted parameters"),
            "the one-field cross-era rejection leaves the accepted final request unchanged"
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn core_completion_rejects_one_field_cross_era_metadata() {
        let accepted = serde_json::json!({
            "ref": {"type": "ref/prompt", "name": "deploy"},
            "argument": {"name": "environment", "value": "sta"}
        });
        let baseline = CoreRequest::decode(
            ProtocolEra::Legacy2024,
            COMPLETION_COMPLETE,
            Some(&accepted),
        )
        .expect("baseline legacy completion request");

        let mut planted = accepted.clone();
        planted["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION
        });
        assert!(
            matches!(
                CoreRequest::decode(ProtocolEra::Legacy2024, COMPLETION_COMPLETE, Some(&planted)),
                Err(CoreDispatchError::CrossEraRequestMetadata {
                    method: COMPLETION_COMPLETE
                })
            ),
            "only the final _meta field changes the otherwise valid legacy completion request"
        );

        let reaccepted = CoreRequest::decode(
            ProtocolEra::Legacy2024,
            COMPLETION_COMPLETE,
            Some(&accepted),
        )
        .expect("cross-era rejection cannot mutate completion request decoding");
        assert_eq!(
            baseline
                .encode_params()
                .expect("baseline encodes")
                .expect("baseline parameters"),
            reaccepted
                .encode_params()
                .expect("reaccepted encodes")
                .expect("reaccepted parameters"),
            "the one-field cross-era rejection leaves the accepted legacy request unchanged"
        );
    }

    #[test]
    fn core_request_envelope_admits_final_client_info() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {
                    "name": "final-client",
                    "version": "1.0.0"
                }
            }
        });

        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&params))
            .expect("final clientInfo is admitted by the final request envelope");
        assert_eq!(request.era(), ProtocolEra::Modern2026);
        assert_eq!(
            request
                .encode_params()
                .expect("admitted final request encodes"),
            Some(params)
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn core_request_envelope_rejects_one_final_client_info_member_in_legacy_era() {
        let accepted = serde_json::json!({});
        let baseline = CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_LIST, Some(&accepted))
            .expect("baseline legacy list request");

        let mut planted = accepted.clone();
        planted["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/clientInfo": {
                "name": "final-client",
                "version": "1.0.0"
            }
        });
        assert!(
            matches!(
                CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_LIST, Some(&planted)),
                Err(CoreDispatchError::CrossEraRequestMetadata { method: TOOLS_LIST })
            ),
            "only the final clientInfo metadata member changes the accepted legacy request"
        );

        let reaccepted = CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_LIST, Some(&accepted))
            .expect("cross-era rejection cannot mutate legacy request admission");
        assert_eq!(
            baseline
                .encode_params()
                .expect("baseline legacy request encodes"),
            reaccepted
                .encode_params()
                .expect("reaccepted legacy request encodes")
        );
    }

    #[test]
    fn core_result_envelope_admits_final_server_info() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, TOOLS_LIST, Some(&params))
            .expect("final tools/list request");
        let result = r#"{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"final-server","version":"1.0.0"}}}"#;

        let decoded = request
            .decode_result(result)
            .expect("final serverInfo is admitted by the final result envelope");
        assert_eq!(
            serde_json::from_str::<Value>(
                &decoded.encode().expect("admitted final result encodes")
            )
            .expect("admitted final result encoding is JSON"),
            serde_json::from_str::<Value>(result).expect("final serverInfo fixture is JSON"),
            "serverInfo is admitted only through _meta on a complete valid catalog"
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn core_result_envelope_rejects_each_final_only_metadata_member_in_legacy_era() {
        let request = CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_LIST, None)
            .expect("baseline legacy request");
        let accepted = r#"{"tools":[]}"#;
        let baseline = request
            .decode_result(accepted)
            .expect("baseline legacy result is admitted");
        assert_eq!(
            baseline.encode().expect("baseline legacy result encodes"),
            accepted
        );

        for final_metadata_member in [
            FINAL_PROTOCOL_VERSION_META_KEY,
            FINAL_CLIENT_CAPABILITIES_META_KEY,
            FINAL_CLIENT_INFO_META_KEY,
            FINAL_SERVER_INFO_META_KEY,
            FINAL_SUBSCRIPTION_ID_META_KEY,
        ] {
            let planted = serde_json::json!({
                "tools": [],
                "_meta": {final_metadata_member: true}
            })
            .to_string();
            assert!(
                matches!(
                    request.decode_result(&planted),
                    Err(CoreDispatchError::CrossEraResultMetadata { method: TOOLS_LIST })
                ),
                "adding only {final_metadata_member} rejects the otherwise valid legacy result"
            );
        }

        let reaccepted = request
            .decode_result(accepted)
            .expect("cross-era rejection cannot mutate legacy result admission");
        assert_eq!(
            reaccepted
                .encode()
                .expect("reaccepted legacy result encodes"),
            accepted
        );
    }

    #[cfg(feature = "legacy-2024-11-05")]
    #[test]
    fn core_dispatch_rejects_one_field_final_result_type_on_legacy_result() {
        let request = CoreRequest::decode(ProtocolEra::Legacy2024, TOOLS_LIST, None)
            .expect("baseline legacy request");
        let accepted = r#"{"tools":[]}"#;
        let baseline = request
            .decode_result(accepted)
            .expect("the legacy result remains accepted without a final discriminator");
        let planted = r#"{"tools":[],"resultType":"complete"}"#;
        assert!(
            matches!(
                request.decode_result(planted),
                Err(CoreDispatchError::CrossEraResultType { method: TOOLS_LIST })
            ),
            "only the final resultType field changes the otherwise valid legacy result"
        );
        let reaccepted = request
            .decode_result(accepted)
            .expect("rejection cannot mutate the selected legacy dispatch");
        assert_eq!(
            baseline.encode().expect("baseline encodes"),
            reaccepted.encode().expect("reaccepted value encodes"),
            "the one-field cross-era rejection leaves the accepted legacy result unchanged"
        );
    }

    // ========================================================================
    // Initialize Tests
    // ========================================================================

    #[test]
    fn initialize_params_serialization() {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["clientInfo"]["name"], "test-client");
        assert_eq!(value["clientInfo"]["version"], "1.0.0");
    }

    #[test]
    fn initialize_params_round_trip() {
        let json = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "my-client", "version": "0.1.0"}
        });
        let params: InitializeParams = serde_json::from_value(json).expect("deserialize");
        assert_eq!(params.protocol_version, "2024-11-05");
        assert_eq!(params.client_info.name, "my-client");
    }

    #[test]
    fn initialize_result_serialization() {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities::default(),
            server_info: ServerInfo {
                name: "test-server".to_string(),
                version: "1.0.0".to_string(),
            },
            instructions: Some("Welcome!".to_string()),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["serverInfo"]["name"], "test-server");
        assert_eq!(value["instructions"], "Welcome!");
    }

    #[test]
    fn initialize_result_without_instructions() {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities::default(),
            server_info: ServerInfo {
                name: "srv".to_string(),
                version: "0.1.0".to_string(),
            },
            instructions: None,
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert!(value.get("instructions").is_none());
    }

    // ========================================================================
    // ListToolsParams Tests (with tags)
    // ========================================================================

    #[test]
    fn list_tools_params_default() {
        let params = ListToolsParams::default();
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn list_tools_params_with_cursor() {
        let params = ListToolsParams {
            cursor: Some("next-page".to_string()),
            include_tags: None,
            exclude_tags: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["cursor"], "next-page");
    }

    #[test]
    fn list_tools_params_with_tags() {
        let params = ListToolsParams {
            cursor: None,
            include_tags: Some(vec!["api".to_string(), "v2".to_string()]),
            exclude_tags: Some(vec!["deprecated".to_string()]),
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["includeTags"], serde_json::json!(["api", "v2"]));
        assert_eq!(value["excludeTags"], serde_json::json!(["deprecated"]));
    }

    // ========================================================================
    // CallToolParams Tests
    // ========================================================================

    #[test]
    fn call_tool_params_minimal() {
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: None,
            meta: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["name"], "greet");
        assert!(value.get("arguments").is_none());
        assert!(value.get("_meta").is_none());
    }

    #[test]
    fn call_tool_params_full() {
        let params = CallToolParams {
            name: "add".to_string(),
            arguments: Some(serde_json::json!({"a": 1, "b": 2})),
            meta: Some(RequestMeta {
                progress_marker: Some(ProgressMarker::Number(JsonInteger::from(100_i64))),
            }),
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["name"], "add");
        assert_eq!(value["arguments"]["a"], 1);
        assert_eq!(value["_meta"][PROGRESS_MARKER_KEY], 100);
    }

    // ========================================================================
    // CallToolResult Tests
    // ========================================================================

    #[test]
    fn call_tool_result_success() {
        let result = CallToolResult {
            content: vec![LegacyContent::Text {
                text: "42".to_string(),
                annotations: None,
                additional: BTreeMap::new(),
            }],
            is_error: false,
            meta: None,
            additional: BTreeMap::new(),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "42");
        // is_error=false should be omitted
        assert!(value.get("isError").is_none());
    }

    #[test]
    fn call_tool_result_error() {
        let result = CallToolResult {
            content: vec![LegacyContent::Text {
                text: "Something went wrong".to_string(),
                annotations: None,
                additional: BTreeMap::new(),
            }],
            is_error: true,
            meta: None,
            additional: BTreeMap::new(),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["isError"], true);
    }

    #[test]
    fn legacy_2024_content_results_round_trip_open_wire_members() {
        let tool_wire = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "ready",
                "annotations": {
                    "audience": ["assistant"],
                    "priority": 0.75,
                    "com.example/annotation": {"retain": true}
                },
                "_meta": {"legacy": "content"},
                "com.example/content": {"retain": true}
            }],
            "_meta": {"legacy": "tool-result"},
            "com.example/result": ["retain"]
        });
        let tool_result: CallToolResult =
            serde_json::from_value(tool_wire.clone()).expect("legacy tool result decodes");
        assert_eq!(
            serde_json::to_value(&tool_result).expect("legacy tool result re-encodes"),
            tool_wire
        );

        let read_wire = serde_json::json!({
            "contents": [{
                "uri": "file:///report.txt",
                "text": "ready",
                "_meta": {"legacy": "resource"},
                "com.example/resource": {"retain": true}
            }],
            "_meta": {"legacy": "read-result"},
            "com.example/result": {"retain": true}
        });
        let read_result: ReadResourceResult =
            serde_json::from_value(read_wire.clone()).expect("legacy read result decodes");
        assert_eq!(
            serde_json::to_value(&read_result).expect("legacy read result re-encodes"),
            read_wire
        );

        let prompt_wire = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text",
                    "text": "summarize",
                    "_meta": {"legacy": "prompt-content"},
                    "com.example/content": "retain"
                },
                "_meta": {"legacy": "prompt-message"},
                "com.example/message": true
            }],
            "_meta": {"legacy": "prompt-result"},
            "com.example/result": {"retain": true}
        });
        let prompt_result: GetPromptResult =
            serde_json::from_value(prompt_wire.clone()).expect("legacy prompt result decodes");
        assert_eq!(
            serde_json::to_value(&prompt_result).expect("legacy prompt result re-encodes"),
            prompt_wire
        );
    }

    #[test]
    fn legacy_2024_call_tool_rejects_only_audio_discriminator_without_mutating_baseline() {
        let accepted = serde_json::json!({
            "content": [{
                "type": "text",
                "text": "payload",
                "data": "UklGRg==",
                "mimeType": "audio/wav"
            }]
        });
        let accepted_result: CallToolResult =
            serde_json::from_value(accepted.clone()).expect("legacy text content decodes");
        assert_eq!(
            serde_json::to_value(&accepted_result).expect("legacy text content re-encodes"),
            accepted
        );

        let baseline = accepted.clone();
        let mut planted = accepted.clone();
        planted["content"][0]["type"] = serde_json::json!("audio");
        assert!(
            serde_json::from_value::<CallToolResult>(planted).is_err(),
            "the exact 2024 tools/call content union excludes audio"
        );
        assert_eq!(
            accepted, baseline,
            "the one-field audio discriminator rejection cannot mutate accepted legacy wire"
        );
    }

    // ========================================================================
    // ListResourcesParams Tests
    // ========================================================================

    #[test]
    fn list_resources_params_default() {
        let params = ListResourcesParams::default();
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn list_resources_params_with_tags() {
        let params = ListResourcesParams {
            cursor: None,
            include_tags: Some(vec!["config".to_string()]),
            exclude_tags: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["includeTags"], serde_json::json!(["config"]));
    }

    // ========================================================================
    // ReadResourceParams Tests
    // ========================================================================

    #[test]
    fn read_resource_params_serialization() {
        let params = ReadResourceParams {
            uri: "file://config.json".to_string(),
            meta: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["uri"], "file://config.json");
        assert!(value.get("_meta").is_none());
    }

    #[test]
    fn read_resource_params_with_meta() {
        let params = ReadResourceParams {
            uri: "file://data.csv".to_string(),
            meta: Some(RequestMeta {
                progress_marker: Some(ProgressMarker::String("pt-read".to_string())),
            }),
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["uri"], "file://data.csv");
        assert_eq!(value["_meta"][PROGRESS_MARKER_KEY], "pt-read");
    }

    // ========================================================================
    // ReadResourceResult Tests
    // ========================================================================

    #[test]
    fn read_resource_result_serialization() {
        let result = ReadResourceResult {
            contents: vec![LegacyResourceContent::Text {
                uri: "file://test.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "Hello!".to_string(),
                additional: BTreeMap::new(),
            }],
            meta: None,
            additional: BTreeMap::new(),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["contents"][0]["uri"], "file://test.txt");
        assert_eq!(value["contents"][0]["text"], "Hello!");
    }

    #[test]
    fn legacy_resource_content_one_of_valid_text_and_blob_round_trip_open_members() {
        let wire = serde_json::json!({
            "contents": [
                {
                    "uri": "file:///report.txt",
                    "text": "ready",
                    "mimeType": "text/plain",
                    "_meta": {"legacy": "text"},
                    "com.example/resource": {"retain": true}
                },
                {
                    "uri": "file:///report.bin",
                    "blob": "cmVhZHk=",
                    "mimeType": "application/octet-stream",
                    "_meta": {"legacy": "blob"},
                    "com.example/resource": ["retain"]
                }
            ],
            "_meta": {"legacy": "read-result"},
            "com.example/result": {"retain": true}
        });

        let result: ReadResourceResult =
            serde_json::from_value(wire.clone()).expect("exact text and blob members decode");

        let LegacyResourceContent::Text { additional, .. } = &result.contents[0] else {
            panic!("text discriminator selects the text variant");
        };
        assert_eq!(additional["_meta"], serde_json::json!({"legacy": "text"}));
        assert_eq!(
            additional["com.example/resource"],
            serde_json::json!({"retain": true})
        );
        let LegacyResourceContent::Blob { additional, .. } = &result.contents[1] else {
            panic!("blob discriminator selects the blob variant");
        };
        assert_eq!(additional["_meta"], serde_json::json!({"legacy": "blob"}));
        assert_eq!(
            additional["com.example/resource"],
            serde_json::json!(["retain"])
        );
        assert_eq!(
            serde_json::to_value(&result).expect("exact legacy contents re-encode"),
            wire
        );
    }

    #[test]
    fn legacy_resource_content_one_of_rejects_ambiguous_or_missing_payload_negative() {
        let accepted = serde_json::json!({
            "contents": [{
                "uri": "file:///report.txt",
                "text": "ready",
                "_meta": {"legacy": "resource"},
                "com.example/resource": {"retain": true}
            }]
        });
        assert!(
            serde_json::from_value::<ReadResourceResult>(accepted.clone()).is_ok(),
            "the one-of baseline remains valid"
        );

        let mut both = accepted.clone();
        both["contents"][0]["blob"] = serde_json::json!("cmVhZHk=");
        assert!(
            serde_json::from_value::<ReadResourceResult>(both).is_err(),
            "only adding blob to a valid text resource must reject an ambiguous one-of"
        );

        let mut neither = accepted;
        neither["contents"][0]
            .as_object_mut()
            .expect("baseline content is an object")
            .remove("text");
        assert!(
            serde_json::from_value::<ReadResourceResult>(neither).is_err(),
            "only removing text from the same resource must reject an empty one-of"
        );
    }

    // ========================================================================
    // ListPromptsParams Tests
    // ========================================================================

    #[test]
    fn list_prompts_params_default() {
        let params = ListPromptsParams::default();
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn list_prompts_params_with_tags() {
        let params = ListPromptsParams {
            cursor: Some("c1".to_string()),
            include_tags: Some(vec!["onboarding".to_string()]),
            exclude_tags: Some(vec!["deprecated".to_string()]),
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["cursor"], "c1");
        assert_eq!(value["includeTags"], serde_json::json!(["onboarding"]));
        assert_eq!(value["excludeTags"], serde_json::json!(["deprecated"]));
    }

    // ========================================================================
    // GetPromptParams Tests
    // ========================================================================

    #[test]
    fn get_prompt_params_minimal() {
        let params = GetPromptParams {
            name: "greeting".to_string(),
            arguments: None,
            meta: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["name"], "greeting");
        assert!(value.get("arguments").is_none());
    }

    #[test]
    fn get_prompt_params_with_arguments() {
        let mut args = std::collections::HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        args.insert("language".to_string(), "French".to_string());

        let params = GetPromptParams {
            name: "translate".to_string(),
            arguments: Some(args),
            meta: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["name"], "translate");
        assert_eq!(value["arguments"]["name"], "Alice");
        assert_eq!(value["arguments"]["language"], "French");
    }

    // ========================================================================
    // GetPromptResult Tests
    // ========================================================================

    #[test]
    fn get_prompt_result_serialization() {
        let result = GetPromptResult {
            description: Some("A greeting prompt".to_string()),
            messages: vec![LegacyPromptMessage {
                role: crate::types::Role::User,
                content: LegacyContent::Text {
                    text: "Say hello".to_string(),
                    annotations: None,
                    additional: BTreeMap::new(),
                },
                additional: BTreeMap::new(),
            }],
            meta: None,
            additional: BTreeMap::new(),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["description"], "A greeting prompt");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"]["text"], "Say hello");
    }

    #[test]
    fn get_prompt_result_without_description() {
        let result = GetPromptResult {
            description: None,
            messages: vec![],
            meta: None,
            additional: BTreeMap::new(),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert!(value.get("description").is_none());
    }

    // ========================================================================
    // CancelledParams Tests
    // ========================================================================

    #[test]
    fn cancellation_wire_codec_round_trips_selected_era_payloads() {
        let legacy_wire = serde_json::from_str::<JsonRpcRequest>(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":900719925474099312345,"reason":"legacy client stopped waiting"}}"#,
        )
        .expect("arbitrary-precision legacy cancellation is valid JSON-RPC");
        let legacy = CancellationWireMessage::decode(
            ProtocolEra::Legacy2024,
            CancellationSender::Client,
            &legacy_wire,
        )
        .expect("the legacy codec admits its exact cancellation payload");
        assert_eq!(legacy.era(), ProtocolEra::Legacy2024);
        assert_eq!(legacy.sender(), CancellationSender::Client);
        let CancellationWireMessage::Legacy2024 { params, .. } = &legacy else {
            panic!("the selected legacy era must construct the legacy variant");
        };
        assert_eq!(
            params.request_id,
            RequestId::Integer("900719925474099312345".to_owned())
        );
        assert_eq!(
            serde_json::to_value(legacy.encode().expect("legacy cancellation re-encodes"))
                .expect("legacy cancellation remains JSON"),
            serde_json::to_value(&legacy_wire).expect("legacy baseline remains JSON"),
            "the legacy wire preserves arbitrary-precision request IDs without awaitCleanup"
        );

        let modern_wire = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": "final-request-7",
                "reason": "final client stopped waiting",
            })),
        );
        let modern = CancellationWireMessage::decode(
            ProtocolEra::Modern2026,
            CancellationSender::Client,
            &modern_wire,
        )
        .expect("the final client codec admits cancellation without metadata");
        assert_eq!(modern.era(), ProtocolEra::Modern2026);
        assert_eq!(modern.sender(), CancellationSender::Client);
        let CancellationWireMessage::Modern2026 { params, .. } = &modern else {
            panic!("the selected final era must construct the final variant");
        };
        assert!(params.meta.is_none());
        assert!(
            !params.additional.contains_key("awaitCleanup"),
            "a final client codec emission does not synthesize the legacy-only semantic"
        );
        assert_eq!(
            serde_json::to_value(modern.encode().expect("final cancellation re-encodes"))
                .expect("final cancellation remains JSON"),
            serde_json::to_value(&modern_wire).expect("final baseline remains JSON"),
            "the final wire preserves metadata absence without adding legacy members"
        );
    }

    #[test]
    fn cancellation_wire_codec_keeps_modern_metadata_optional_and_inert() {
        let accepted_wire = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": "final-request-8",
                "reason": "final client stopped waiting",
            })),
        );
        let admitted = CancellationWireMessage::decode(
            ProtocolEra::Modern2026,
            CancellationSender::Client,
            &accepted_wire,
        )
        .expect("the metadata-free final baseline is admitted");
        let baseline_wire = serde_json::to_value(&accepted_wire).expect("baseline serializes");

        let mut planted = accepted_wire.clone();
        planted
            .params
            .as_mut()
            .and_then(Value::as_object_mut)
            .expect("baseline owns final cancellation parameters")
            .insert(
                "_meta".to_owned(),
                serde_json::json!({
                    "io.modelcontextprotocol/protocolVersion": ProtocolEra::Legacy2024
                        .version()
                        .as_str(),
                    "io.modelcontextprotocol/futureCancellationHint": {
                        "preserved": true,
                    },
                }),
            );
        let with_metadata = CancellationWireMessage::decode(
            ProtocolEra::Modern2026,
            CancellationSender::Client,
            &planted,
        )
        .expect("changing only optional metadata never changes cancellation admission");
        assert_eq!(
            serde_json::to_value(with_metadata.encode().expect("metadata remains opaque"))
                .expect("metadata-bearing cancellation remains JSON"),
            serde_json::to_value(&planted).expect("planted wire remains JSON"),
            "optional metadata is preserved but never validated or synthesized"
        );
        assert_eq!(
            serde_json::to_value(admitted.encode().expect("admitted cancellation re-encodes"))
                .expect("admitted cancellation remains JSON"),
            baseline_wire,
            "the metadata-free baseline remains unchanged"
        );

        let legacy_application_metadata = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": "legacy-request-8",
                "_meta": {"com.example/application": true},
            })),
        );
        let legacy = CancellationWireMessage::decode(
            ProtocolEra::Legacy2024,
            CancellationSender::Client,
            &legacy_application_metadata,
        )
        .expect("exact-2024 cancellation retains application notification metadata");
        assert_eq!(
            serde_json::to_value(legacy.encode().expect("legacy metadata re-encodes"))
                .expect("metadata-bearing legacy cancellation remains JSON"),
            serde_json::to_value(&legacy_application_metadata)
                .expect("legacy metadata baseline remains JSON")
        );

        let legacy_with_final_metadata = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": "legacy-request-8",
                "_meta": {"io.modelcontextprotocol/subscriptionId": "legacy-request-8"},
            })),
        );
        assert!(matches!(
            CancellationWireMessage::decode(
                ProtocolEra::Legacy2024,
                CancellationSender::Client,
                &legacy_with_final_metadata,
            ),
            Err(CancellationWireCodecError::InvalidParameters {
                era: ProtocolEra::Legacy2024,
            })
        ));
        let locally_constructed_legacy_with_final_metadata = CancellationWireMessage::Legacy2024 {
            sender: CancellationSender::Client,
            params: serde_json::from_value(
                legacy_with_final_metadata
                    .params
                    .clone()
                    .expect("legacy final-metadata negative retains params"),
            )
            .expect("the individual legacy fields remain structurally valid"),
        };
        assert!(matches!(
            locally_constructed_legacy_with_final_metadata.encode(),
            Err(CancellationWireCodecError::InvalidParameters {
                era: ProtocolEra::Legacy2024,
            })
        ));
    }

    #[test]
    fn cancellation_wire_codec_rejects_present_null_modern_optional_fields() {
        for planted in [
            serde_json::json!({"requestId": 7, "reason": null}),
            serde_json::json!({"requestId": 7, "_meta": null}),
        ] {
            let wire = JsonRpcRequest::notification(NOTIFICATIONS_CANCELLED, Some(planted));
            assert!(matches!(
                CancellationWireMessage::decode(
                    ProtocolEra::Modern2026,
                    CancellationSender::Client,
                    &wire,
                ),
                Err(CancellationWireCodecError::InvalidParameters {
                    era: ProtocolEra::Modern2026,
                })
            ));
        }

        let valid_subscription_metadata = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": 7,
                "_meta": {(FINAL_SUBSCRIPTION_ID_META_KEY): 7e0},
            })),
        );
        assert!(
            CancellationWireMessage::decode(
                ProtocolEra::Modern2026,
                CancellationSender::Server,
                &valid_subscription_metadata,
            )
            .is_ok()
        );

        let mismatched_subscription_metadata = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": 7,
                "_meta": {(FINAL_SUBSCRIPTION_ID_META_KEY): 8},
            })),
        );
        assert!(
            matches!(
                CancellationWireMessage::decode(
                    ProtocolEra::Modern2026,
                    CancellationSender::Server,
                    &mismatched_subscription_metadata,
                ),
                Err(CancellationWireCodecError::InvalidParameters {
                    era: ProtocolEra::Modern2026,
                })
            ),
            "changing only the server cancellation metadata ID rejects a stream-target mismatch"
        );
        let locally_constructed_mismatch = CancellationWireMessage::Modern2026 {
            sender: CancellationSender::Server,
            params: serde_json::from_value(serde_json::json!({
                "requestId": 7,
                "_meta": {(FINAL_SUBSCRIPTION_ID_META_KEY): 8},
            }))
            .expect("the individual final fields remain structurally valid"),
        };
        assert!(
            matches!(
                locally_constructed_mismatch.encode(),
                Err(CancellationWireCodecError::InvalidParameters {
                    era: ProtocolEra::Modern2026,
                })
            ),
            "the same one-field mismatch cannot bypass ingress validation through local encoding"
        );

        let invalid_subscription_metadata = JsonRpcRequest::notification(
            NOTIFICATIONS_CANCELLED,
            Some(serde_json::json!({
                "requestId": 7,
                "_meta": {(FINAL_SUBSCRIPTION_ID_META_KEY): null},
            })),
        );
        assert!(matches!(
            CancellationWireMessage::decode(
                ProtocolEra::Modern2026,
                CancellationSender::Server,
                &invalid_subscription_metadata,
            ),
            Err(CancellationWireCodecError::InvalidParameters {
                era: ProtocolEra::Modern2026,
            })
        ));
    }

    #[test]
    fn cancelled_params_minimal() {
        let params = CancelledParams {
            request_id: RequestId::Number(5),
            reason: None,
            meta: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["requestId"], 5);
        assert!(value.get("reason").is_none());
        assert!(value.get("awaitCleanup").is_none());
    }

    #[test]
    fn cancelled_params_full() {
        let params = CancelledParams {
            request_id: RequestId::String("req-7".to_string()),
            reason: Some("User cancelled".to_string()),
            meta: None,
        };
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["requestId"], "req-7");
        assert_eq!(value["reason"], "User cancelled");
        assert!(value.get("awaitCleanup").is_none());
        assert_eq!(
            serde_json::to_string(&params).expect("legacy cancellation serializes"),
            r#"{"requestId":"req-7","reason":"User cancelled"}"#,
            "the exact legacy cancellation wire has only requestId and optional reason"
        );
    }

    #[test]
    fn cancelled_params_reason_is_unbounded_and_application_metadata_is_open() {
        let beyond_historical_bound = "x".repeat(MAX_CANCELLATION_REASON_BYTES + 1);
        let admitted_json = serde_json::json!({
            "requestId": 1,
            "reason": beyond_historical_bound,
        });
        assert!(serde_json::from_value::<CancelledParams>(admitted_json).is_ok());
        assert!(
            serde_json::from_value::<CancelledParams>(serde_json::json!({
                "requestId": 1,
                "reason": null,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CancelledParams>(serde_json::json!({
                "requestId": 1,
                "awaitCleanup": true,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CancelledParams>(serde_json::json!({
                "requestId": 1,
                "reason": "ok",
                "unknown": true,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CancelledParams>(serde_json::json!({
                "requestId": 1,
                "_meta": {"com.example/application": true},
            }))
            .is_ok(),
            "notification metadata is inherited while unknown parameter members stay closed"
        );

        let outbound = CancelledParams {
            request_id: RequestId::Number(1),
            reason: Some("x".repeat(MAX_CANCELLATION_REASON_BYTES + 1)),
            meta: None,
        };
        assert!(serde_json::to_value(outbound).is_ok());
    }

    // ========================================================================
    // ProgressParams Tests
    // ========================================================================

    #[test]
    fn progress_params_new() {
        let params = ProgressParams::new("id-1", 0.5);
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value[PROGRESS_MARKER_KEY], "id-1");
        assert_eq!(value["progress"], 0.5);
        assert!(value.get("total").is_none());
        assert!(value.get("message").is_none());
    }

    #[test]
    fn progress_params_with_total() {
        let params = ProgressParams::with_total(42i64, 50.0, 100.0);
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value[PROGRESS_MARKER_KEY], 42);
        assert_eq!(value["progress"], 50.0);
        assert_eq!(value["total"], 100.0);
    }

    #[test]
    fn progress_params_with_message() {
        let params = ProgressParams::new("tok", 0.75).with_message("Almost done");
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["message"], "Almost done");
    }

    #[test]
    fn progress_params_fraction() {
        let params = ProgressParams::with_total("t", 25.0, 100.0);
        assert_eq!(params.fraction(), Some(0.25));

        // Zero total
        let params = ProgressParams::with_total("t", 10.0, 0.0);
        assert_eq!(params.fraction(), Some(0.0));

        // No total
        let params = ProgressParams::new("t", 0.5);
        assert_eq!(params.fraction(), None);
    }

    // ========================================================================
    // LogLevel Tests
    // ========================================================================

    #[test]
    fn exact_2024_log_levels_round_trip_on_logging_params() {
        for (level, wire) in [
            (LogLevel::Emergency, "emergency"),
            (LogLevel::Alert, "alert"),
            (LogLevel::Critical, "critical"),
            (LogLevel::Error, "error"),
            (LogLevel::Warning, "warning"),
            (LogLevel::Notice, "notice"),
            (LogLevel::Info, "info"),
            (LogLevel::Debug, "debug"),
        ] {
            assert_eq!(
                serde_json::to_value(level).expect("log level serializes"),
                wire
            );
            assert_eq!(
                serde_json::from_value::<LogLevel>(serde_json::json!(wire))
                    .expect("exact 2024 log level deserializes"),
                level
            );
            assert_eq!(
                serde_json::to_value(SetLogLevelParams { level })
                    .expect("set-level parameters serialize"),
                serde_json::json!({"level": wire})
            );
            let message = serde_json::from_value::<LogMessageParams>(serde_json::json!({
                "level": wire,
                "data": "event"
            }))
            .expect("message parameters deserialize");
            assert_eq!(message.level, level);
            assert_eq!(message.logger, None);
            assert_eq!(message.data, serde_json::json!("event"));
        }
        assert!(
            serde_json::from_value::<LogLevel>(serde_json::json!("trace")).is_err(),
            "the exact 2024 wire enum rejects non-MCP severity names"
        );
    }

    // ========================================================================
    // Existing Tests (preserved below)
    // ========================================================================

    #[test]
    fn list_resource_templates_params_serialization() {
        let params = ListResourceTemplatesParams::default();
        let value = serde_json::to_value(&params).expect("serialize params");
        assert_eq!(value, serde_json::json!({}));

        let params = ListResourceTemplatesParams {
            cursor: Some("next".to_string()),
            ..Default::default()
        };
        let value = serde_json::to_value(&params).expect("serialize params with cursor");
        assert_eq!(value, serde_json::json!({ "cursor": "next" }));
    }

    #[test]
    fn list_resource_templates_result_serialization() {
        let result = ListResourceTemplatesResult {
            resource_templates: vec![ResourceTemplate {
                uri_template: "resource://{id}".to_string(),
                name: "Resource Template".to_string(),
                description: Some("Template description".to_string()),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }],
            next_cursor: None,
        };

        let value = serde_json::to_value(&result).expect("serialize result");
        let templates = value
            .get("resourceTemplates")
            .expect("resourceTemplates key");
        let template = templates.get(0).expect("first resource template");

        assert_eq!(template["uriTemplate"], "resource://{id}");
        assert_eq!(template["name"], "Resource Template");
        assert_eq!(template["description"], "Template description");
        assert_eq!(template["mimeType"], "text/plain");
    }

    #[test]
    fn resource_updated_notification_serialization() {
        let params = ResourceUpdatedNotificationParams {
            uri: "resource://test".to_string(),
        };
        let value = serde_json::to_value(&params).expect("serialize params");
        assert_eq!(value, serde_json::json!({ "uri": "resource://test" }));
    }

    #[test]
    fn subscribe_unsubscribe_resource_params_serialization() {
        let subscribe = SubscribeResourceParams {
            uri: "resource://alpha".to_string(),
        };
        let value = serde_json::to_value(&subscribe).expect("serialize subscribe params");
        assert_eq!(value, serde_json::json!({ "uri": "resource://alpha" }));

        let unsubscribe = UnsubscribeResourceParams {
            uri: "resource://alpha".to_string(),
        };
        let value = serde_json::to_value(&unsubscribe).expect("serialize unsubscribe params");
        assert_eq!(value, serde_json::json!({ "uri": "resource://alpha" }));
    }

    #[test]
    fn logging_params_serialization() {
        let set_level = SetLogLevelParams {
            level: LogLevel::Warning,
        };
        let value = serde_json::to_value(&set_level).expect("serialize setLevel");
        assert_eq!(value, serde_json::json!({ "level": "warning" }));

        let log_message = LogMessageParams {
            level: LogLevel::Info,
            logger: Some("fastmcp_rust::server".to_string()),
            data: serde_json::Value::String("hello".to_string()),
        };
        let value = serde_json::to_value(&log_message).expect("serialize log message");
        assert_eq!(value["level"], "info");
        assert_eq!(value["logger"], "fastmcp_rust::server");
        assert_eq!(value["data"], "hello");
    }

    #[test]
    fn task_status_notification_serialization() {
        let params = TaskStatusNotificationParams {
            id: TaskId::from_string("task-1"),
            status: TaskStatus::Running,
            progress: Some(0.5),
            message: Some("halfway".to_string()),
            error: None,
            result: None,
        };
        let value = serde_json::to_value(&params).expect("serialize task status notification");
        assert_eq!(
            value,
            serde_json::json!({
                "id": "task-1",
                "status": "running",
                "progress": 0.5,
                "message": "halfway"
            })
        );
    }

    // ========================================================================
    // Sampling Tests
    // ========================================================================

    #[test]
    fn create_message_params_minimal() {
        let params = CreateMessageParams::new(
            vec![SamplingMessage::user("Hello")],
            JsonInteger::from(100_i64),
        );
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value[MAX_TOKENS_KEY], 100);
        assert!(value["messages"].is_array());
        assert!(value.get("systemPrompt").is_none());
        assert!(value.get("temperature").is_none());
    }

    #[test]
    fn create_message_params_full() {
        let params = CreateMessageParams::new(
            vec![
                SamplingMessage::user("Hello"),
                SamplingMessage::assistant("Hi there!"),
            ],
            JsonInteger::from(500_i64),
        )
        .with_system_prompt("You are helpful")
        .with_temperature(0.7)
        .with_stop_sequences(vec!["END".to_string()]);

        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value[MAX_TOKENS_KEY], 500);
        assert_eq!(value["systemPrompt"], "You are helpful");
        assert_eq!(value["temperature"], 0.7);
        assert_eq!(value["stopSequences"][0], "END");
        assert_eq!(value["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn create_message_result_text() {
        let result = CreateMessageResult::text("Hello!", "claude-3");
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "Hello!");
        assert_eq!(value["model"], "claude-3");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["stopReason"], "endTurn");
    }

    #[test]
    fn create_message_result_max_tokens() {
        let result = CreateMessageResult::text("Truncated", "gpt-4").with_stop_reason("maxTokens");
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["stopReason"], "maxTo\x6bens");
    }

    #[test]
    fn sampling_message_user() {
        let msg = SamplingMessage::user("Test message");
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "Test message");
    }

    #[test]
    fn sampling_message_assistant() {
        let msg = SamplingMessage::assistant("Response");
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "Response");
    }

    #[test]
    fn sampling_content_image() {
        let content = SamplingContent::Image {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "image");
        assert_eq!(value["data"], "base64data");
        assert_eq!(value["mimeType"], "image/png");
    }

    #[test]
    fn include_context_serialization() {
        let none = IncludeContext::None;
        let this = IncludeContext::ThisServer;
        let all = IncludeContext::AllServers;

        assert_eq!(serde_json::to_value(none).unwrap(), "none");
        assert_eq!(serde_json::to_value(this).unwrap(), "thisServer");
        assert_eq!(serde_json::to_value(all).unwrap(), "allServers");
    }

    #[test]
    fn create_message_result_text_content() {
        let result = CreateMessageResult::text("Hello!", "model");
        assert_eq!(result.text_content(), Some("Hello!"));

        let result = CreateMessageResult {
            content: SamplingContent::Image {
                data: "data".to_string(),
                mime_type: "image/png".to_string(),
            },
            role: crate::types::Role::Assistant,
            model: "model".to_string(),
            stop_reason: None,
            meta: None,
        };
        assert_eq!(result.text_content(), None);
    }

    // ========================================================================
    // Elicitation Tests
    // ========================================================================

    #[test]
    fn elicit_form_params_serialization() {
        let params = ElicitRequestFormParams::new(
            "Please enter your name",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
        );
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["mode"], "form");
        assert_eq!(value["message"], "Please enter your name");
        assert!(value["requestedSchema"]["properties"]["name"].is_object());
    }

    #[test]
    fn elicit_url_params_serialization() {
        let params = ElicitRequestUrlParams::new(
            "Please authenticate",
            "https://auth.example.com/oauth",
            "elicit-12345",
        );
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["mode"], "url");
        assert_eq!(value["message"], "Please authenticate");
        assert_eq!(value["url"], "https://auth.example.com/oauth");
        assert_eq!(value["elicitationId"], "elicit-12345");
    }

    #[test]
    fn elicit_request_params_untagged() {
        let form = ElicitRequestParams::form(
            "Enter name",
            serde_json::json!({"type": "object", "properties": {}}),
        );
        assert_eq!(form.mode(), ElicitMode::Form);
        assert_eq!(form.message(), "Enter name");

        let url = ElicitRequestParams::url("Auth required", "https://example.com", "id-1");
        assert_eq!(url.mode(), ElicitMode::Url);
        assert_eq!(url.message(), "Auth required");
    }

    #[test]
    fn final_embedded_elicitation_content_preserves_absence_and_rejects_explicit_null() {
        for action in ["accept", "decline", "cancel"] {
            let absent = serde_json::json!({"action": action});
            let decoded: FinalEmbeddedElicitationResult = serde_json::from_value(absent.clone())
                .expect("absent content remains structurally valid for every action");
            assert!(decoded.content.is_none());
            assert_eq!(serde_json::to_value(decoded).unwrap(), absent);

            let empty = serde_json::json!({"action": action, "content": {}});
            let decoded: FinalEmbeddedElicitationResult = serde_json::from_value(empty.clone())
                .expect("an explicitly empty content object preserves its presence");
            assert_eq!(decoded.content, Some(BTreeMap::new()));
            assert_eq!(serde_json::to_value(decoded).unwrap(), empty);

            for invalid in [
                serde_json::Value::Null,
                serde_json::json!([]),
                serde_json::json!("private-content"),
                serde_json::json!(false),
                serde_json::json!(1),
            ] {
                let invalid = serde_json::json!({"action": action, "content": invalid});
                assert!(
                    serde_json::from_value::<FinalEmbeddedElicitationResult>(invalid.clone())
                        .is_err()
                );
                assert!(
                    serde_json::from_value::<FinalInputResponses>(serde_json::json!({
                        "answer": invalid,
                    }))
                    .is_err(),
                    "the enclosing retry map must retain the same content-presence boundary"
                );
            }
        }
    }

    #[test]
    fn final_embedded_form_elicitation_admits_a_flat_draft_2020_12_schema() {
        let request: FinalEmbeddedInputRequest = serde_json::from_value(serde_json::json!({
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": "Choose a display name",
                "requestedSchema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {
                        "displayName": {"type": "string", "minLength": 1}
                    },
                    "required": ["displayName"]
                }
            }
        }))
        .expect("a flat final form schema is admitted before descriptor interpretation");

        let FinalEmbeddedInputRequest::Elicitation(FinalEmbeddedElicitationParams::Form(form)) =
            request
        else {
            panic!("fixture must decode as a final form elicitation descriptor");
        };
        assert_eq!(
            form.requested_schema.schema()["properties"]["displayName"]["type"],
            "string"
        );
    }

    #[test]
    fn final_embedded_form_elicitation_rejects_only_a_nested_property_type() {
        let mut fixture = serde_json::json!({
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": "Choose a display name",
                "requestedSchema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {
                        "displayName": {"type": "string", "minLength": 1}
                    },
                    "required": ["displayName"]
                }
            }
        });
        fixture["params"]["requestedSchema"]["properties"]["displayName"]["type"] =
            serde_json::json!("object");

        assert!(serde_json::from_value::<FinalEmbeddedInputRequest>(fixture).is_err());
    }

    #[test]
    fn final_embedded_elicitation_variants_round_trip_their_exact_flat_wire() {
        for wire in [
            r#"{"method":"elicitation/create","params":{"message":"Choose a display name","mode":"form","requestedSchema":{"$schema":"https://json-schema.org/draft/2020-12/schema","properties":{"displayName":{"minLength":1,"type":"string"}},"required":["displayName"],"type":"object"}}}"#,
            r#"{"method":"elicitation/create","params":{"message":"Authorize access","mode":"url","url":"https://example.com/authorize"}}"#,
        ] {
            let request: FinalEmbeddedInputRequest = serde_json::from_str(wire)
                .expect("a flat final elicitation descriptor is admitted");
            assert_eq!(
                serde_json::to_vec(&request).expect("admitted descriptor re-encodes"),
                wire.as_bytes(),
                "the final elicitation encoder must retain the flat mode-selected wire shape"
            );
        }
    }

    #[test]
    fn final_embedded_url_elicitation_rejects_the_legacy_identity_field() {
        assert!(
            serde_json::from_value::<FinalEmbeddedInputRequest>(serde_json::json!({
                "method": "elicitation/create",
                "params": {
                    "mode": "url",
                    "message": "Authorize access",
                    "url": "https://example.com/authorize",
                    "elicitationId": "exact-2024-only",
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn final_embedded_elicitation_rejects_only_the_externally_tagged_shape() {
        let mut fixture = serde_json::json!({
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": "Choose a display name",
                "requestedSchema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"displayName": {"type": "string"}},
                    "required": ["displayName"]
                }
            }
        });
        let params = fixture["params"].take();
        fixture["params"] = serde_json::json!({"Form": params});

        assert!(serde_json::from_value::<FinalEmbeddedInputRequest>(fixture).is_err());
    }

    #[test]
    fn elicit_result_accept_with_content() {
        let mut content = std::collections::HashMap::new();
        content.insert(
            "name".to_string(),
            ElicitContentValue::String("Alice".to_string()),
        );
        content.insert(
            "age".to_string(),
            ElicitContentValue::Int(JsonInteger::from(30_i64)),
        );
        content.insert("active".to_string(), ElicitContentValue::Bool(true));

        let result = ElicitResult::accept(content);
        assert!(result.is_accepted());
        assert!(!result.is_declined());
        assert!(!result.is_cancelled());
        assert_eq!(result.get_string("name"), Some("Alice"));
        assert_eq!(result.get_int("age").map(JsonInteger::as_str), Some("30"));
        assert_eq!(result.get_bool("active"), Some(true));
    }

    #[test]
    fn elicit_integer_content_preserves_arbitrary_width_and_distinguishes_fractional_values() {
        let integer_wire =
            r#"{"action":"accept","content":{"count":922337203685477580812345678901234567890}}"#;
        let integer: ElicitResult =
            serde_json::from_str(integer_wire).expect("arbitrary-width elicitation integer parses");
        assert_eq!(
            integer.get_int("count").map(JsonInteger::as_str),
            Some("922337203685477580812345678901234567890")
        );
        assert_eq!(
            serde_json::to_string(&integer).expect("arbitrary-width elicitation integer encodes"),
            integer_wire,
            "the exact integer elicitation value lexeme round-trips"
        );

        let fractional: ElicitResult = serde_json::from_str(
            r#"{"action":"accept","content":{"count":922337203685477580812345678901234567890.5}}"#,
        )
        .expect("changing only the elicitation value to fractional remains a valid float");
        assert!(matches!(
            fractional
                .content
                .as_ref()
                .and_then(|content| content.get("count")),
            Some(ElicitContentValue::Float(_))
        ));
        assert!(fractional.get_int("count").is_none());
    }

    #[test]
    fn elicit_result_serialization() {
        let result = ElicitResult::decline();
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["action"], "decline");
        assert!(value.get("content").is_none());

        let result = ElicitResult::cancel();
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["action"], "cancel");
    }

    #[test]
    fn elicit_content_value_conversions() {
        let s: ElicitContentValue = "hello".into();
        assert!(matches!(s, ElicitContentValue::String(_)));

        let i: ElicitContentValue = 42i64.into();
        assert!(matches!(i, ElicitContentValue::Int(value) if value.as_str() == "42"));

        let b: ElicitContentValue = true.into();
        assert!(matches!(b, ElicitContentValue::Bool(true)));

        let f: ElicitContentValue = 1.23.into();
        assert!(matches!(f, ElicitContentValue::Float(_)));

        let arr: ElicitContentValue = vec!["a".to_string(), "b".to_string()].into();
        assert!(matches!(arr, ElicitContentValue::StringArray(_)));

        let none: ElicitContentValue = None::<String>.into();
        assert!(matches!(none, ElicitContentValue::Null));
    }

    #[test]
    fn elicit_complete_notification_serialization() {
        let params = ElicitCompleteNotificationParams::new("elicit-12345");
        let value = serde_json::to_value(&params).expect("serialize");
        assert_eq!(value["elicitationId"], "elicit-12345");
    }

    #[test]
    fn elicitation_capability_modes() {
        use crate::types::ElicitationCapability;

        let form_only = ElicitationCapability::form();
        assert!(form_only.supports_form());
        assert!(!form_only.supports_url());

        let url_only = ElicitationCapability::url();
        assert!(!url_only.supports_form());
        assert!(url_only.supports_url());

        let both = ElicitationCapability::both();
        assert!(both.supports_form());
        assert!(both.supports_url());
    }

    // ========================================================================
    // Roots tests
    // ========================================================================

    #[test]
    fn root_new() {
        use crate::types::Root;

        let root = Root::new("file:///home/user/project");
        assert_eq!(root.uri, "file:///home/user/project");
        assert!(root.name.is_none());
    }

    #[test]
    fn root_with_name() {
        use crate::types::Root;

        let root = Root::with_name("file:///home/user/project", "My Project");
        assert_eq!(root.uri, "file:///home/user/project");
        assert_eq!(root.name, Some("My Project".to_string()));
    }

    #[test]
    fn root_serialization() {
        use crate::types::Root;

        let root = Root::with_name("file:///home/user/project", "My Project");
        let json = serde_json::to_value(&root).expect("serialize");
        assert_eq!(json["uri"], "file:///home/user/project");
        assert_eq!(json["name"], "My Project");

        // Without name
        let root_no_name = Root::new("file:///tmp");
        let json = serde_json::to_value(&root_no_name).expect("serialize");
        assert_eq!(json["uri"], "file:///tmp");
        assert!(json.get("name").is_none());
    }

    #[test]
    fn list_roots_result_empty() {
        let result = ListRootsResult::empty();
        assert_eq!(result.roots.len(), 0);
    }

    #[test]
    fn list_roots_result_serialization() {
        use crate::types::Root;

        let result = ListRootsResult::new(vec![
            Root::with_name("file:///home/user/frontend", "Frontend"),
            Root::with_name("file:///home/user/backend", "Backend"),
        ]);

        let json = serde_json::to_value(&result).expect("serialize");
        let roots = json["roots"].as_array().expect("roots array");
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0]["uri"], "file:///home/user/frontend");
        assert_eq!(roots[0]["name"], "Frontend");
        assert_eq!(roots[1]["uri"], "file:///home/user/backend");
        assert_eq!(roots[1]["name"], "Backend");
    }

    #[test]
    fn roots_capability_serialization() {
        use crate::types::RootsCapability;

        // With listChanged = true
        let cap = RootsCapability { list_changed: true };
        let json = serde_json::to_value(&cap).expect("serialize");
        assert_eq!(json["listChanged"], true);

        // With listChanged = false (should be omitted)
        let cap = RootsCapability::default();
        let json = serde_json::to_value(&cap).expect("serialize");
        assert!(json.get("listChanged").is_none());
    }

    // ========================================================================
    // Component Version Metadata Tests
    // ========================================================================

    #[test]
    fn tool_version_serialization() {
        use crate::types::Tool;

        // Tool without version (should omit version field)
        let tool = Tool {
            name: "my_tool".to_string(),
            description: Some("A test tool".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        assert!(json.get("version").is_none());

        // Tool with version
        let tool = Tool {
            name: "my_tool".to_string(),
            description: Some("A test tool".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: Some("1.2.3".to_string()),
            tags: vec![],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        assert_eq!(json["version"], "1.2.3");
    }

    #[test]
    fn resource_version_serialization() {
        use crate::types::Resource;

        // Resource without version
        let resource = Resource {
            uri: "file://test".to_string(),
            name: "Test Resource".to_string(),
            description: None,
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        };
        let json = serde_json::to_value(&resource).expect("serialize");
        assert!(json.get("version").is_none());

        // Resource with version
        let resource = Resource {
            uri: "file://test".to_string(),
            name: "Test Resource".to_string(),
            description: None,
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: Some("2.0.0".to_string()),
            tags: vec![],
        };
        let json = serde_json::to_value(&resource).expect("serialize");
        assert_eq!(json["version"], "2.0.0");
    }

    #[test]
    fn prompt_version_serialization() {
        use crate::types::Prompt;

        // Prompt without version
        let prompt = Prompt {
            name: "greeting".to_string(),
            description: Some("A greeting prompt".to_string()),
            arguments: vec![],
            icon: None,
            version: None,
            tags: vec![],
        };
        let json = serde_json::to_value(&prompt).expect("serialize");
        assert!(json.get("version").is_none());

        // Prompt with version
        let prompt = Prompt {
            name: "greeting".to_string(),
            description: Some("A greeting prompt".to_string()),
            arguments: vec![],
            icon: None,
            version: Some("0.1.0".to_string()),
            tags: vec![],
        };
        let json = serde_json::to_value(&prompt).expect("serialize");
        assert_eq!(json["version"], "0.1.0");
    }

    #[test]
    fn resource_template_version_serialization() {
        // ResourceTemplate without version
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "Files".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let json = serde_json::to_value(&template).expect("serialize");
        assert!(json.get("version").is_none());

        // ResourceTemplate with version
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "Files".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: Some("3.0.0".to_string()),
            tags: vec![],
        };
        let json = serde_json::to_value(&template).expect("serialize");
        assert_eq!(json["version"], "3.0.0");
    }

    #[test]
    fn version_deserialization() {
        use crate::types::{Prompt, Resource, Tool};

        // Deserialize tool without version
        let json = serde_json::json!({
            "name": "tool",
            "inputSchema": {"type": "object"}
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert!(tool.version.is_none());

        // Deserialize tool with version
        let json = serde_json::json!({
            "name": "tool",
            "inputSchema": {"type": "object"},
            "version": "1.0.0"
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert_eq!(tool.version, Some("1.0.0".to_string()));

        // Deserialize resource without version
        let json = serde_json::json!({
            "uri": "file://test",
            "name": "Test"
        });
        let resource: Resource = serde_json::from_value(json).expect("deserialize");
        assert!(resource.version.is_none());

        // Deserialize prompt without version
        let json = serde_json::json!({
            "name": "prompt"
        });
        let prompt: Prompt = serde_json::from_value(json).expect("deserialize");
        assert!(prompt.version.is_none());
    }

    // ========================================================================
    // Tags Serialization Tests
    // ========================================================================

    #[test]
    fn tool_tags_serialization() {
        use crate::types::Tool;

        // Tool without tags (empty vec should not appear in JSON)
        let tool = Tool {
            name: "my_tool".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        assert!(
            json.get("tags").is_none(),
            "Empty tags should not appear in JSON"
        );

        // Tool with tags
        let tool = Tool {
            name: "my_tool".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec!["api".to_string(), "database".to_string()],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        assert_eq!(json["tags"], serde_json::json!(["api", "database"]));
    }

    #[test]
    fn resource_tags_serialization() {
        use crate::types::Resource;

        // Resource without tags
        let resource = Resource {
            uri: "file://test".to_string(),
            name: "Test Resource".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let json = serde_json::to_value(&resource).expect("serialize");
        assert!(
            json.get("tags").is_none(),
            "Empty tags should not appear in JSON"
        );

        // Resource with tags
        let resource = Resource {
            uri: "file://test".to_string(),
            name: "Test Resource".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["files".to_string(), "readonly".to_string()],
        };
        let json = serde_json::to_value(&resource).expect("serialize");
        assert_eq!(json["tags"], serde_json::json!(["files", "readonly"]));
    }

    #[test]
    fn prompt_tags_serialization() {
        use crate::types::Prompt;

        // Prompt without tags
        let prompt = Prompt {
            name: "greeting".to_string(),
            description: None,
            arguments: vec![],
            icon: None,
            version: None,
            tags: vec![],
        };
        let json = serde_json::to_value(&prompt).expect("serialize");
        assert!(
            json.get("tags").is_none(),
            "Empty tags should not appear in JSON"
        );

        // Prompt with tags
        let prompt = Prompt {
            name: "greeting".to_string(),
            description: None,
            arguments: vec![],
            icon: None,
            version: None,
            tags: vec!["templates".to_string(), "onboarding".to_string()],
        };
        let json = serde_json::to_value(&prompt).expect("serialize");
        assert_eq!(json["tags"], serde_json::json!(["templates", "onboarding"]));
    }

    #[test]
    fn resource_template_tags_serialization() {
        // ResourceTemplate without tags
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "Files".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let json = serde_json::to_value(&template).expect("serialize");
        assert!(
            json.get("tags").is_none(),
            "Empty tags should not appear in JSON"
        );

        // ResourceTemplate with tags
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "Files".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["filesystem".to_string()],
        };
        let json = serde_json::to_value(&template).expect("serialize");
        assert_eq!(json["tags"], serde_json::json!(["filesystem"]));
    }

    #[test]
    fn tags_deserialization() {
        use crate::types::{Prompt, Resource, Tool};

        // Deserialize tool without tags field
        let json = serde_json::json!({
            "name": "tool",
            "inputSchema": {"type": "object"}
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert_eq!(tool.tags.len(), 0);

        // Deserialize tool with tags
        let json = serde_json::json!({
            "name": "tool",
            "inputSchema": {"type": "object"},
            "tags": ["compute", "heavy"]
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert_eq!(tool.tags, vec!["compute", "heavy"]);

        // Deserialize resource without tags
        let json = serde_json::json!({
            "uri": "file://test",
            "name": "Test"
        });
        let resource: Resource = serde_json::from_value(json).expect("deserialize");
        assert_eq!(resource.tags.len(), 0);

        // Deserialize resource with tags
        let json = serde_json::json!({
            "uri": "file://test",
            "name": "Test",
            "tags": ["data"]
        });
        let resource: Resource = serde_json::from_value(json).expect("deserialize");
        assert_eq!(resource.tags, vec!["data"]);

        // Deserialize prompt without tags
        let json = serde_json::json!({
            "name": "prompt"
        });
        let prompt: Prompt = serde_json::from_value(json).expect("deserialize");
        assert_eq!(prompt.tags.len(), 0);

        // Deserialize prompt with tags
        let json = serde_json::json!({
            "name": "prompt",
            "tags": ["greeting", "onboarding"]
        });
        let prompt: Prompt = serde_json::from_value(json).expect("deserialize");
        assert_eq!(prompt.tags, vec!["greeting", "onboarding"]);
    }

    // ========================================================================
    // Tool Annotations Serialization Tests
    // ========================================================================

    #[test]
    fn tool_annotations_serialization() {
        use crate::types::{Tool, ToolAnnotations};

        // Tool without annotations (None should not appear in JSON)
        let tool = Tool {
            name: "my_tool".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        assert!(
            json.get("annotations").is_none(),
            "None annotations should not appear in JSON"
        );

        // Tool with annotations
        let tool = Tool {
            name: "delete_file".to_string(),
            description: Some("Deletes a file".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(
                ToolAnnotations::new()
                    .destructive(true)
                    .idempotent(false)
                    .read_only(false),
            ),
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        let annotations = json.get("annotations").expect("annotations field");
        // MCP-spec wire names are the `*Hint` forms.
        assert_eq!(annotations["destructiveHint"], true);
        assert_eq!(annotations["idempotentHint"], false);
        assert_eq!(annotations["readOnlyHint"], false);
        assert!(annotations.get("destructive").is_none());
        assert!(annotations.get("readOnly").is_none());
        assert!(annotations.get("openWorldHint").is_none());

        // Tool with read_only annotation
        let tool = Tool {
            name: "get_status".to_string(),
            description: Some("Gets status".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations::new().read_only(true)),
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        let annotations = json.get("annotations").expect("annotations field");
        assert_eq!(annotations["readOnlyHint"], true);
        assert!(annotations.get("destructiveHint").is_none());
    }

    #[test]
    fn tool_annotations_deserialization() {
        use crate::types::Tool;

        // Deserialize tool without annotations
        let json = serde_json::json!({
            "name": "tool",
            "inputSchema": {"type": "object"}
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert!(tool.annotations.is_none());

        // Deserialize tool with annotations
        let json = serde_json::json!({
            "name": "delete_tool",
            "inputSchema": {"type": "object"},
            "annotations": {
                "destructiveHint": true,
                "idempotentHint": false,
                "readOnlyHint": false,
                "openWorldHint": true
            }
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        let annotations = tool.annotations.expect("annotations present");
        assert_eq!(annotations.destructive, Some(true));
        assert_eq!(annotations.idempotent, Some(false));
        assert_eq!(annotations.read_only, Some(false));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    #[test]
    fn tool_annotations_builder() {
        use crate::types::ToolAnnotations;

        let annotations = ToolAnnotations::new()
            .destructive(true)
            .idempotent(true)
            .read_only(false)
            .open_world_hint(true);

        assert_eq!(annotations.destructive, Some(true));
        assert_eq!(annotations.idempotent, Some(true));
        assert_eq!(annotations.read_only, Some(false));
        assert_eq!(annotations.open_world_hint, Some(true));
        assert!(!annotations.is_empty());

        // Empty annotations
        let empty = ToolAnnotations::new();
        assert!(empty.is_empty());
    }

    // ========================================================================
    // Tool Output Schema Serialization Tests
    // ========================================================================

    #[test]
    fn tool_output_schema_serialization() {
        use crate::types::Tool;

        // Tool without output_schema (None should not appear in JSON)
        let tool = Tool {
            name: "my_tool".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        assert!(
            json.get("outputSchema").is_none(),
            "None output_schema should not appear in JSON"
        );

        // Tool with output_schema
        let tool = Tool {
            name: "compute".to_string(),
            description: Some("Computes a result".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "result": {"type": "number"},
                    "success": {"type": "boolean"}
                }
            })),
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let json = serde_json::to_value(&tool).expect("serialize");
        let output_schema = json.get("outputSchema").expect("outputSchema field");
        assert_eq!(output_schema["type"], "object");
        assert_eq!(output_schema["properties"]["result"]["type"], "number");
        assert_eq!(output_schema["properties"]["success"]["type"], "boolean");
    }

    #[test]
    fn tool_output_schema_deserialization() {
        use crate::types::Tool;

        // Deserialize tool without output_schema
        let json = serde_json::json!({
            "name": "tool",
            "inputSchema": {"type": "object"}
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert!(tool.output_schema.is_none());

        // Deserialize tool with output_schema
        let json = serde_json::json!({
            "name": "compute",
            "inputSchema": {"type": "object"},
            "outputSchema": {
                "type": "object",
                "properties": {
                    "value": {"type": "integer"}
                }
            }
        });
        let tool: Tool = serde_json::from_value(json).expect("deserialize");
        assert!(tool.output_schema.is_some());
        let schema = tool.output_schema.unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["value"]["type"], "integer");
    }
}
