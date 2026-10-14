//! Browser-agnostic MCP Apps Host/View bridge vocabulary.
//!
//! These are the stable `2026-01-26` Apps messages carried by an embedder
//! (normally JSON-RPC over `postMessage`). They intentionally are not MCP
//! client/server extension-dispatch messages.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, value::RawValue};

use crate::common_types::{ContentBlock, EmbeddedResourceContents, ResourceLink};
use crate::extensions::McpAppsActivationReceipt;
use crate::types::{
    McpAppsDisplayMode, McpAppsResourceCsp, McpAppsResourcePermissions, McpAppsToolResult,
};

/// Stable Apps Host/View protocol version.
pub const MCP_APPS_HOST_VIEW_PROTOCOL_VERSION: &str = "2026-01-26";
/// Maximum concurrent Host-originated requests retained for one View.
pub const MAX_MCP_APPS_BRIDGE_IN_FLIGHT: usize = 64;
/// Maximum bytes in a bridge reason or HTML sandbox payload field.
pub const MAX_MCP_APPS_BRIDGE_TEXT_BYTES: usize = 64 * 1024;
/// Maximum complete JSON-RPC envelope bytes admitted on one Apps carrier.
pub const MAX_MCP_APPS_BRIDGE_MESSAGE_BYTES: usize = 1024 * 1024;
/// Maximum encoded bytes in one string Apps request identifier.
pub const MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES: usize = 256;
/// Largest integer exactly representable by a JavaScript Number.
pub const MAX_MCP_APPS_BRIDGE_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Maximum nesting depth retained by the bounded JSON payload validator.
pub const MAX_MCP_APPS_BRIDGE_JSON_DEPTH: usize = 32;
/// Maximum members retained by one bridge JSON object.
pub const MAX_MCP_APPS_BRIDGE_JSON_OBJECT_MEMBERS: usize = 256;
/// Maximum elements retained by one bridge JSON array.
pub const MAX_MCP_APPS_BRIDGE_JSON_ARRAY_ITEMS: usize = 1_024;

/// Legacy typed-carrier identifier retained for the existing host-runtime API.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpAppsBridgeRequestId(u64);

impl McpAppsBridgeRequestId {
    /// Admits a Host-runtime positive numeric identifier.
    pub fn new(value: u64) -> Result<Self, McpAppsBridgeError> {
        if value == 0 {
            Err(McpAppsBridgeError::ZeroRequestId)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the retained numeric identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Browser-safe JSON-RPC correlation identifier used by the closed Apps wire
/// envelope. View-originated identifiers may be zero or strings; Host
/// allocation is deliberately separate and positive only.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum McpAppsJsonRpcRequestId {
    Number(u64),
    String(String),
}

impl McpAppsJsonRpcRequestId {
    /// Admits an incoming numeric identifier, including zero.
    pub fn new(value: u64) -> Result<Self, McpAppsBridgeError> {
        if value > MAX_MCP_APPS_BRIDGE_SAFE_INTEGER {
            return Err(McpAppsBridgeError::UnsafeRequestId);
        }
        Ok(Self::Number(value))
    }

    /// Admits an incoming bounded string identifier without normalization.
    pub fn string(value: String) -> Result<Self, McpAppsBridgeError> {
        if value.is_empty() || value.len() > MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES {
            return Err(McpAppsBridgeError::InvalidRequestId);
        }
        Ok(Self::String(value))
    }

    /// Allocates one Host-originated positive JavaScript-safe identifier.
    pub fn host(value: u64) -> Result<Self, McpAppsBridgeError> {
        if value == 0 {
            return Err(McpAppsBridgeError::ZeroRequestId);
        }
        Self::new(value)
    }

    /// Returns the numeric identifier when this is a numeric ID.
    #[must_use]
    pub const fn as_number(&self) -> Option<u64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::String(_) => None,
        }
    }

    /// Returns a formatting wrapper for the exact retained identifier.
    #[must_use]
    pub fn get(&self) -> McpAppsJsonRpcRequestIdDisplay<'_> {
        McpAppsJsonRpcRequestIdDisplay(self)
    }
}

/// Display wrapper returned by [`McpAppsJsonRpcRequestId::get`].
pub struct McpAppsJsonRpcRequestIdDisplay<'a>(&'a McpAppsJsonRpcRequestId);

impl fmt::Display for McpAppsJsonRpcRequestIdDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            McpAppsJsonRpcRequestId::Number(value) => value.fmt(formatter),
            McpAppsJsonRpcRequestId::String(value) => formatter.write_str(value),
        }
    }
}

impl Serialize for McpAppsJsonRpcRequestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for McpAppsJsonRpcRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::Number(value) => value
                .as_u64()
                .ok_or_else(|| serde::de::Error::custom("Apps request ID must be an integer"))
                .and_then(|value| Self::new(value).map_err(serde::de::Error::custom)),
            Value::String(value) => Self::string(value).map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "Apps request ID must be a string or safe integer",
            )),
        }
    }
}

/// Programmatic App or Host identity used during `ui/initialize`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsBridgeImplementation {
    /// Programmatic name.
    pub name: String,
    /// Implementation version.
    pub version: String,
}

/// View capabilities advertised during initialization.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsViewCapabilities {
    /// Display modes supported by the View.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_display_modes: Vec<McpAppsDisplayMode>,
}

/// Host capabilities returned during initialization.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsHostCapabilities {
    /// The Host will accept `ui/open-link`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_links: bool,
    /// The Host will accept `ui/download-file`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub download_file: bool,
    /// The Host will accept `ui/update-model-context`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update_model_context: bool,
    /// The Host will accept `ui/message`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub message: bool,
}

/// Typed, finite Host context supplied to a View.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsHostContext {
    /// Current Host-selected display mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_mode: Option<McpAppsDisplayMode>,
    /// Display modes the Host can select.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_display_modes: Vec<McpAppsDisplayMode>,
    /// BCP-47 locale when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
}

/// `ui/initialize` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsInitializeParams {
    pub app_info: McpAppsBridgeImplementation,
    pub app_capabilities: McpAppsViewCapabilities,
    pub protocol_version: String,
}

/// `ui/initialize` result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsInitializeResult {
    pub protocol_version: String,
    pub host_info: McpAppsBridgeImplementation,
    pub host_capabilities: McpAppsHostCapabilities,
    pub host_context: McpAppsHostContext,
}

/// `ui/open-link` parameters.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsOpenLinkParams {
    pub url: String,
}

/// `ui/download-file` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsDownloadFileParams {
    pub contents: Vec<McpAppsDownloadContent>,
}

/// One downloadable embedded resource or resource link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpAppsDownloadContent {
    Embedded(EmbeddedResourceContents),
    Link(ResourceLink),
}

/// `ui/message` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsMessageParams {
    pub role: McpAppsMessageRole,
    pub content: Vec<ContentBlock>,
}

/// Stable `ui/message` role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAppsMessageRole {
    User,
}

/// `ui/update-model-context` parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsUpdateModelContextParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<BTreeMap<String, Value>>,
}

/// `ui/request-display-mode` parameters and result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsDisplayModeParams {
    pub mode: McpAppsDisplayMode,
}

/// Simple acknowledgement used by Host policy requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsOperationResult {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// `ui/resource-teardown` parameters.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsResourceTeardownParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Exact bridge-local app-tool invocation parameters. These are not a core
/// `tools/call` request and carry no core metadata, task, or result members.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsToolCallParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<BTreeMap<String, Value>>,
}

/// Exact bridge-local resource read parameters.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsResourceReadParams {
    pub uri: crate::common_types::AbsoluteUri,
}

/// Shared bounded catalog request shape for the three View catalog methods and
/// the Host's isolated app-tool list method.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Bridge-local ping has omitted or exact-empty parameters and an exact-empty result.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAppsPingParams {}

/// Standard-reused Apps-domain log notification, distinct from `ui/message`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsLogMessageNotification {
    pub level: String,
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logger: Option<String>,
}

/// Direction-indexed inherited Apps progress control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsProgressNotification {
    pub progress_token: Value,
    pub progress: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
}

/// Direction-indexed inherited Apps cancellation control.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsCancelledNotification {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<McpAppsBridgeRequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Closed View-to-Host request vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsViewRequest {
    Initialize(McpAppsInitializeParams),
    OpenLink(McpAppsOpenLinkParams),
    DownloadFile(McpAppsDownloadFileParams),
    Message(McpAppsMessageParams),
    UpdateModelContext(McpAppsUpdateModelContextParams),
    RequestDisplayMode(McpAppsDisplayModeParams),
    CallTool(McpAppsToolCallParams),
    ResourceRead(McpAppsResourceReadParams),
    ResourcesList(McpAppsListParams),
    ResourceTemplatesList(McpAppsListParams),
    PromptsList(McpAppsListParams),
    Ping(McpAppsPingParams),
}

/// Closed Host-to-View request vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsHostRequest {
    ResourceTeardown(McpAppsResourceTeardownParams),
    ToolsList(McpAppsListParams),
    CallTool(McpAppsToolCallParams),
    Ping(McpAppsPingParams),
}

/// Result for a View-to-Host request.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsHostResponse {
    Initialize(McpAppsInitializeResult),
    OpenLink(McpAppsOperationResult),
    DownloadFile(McpAppsOperationResult),
    Message(McpAppsOperationResult),
    UpdateModelContext(McpAppsOperationResult),
    RequestDisplayMode(McpAppsDisplayModeParams),
    BridgeUnavailable,
    Ping,
}

/// Result for a Host-to-View request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpAppsViewResponse;

/// Closed View-to-Host notification vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsViewNotification {
    SizeChanged {
        width: Option<f64>,
        height: Option<f64>,
    },
    RequestTeardown,
    Initialized,
    ToolsListChanged,
    LogMessage(McpAppsLogMessageNotification),
    Progress(McpAppsProgressNotification),
    Cancelled(McpAppsCancelledNotification),
}

/// Closed Host-to-View notification vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsHostNotification {
    ToolInput {
        arguments: Option<BTreeMap<String, Value>>,
    },
    ToolInputPartial {
        arguments: Option<BTreeMap<String, Value>>,
    },
    ToolResult(McpAppsToolResult),
    ToolCancelled {
        reason: Option<String>,
    },
    HostContextChanged(McpAppsHostContext),
    ToolsListChanged,
    ResourcesListChanged,
    PromptsListChanged,
    Progress(McpAppsProgressNotification),
    Cancelled(McpAppsCancelledNotification),
}

/// Sandbox-only signals. They have a distinct proxy carrier and are never
/// admitted by [`McpAppsViewToHost`] or [`McpAppsHostToView`].
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsSandboxSignal {
    ProxyReady,
    ResourceReady {
        html: String,
        sandbox: Option<String>,
        csp: Option<McpAppsResourceCsp>,
        permissions: Option<McpAppsResourcePermissions>,
    },
}

/// One typed message delivered from View to Host.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsViewToHost {
    Request {
        id: McpAppsBridgeRequestId,
        request: McpAppsViewRequest,
    },
    Notification(McpAppsViewNotification),
    Response {
        id: McpAppsBridgeRequestId,
        response: McpAppsViewResponse,
    },
}

/// One typed message delivered from Host to View.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsHostToView {
    Request {
        id: McpAppsBridgeRequestId,
        request: McpAppsHostRequest,
    },
    Notification(McpAppsHostNotification),
    Response {
        id: McpAppsBridgeRequestId,
        response: McpAppsHostResponse,
    },
}

/// Typed bridge admission errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpAppsBridgeError {
    ZeroRequestId,
    InvalidRequestId,
    UnsafeRequestId,
    RequestIdTooLong,
    TextTooLong,
    MessageTooLarge,
    InvalidEnvelope,
    InvalidJsonRpcVersion,
    InvalidMethodDirection,
    InvalidParams,
    InvalidLifecycle,
    UnknownProgressToken,
    InvalidError,
    DuplicateLiveRequest,
    UnknownCorrelation,
    TooManyInFlight,
    RequestIdExhausted,
}

impl fmt::Display for McpAppsBridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroRequestId => "MCP Apps bridge request IDs must be non-zero",
            Self::InvalidRequestId => "MCP Apps bridge request ID is invalid",
            Self::UnsafeRequestId => "MCP Apps bridge request ID exceeds JavaScript safe range",
            Self::RequestIdTooLong => {
                "MCP Apps bridge encoded request ID exceeds its bounded limit"
            }
            Self::TextTooLong => "MCP Apps bridge text exceeds its bounded limit",
            Self::MessageTooLarge => "MCP Apps bridge message exceeds its bounded limit",
            Self::InvalidEnvelope => "MCP Apps bridge JSON-RPC envelope is invalid",
            Self::InvalidJsonRpcVersion => "MCP Apps bridge requires JSON-RPC 2.0",
            Self::InvalidMethodDirection => "MCP Apps bridge method is invalid in this direction",
            Self::InvalidParams => "MCP Apps bridge parameters are invalid",
            Self::InvalidLifecycle => "MCP Apps bridge lifecycle does not admit this message",
            Self::UnknownProgressToken => {
                "MCP Apps bridge progress token is not live in this direction"
            }
            Self::InvalidError => "MCP Apps bridge error is invalid",
            Self::DuplicateLiveRequest => "MCP Apps bridge request ID is already live",
            Self::UnknownCorrelation => "MCP Apps bridge response does not match a live request",
            Self::TooManyInFlight => "MCP Apps bridge has too many in-flight requests",
            Self::RequestIdExhausted => "MCP Apps bridge request ID space is exhausted",
        })
    }
}
impl std::error::Error for McpAppsBridgeError {}

impl McpAppsResourceTeardownParams {
    /// Validates the optional diagnostic reason before it crosses the bridge.
    pub fn try_new(reason: Option<String>) -> Result<Self, McpAppsBridgeError> {
        if reason
            .as_ref()
            .is_some_and(|value| value.len() > MAX_MCP_APPS_BRIDGE_TEXT_BYTES)
        {
            return Err(McpAppsBridgeError::TextTooLong);
        }
        Ok(Self { reason })
    }
}

/// Exact empty-object marker used by the pinned Apps capability shapes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAppsCapabilityMarker {}

/// Pinned App-side `tools` capability.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsAppToolsCapability {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list_changed: bool,
}

/// Pinned source-parity App capabilities retained at the JSON-RPC boundary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsPinnedViewCapabilities {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub experimental: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<McpAppsAppToolsCapability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_display_modes: Vec<McpAppsDisplayMode>,
}

/// Pinned content-modalities map used by `message` and `updateModelContext`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsContentBlockModalities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_link: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<McpAppsCapabilityMarker>,
}

/// Pinned `serverTools` capability.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsServerToolsCapability {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list_changed: bool,
}

/// Pinned `serverResources` capability.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsServerResourcesCapability {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list_changed: bool,
}

/// Pinned sandbox declaration. It is data only and never grants authority.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsSandboxCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<McpAppsResourcePermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csp: Option<McpAppsResourceCsp>,
}

/// Pinned source-parity Host capabilities retained at the JSON-RPC boundary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsPinnedHostCapabilities {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub experimental: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_links: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_file: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tools: Option<McpAppsServerToolsCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_resources: Option<McpAppsServerResourcesCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<McpAppsCapabilityMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<McpAppsSandboxCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_model_context: Option<McpAppsContentBlockModalities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<McpAppsContentBlockModalities>,
}

/// Source-parity Host context. Unknown source-forward fields are retained as
/// bounded inert values rather than being treated as capabilities.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAppsPinnedHostContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_info: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub styles: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_mode: Option<McpAppsDisplayMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_display_modes: Vec<McpAppsDisplayMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_dimensions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_capabilities: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_area_insets: Option<Value>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// Source-parity `ui/initialize` request payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsPinnedInitializeParams {
    pub app_info: McpAppsBridgeImplementation,
    pub app_capabilities: McpAppsPinnedViewCapabilities,
    pub protocol_version: String,
}

/// Source-parity `ui/initialize` response payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAppsPinnedInitializeResult {
    pub protocol_version: String,
    pub host_info: McpAppsBridgeImplementation,
    pub host_capabilities: McpAppsPinnedHostCapabilities,
    pub host_context: McpAppsPinnedHostContext,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// Direction in which a closed Apps JSON-RPC envelope is admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsBridgeDirection {
    ViewToHost,
    HostToView,
}

/// A decoded, closed JSON-RPC error envelope payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsJsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Direction-indexed, SDK-reused `notifications/progress` payload.
///
/// This is deliberately distinct from the legacy host-runtime notification
/// type above so the closed wire codec retains its exact token/optional
/// message contract without importing final-core progress state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsProgressControlParams {
    pub progress_token: McpAppsJsonRpcRequestId,
    pub progress: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl McpAppsProgressControlParams {
    /// Validates finite control values and bounded optional text.
    pub fn try_new(
        progress_token: McpAppsJsonRpcRequestId,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) -> Result<Self, McpAppsBridgeError> {
        if !progress.is_finite()
            || total.is_some_and(|total| !total.is_finite())
            || message
                .as_ref()
                .is_some_and(|message| message.len() > MAX_MCP_APPS_BRIDGE_TEXT_BYTES)
        {
            return Err(McpAppsBridgeError::InvalidParams);
        }
        Ok(Self {
            progress_token,
            progress,
            total,
            message,
        })
    }
}

/// Direction-indexed, SDK-reused `notifications/cancelled` payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsCancelledControlParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<McpAppsJsonRpcRequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl McpAppsCancelledControlParams {
    /// Validates a bounded optional cancellation reason.
    pub fn try_new(
        request_id: Option<McpAppsJsonRpcRequestId>,
        reason: Option<String>,
    ) -> Result<Self, McpAppsBridgeError> {
        if reason
            .as_ref()
            .is_some_and(|reason| reason.len() > MAX_MCP_APPS_BRIDGE_TEXT_BYTES)
        {
            return Err(McpAppsBridgeError::InvalidParams);
        }
        Ok(Self { request_id, reason })
    }
}

/// Closed `ui/notifications/size-changed` parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsSizeChangedParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f64>,
}

impl McpAppsSizeChangedParams {
    /// Admits only finite optional dimensions.
    pub fn try_new(width: Option<f64>, height: Option<f64>) -> Result<Self, McpAppsBridgeError> {
        if width.is_some_and(|width| !width.is_finite())
            || height.is_some_and(|height| !height.is_finite())
        {
            return Err(McpAppsBridgeError::InvalidParams);
        }
        Ok(Self { width, height })
    }
}

/// Closed `ui/notifications/tool-input[-partial]` parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsToolInputParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<BTreeMap<String, Value>>,
}

impl McpAppsToolInputParams {
    /// Admits bounded optional object arguments without scalar coercion.
    pub fn try_new(arguments: Option<BTreeMap<String, Value>>) -> Result<Self, McpAppsBridgeError> {
        if arguments.as_ref().is_some_and(|arguments| {
            let object: Map<String, Value> = arguments
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            !bounded_json(&Value::Object(object), 0)
        }) {
            return Err(McpAppsBridgeError::InvalidParams);
        }
        Ok(Self { arguments })
    }
}

/// Closed `ui/notifications/tool-cancelled` parameters.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsToolCancelledParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl McpAppsToolCancelledParams {
    /// Admits a bounded optional cancellation reason.
    pub fn try_new(reason: Option<String>) -> Result<Self, McpAppsBridgeError> {
        if reason
            .as_ref()
            .is_some_and(|reason| reason.len() > MAX_MCP_APPS_BRIDGE_TEXT_BYTES)
        {
            return Err(McpAppsBridgeError::InvalidParams);
        }
        Ok(Self { reason })
    }
}

impl McpAppsJsonRpcError {
    /// Validates the bounded Apps error profile without assigning authority to
    /// peer-supplied code or data.
    pub fn try_new(
        code: i64,
        message: String,
        data: Option<Value>,
    ) -> Result<Self, McpAppsBridgeError> {
        if message.len() > MAX_MCP_APPS_BRIDGE_TEXT_BYTES
            || code.unsigned_abs() > MAX_MCP_APPS_BRIDGE_SAFE_INTEGER
            || data.as_ref().is_some_and(|value| !bounded_json(value, 0))
        {
            return Err(McpAppsBridgeError::InvalidError);
        }
        Ok(Self {
            code,
            message,
            data,
        })
    }
}

/// The exact routed method set for one admitted Apps envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsRoutedMethod {
    Initialize,
    OpenLink,
    DownloadFile,
    Message,
    UpdateModelContext,
    RequestDisplayMode,
    ResourceTeardown,
    ToolsList,
    ToolsCall,
    ResourcesList,
    ResourceTemplatesList,
    ResourcesRead,
    PromptsList,
    SamplingCreateMessageRejected,
    Ping,
    Initialized,
    SizeChanged,
    RequestTeardown,
    AppToolsListChanged,
    LoggingMessage,
    ToolInput,
    ToolInputPartial,
    ToolResult,
    ToolCancelled,
    HostContextChanged,
    ToolsListChanged,
    ResourcesListChanged,
    PromptsListChanged,
    Progress,
    Cancelled,
}

/// Closed, routed Apps JSON-RPC envelope.
#[derive(Clone, Debug, PartialEq)]
pub enum McpAppsJsonRpcEnvelope {
    Request {
        id: McpAppsJsonRpcRequestId,
        method: McpAppsRoutedMethod,
        params: Option<Value>,
        progress_token: Option<McpAppsJsonRpcRequestId>,
    },
    Notification {
        method: McpAppsRoutedMethod,
        params: Option<Value>,
    },
    Response {
        id: McpAppsJsonRpcRequestId,
        result: Value,
    },
    Error {
        id: McpAppsJsonRpcRequestId,
        error: McpAppsJsonRpcError,
    },
}

#[derive(Deserialize)]
struct McpAppsRawEnvelope {
    #[serde(default)]
    id: Option<Box<RawValue>>,
    #[serde(default)]
    params: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct McpAppsRawParams {
    #[serde(default, rename = "_meta")]
    meta: Option<McpAppsRawMeta>,
}

#[derive(Deserialize)]
struct McpAppsRawMeta {
    #[serde(default, rename = "progressToken")]
    progress_token: Option<Box<RawValue>>,
}

impl McpAppsJsonRpcEnvelope {
    /// Decodes one complete bounded JSON-RPC envelope and routes its exact
    /// method only in the declared direction.
    pub fn decode(
        direction: McpAppsBridgeDirection,
        input: &str,
    ) -> Result<Self, McpAppsBridgeError> {
        if input.len() > MAX_MCP_APPS_BRIDGE_MESSAGE_BYTES {
            return Err(McpAppsBridgeError::MessageTooLarge);
        }
        let raw: McpAppsRawEnvelope =
            serde_json::from_str(input).map_err(|_| McpAppsBridgeError::InvalidEnvelope)?;
        let raw_progress_token = raw_progress_token(raw.params.as_deref());
        let value: Value =
            serde_json::from_str(input).map_err(|_| McpAppsBridgeError::InvalidEnvelope)?;
        let Value::Object(mut object) = value else {
            return Err(McpAppsBridgeError::InvalidEnvelope);
        };
        match object.remove("jsonrpc") {
            Some(Value::String(version)) if version == "2.0" => {}
            Some(_) => return Err(McpAppsBridgeError::InvalidJsonRpcVersion),
            None => return Err(McpAppsBridgeError::InvalidEnvelope),
        }
        let id = object.remove("id");
        let method = object.remove("method");
        let params = object.remove("params");
        let result = object.remove("result");
        let error = object.remove("error");
        if !object.is_empty() {
            return Err(McpAppsBridgeError::InvalidEnvelope);
        }

        match (id, method, params, result, error) {
            (Some(id), Some(Value::String(method)), params, None, None) => {
                let id = parse_raw_request_id(raw.id.as_deref(), id)?;
                let (params, progress_token) =
                    split_progress_wrapper(params, raw_progress_token.as_deref())?;
                let method = route_method(direction, &method, false)?;
                validate_routed_payload(method, params.as_ref())?;
                Ok(Self::Request {
                    id,
                    method,
                    params,
                    progress_token,
                })
            }
            (None, Some(Value::String(method)), params, None, None) => {
                let method = route_method(direction, &method, true)?;
                validate_routed_payload(method, params.as_ref())?;
                Ok(Self::Notification { method, params })
            }
            (Some(id), None, None, Some(result), None) => {
                let id = parse_raw_request_id(raw.id.as_deref(), id)?;
                if !bounded_json(&result, 0) {
                    return Err(McpAppsBridgeError::InvalidParams);
                }
                Ok(Self::Response { id, result })
            }
            (Some(id), None, None, None, Some(error)) => {
                let id = parse_raw_request_id(raw.id.as_deref(), id)?;
                let error: McpAppsJsonRpcError =
                    serde_json::from_value(error).map_err(|_| McpAppsBridgeError::InvalidError)?;
                let error = McpAppsJsonRpcError::try_new(error.code, error.message, error.data)?;
                Ok(Self::Error { id, error })
            }
            _ => Err(McpAppsBridgeError::InvalidEnvelope),
        }
    }

    /// Encodes one already validated closed JSON-RPC envelope in its exact
    /// carrier direction.
    pub fn encode(&self, direction: McpAppsBridgeDirection) -> Result<String, McpAppsBridgeError> {
        let mut object = Map::new();
        object.insert("jsonrpc".into(), Value::String("2.0".into()));
        match self {
            Self::Request {
                id,
                method,
                params,
                progress_token,
            } => {
                if route_method(direction, method_name(*method), false)? != *method {
                    return Err(McpAppsBridgeError::InvalidMethodDirection);
                }
                validate_routed_payload(*method, params.as_ref())?;
                object.insert(
                    "id".into(),
                    serde_json::to_value(id).map_err(|_| McpAppsBridgeError::InvalidEnvelope)?,
                );
                object.insert("method".into(), Value::String(method_name(*method).into()));
                if let Some(params) = params {
                    let params = add_progress_wrapper(params.clone(), progress_token.as_ref())?;
                    object.insert("params".into(), params);
                } else if progress_token.is_some() {
                    object.insert(
                        "params".into(),
                        add_progress_wrapper(Value::Object(Map::new()), progress_token.as_ref())?,
                    );
                }
            }
            Self::Notification { method, params } => {
                if route_method(direction, method_name(*method), true)? != *method {
                    return Err(McpAppsBridgeError::InvalidMethodDirection);
                }
                validate_routed_payload(*method, params.as_ref())?;
                object.insert("method".into(), Value::String(method_name(*method).into()));
                if let Some(params) = params {
                    object.insert("params".into(), params.clone());
                }
            }
            Self::Response { id, result } => {
                require_bounded(Some(result))?;
                object.insert(
                    "id".into(),
                    serde_json::to_value(id).map_err(|_| McpAppsBridgeError::InvalidEnvelope)?,
                );
                object.insert("result".into(), result.clone());
            }
            Self::Error { id, error } => {
                McpAppsJsonRpcError::try_new(
                    error.code,
                    error.message.clone(),
                    error.data.clone(),
                )?;
                object.insert(
                    "id".into(),
                    serde_json::to_value(id).map_err(|_| McpAppsBridgeError::InvalidEnvelope)?,
                );
                object.insert(
                    "error".into(),
                    serde_json::to_value(error).map_err(|_| McpAppsBridgeError::InvalidEnvelope)?,
                );
            }
        }
        let encoded = serde_json::to_string(&Value::Object(object))
            .map_err(|_| McpAppsBridgeError::InvalidEnvelope)?;
        if encoded.len() > MAX_MCP_APPS_BRIDGE_MESSAGE_BYTES {
            Err(McpAppsBridgeError::MessageTooLarge)
        } else {
            Ok(encoded)
        }
    }

    /// Validates the response payload against the method retained by the
    /// correlation registry before a terminal response is accepted or emitted.
    pub fn validate_response_for(
        method: McpAppsRoutedMethod,
        result: &Value,
    ) -> Result<(), McpAppsBridgeError> {
        match method {
            McpAppsRoutedMethod::Ping => validate_empty_or_omitted(Some(result)),
            McpAppsRoutedMethod::Initialize => {
                reject_reserved_result_members(result)?;
                let initialized: McpAppsPinnedInitializeResult =
                    serde_json::from_value(result.clone())
                        .map_err(|_| McpAppsBridgeError::InvalidParams)?;
                if initialized.protocol_version != MCP_APPS_HOST_VIEW_PROTOCOL_VERSION
                    || !bounded_json(result, 0)
                {
                    return Err(McpAppsBridgeError::InvalidParams);
                }
                Ok(())
            }
            McpAppsRoutedMethod::UpdateModelContext => validate_exact_object(Some(result), &[]),
            McpAppsRoutedMethod::RequestDisplayMode => validate_display_mode_result(result),
            McpAppsRoutedMethod::ResourceTeardown
            | McpAppsRoutedMethod::OpenLink
            | McpAppsRoutedMethod::DownloadFile
            | McpAppsRoutedMethod::Message => validate_forward_open_result(result),
            McpAppsRoutedMethod::ToolsList
            | McpAppsRoutedMethod::ToolsCall
            | McpAppsRoutedMethod::ResourcesList
            | McpAppsRoutedMethod::ResourceTemplatesList
            | McpAppsRoutedMethod::ResourcesRead
            | McpAppsRoutedMethod::PromptsList => require_bounded(Some(result)),
            McpAppsRoutedMethod::SamplingCreateMessageRejected => {
                Err(McpAppsBridgeError::InvalidParams)
            }
            McpAppsRoutedMethod::Initialized
            | McpAppsRoutedMethod::SizeChanged
            | McpAppsRoutedMethod::RequestTeardown
            | McpAppsRoutedMethod::AppToolsListChanged
            | McpAppsRoutedMethod::LoggingMessage
            | McpAppsRoutedMethod::ToolInput
            | McpAppsRoutedMethod::ToolInputPartial
            | McpAppsRoutedMethod::ToolResult
            | McpAppsRoutedMethod::ToolCancelled
            | McpAppsRoutedMethod::HostContextChanged
            | McpAppsRoutedMethod::ToolsListChanged
            | McpAppsRoutedMethod::ResourcesListChanged
            | McpAppsRoutedMethod::PromptsListChanged
            | McpAppsRoutedMethod::Progress
            | McpAppsRoutedMethod::Cancelled => Err(McpAppsBridgeError::InvalidParams),
        }
    }
}

/// Independent bounded correlation state for both Apps directions.
#[derive(Clone, Debug, Default)]
pub struct McpAppsBridgeCorrelations {
    view_live: BTreeSet<McpAppsJsonRpcRequestId>,
    host_live: BTreeSet<McpAppsJsonRpcRequestId>,
    view_methods: BTreeMap<McpAppsJsonRpcRequestId, McpAppsRoutedMethod>,
    host_methods: BTreeMap<McpAppsJsonRpcRequestId, McpAppsRoutedMethod>,
}

impl McpAppsBridgeCorrelations {
    /// Reserves a live request ID atomically in its owning direction.
    pub fn reserve(
        &mut self,
        direction: McpAppsBridgeDirection,
        id: McpAppsJsonRpcRequestId,
    ) -> Result<(), McpAppsBridgeError> {
        let live = match direction {
            McpAppsBridgeDirection::ViewToHost => &mut self.view_live,
            McpAppsBridgeDirection::HostToView => &mut self.host_live,
        };
        if live.len() >= MAX_MCP_APPS_BRIDGE_IN_FLIGHT {
            return Err(McpAppsBridgeError::TooManyInFlight);
        }
        if !live.insert(id) {
            return Err(McpAppsBridgeError::DuplicateLiveRequest);
        }
        Ok(())
    }

    /// Reserves a request together with the exact method that owns its response.
    pub fn reserve_request(
        &mut self,
        direction: McpAppsBridgeDirection,
        id: McpAppsJsonRpcRequestId,
        method: McpAppsRoutedMethod,
    ) -> Result<(), McpAppsBridgeError> {
        self.reserve(direction, id.clone())?;
        match direction {
            McpAppsBridgeDirection::ViewToHost => {
                self.view_methods.insert(id, method);
            }
            McpAppsBridgeDirection::HostToView => {
                self.host_methods.insert(id, method);
            }
        }
        Ok(())
    }

    /// Releases exactly one matching terminal correlation.
    pub fn complete(
        &mut self,
        direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
    ) -> Result<(), McpAppsBridgeError> {
        let live = match direction {
            McpAppsBridgeDirection::ViewToHost => &mut self.view_live,
            McpAppsBridgeDirection::HostToView => &mut self.host_live,
        };
        if !live.remove(id) {
            return Err(McpAppsBridgeError::UnknownCorrelation);
        }
        match direction {
            McpAppsBridgeDirection::ViewToHost => {
                self.view_methods.remove(id);
            }
            McpAppsBridgeDirection::HostToView => {
                self.host_methods.remove(id);
            }
        }
        Ok(())
    }

    /// Returns the retained response-owning method without completing it.
    #[must_use]
    pub fn method(
        &self,
        direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
    ) -> Option<McpAppsRoutedMethod> {
        match direction {
            McpAppsBridgeDirection::ViewToHost => self.view_methods.get(id).copied(),
            McpAppsBridgeDirection::HostToView => self.host_methods.get(id).copied(),
        }
    }

    /// Admits one terminal result only when the exact request-owning direction,
    /// ID, and routed result shape still match; successful admission releases
    /// the live reservation exactly once.
    pub fn complete_response(
        &mut self,
        request_direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
        result: &Value,
    ) -> Result<McpAppsRoutedMethod, McpAppsBridgeError> {
        let method = self
            .method(request_direction, id)
            .ok_or(McpAppsBridgeError::UnknownCorrelation)?;
        McpAppsJsonRpcEnvelope::validate_response_for(method, result)?;
        self.complete(request_direction, id)?;
        Ok(method)
    }

    /// Releases an exact terminal JSON-RPC error correlation. Error envelopes
    /// have already been structurally validated at the closed wire boundary.
    pub fn complete_error(
        &mut self,
        request_direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
    ) -> Result<McpAppsRoutedMethod, McpAppsBridgeError> {
        let method = self
            .method(request_direction, id)
            .ok_or(McpAppsBridgeError::UnknownCorrelation)?;
        self.complete(request_direction, id)?;
        Ok(method)
    }

    /// Returns whether this exact direction owns the live ID.
    #[must_use]
    pub fn contains(
        &self,
        direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
    ) -> bool {
        match direction {
            McpAppsBridgeDirection::ViewToHost => self.view_live.contains(id),
            McpAppsBridgeDirection::HostToView => self.host_live.contains(id),
        }
    }
}

/// Positive JavaScript-safe Host ID allocator, independent from View IDs.
#[derive(Clone, Debug, Default)]
pub struct McpAppsHostIdAllocator {
    next: u64,
}

/// Protocol-only lifecycle retained by one activated Apps View.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsBridgeLifecycle {
    New,
    InitializeInFlight,
    AwaitingInitialized,
    Active,
    Closing,
    Closed,
}

/// Disposition of a legal Apps transport control notification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpAppsControlDisposition {
    /// The control selected this exact live request in its own direction.
    Bound(McpAppsJsonRpcRequestId),
    /// An absent-ID cancellation is an explicit bounded no-op.
    Inert,
}

/// Stateful activation, lifecycle, correlation, and transport-control
/// admission surface for one Apps View. It performs no client/server RPC.
#[derive(Clone, Debug)]
pub struct McpAppsBridgeAdmission {
    activation: McpAppsActivationReceipt,
    lifecycle: McpAppsBridgeLifecycle,
    correlations: McpAppsBridgeCorrelations,
    view_progress: BTreeMap<McpAppsJsonRpcRequestId, McpAppsJsonRpcRequestId>,
    host_progress: BTreeMap<McpAppsJsonRpcRequestId, McpAppsJsonRpcRequestId>,
}

impl McpAppsBridgeAdmission {
    /// Starts one View in `New` using the current negotiated Apps receipt.
    #[must_use]
    pub fn new(activation: McpAppsActivationReceipt) -> Self {
        Self {
            activation,
            lifecycle: McpAppsBridgeLifecycle::New,
            correlations: McpAppsBridgeCorrelations::default(),
            view_progress: BTreeMap::new(),
            host_progress: BTreeMap::new(),
        }
    }

    /// Returns the immutable negotiated Apps activation receipt.
    #[must_use]
    pub const fn activation(&self) -> &McpAppsActivationReceipt {
        &self.activation
    }

    /// Returns the current per-View lifecycle phase.
    #[must_use]
    pub const fn lifecycle(&self) -> McpAppsBridgeLifecycle {
        self.lifecycle
    }

    /// Decodes a closed wire envelope and immediately subjects it to the
    /// stateful admission gate. Callers that route Apps traffic use this entry
    /// point rather than decoding an envelope and dispatching it directly.
    ///
    /// A successful `ui/initialize` response still requires the Host to call
    /// [`Self::initialization_response_committed`] after it has committed that
    /// response to its transport, before accepting `initialized`.
    pub fn decode_and_admit(
        &mut self,
        direction: McpAppsBridgeDirection,
        input: &str,
    ) -> Result<McpAppsJsonRpcEnvelope, McpAppsBridgeError> {
        let envelope = McpAppsJsonRpcEnvelope::decode(direction, input)?;
        match &envelope {
            McpAppsJsonRpcEnvelope::Request {
                id,
                method,
                progress_token,
                ..
            } => self.admit_request(direction, id.clone(), *method, progress_token.clone())?,
            McpAppsJsonRpcEnvelope::Notification { method, params } => {
                self.admit_notification(direction, *method, params.as_ref())?;
            }
            McpAppsJsonRpcEnvelope::Response { id, result } => {
                self.complete_response(opposite_direction(direction), id, result)?;
            }
            McpAppsJsonRpcEnvelope::Error { id, .. } => {
                self.complete_error(opposite_direction(direction), id)?;
            }
        }
        Ok(envelope)
    }

    /// Admits one direction-correct request only in the lifecycle phase that
    /// owns it, atomically reserving its correlation before dispatch.
    pub fn admit_request(
        &mut self,
        direction: McpAppsBridgeDirection,
        id: McpAppsJsonRpcRequestId,
        method: McpAppsRoutedMethod,
        progress_token: Option<McpAppsJsonRpcRequestId>,
    ) -> Result<(), McpAppsBridgeError> {
        if method == McpAppsRoutedMethod::SamplingCreateMessageRejected {
            return Err(McpAppsBridgeError::InvalidMethodDirection);
        }
        match method {
            McpAppsRoutedMethod::Initialize => {
                if direction != McpAppsBridgeDirection::ViewToHost
                    || self.lifecycle != McpAppsBridgeLifecycle::New
                {
                    return Err(McpAppsBridgeError::InvalidLifecycle);
                }
                self.reserve_and_bind(direction, id, method, progress_token)?;
                self.lifecycle = McpAppsBridgeLifecycle::InitializeInFlight;
                Ok(())
            }
            McpAppsRoutedMethod::Ping => {
                if self.lifecycle == McpAppsBridgeLifecycle::Closing
                    || self.lifecycle == McpAppsBridgeLifecycle::Closed
                    || (direction == McpAppsBridgeDirection::HostToView
                        && self.lifecycle != McpAppsBridgeLifecycle::Active)
                {
                    return Err(McpAppsBridgeError::InvalidLifecycle);
                }
                self.reserve_and_bind(direction, id, method, progress_token)
            }
            _ if self.lifecycle == McpAppsBridgeLifecycle::Active => {
                self.reserve_and_bind(direction, id, method, progress_token)
            }
            _ => Err(McpAppsBridgeError::InvalidLifecycle),
        }
    }

    /// Commits the successful `ui/initialize` response before the View's sole
    /// `ui/notifications/initialized` activation acknowledgement.
    pub fn initialization_response_committed(&mut self) -> Result<(), McpAppsBridgeError> {
        if self.lifecycle != McpAppsBridgeLifecycle::InitializeInFlight {
            return Err(McpAppsBridgeError::InvalidLifecycle);
        }
        self.lifecycle = McpAppsBridgeLifecycle::AwaitingInitialized;
        Ok(())
    }

    /// Begins one Host-approved teardown, atomically blocking new Apps
    /// application traffic while cleanup is prepared.
    pub fn begin_teardown(&mut self) -> Result<(), McpAppsBridgeError> {
        if self.lifecycle != McpAppsBridgeLifecycle::Active {
            return Err(McpAppsBridgeError::InvalidLifecycle);
        }
        self.lifecycle = McpAppsBridgeLifecycle::Closing;
        Ok(())
    }

    /// Commits a prepared teardown and releases every retained request and
    /// progress-token binding exactly once.
    pub fn commit_teardown(&mut self) -> Result<(), McpAppsBridgeError> {
        if self.lifecycle != McpAppsBridgeLifecycle::Closing {
            return Err(McpAppsBridgeError::InvalidLifecycle);
        }
        self.correlations = McpAppsBridgeCorrelations::default();
        self.view_progress.clear();
        self.host_progress.clear();
        self.lifecycle = McpAppsBridgeLifecycle::Closed;
        Ok(())
    }

    /// Rolls back a prepared but uncommitted teardown without disturbing live
    /// correlations or progress-token bindings.
    pub fn rollback_teardown(&mut self) -> Result<(), McpAppsBridgeError> {
        if self.lifecycle != McpAppsBridgeLifecycle::Closing {
            return Err(McpAppsBridgeError::InvalidLifecycle);
        }
        self.lifecycle = McpAppsBridgeLifecycle::Active;
        Ok(())
    }

    /// Admits one notification with lifecycle and direction rules. The four
    /// pre-Active View compatibility notifications are deliberately inert.
    pub fn admit_notification(
        &mut self,
        direction: McpAppsBridgeDirection,
        method: McpAppsRoutedMethod,
        params: Option<&Value>,
    ) -> Result<Option<McpAppsControlDisposition>, McpAppsBridgeError> {
        match method {
            McpAppsRoutedMethod::Initialized => {
                if direction != McpAppsBridgeDirection::ViewToHost
                    || self.lifecycle != McpAppsBridgeLifecycle::AwaitingInitialized
                {
                    return Err(McpAppsBridgeError::InvalidLifecycle);
                }
                self.lifecycle = McpAppsBridgeLifecycle::Active;
                Ok(None)
            }
            McpAppsRoutedMethod::Progress | McpAppsRoutedMethod::Cancelled => {
                self.admit_control(direction, method, params).map(Some)
            }
            _ if self.lifecycle == McpAppsBridgeLifecycle::Active => Ok(None),
            McpAppsRoutedMethod::SizeChanged
            | McpAppsRoutedMethod::RequestTeardown
            | McpAppsRoutedMethod::AppToolsListChanged
            | McpAppsRoutedMethod::LoggingMessage
                if direction == McpAppsBridgeDirection::ViewToHost =>
            {
                Ok(None)
            }
            _ => Err(McpAppsBridgeError::InvalidLifecycle),
        }
    }

    /// Binds one exact transport progress token to its originating live
    /// request. Progress notifications select this binding from the opposite
    /// carrier direction.
    fn bind_progress(
        &mut self,
        direction: McpAppsBridgeDirection,
        request_id: &McpAppsJsonRpcRequestId,
        progress_token: Option<McpAppsJsonRpcRequestId>,
    ) -> Result<(), McpAppsBridgeError> {
        let Some(progress_token) = progress_token else {
            return Ok(());
        };
        let bindings = match direction {
            McpAppsBridgeDirection::ViewToHost => &mut self.view_progress,
            McpAppsBridgeDirection::HostToView => &mut self.host_progress,
        };
        if bindings.len() >= MAX_MCP_APPS_BRIDGE_IN_FLIGHT
            || bindings
                .insert(progress_token, request_id.clone())
                .is_some()
        {
            return Err(McpAppsBridgeError::DuplicateLiveRequest);
        }
        Ok(())
    }

    fn reserve_and_bind(
        &mut self,
        direction: McpAppsBridgeDirection,
        id: McpAppsJsonRpcRequestId,
        method: McpAppsRoutedMethod,
        progress_token: Option<McpAppsJsonRpcRequestId>,
    ) -> Result<(), McpAppsBridgeError> {
        self.correlations
            .reserve_request(direction, id.clone(), method)?;
        if let Err(error) = self.bind_progress(direction, &id, progress_token) {
            let _ = self.correlations.complete(direction, &id);
            return Err(error);
        }
        Ok(())
    }

    /// Admits progress only for a live opposite-origin request and cancellation
    /// only for a live request previously issued in the same direction.
    pub fn admit_control(
        &self,
        direction: McpAppsBridgeDirection,
        method: McpAppsRoutedMethod,
        params: Option<&Value>,
    ) -> Result<McpAppsControlDisposition, McpAppsBridgeError> {
        match method {
            McpAppsRoutedMethod::Progress => {
                if direction == McpAppsBridgeDirection::HostToView
                    && self.lifecycle != McpAppsBridgeLifecycle::Active
                {
                    return Err(McpAppsBridgeError::InvalidLifecycle);
                }
                let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
                let progress: McpAppsProgressControlParams = serde_json::from_value(value.clone())
                    .map_err(|_| McpAppsBridgeError::InvalidParams)?;
                McpAppsProgressControlParams::try_new(
                    progress.progress_token.clone(),
                    progress.progress,
                    progress.total,
                    progress.message,
                )?;
                let request_direction = opposite_direction(direction);
                let bindings = match request_direction {
                    McpAppsBridgeDirection::ViewToHost => &self.view_progress,
                    McpAppsBridgeDirection::HostToView => &self.host_progress,
                };
                let request_id = bindings
                    .get(&progress.progress_token)
                    .ok_or(McpAppsBridgeError::UnknownProgressToken)?;
                self.correlations
                    .contains(request_direction, request_id)
                    .then(|| McpAppsControlDisposition::Bound(request_id.clone()))
                    .ok_or(McpAppsBridgeError::UnknownProgressToken)
            }
            McpAppsRoutedMethod::Cancelled => {
                let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
                let cancelled: McpAppsCancelledControlParams =
                    serde_json::from_value(value.clone())
                        .map_err(|_| McpAppsBridgeError::InvalidParams)?;
                McpAppsCancelledControlParams::try_new(
                    cancelled.request_id.clone(),
                    cancelled.reason,
                )?;
                if let Some(request_id) = cancelled.request_id {
                    return self
                        .correlations
                        .contains(direction, &request_id)
                        .then_some(McpAppsControlDisposition::Bound(request_id))
                        .ok_or(McpAppsBridgeError::UnknownCorrelation);
                }
                match (direction, self.lifecycle) {
                    (
                        McpAppsBridgeDirection::ViewToHost,
                        McpAppsBridgeLifecycle::New
                        | McpAppsBridgeLifecycle::InitializeInFlight
                        | McpAppsBridgeLifecycle::AwaitingInitialized
                        | McpAppsBridgeLifecycle::Active,
                    )
                    | (McpAppsBridgeDirection::HostToView, McpAppsBridgeLifecycle::Active) => {
                        Ok(McpAppsControlDisposition::Inert)
                    }
                    _ => Err(McpAppsBridgeError::InvalidLifecycle),
                }
            }
            _ => Err(McpAppsBridgeError::InvalidMethodDirection),
        }
    }

    /// Completes one validated terminal result and atomically releases any
    /// exact-direction progress-token binding owned by the request.
    pub fn complete_response(
        &mut self,
        request_direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
        result: &Value,
    ) -> Result<McpAppsRoutedMethod, McpAppsBridgeError> {
        let method = self
            .correlations
            .complete_response(request_direction, id, result)?;
        let bindings = match request_direction {
            McpAppsBridgeDirection::ViewToHost => &mut self.view_progress,
            McpAppsBridgeDirection::HostToView => &mut self.host_progress,
        };
        bindings.retain(|_, request_id| request_id != id);
        Ok(method)
    }

    /// Completes one terminal error and releases any exact-direction progress
    /// binding owned by the failed request.
    pub fn complete_error(
        &mut self,
        request_direction: McpAppsBridgeDirection,
        id: &McpAppsJsonRpcRequestId,
    ) -> Result<McpAppsRoutedMethod, McpAppsBridgeError> {
        let method = self.correlations.complete_error(request_direction, id)?;
        let bindings = match request_direction {
            McpAppsBridgeDirection::ViewToHost => &mut self.view_progress,
            McpAppsBridgeDirection::HostToView => &mut self.host_progress,
        };
        bindings.retain(|_, request_id| request_id != id);
        Ok(method)
    }
}

const fn opposite_direction(direction: McpAppsBridgeDirection) -> McpAppsBridgeDirection {
    match direction {
        McpAppsBridgeDirection::ViewToHost => McpAppsBridgeDirection::HostToView,
        McpAppsBridgeDirection::HostToView => McpAppsBridgeDirection::ViewToHost,
    }
}

impl McpAppsHostIdAllocator {
    /// Allocates IDs beginning at one and never crossing the JS-safe boundary.
    pub fn allocate(&mut self) -> Result<McpAppsJsonRpcRequestId, McpAppsBridgeError> {
        let next = if self.next == 0 { 1 } else { self.next };
        if next > MAX_MCP_APPS_BRIDGE_SAFE_INTEGER {
            return Err(McpAppsBridgeError::RequestIdExhausted);
        }
        let id = McpAppsJsonRpcRequestId::host(next)?;
        self.next = next
            .checked_add(1)
            .ok_or(McpAppsBridgeError::RequestIdExhausted)?;
        Ok(id)
    }
}

fn parse_raw_request_id(
    raw: Option<&RawValue>,
    value: Value,
) -> Result<McpAppsJsonRpcRequestId, McpAppsBridgeError> {
    let raw = raw.ok_or(McpAppsBridgeError::InvalidRequestId)?;
    if raw.get().len() > MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES {
        return Err(McpAppsBridgeError::RequestIdTooLong);
    }
    // Decode only after charging the exact quoted/escaped token or numeric
    // lexeme. The parsed Value is retained solely to ensure both serde passes
    // observed the same concrete envelope member.
    let parsed: McpAppsJsonRpcRequestId =
        serde_json::from_str(raw.get()).map_err(|_| McpAppsBridgeError::InvalidRequestId)?;
    if serde_json::to_value(&parsed).ok() != Some(value) {
        return Err(McpAppsBridgeError::InvalidRequestId);
    }
    Ok(parsed)
}

/// Retains a nested request-wrapper progress token as raw JSON when present.
///
/// A non-object parameter value, or malformed/non-object `_meta`, is left for
/// the closed parameter validator. A present raw token is instead charged
/// before its escaped spelling is decoded or normalized.
fn raw_progress_token(params: Option<&RawValue>) -> Option<Box<RawValue>> {
    let params = params?;
    serde_json::from_str::<McpAppsRawParams>(params.get())
        .ok()?
        .meta?
        .progress_token
}

fn split_progress_wrapper(
    params: Option<Value>,
    raw_progress_token: Option<&RawValue>,
) -> Result<(Option<Value>, Option<McpAppsJsonRpcRequestId>), McpAppsBridgeError> {
    match params {
        Some(Value::Object(mut params)) => {
            let Some(meta) = params.remove("_meta") else {
                return Ok((Some(Value::Object(params)), None));
            };
            let Value::Object(mut meta) = meta else {
                return Err(McpAppsBridgeError::InvalidParams);
            };
            let Some(token) = meta.remove("progressToken") else {
                return Err(McpAppsBridgeError::InvalidParams);
            };
            if !meta.is_empty() {
                return Err(McpAppsBridgeError::InvalidParams);
            }
            let progress_token = parse_raw_request_id(raw_progress_token, token)?;
            Ok((Some(Value::Object(params)), Some(progress_token)))
        }
        other => Ok((other, None)),
    }
}

fn add_progress_wrapper(
    params: Value,
    progress_token: Option<&McpAppsJsonRpcRequestId>,
) -> Result<Value, McpAppsBridgeError> {
    let Some(progress_token) = progress_token else {
        return Ok(params);
    };
    let Value::Object(mut params) = params else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    if params.contains_key("_meta") {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    params.insert(
        "_meta".into(),
        serde_json::json!({ "progressToken": progress_token }),
    );
    Ok(Value::Object(params))
}

fn route_method(
    direction: McpAppsBridgeDirection,
    method: &str,
    notification: bool,
) -> Result<McpAppsRoutedMethod, McpAppsBridgeError> {
    use McpAppsBridgeDirection::{HostToView, ViewToHost};
    let routed = match (direction, notification, method) {
        (ViewToHost, false, "ui/initialize") => McpAppsRoutedMethod::Initialize,
        (ViewToHost, false, "ui/open-link") => McpAppsRoutedMethod::OpenLink,
        (ViewToHost, false, "ui/download-file") => McpAppsRoutedMethod::DownloadFile,
        (ViewToHost, false, "ui/message") => McpAppsRoutedMethod::Message,
        (ViewToHost, false, "ui/update-model-context") => McpAppsRoutedMethod::UpdateModelContext,
        (ViewToHost, false, "ui/request-display-mode") => McpAppsRoutedMethod::RequestDisplayMode,
        (ViewToHost, false, "tools/call") => McpAppsRoutedMethod::ToolsCall,
        (ViewToHost, false, "resources/read") => McpAppsRoutedMethod::ResourcesRead,
        (ViewToHost, false, "resources/list") => McpAppsRoutedMethod::ResourcesList,
        (ViewToHost, false, "resources/templates/list") => {
            McpAppsRoutedMethod::ResourceTemplatesList
        }
        (ViewToHost, false, "prompts/list") => McpAppsRoutedMethod::PromptsList,
        (ViewToHost, false, "sampling/createMessage") => {
            McpAppsRoutedMethod::SamplingCreateMessageRejected
        }
        (ViewToHost, false, "ping") => McpAppsRoutedMethod::Ping,
        (HostToView, false, "ui/resource-teardown") => McpAppsRoutedMethod::ResourceTeardown,
        (HostToView, false, "tools/list") => McpAppsRoutedMethod::ToolsList,
        (HostToView, false, "tools/call") => McpAppsRoutedMethod::ToolsCall,
        (HostToView, false, "ping") => McpAppsRoutedMethod::Ping,
        (ViewToHost, true, "ui/notifications/initialized") => McpAppsRoutedMethod::Initialized,
        (ViewToHost, true, "ui/notifications/size-changed") => McpAppsRoutedMethod::SizeChanged,
        (ViewToHost, true, "ui/notifications/request-teardown") => {
            McpAppsRoutedMethod::RequestTeardown
        }
        (ViewToHost, true, "notifications/tools/list_changed") => {
            McpAppsRoutedMethod::AppToolsListChanged
        }
        (ViewToHost, true, "notifications/message") => McpAppsRoutedMethod::LoggingMessage,
        (ViewToHost, true, "notifications/progress") => McpAppsRoutedMethod::Progress,
        (ViewToHost, true, "notifications/cancelled") => McpAppsRoutedMethod::Cancelled,
        (HostToView, true, "notifications/tools/list_changed") => {
            McpAppsRoutedMethod::ToolsListChanged
        }
        (HostToView, true, "notifications/resources/list_changed") => {
            McpAppsRoutedMethod::ResourcesListChanged
        }
        (HostToView, true, "notifications/prompts/list_changed") => {
            McpAppsRoutedMethod::PromptsListChanged
        }
        (HostToView, true, "ui/notifications/tool-input") => McpAppsRoutedMethod::ToolInput,
        (HostToView, true, "ui/notifications/tool-input-partial") => {
            McpAppsRoutedMethod::ToolInputPartial
        }
        (HostToView, true, "ui/notifications/tool-result") => McpAppsRoutedMethod::ToolResult,
        (HostToView, true, "ui/notifications/tool-cancelled") => McpAppsRoutedMethod::ToolCancelled,
        (HostToView, true, "ui/notifications/host-context-changed") => {
            McpAppsRoutedMethod::HostContextChanged
        }
        (HostToView, true, "notifications/progress") => McpAppsRoutedMethod::Progress,
        (HostToView, true, "notifications/cancelled") => McpAppsRoutedMethod::Cancelled,
        _ => return Err(McpAppsBridgeError::InvalidMethodDirection),
    };
    Ok(routed)
}

fn method_name(method: McpAppsRoutedMethod) -> &'static str {
    match method {
        McpAppsRoutedMethod::Initialize => "ui/initialize",
        McpAppsRoutedMethod::OpenLink => "ui/open-link",
        McpAppsRoutedMethod::DownloadFile => "ui/download-file",
        McpAppsRoutedMethod::Message => "ui/message",
        McpAppsRoutedMethod::UpdateModelContext => "ui/update-model-context",
        McpAppsRoutedMethod::RequestDisplayMode => "ui/request-display-mode",
        McpAppsRoutedMethod::ResourceTeardown => "ui/resource-teardown",
        McpAppsRoutedMethod::ToolsList => "tools/list",
        McpAppsRoutedMethod::ToolsCall => "tools/call",
        McpAppsRoutedMethod::ResourcesList => "resources/list",
        McpAppsRoutedMethod::ResourceTemplatesList => "resources/templates/list",
        McpAppsRoutedMethod::ResourcesRead => "resources/read",
        McpAppsRoutedMethod::PromptsList => "prompts/list",
        McpAppsRoutedMethod::SamplingCreateMessageRejected => "sampling/createMessage",
        McpAppsRoutedMethod::Ping => "ping",
        McpAppsRoutedMethod::Initialized => "ui/notifications/initialized",
        McpAppsRoutedMethod::SizeChanged => "ui/notifications/size-changed",
        McpAppsRoutedMethod::RequestTeardown => "ui/notifications/request-teardown",
        McpAppsRoutedMethod::AppToolsListChanged | McpAppsRoutedMethod::ToolsListChanged => {
            "notifications/tools/list_changed"
        }
        McpAppsRoutedMethod::LoggingMessage => "notifications/message",
        McpAppsRoutedMethod::ToolInput => "ui/notifications/tool-input",
        McpAppsRoutedMethod::ToolInputPartial => "ui/notifications/tool-input-partial",
        McpAppsRoutedMethod::ToolResult => "ui/notifications/tool-result",
        McpAppsRoutedMethod::ToolCancelled => "ui/notifications/tool-cancelled",
        McpAppsRoutedMethod::HostContextChanged => "ui/notifications/host-context-changed",
        McpAppsRoutedMethod::ResourcesListChanged => "notifications/resources/list_changed",
        McpAppsRoutedMethod::PromptsListChanged => "notifications/prompts/list_changed",
        McpAppsRoutedMethod::Progress => "notifications/progress",
        McpAppsRoutedMethod::Cancelled => "notifications/cancelled",
    }
}

fn validate_routed_payload(
    method: McpAppsRoutedMethod,
    params: Option<&Value>,
) -> Result<(), McpAppsBridgeError> {
    match method {
        McpAppsRoutedMethod::Ping => validate_empty_or_omitted(params),
        McpAppsRoutedMethod::Initialized
        | McpAppsRoutedMethod::RequestTeardown
        | McpAppsRoutedMethod::AppToolsListChanged
        | McpAppsRoutedMethod::ToolsListChanged
        | McpAppsRoutedMethod::ResourcesListChanged
        | McpAppsRoutedMethod::PromptsListChanged => validate_empty_or_omitted(params),
        McpAppsRoutedMethod::ResourceTeardown => validate_exact_object(params, &[]),
        McpAppsRoutedMethod::Progress => validate_progress(params),
        McpAppsRoutedMethod::Cancelled => validate_cancelled(params),
        McpAppsRoutedMethod::Initialize => validate_initialize(params),
        McpAppsRoutedMethod::OpenLink => validate_typed::<McpAppsOpenLinkParams>(params),
        McpAppsRoutedMethod::DownloadFile => validate_typed::<McpAppsDownloadFileParams>(params),
        McpAppsRoutedMethod::Message => validate_typed::<McpAppsMessageParams>(params),
        McpAppsRoutedMethod::UpdateModelContext => {
            validate_typed::<McpAppsUpdateModelContextParams>(params)
        }
        McpAppsRoutedMethod::RequestDisplayMode => {
            validate_typed::<McpAppsDisplayModeParams>(params)
        }
        McpAppsRoutedMethod::ToolsCall => validate_typed::<McpAppsToolCallParams>(params),
        McpAppsRoutedMethod::ResourcesRead => validate_typed::<McpAppsResourceReadParams>(params),
        McpAppsRoutedMethod::ResourcesList
        | McpAppsRoutedMethod::ResourceTemplatesList
        | McpAppsRoutedMethod::PromptsList
        | McpAppsRoutedMethod::ToolsList => validate_list_params(params),
        McpAppsRoutedMethod::SizeChanged => validate_size_changed(params),
        McpAppsRoutedMethod::LoggingMessage => {
            validate_typed::<McpAppsLogMessageNotification>(params)
        }
        McpAppsRoutedMethod::ToolInput | McpAppsRoutedMethod::ToolInputPartial => {
            validate_tool_input(params)
        }
        McpAppsRoutedMethod::ToolResult => require_bounded(params),
        McpAppsRoutedMethod::ToolCancelled => validate_tool_cancelled(params),
        McpAppsRoutedMethod::HostContextChanged => validate_host_context(params),
        McpAppsRoutedMethod::SamplingCreateMessageRejected => require_bounded(params),
    }
}

fn validate_initialize(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    if !bounded_json(value, 0) {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    let initialize: McpAppsPinnedInitializeParams =
        serde_json::from_value(value.clone()).map_err(|_| McpAppsBridgeError::InvalidParams)?;
    (initialize.protocol_version == MCP_APPS_HOST_VIEW_PROTOCOL_VERSION)
        .then_some(())
        .ok_or(McpAppsBridgeError::InvalidParams)
}

fn validate_host_context(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    if !bounded_json(value, 0)
        || serde_json::from_value::<McpAppsPinnedHostContext>(value.clone()).is_err()
    {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    let Value::Object(context) = value else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    if context
        .get("theme")
        .is_some_and(|theme| !matches!(theme.as_str(), Some("light" | "dark")))
        || context.get("platform").is_some_and(|platform| {
            !matches!(platform.as_str(), Some("web" | "desktop" | "mobile"))
        })
        || !validate_optional_styles(context.get("styles"))
        || !validate_optional_dimensions(context.get("containerDimensions"))
        || !validate_optional_device_capabilities(context.get("deviceCapabilities"))
        || !validate_optional_safe_area(context.get("safeAreaInsets"))
    {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    Ok(())
}

fn validate_optional_styles(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let Value::Object(styles) = value else {
        return false;
    };
    if styles
        .keys()
        .any(|key| !matches!(key.as_str(), "variables" | "css"))
    {
        return false;
    }
    if let Some(variables) = styles.get("variables") {
        let Value::Object(variables) = variables else {
            return false;
        };
        if variables.values().any(|value| !value.is_string()) {
            return false;
        }
    }
    if let Some(css) = styles.get("css") {
        let Value::Object(css) = css else {
            return false;
        };
        if css.len() > 1 || css.get("fonts").is_some_and(|fonts| !fonts.is_string()) {
            return false;
        }
    }
    true
}

fn validate_optional_dimensions(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let Value::Object(dimensions) = value else {
        return false;
    };
    if dimensions
        .keys()
        .any(|key| !matches!(key.as_str(), "width" | "maxWidth" | "height" | "maxHeight"))
    {
        return false;
    }
    let width = dimensions.contains_key("width") as u8 + dimensions.contains_key("maxWidth") as u8;
    let height =
        dimensions.contains_key("height") as u8 + dimensions.contains_key("maxHeight") as u8;
    width <= 1
        && height <= 1
        && dimensions
            .values()
            .all(|value| value.as_f64().is_some_and(f64::is_finite))
}

fn validate_optional_device_capabilities(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let Value::Object(capabilities) = value else {
        return false;
    };
    capabilities
        .keys()
        .all(|key| matches!(key.as_str(), "touch" | "hover"))
        && capabilities.values().all(Value::is_boolean)
}

fn validate_optional_safe_area(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let Value::Object(insets) = value else {
        return false;
    };
    ["top", "right", "bottom", "left"].into_iter().all(|key| {
        insets
            .get(key)
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
    }) && insets.len() == 4
}

fn validate_typed<T>(params: Option<&Value>) -> Result<(), McpAppsBridgeError>
where
    T: serde::de::DeserializeOwned,
{
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    if !bounded_json(value, 0) || serde_json::from_value::<T>(value.clone()).is_err() {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    Ok(())
}

fn validate_empty_or_omitted(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    match params {
        None => Ok(()),
        Some(Value::Object(object)) if object.is_empty() => Ok(()),
        _ => Err(McpAppsBridgeError::InvalidParams),
    }
}

fn validate_exact_object(
    params: Option<&Value>,
    allowed: &[&str],
) -> Result<(), McpAppsBridgeError> {
    let Some(Value::Object(object)) = params else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    if object.len() > allowed.len()
        || object.keys().any(|key| !allowed.contains(&key.as_str()))
        || !bounded_json(&Value::Object(object.clone()), 0)
    {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    Ok(())
}

fn validate_list_params(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    match params {
        None => Ok(()),
        Some(Value::Object(object)) if object.len() <= 1 => {
            if object.keys().any(|key| key != "cursor") {
                return Err(McpAppsBridgeError::InvalidParams);
            }
            let Some(cursor) = object.get("cursor") else {
                return Ok(());
            };
            if cursor
                .as_str()
                .is_some_and(|cursor| cursor.len() <= MAX_MCP_APPS_BRIDGE_TEXT_BYTES)
            {
                Ok(())
            } else {
                Err(McpAppsBridgeError::InvalidParams)
            }
        }
        _ => Err(McpAppsBridgeError::InvalidParams),
    }
}

fn validate_size_changed(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    reject_explicit_null_members(value, &["width", "height"])?;
    let size: McpAppsSizeChangedParams =
        serde_json::from_value(value.clone()).map_err(|_| McpAppsBridgeError::InvalidParams)?;
    McpAppsSizeChangedParams::try_new(size.width, size.height).map(|_| ())
}

fn validate_tool_input(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    reject_explicit_null_members(value, &["arguments"])?;
    let input: McpAppsToolInputParams =
        serde_json::from_value(value.clone()).map_err(|_| McpAppsBridgeError::InvalidParams)?;
    McpAppsToolInputParams::try_new(input.arguments).map(|_| ())
}

fn validate_tool_cancelled(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    reject_explicit_null_members(value, &["reason"])?;
    let cancelled: McpAppsToolCancelledParams =
        serde_json::from_value(value.clone()).map_err(|_| McpAppsBridgeError::InvalidParams)?;
    McpAppsToolCancelledParams::try_new(cancelled.reason).map(|_| ())
}

fn reject_explicit_null_members(value: &Value, members: &[&str]) -> Result<(), McpAppsBridgeError> {
    let Value::Object(object) = value else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    if members
        .iter()
        .any(|member| object.get(*member).is_some_and(Value::is_null))
    {
        Err(McpAppsBridgeError::InvalidParams)
    } else {
        Ok(())
    }
}

fn validate_progress(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    let progress: McpAppsProgressControlParams =
        serde_json::from_value(value.clone()).map_err(|_| McpAppsBridgeError::InvalidParams)?;
    McpAppsProgressControlParams::try_new(
        progress.progress_token,
        progress.progress,
        progress.total,
        progress.message,
    )
    .map(|_| ())
}

fn validate_cancelled(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    let value = params.ok_or(McpAppsBridgeError::InvalidParams)?;
    let cancelled: McpAppsCancelledControlParams =
        serde_json::from_value(value.clone()).map_err(|_| McpAppsBridgeError::InvalidParams)?;
    McpAppsCancelledControlParams::try_new(cancelled.request_id, cancelled.reason).map(|_| ())
}

fn require_bounded(params: Option<&Value>) -> Result<(), McpAppsBridgeError> {
    params
        .filter(|value| bounded_json(value, 0))
        .map(|_| ())
        .ok_or(McpAppsBridgeError::InvalidParams)
}

fn validate_forward_open_result(result: &Value) -> Result<(), McpAppsBridgeError> {
    let Value::Object(object) = result else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    reject_reserved_result_members(result)?;
    if object
        .get("isError")
        .is_some_and(|is_error| !is_error.is_boolean())
        || !bounded_json(result, 0)
    {
        return Err(McpAppsBridgeError::InvalidParams);
    }
    Ok(())
}

fn validate_display_mode_result(result: &Value) -> Result<(), McpAppsBridgeError> {
    let Value::Object(object) = result else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    reject_reserved_result_members(result)?;
    let Some(mode) = object.get("mode") else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    serde_json::from_value::<McpAppsDisplayMode>(mode.clone())
        .map_err(|_| McpAppsBridgeError::InvalidParams)?;
    bounded_json(result, 0)
        .then_some(())
        .ok_or(McpAppsBridgeError::InvalidParams)
}

fn reject_reserved_result_members(result: &Value) -> Result<(), McpAppsBridgeError> {
    let Value::Object(object) = result else {
        return Err(McpAppsBridgeError::InvalidParams);
    };
    if object.contains_key("_meta") || object.contains_key("resultType") {
        Err(McpAppsBridgeError::InvalidParams)
    } else {
        Ok(())
    }
}

fn bounded_json(value: &Value, depth: usize) -> bool {
    if depth > MAX_MCP_APPS_BRIDGE_JSON_DEPTH {
        return false;
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(value) => value.len() <= MAX_MCP_APPS_BRIDGE_TEXT_BYTES,
        Value::Array(values) => {
            values.len() <= MAX_MCP_APPS_BRIDGE_JSON_ARRAY_ITEMS
                && values.iter().all(|value| bounded_json(value, depth + 1))
        }
        Value::Object(values) => {
            values.len() <= MAX_MCP_APPS_BRIDGE_JSON_OBJECT_MEMBERS
                && values.iter().all(|(key, value)| {
                    key.len() <= MAX_MCP_APPS_BRIDGE_TEXT_BYTES && bounded_json(value, depth + 1)
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn apps_activation_receipt(mime_type: &str) -> Option<McpAppsActivationReceipt> {
        use crate::extensions::{
            ClientExtensionDiscovery, ExtensionDescriptorRegistry, ExtensionLocalEnablement,
            ExtensionSettings, ServerExtensionDiscovery, official_mcp_apps_empty_server_settings,
            official_mcp_apps_negotiation_resolver, register_official_mcp_apps_extension,
        };
        use crate::protocol_policy::ProtocolEra;

        let mut registry = ExtensionDescriptorRegistry::new();
        let id = register_official_mcp_apps_extension(&mut registry)
            .expect("the frozen official Apps descriptor registers");
        registry.freeze().expect("the Apps registry freezes");
        let client = ClientExtensionDiscovery {
            extensions: BTreeMap::from([(
                id.clone(),
                ExtensionSettings::new(json!({"mimeTypes": [mime_type]}))
                    .expect("the bounded client settings are an object"),
            )]),
        };
        let server = ServerExtensionDiscovery {
            extensions: BTreeMap::from([(id.clone(), official_mcp_apps_empty_server_settings())]),
        };
        let mut local = ExtensionLocalEnablement::default();
        local.enable(id);
        let mut resolver = official_mcp_apps_negotiation_resolver();
        registry
            .negotiate(
                ProtocolEra::Modern2026,
                &local,
                &client,
                &server,
                &mut resolver,
            )
            .expect("Apps negotiation is structurally valid")
            .mcp_apps_activation_receipt(&registry)
    }

    fn active_admission() -> McpAppsBridgeAdmission {
        let mut admission = McpAppsBridgeAdmission::new(
            apps_activation_receipt(crate::extensions::MCP_APPS_HTML_MIME_TYPE)
                .expect("active official Apps negotiation yields the opaque bridge receipt"),
        );
        admission
            .admit_request(
                McpAppsBridgeDirection::ViewToHost,
                McpAppsJsonRpcRequestId::new(0).unwrap(),
                McpAppsRoutedMethod::Initialize,
                None,
            )
            .expect("only the View initializes a new admission");
        admission
            .initialization_response_committed()
            .expect("the initialize response commits");
        admission
            .admit_notification(
                McpAppsBridgeDirection::ViewToHost,
                McpAppsRoutedMethod::Initialized,
                Some(&json!({})),
            )
            .expect("the initialized notification activates the View");
        admission
    }

    #[test]
    fn sandbox_signals_remain_outside_the_host_view_message_unions() {
        let proxy = McpAppsSandboxSignal::ProxyReady;
        let resource = McpAppsSandboxSignal::ResourceReady {
            html: "<main></main>".to_owned(),
            sandbox: None,
            csp: None,
            permissions: None,
        };

        assert!(matches!(proxy, McpAppsSandboxSignal::ProxyReady));
        assert!(matches!(
            resource,
            McpAppsSandboxSignal::ResourceReady { .. }
        ));

        // `McpAppsSandboxSignal` is intentionally a different type from both
        // regular carrier unions, so it cannot be dispatched accidentally.
        assert_ne!(
            std::any::type_name::<McpAppsSandboxSignal>(),
            std::any::type_name::<McpAppsHostToView>()
        );
    }

    #[test]
    fn incoming_zero_and_string_ids_are_preserved_while_host_ids_are_positive_and_safe() {
        assert_eq!(
            McpAppsJsonRpcRequestId::new(0).unwrap().as_number(),
            Some(0)
        );
        assert_eq!(
            McpAppsJsonRpcRequestId::string("view-request".into()).unwrap(),
            McpAppsJsonRpcRequestId::String("view-request".into())
        );
        assert_eq!(
            McpAppsJsonRpcRequestId::host(0),
            Err(McpAppsBridgeError::ZeroRequestId)
        );
        assert_eq!(
            McpAppsJsonRpcRequestId::new(MAX_MCP_APPS_BRIDGE_SAFE_INTEGER + 1),
            Err(McpAppsBridgeError::UnsafeRequestId)
        );
        assert_eq!(
            McpAppsResourceTeardownParams::try_new(Some(
                "x".repeat(MAX_MCP_APPS_BRIDGE_TEXT_BYTES + 1)
            )),
            Err(McpAppsBridgeError::TextTooLong)
        );
    }

    #[test]
    fn wire_initialize_preserves_a_string_id_exact_method_and_progress_wrapper() {
        let wire = r#"{"jsonrpc":"2.0","id":"view-0","method":"ui/initialize","params":{"appInfo":{"name":"view","version":"1"},"appCapabilities":{"tools":{"listChanged":true},"availableDisplayModes":["inline"]},"protocolVersion":"2026-01-26","_meta":{"progressToken":0}}}"#;
        let decoded = McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, wire)
            .expect("pinned initialize envelope is admitted");
        let McpAppsJsonRpcEnvelope::Request {
            id,
            method,
            params,
            progress_token,
        } = &decoded
        else {
            panic!("expected routed request");
        };
        assert_eq!(id, &McpAppsJsonRpcRequestId::String("view-0".into()));
        assert_eq!(*method, McpAppsRoutedMethod::Initialize);
        assert_eq!(progress_token.as_ref().unwrap().as_number(), Some(0));
        assert_eq!(
            params.as_ref().unwrap()["appCapabilities"]["tools"]["listChanged"],
            true
        );

        let encoded = decoded
            .encode(McpAppsBridgeDirection::ViewToHost)
            .expect("validated envelope re-encodes");
        let encoded: Value = serde_json::from_str(&encoded).expect("encoded JSON");
        assert_eq!(encoded["id"], "view-0");
        assert_eq!(encoded["method"], "ui/initialize");
        assert_eq!(encoded["params"]["_meta"]["progressToken"], 0);
    }

    #[test]
    fn wire_initialize_rejects_only_a_wrong_apps_protocol_version() {
        let accepted = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "ui/initialize",
            "params": {
                "appInfo": {"name": "view", "version": "1"},
                "appCapabilities": {},
                "protocolVersion": MCP_APPS_HOST_VIEW_PROTOCOL_VERSION,
            }
        });
        McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, &accepted.to_string())
            .expect("control wire is valid");
        let mut wrong_era = accepted;
        wrong_era["params"]["protocolVersion"] = json!("2025-11-25");
        assert_eq!(
            McpAppsJsonRpcEnvelope::decode(
                McpAppsBridgeDirection::ViewToHost,
                &wrong_era.to_string(),
            ),
            Err(McpAppsBridgeError::InvalidParams)
        );
    }

    #[test]
    fn raw_escaped_request_id_token_is_charged_before_decoding_at_n_and_n_plus_one() {
        let escaped_at_limit = format!(
            "\"{}\"",
            "\\\\".repeat((MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES - 2) / 2)
        );
        assert_eq!(escaped_at_limit.len(), MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES);
        let accepted =
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{escaped_at_limit},\"method\":\"ping\"}}");
        McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, &accepted)
            .expect("the exact raw escaped-token limit is admitted");

        let escaped_over_limit = format!(
            "\"{}a\"",
            "\\\\".repeat((MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES - 2) / 2)
        );
        assert_eq!(
            escaped_over_limit.len(),
            MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES + 1
        );
        let rejected =
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{escaped_over_limit},\"method\":\"ping\"}}");
        assert_eq!(
            McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, &rejected),
            Err(McpAppsBridgeError::RequestIdTooLong)
        );

        let token_at_limit = format!(
            "\"{}\"",
            "\\\\".repeat((MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES - 2) / 2)
        );
        let accepted_token = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"ping\",\"params\":{{\"_meta\":{{\"progressToken\":{token_at_limit}}}}}}}"
        );
        McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, &accepted_token)
            .expect("the exact raw escaped progress-token limit is admitted");

        let token_over_limit = format!(
            "\"{}a\"",
            "\\\\".repeat((MAX_MCP_APPS_BRIDGE_REQUEST_ID_BYTES - 2) / 2)
        );
        let rejected_token = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"ping\",\"params\":{{\"_meta\":{{\"progressToken\":{token_over_limit}}}}}}}"
        );
        assert_eq!(
            McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, &rejected_token),
            Err(McpAppsBridgeError::RequestIdTooLong),
            "changing only the escaped progress-token spelling beyond the raw limit rejects"
        );
    }

    #[test]
    fn wire_routes_controls_by_direction_and_rejects_the_same_method_in_the_wrong_direction() {
        let progress = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"view-token","progress":1.0,"total":2.0,"message":"half"}}"#;
        let accepted = McpAppsJsonRpcEnvelope::decode(McpAppsBridgeDirection::ViewToHost, progress)
            .expect("View progress control is routed");
        assert!(matches!(
            accepted,
            McpAppsJsonRpcEnvelope::Notification {
                method: McpAppsRoutedMethod::Progress,
                ..
            }
        ));
        assert_eq!(
            McpAppsJsonRpcEnvelope::decode(
                McpAppsBridgeDirection::HostToView,
                r#"{"jsonrpc":"2.0","method":"ui/notifications/initialized"}"#
            ),
            Err(McpAppsBridgeError::InvalidMethodDirection)
        );
    }

    #[test]
    fn wire_error_is_closed_and_rejects_one_extra_inner_member() {
        let accepted = json!({
            "jsonrpc": "2.0",
            "id": "view-error",
            "error": {"code": -32602, "message": "invalid params", "data": {"field": "name"}}
        });
        let decoded = McpAppsJsonRpcEnvelope::decode(
            McpAppsBridgeDirection::HostToView,
            &accepted.to_string(),
        )
        .expect("closed Apps error is admitted");
        assert!(matches!(
            decoded,
            McpAppsJsonRpcEnvelope::Error {
                id: McpAppsJsonRpcRequestId::String(_),
                error: McpAppsJsonRpcError { code: -32602, .. }
            }
        ));

        let mut extra_member = accepted;
        extra_member["error"]["unexpected"] = json!(true);
        assert_eq!(
            McpAppsJsonRpcEnvelope::decode(
                McpAppsBridgeDirection::HostToView,
                &extra_member.to_string(),
            ),
            Err(McpAppsBridgeError::InvalidError)
        );
    }

    #[test]
    fn correlations_preserve_the_original_request_on_one_variable_duplicate() {
        let id = McpAppsJsonRpcRequestId::string("same-id".into()).unwrap();
        let mut correlations = McpAppsBridgeCorrelations::default();
        correlations
            .reserve_request(
                McpAppsBridgeDirection::ViewToHost,
                id.clone(),
                McpAppsRoutedMethod::ToolsCall,
            )
            .expect("first request reserves its ID");
        assert_eq!(
            correlations.reserve_request(
                McpAppsBridgeDirection::ViewToHost,
                id.clone(),
                McpAppsRoutedMethod::ToolsCall,
            ),
            Err(McpAppsBridgeError::DuplicateLiveRequest)
        );
        assert!(correlations.contains(McpAppsBridgeDirection::ViewToHost, &id));
        assert_eq!(
            correlations.method(McpAppsBridgeDirection::ViewToHost, &id),
            Some(McpAppsRoutedMethod::ToolsCall)
        );
    }

    #[test]
    fn activation_lifecycle_and_directional_controls_require_one_live_binding() {
        let mut admission = active_admission();
        assert_eq!(
            admission.admit_request(
                McpAppsBridgeDirection::HostToView,
                McpAppsJsonRpcRequestId::host(1).unwrap(),
                McpAppsRoutedMethod::ToolsCall,
                None,
            ),
            Ok(())
        );
        admission
            .complete_error(
                McpAppsBridgeDirection::HostToView,
                &McpAppsJsonRpcRequestId::host(1).unwrap(),
            )
            .expect("the unrelated Host request is released");
        let request_id = McpAppsJsonRpcRequestId::string("request".into()).unwrap();
        let token = McpAppsJsonRpcRequestId::string("token".into()).unwrap();
        admission
            .admit_request(
                McpAppsBridgeDirection::ViewToHost,
                request_id.clone(),
                McpAppsRoutedMethod::ToolsCall,
                Some(token.clone()),
            )
            .expect("active View request reserves its exact progress token");
        let progress = json!({"progressToken": "token", "progress": 1.0});
        assert_eq!(
            admission
                .admit_control(
                    McpAppsBridgeDirection::HostToView,
                    McpAppsRoutedMethod::Progress,
                    Some(&progress),
                )
                .expect("opposite-direction live token binds"),
            McpAppsControlDisposition::Bound(request_id.clone())
        );
        assert_eq!(
            admission.admit_control(
                McpAppsBridgeDirection::ViewToHost,
                McpAppsRoutedMethod::Progress,
                Some(&progress),
            ),
            Err(McpAppsBridgeError::UnknownProgressToken)
        );
        let cancelled = json!({"requestId": "request"});
        assert_eq!(
            admission
                .admit_control(
                    McpAppsBridgeDirection::ViewToHost,
                    McpAppsRoutedMethod::Cancelled,
                    Some(&cancelled),
                )
                .expect("same-direction cancellation binds"),
            McpAppsControlDisposition::Bound(request_id)
        );
    }

    #[test]
    fn stateful_wire_admission_rejects_a_routable_request_before_initialized() {
        let Some(receipt) = apps_activation_receipt(crate::extensions::MCP_APPS_HTML_MIME_TYPE)
        else {
            panic!("active official Apps negotiation yields the opaque bridge receipt");
        };
        assert!(
            apps_activation_receipt("application/json").is_none(),
            "changing only the negotiated MIME member leaves Apps activation unavailable"
        );
        let mut admission = McpAppsBridgeAdmission::new(receipt);

        // `tools/call` is a valid Host-to-View wire method, but it remains
        // unrouteable through the admission entry point before activation.
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x","arguments":{}}}"#;
        assert_eq!(
            admission.decode_and_admit(McpAppsBridgeDirection::HostToView, call),
            Err(McpAppsBridgeError::InvalidLifecycle)
        );
        assert_eq!(admission.lifecycle(), McpAppsBridgeLifecycle::New);

        let initialize = r#"{"jsonrpc":"2.0","id":"view-init","method":"ui/initialize","params":{"appInfo":{"name":"view","version":"1"},"appCapabilities":{},"protocolVersion":"2026-01-26"}}"#;
        admission
            .decode_and_admit(McpAppsBridgeDirection::ViewToHost, initialize)
            .expect("only the View initialize request enters the lifecycle");
        assert_eq!(
            admission.lifecycle(),
            McpAppsBridgeLifecycle::InitializeInFlight
        );
    }

    #[test]
    fn approved_teardown_transitions_atomically_and_rollback_preserves_live_state() {
        let mut admission = active_admission();
        let request_id = McpAppsJsonRpcRequestId::string("teardown-request".into()).unwrap();
        let token = McpAppsJsonRpcRequestId::string("teardown-token".into()).unwrap();
        admission
            .admit_request(
                McpAppsBridgeDirection::ViewToHost,
                request_id.clone(),
                McpAppsRoutedMethod::ToolsCall,
                Some(token),
            )
            .expect("the active request is retained before teardown");

        admission
            .begin_teardown()
            .expect("approved teardown enters Closing");
        assert_eq!(admission.lifecycle(), McpAppsBridgeLifecycle::Closing);
        assert_eq!(
            admission.admit_request(
                McpAppsBridgeDirection::ViewToHost,
                McpAppsJsonRpcRequestId::string("blocked".into()).unwrap(),
                McpAppsRoutedMethod::ToolsCall,
                None,
            ),
            Err(McpAppsBridgeError::InvalidLifecycle)
        );
        admission
            .rollback_teardown()
            .expect("an uncommitted teardown returns atomically to Active");
        assert_eq!(admission.lifecycle(), McpAppsBridgeLifecycle::Active);
        assert_eq!(
            admission
                .admit_control(
                    McpAppsBridgeDirection::HostToView,
                    McpAppsRoutedMethod::Progress,
                    Some(&json!({"progressToken": "teardown-token", "progress": 1.0})),
                )
                .expect("rollback retains the pre-existing request/token binding"),
            McpAppsControlDisposition::Bound(request_id)
        );

        admission
            .begin_teardown()
            .expect("the same active View may begin one later approved teardown");
        admission
            .commit_teardown()
            .expect("committing teardown reaches Closed and releases state");
        assert_eq!(admission.lifecycle(), McpAppsBridgeLifecycle::Closed);
        assert_eq!(
            admission.admit_request(
                McpAppsBridgeDirection::ViewToHost,
                McpAppsJsonRpcRequestId::string("after-close".into()).unwrap(),
                McpAppsRoutedMethod::Ping,
                None,
            ),
            Err(McpAppsBridgeError::InvalidLifecycle)
        );
    }

    #[test]
    fn closed_list_size_tool_input_and_tool_cancelled_descriptors_reject_one_changed_field() {
        let list = json!({"cursor": "page-1"});
        validate_list_params(Some(&list)).expect("closed cursor list is valid");
        let mut wrong_list_key = list;
        let cursor = wrong_list_key["cursor"].clone();
        wrong_list_key["other"] = cursor;
        wrong_list_key.as_object_mut().unwrap().remove("cursor");
        assert_eq!(
            validate_list_params(Some(&wrong_list_key)),
            Err(McpAppsBridgeError::InvalidParams)
        );

        let size = json!({"width": 1.0});
        validate_size_changed(Some(&size)).expect("finite numeric dimension is valid");
        let mut wrong_width_type = size;
        wrong_width_type["width"] = json!("1.0");
        assert_eq!(
            validate_size_changed(Some(&wrong_width_type)),
            Err(McpAppsBridgeError::InvalidParams)
        );
        let mut null_width = json!({"width": 1.0});
        null_width["width"] = Value::Null;
        assert_eq!(
            validate_size_changed(Some(&null_width)),
            Err(McpAppsBridgeError::InvalidParams),
            "changing only a present numeric dimension to null is invalid"
        );

        let input = json!({"arguments": {"city": "Boston"}});
        validate_tool_input(Some(&input)).expect("object arguments are valid");
        let mut scalar_arguments = input;
        scalar_arguments["arguments"] = json!(["Boston"]);
        assert_eq!(
            validate_tool_input(Some(&scalar_arguments)),
            Err(McpAppsBridgeError::InvalidParams)
        );
        let mut null_arguments = json!({"arguments": {"city": "Boston"}});
        null_arguments["arguments"] = Value::Null;
        assert_eq!(
            validate_tool_input(Some(&null_arguments)),
            Err(McpAppsBridgeError::InvalidParams),
            "changing only a present arguments object to null is invalid"
        );

        let cancelled = json!({"reason": "user action"});
        validate_tool_cancelled(Some(&cancelled)).expect("closed cancellation reason is valid");
        let mut extra_member = cancelled;
        extra_member["requestId"] = json!(1);
        assert_eq!(
            validate_tool_cancelled(Some(&extra_member)),
            Err(McpAppsBridgeError::InvalidParams)
        );
        let mut null_reason = json!({"reason": "user action"});
        null_reason["reason"] = Value::Null;
        assert_eq!(
            validate_tool_cancelled(Some(&null_reason)),
            Err(McpAppsBridgeError::InvalidParams),
            "changing only a present cancellation reason to null is invalid"
        );
    }

    #[test]
    fn pinned_capability_and_context_wire_shapes_preserve_source_fields() {
        let capabilities: McpAppsPinnedHostCapabilities = serde_json::from_value(json!({
            "openLinks": {},
            "serverTools": {"listChanged": true},
            "message": {"text": {}, "structuredContent": {}},
            "sandbox": {"permissions": {"camera": {}}}
        }))
        .expect("pinned object-marker capabilities decode");
        assert!(capabilities.open_links.is_some());
        assert!(capabilities.server_tools.as_ref().unwrap().list_changed);
        assert!(
            capabilities
                .message
                .as_ref()
                .unwrap()
                .structured_content
                .is_some()
        );

        let context: McpAppsPinnedHostContext = serde_json::from_value(json!({
            "timeZone": "America/New_York",
            "platform": "desktop",
            "futureHostField": {"retained": true}
        }))
        .expect("forward-open host context decodes");
        assert_eq!(context.time_zone.as_deref(), Some("America/New_York"));
        assert_eq!(context.unknown["futureHostField"]["retained"], true);
    }

    #[test]
    fn forward_open_result_rejects_only_reserved_result_type_member() {
        let accepted = json!({"isError": false, "future": {"safe": true}});
        McpAppsJsonRpcEnvelope::validate_response_for(McpAppsRoutedMethod::OpenLink, &accepted)
            .expect("forward-open non-reserved result member remains inert");
        let mut reserved = accepted;
        reserved["resultType"] = json!("core-result");
        assert_eq!(
            McpAppsJsonRpcEnvelope::validate_response_for(McpAppsRoutedMethod::OpenLink, &reserved),
            Err(McpAppsBridgeError::InvalidParams)
        );
    }
}
