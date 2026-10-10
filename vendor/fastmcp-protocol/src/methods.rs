//! Canonical MCP JSON-RPC method names.
//!
//! Centralizing these as constants prevents the class of typo bug where a method
//! name is spelled slightly wrong at a call site (e.g. the lifecycle notification
//! `notifications/initialized` being sent as bare `initialized`), which the wire
//! protocol silently ignores rather than rejecting.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common_types::JsonInteger;

/// The only legacy MCP wire version represented by this isolated surface.
pub const LEGACY_2024_11_05_PROTOCOL_VERSION: &str = "2024-11-05";

/// Final stateless server-discovery request.
///
/// This aliases the discovery module's exact wire literal so method dispatch
/// and public protocol consumers share one source of truth.
pub const SERVER_DISCOVER: &str = crate::server_discovery::SERVER_DISCOVER_METHOD;

/// Final long-lived subscription request.
pub const SUBSCRIPTIONS_LISTEN: &str = "subscriptions/listen";

/// Final subscription-established notification.
pub const NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED: &str =
    "notifications/subscriptions/acknowledged";

/// SHA-256 of the pinned official 2024-11-05 JSON schema.
pub const LEGACY_2024_11_05_SCHEMA_SHA256: &str =
    "61cea2392d4f284092d09bc84b9ac488c0d5618ac2b38a56942fc5b99fd960ce";

/// Exact official MCP 2024-11-05 Draft 7 JSON schema, vendored as a read-only
/// source input.  This is deliberately not synthesized from newer protocol
/// types: consumers can inspect the pinned source of truth directly.
pub const LEGACY_2024_11_05_SCHEMA_JSON: &str =
    include_str!("../schema/mcp-schema-2024-11-05-48234828.json");

/// Parses the pinned legacy schema, retaining its exact source bytes in
/// [`LEGACY_2024_11_05_SCHEMA_JSON`].
///
/// This remains fallible so a malformed vendored input fails closed rather
/// than introducing a process-wide panic in a protocol consumer.
pub fn legacy_2024_11_05_schema() -> Result<&'static Value, Legacy2024WireError> {
    static SCHEMA: OnceLock<Result<Value, Legacy2024WireError>> = OnceLock::new();
    match SCHEMA.get_or_init(|| {
        serde_json::from_str(LEGACY_2024_11_05_SCHEMA_JSON)
            .map_err(|_| Legacy2024WireError("pinned MCP 2024-11-05 schema is not valid JSON"))
    }) {
        Ok(schema) => Ok(schema),
        Err(error) => Err(error.clone()),
    }
}

/// Lifecycle `initialize` request.
pub const INITIALIZE: &str = "initialize";

/// Lifecycle `initialized` notification (spec-correct name).
pub const NOTIFICATIONS_INITIALIZED: &str = "notifications/initialized";

/// Tools list request.
pub const TOOLS_LIST: &str = "tools/list";

/// Tools call request.
pub const TOOLS_CALL: &str = "tools/call";

/// Resources list request.
pub const RESOURCES_LIST: &str = "resources/list";

/// Resource templates list request.
pub const RESOURCES_TEMPLATES_LIST: &str = "resources/templates/list";

/// Resources read request.
pub const RESOURCES_READ: &str = "resources/read";

/// Prompts list request.
pub const PROMPTS_LIST: &str = "prompts/list";

/// Prompts get request.
pub const PROMPTS_GET: &str = "prompts/get";

/// Logging set-level request.
pub const LOGGING_SET_LEVEL: &str = "logging/setLevel";

/// Cancellation notification.
pub const NOTIFICATIONS_CANCELLED: &str = "notifications/cancelled";

/// Logging message notification.
pub const NOTIFICATIONS_MESSAGE: &str = "notifications/message";

/// Ping request.
pub const PING: &str = "ping";

/// Completion request.
pub const COMPLETION_COMPLETE: &str = "completion/complete";

/// Server-to-client sampling request.
pub const SAMPLING_CREATE_MESSAGE: &str = "sampling/createMessage";

/// Server-to-client roots list request.
pub const ROOTS_LIST: &str = "roots/list";

/// Progress notification, valid in either direction.
pub const NOTIFICATIONS_PROGRESS: &str = "notifications/progress";

/// Prompt-list-change notification.
pub const NOTIFICATIONS_PROMPTS_LIST_CHANGED: &str = "notifications/prompts/list_changed";

/// Resource-list-change notification.
pub const NOTIFICATIONS_RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";

/// Resource-update notification.
pub const NOTIFICATIONS_RESOURCES_UPDATED: &str = "notifications/resources/updated";

/// Roots-list-change notification.
pub const NOTIFICATIONS_ROOTS_LIST_CHANGED: &str = "notifications/roots/list_changed";

/// Tool-list-change notification.
pub const NOTIFICATIONS_TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";

/// Resource-subscription request.
pub const RESOURCES_SUBSCRIBE: &str = "resources/subscribe";

/// Resource-unsubscription request.
pub const RESOURCES_UNSUBSCRIBE: &str = "resources/unsubscribe";

/// Direction permitted by the active MCP 2026-07-28 core message unions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Final2026Direction {
    /// Only clients may send this method.
    ClientToServer,
    /// Only servers may send this method.
    ServerToClient,
    /// Either peer may send this method.
    Bidirectional,
}

/// Peer that originated an MCP 2026-07-28 core message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Final2026Peer {
    /// A client-originated message.
    Client,
    /// A server-originated message.
    Server,
}

impl Final2026Direction {
    /// Returns whether this direction admits a message from `peer`.
    #[must_use]
    pub const fn admits_sender(self, peer: Final2026Peer) -> bool {
        matches!(
            (self, peer),
            (Self::ClientToServer, Final2026Peer::Client)
                | (Self::ServerToClient, Final2026Peer::Server)
                | (Self::Bidirectional, _)
        )
    }
}

/// JSON-RPC envelope kind required by an active MCP 2026-07-28 core method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Final2026EnvelopeKind {
    /// The method is a request and therefore requires a non-null request ID.
    Request,
    /// The method is a notification and therefore must omit its request ID.
    Notification,
}

/// Exact direction and envelope metadata for one active MCP 2026-07-28 core
/// method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Final2026Method {
    /// Exact JSON-RPC method literal.
    pub name: &'static str,
    /// Peer direction admitted by the active final core union.
    pub direction: Final2026Direction,
    /// Request-versus-notification envelope constraint.
    pub envelope: Final2026EnvelopeKind,
}

impl Final2026Method {
    /// Returns whether this method is a notification admitted from `peer`.
    #[must_use]
    pub const fn admits_notification_from(self, peer: Final2026Peer) -> bool {
        matches!(self.envelope, Final2026EnvelopeKind::Notification)
            && self.direction.admits_sender(peer)
    }
}

/// All and only the method literals in the active MCP 2026-07-28 core
/// request and notification unions.
///
/// The pinned final schema retains types for historical reverse requests, but
/// they are not members of its active `ClientRequest`, `ClientNotification`,
/// or `ServerNotification` unions. They therefore do not enter this dispatch
/// table. The exact 2024-11-05 table remains separate below.
pub const FINAL_2026_07_28_METHODS: [Final2026Method; 18] = [
    Final2026Method {
        name: SERVER_DISCOVER,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: COMPLETION_COMPLETE,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: PROMPTS_GET,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: PROMPTS_LIST,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: RESOURCES_LIST,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: RESOURCES_TEMPLATES_LIST,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: RESOURCES_READ,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: SUBSCRIPTIONS_LISTEN,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: TOOLS_CALL,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: TOOLS_LIST,
        direction: Final2026Direction::ClientToServer,
        envelope: Final2026EnvelopeKind::Request,
    },
    Final2026Method {
        name: NOTIFICATIONS_CANCELLED,
        direction: Final2026Direction::Bidirectional,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_PROGRESS,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_MESSAGE,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_RESOURCES_UPDATED,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_RESOURCES_LIST_CHANGED,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_TOOLS_LIST_CHANGED,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_PROMPTS_LIST_CHANGED,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
    Final2026Method {
        name: NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
        direction: Final2026Direction::ServerToClient,
        envelope: Final2026EnvelopeKind::Notification,
    },
];

/// Looks up one exact active MCP 2026-07-28 core method literal.
#[must_use]
pub fn final_2026_07_28_method(name: &str) -> Option<&'static Final2026Method> {
    FINAL_2026_07_28_METHODS
        .iter()
        .find(|method| method.name == name)
}

/// Direction permitted by the 2024-11-05 tagged union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legacy2024Direction {
    /// Only clients may send this method.
    ClientToServer,
    /// Only servers may send this method.
    ServerToClient,
    /// Either peer may send this method.
    Bidirectional,
}

/// JSON-RPC envelope kind required for a tagged method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legacy2024EnvelopeKind {
    /// The method is a request and therefore requires a non-null request ID.
    Request,
    /// The method is a notification and therefore must omit its request ID.
    Notification,
}

/// Capability shape that owns a tagged method in the pinned legacy schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legacy2024Capability {
    /// Client `sampling` capability.
    ClientSampling,
    /// Client `roots` capability.
    ClientRoots,
    /// Client `roots.listChanged` capability.
    ClientRootsListChanged,
    /// Server `logging` capability.
    ServerLogging,
    /// Server `prompts` capability.
    ServerPrompts,
    /// Server `prompts.listChanged` capability.
    ServerPromptsListChanged,
    /// Server `resources` capability.
    ServerResources,
    /// Server `resources.subscribe` capability.
    ServerResourcesSubscribe,
    /// Server `resources.listChanged` capability.
    ServerResourcesListChanged,
    /// Server `tools` capability.
    ServerTools,
    /// Server `tools.listChanged` capability.
    ServerToolsListChanged,
}

/// Exact direction, envelope, and capability metadata for one tagged legacy method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Legacy2024Method {
    /// Exact JSON-RPC method literal.
    pub name: &'static str,
    /// Peer direction admitted by the tagged union.
    pub direction: Legacy2024Direction,
    /// Request-versus-notification envelope constraint.
    pub envelope: Legacy2024EnvelopeKind,
    /// Required advertised capability, when the 2024 schema defines one.
    pub capability: Option<Legacy2024Capability>,
}

/// All and only the 24 method literals in the pinned MCP 2024-11-05 schema.
pub const LEGACY_2024_11_05_METHODS: [Legacy2024Method; 24] = [
    Legacy2024Method {
        name: INITIALIZE,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: None,
    },
    Legacy2024Method {
        name: NOTIFICATIONS_INITIALIZED,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: None,
    },
    Legacy2024Method {
        name: PING,
        direction: Legacy2024Direction::Bidirectional,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: None,
    },
    Legacy2024Method {
        name: TOOLS_LIST,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerTools),
    },
    Legacy2024Method {
        name: TOOLS_CALL,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerTools),
    },
    Legacy2024Method {
        name: RESOURCES_LIST,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerResources),
    },
    Legacy2024Method {
        name: RESOURCES_TEMPLATES_LIST,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerResources),
    },
    Legacy2024Method {
        name: RESOURCES_READ,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerResources),
    },
    Legacy2024Method {
        name: RESOURCES_SUBSCRIBE,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerResourcesSubscribe),
    },
    Legacy2024Method {
        name: RESOURCES_UNSUBSCRIBE,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerResourcesSubscribe),
    },
    Legacy2024Method {
        name: PROMPTS_LIST,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerPrompts),
    },
    Legacy2024Method {
        name: PROMPTS_GET,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerPrompts),
    },
    Legacy2024Method {
        name: LOGGING_SET_LEVEL,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ServerLogging),
    },
    Legacy2024Method {
        name: COMPLETION_COMPLETE,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: None,
    },
    Legacy2024Method {
        name: SAMPLING_CREATE_MESSAGE,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ClientSampling),
    },
    Legacy2024Method {
        name: ROOTS_LIST,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Request,
        capability: Some(Legacy2024Capability::ClientRoots),
    },
    Legacy2024Method {
        name: NOTIFICATIONS_CANCELLED,
        direction: Legacy2024Direction::Bidirectional,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: None,
    },
    Legacy2024Method {
        name: NOTIFICATIONS_PROGRESS,
        direction: Legacy2024Direction::Bidirectional,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: None,
    },
    Legacy2024Method {
        name: NOTIFICATIONS_ROOTS_LIST_CHANGED,
        direction: Legacy2024Direction::ClientToServer,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: Some(Legacy2024Capability::ClientRootsListChanged),
    },
    Legacy2024Method {
        name: NOTIFICATIONS_MESSAGE,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: Some(Legacy2024Capability::ServerLogging),
    },
    Legacy2024Method {
        name: NOTIFICATIONS_PROMPTS_LIST_CHANGED,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: Some(Legacy2024Capability::ServerPromptsListChanged),
    },
    Legacy2024Method {
        name: NOTIFICATIONS_RESOURCES_LIST_CHANGED,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: Some(Legacy2024Capability::ServerResourcesListChanged),
    },
    Legacy2024Method {
        name: NOTIFICATIONS_RESOURCES_UPDATED,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: Some(Legacy2024Capability::ServerResourcesSubscribe),
    },
    Legacy2024Method {
        name: NOTIFICATIONS_TOOLS_LIST_CHANGED,
        direction: Legacy2024Direction::ServerToClient,
        envelope: Legacy2024EnvelopeKind::Notification,
        capability: Some(Legacy2024Capability::ServerToolsListChanged),
    },
];

/// Looks up one exact tagged 2024-11-05 method literal.
#[must_use]
pub fn legacy_2024_11_05_method(name: &str) -> Option<&'static Legacy2024Method> {
    LEGACY_2024_11_05_METHODS
        .iter()
        .find(|method| method.name == name)
}

/// Typed shape of the 2024-11-05 client capabilities object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Legacy2024ClientCapabilities {
    /// Non-standard client capabilities retained by the exact schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<BTreeMap<String, Value>>,
    /// Sampling support, represented by an open object in the pinned schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<BTreeMap<String, Value>>,
    /// Root-list support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<Legacy2024RootsCapability>,
    /// Additional non-standard capability members allowed by the 2024 schema.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Exact 2024-11-05 root capability shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Legacy2024RootsCapability {
    /// Whether root-list-change notifications are supported.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
    /// Additional fields allowed by the pinned open object shape.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Typed shape of the 2024-11-05 server capabilities object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Legacy2024ServerCapabilities {
    /// Non-standard server capabilities retained by the exact schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<BTreeMap<String, Value>>,
    /// Server logging capability, represented by an open object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<BTreeMap<String, Value>>,
    /// Prompt capability shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Legacy2024ListChangedCapability>,
    /// Resource capability shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Legacy2024ResourcesCapability>,
    /// Tool capability shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Legacy2024ListChangedCapability>,
    /// Additional non-standard capability members allowed by the 2024 schema.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Exact 2024-11-05 `listChanged` capability shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Legacy2024ListChangedCapability {
    /// Whether list-change notifications are supported.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
    /// Additional fields allowed by the pinned open object shape.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Exact 2024-11-05 resources capability shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Legacy2024ResourcesCapability {
    /// Whether resource subscriptions are supported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subscribe: bool,
    /// Whether resource-list-change notifications are supported.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
    /// Additional fields allowed by the pinned open object shape.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Validates and decodes the exact 2024-11-05 client capability shape.
pub fn decode_legacy_2024_11_05_client_capabilities(
    value: Value,
) -> Result<Legacy2024ClientCapabilities, Legacy2024WireError> {
    validate_legacy_2024_client_capability_members(&value)?;
    let capabilities: Legacy2024ClientCapabilities = serde_json::from_value(value)
        .map_err(|_| Legacy2024WireError("MCP 2024-11-05 client capabilities must be an object"))?;
    validate_legacy_2024_experimental_capabilities(capabilities.experimental.as_ref())?;
    Ok(capabilities)
}

/// Validates and decodes the exact 2024-11-05 server capability shape.
pub fn decode_legacy_2024_11_05_server_capabilities(
    value: Value,
) -> Result<Legacy2024ServerCapabilities, Legacy2024WireError> {
    let capabilities: Legacy2024ServerCapabilities = serde_json::from_value(value)
        .map_err(|_| Legacy2024WireError("MCP 2024-11-05 server capabilities must be an object"))?;
    validate_legacy_2024_experimental_capabilities(capabilities.experimental.as_ref())?;
    Ok(capabilities)
}

/// Validates initialization-era server metadata before a consumer accepts it.
pub fn validate_legacy_2024_11_05_initialize_result(
    value: &Value,
) -> Result<Legacy2024ServerCapabilities, Legacy2024WireError> {
    let result = value.as_object().ok_or(Legacy2024WireError(
        "MCP 2024-11-05 initialize result must be an object",
    ))?;
    if result.get("protocolVersion")
        != Some(&Value::String(
            LEGACY_2024_11_05_PROTOCOL_VERSION.to_owned(),
        ))
    {
        return Err(Legacy2024WireError(
            "initialize result protocolVersion must be exact MCP 2024-11-05",
        ));
    }
    let server_info =
        result
            .get("serverInfo")
            .and_then(Value::as_object)
            .ok_or(Legacy2024WireError(
                "MCP 2024-11-05 initialize result requires serverInfo object",
            ))?;
    if !server_info.get("name").is_some_and(Value::is_string)
        || !server_info.get("version").is_some_and(Value::is_string)
    {
        return Err(Legacy2024WireError(
            "MCP 2024-11-05 initialize result serverInfo requires string name and version",
        ));
    }
    let capabilities = result
        .get("capabilities")
        .cloned()
        .ok_or(Legacy2024WireError(
            "MCP 2024-11-05 initialize result requires server capabilities",
        ))?;
    decode_legacy_2024_11_05_server_capabilities(capabilities)
}

fn validate_legacy_2024_client_capability_members(
    value: &Value,
) -> Result<(), Legacy2024WireError> {
    let capabilities = value.as_object().ok_or(Legacy2024WireError(
        "MCP 2024-11-05 client capabilities must be an object",
    ))?;
    if ["experimental", "sampling", "roots"]
        .iter()
        .any(|member| capabilities.get(*member).is_some_and(Value::is_null))
    {
        return Err(Legacy2024WireError(
            "MCP 2024-11-05 client capability members must be objects when present",
        ));
    }
    Ok(())
}

fn validate_legacy_2024_experimental_capabilities(
    experimental: Option<&BTreeMap<String, Value>>,
) -> Result<(), Legacy2024WireError> {
    if experimental.is_some_and(|experimental| {
        experimental
            .values()
            .any(|capability| !capability.is_object())
    }) {
        return Err(Legacy2024WireError(
            "MCP 2024-11-05 experimental capabilities must map names to objects",
        ));
    }
    Ok(())
}

/// A decoded exact-2024 JSON-RPC envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum Legacy2024Envelope {
    /// A tagged request with a non-null JSON-RPC request ID.
    Request {
        method: &'static Legacy2024Method,
        id: Value,
        params: Option<Value>,
    },
    /// A tagged notification which omits JSON-RPC request ID.
    Notification {
        method: &'static Legacy2024Method,
        params: Option<Value>,
    },
    /// A successful JSON-RPC result envelope.
    Response { id: Value, result: Value },
    /// A JSON-RPC error envelope.
    Error { id: Value, error: Value },
}

/// An exact-2024 raw wire admission failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy2024WireError(&'static str);

impl Legacy2024WireError {
    /// Stable reason intended for callers that need an exact refusal category.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for Legacy2024WireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for Legacy2024WireError {}

/// Classifies a server result after exact-2024 lossless admission.
///
/// Only the three ordinary server results that can cross this boundary are
/// represented here. Modern task, structured-content, Apps, and elicitation
/// surfaces have no exact 2024 wire equivalent and are refused instead of
/// being silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legacy2024ResultKind {
    /// A `tools/call` result with a `content` array.
    Tool,
    /// A `resources/read` result with a `contents` array.
    Resource,
    /// A `prompts/get` result with a `messages` array.
    Prompt,
}

/// Exact disposition of a result at the 2024-11-05 shared-result boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legacy2024ResultDisposition {
    /// The shared result has a complete, field-preserving legacy representation.
    Lossless(Legacy2024ResultKind),
    /// The method belongs to the exact legacy adapter and has no shared result mapping.
    LegacyOwned,
}

/// Classifies a result before it can cross from shared handling to exact 2024.
///
/// A successful [`Legacy2024ResultDisposition::Lossless`] classification proves
/// that the result uses only the exact 2024 fields and recursively valid
/// legacy values. A recognized legacy method without an ordinary shared result
/// is [`Legacy2024ResultDisposition::LegacyOwned`]; unknown methods and every
/// modern or malformed result are rejected.
pub fn classify_legacy_2024_result(
    method: &str,
    result: &Value,
) -> Result<Legacy2024ResultDisposition, Legacy2024WireError> {
    let kind = match method {
        TOOLS_CALL => Legacy2024ResultKind::Tool,
        RESOURCES_READ => Legacy2024ResultKind::Resource,
        PROMPTS_GET => Legacy2024ResultKind::Prompt,
        _ if legacy_2024_11_05_method(method).is_some() => {
            return Ok(Legacy2024ResultDisposition::LegacyOwned);
        }
        _ => {
            return Err(Legacy2024WireError(
                "method is not part of exact MCP 2024-11-05",
            ));
        }
    };
    let object = result.as_object().ok_or(Legacy2024WireError(
        "ordinary MCP 2024-11-05 results must be objects",
    ))?;
    if crate::result::has_final_only_metadata(result) {
        return Err(Legacy2024WireError(
            "final protocol metadata cannot be represented by exact MCP 2024-11-05",
        ));
    }
    validate_legacy_2024_result_members(kind, object)?;
    Ok(Legacy2024ResultDisposition::Lossless(kind))
}

/// Returns an ordinary complete result unchanged only when it has an exact
/// 2024 representation for `method`.
pub fn translate_legacy_2024_result(
    method: &str,
    result: Value,
) -> Result<Value, Legacy2024WireError> {
    match classify_legacy_2024_result(method, &result)? {
        Legacy2024ResultDisposition::Lossless(_) => Ok(result),
        Legacy2024ResultDisposition::LegacyOwned => Err(Legacy2024WireError(
            "method result is owned by the exact MCP 2024-11-05 adapter",
        )),
    }
}

fn validate_legacy_2024_result_members(
    kind: Legacy2024ResultKind,
    object: &serde_json::Map<String, Value>,
) -> Result<(), Legacy2024WireError> {
    let allowed: &[&str] = match kind {
        Legacy2024ResultKind::Tool => &["content", "isError", "_meta"],
        Legacy2024ResultKind::Resource => &["contents", "_meta"],
        Legacy2024ResultKind::Prompt => &["messages", "description", "_meta"],
    };
    if [
        "structuredContent",
        "task",
        "tasks",
        "apps",
        "elicitation",
        "extensions",
        "meta",
    ]
    .iter()
    .any(|member| object.contains_key(*member))
    {
        return Err(Legacy2024WireError(
            "modern-only result member cannot be represented by exact MCP 2024-11-05",
        ));
    }
    if object
        .keys()
        .any(|member| !allowed.contains(&member.as_str()))
    {
        return Err(Legacy2024WireError(
            "unclassified result member cannot be represented by exact MCP 2024-11-05",
        ));
    }
    if !object.get("_meta").is_none_or(Value::is_object) {
        return Err(Legacy2024WireError(
            "exact MCP 2024-11-05 result _meta must be an object",
        ));
    }
    match kind {
        Legacy2024ResultKind::Tool => {
            if !object.get("isError").is_none_or(Value::is_boolean) {
                return Err(Legacy2024WireError(
                    "tools/call result isError must be a boolean",
                ));
            }
            let content =
                object
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or(Legacy2024WireError(
                        "tools/call result requires a content array",
                    ))?;
            for item in content {
                validate_legacy_2024_content(item)?;
            }
        }
        Legacy2024ResultKind::Resource => {
            let contents =
                object
                    .get("contents")
                    .and_then(Value::as_array)
                    .ok_or(Legacy2024WireError(
                        "resources/read result requires a contents array",
                    ))?;
            for resource in contents {
                validate_legacy_2024_resource_contents(resource)?;
            }
        }
        Legacy2024ResultKind::Prompt => {
            if !object.get("description").is_none_or(Value::is_string) {
                return Err(Legacy2024WireError(
                    "prompts/get result description must be a string",
                ));
            }
            let messages =
                object
                    .get("messages")
                    .and_then(Value::as_array)
                    .ok_or(Legacy2024WireError(
                        "prompts/get result requires a messages array",
                    ))?;
            for message in messages {
                validate_legacy_2024_prompt_message(message)?;
            }
        }
    }
    Ok(())
}

fn validate_legacy_2024_prompt_message(value: &Value) -> Result<(), Legacy2024WireError> {
    let message = value.as_object().ok_or(Legacy2024WireError(
        "prompts/get result messages must contain objects",
    ))?;
    if !matches!(
        message.get("role").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        return Err(Legacy2024WireError(
            "prompts/get result messages require an exact user or assistant role",
        ));
    }
    let content = message.get("content").ok_or(Legacy2024WireError(
        "prompts/get result messages require content",
    ))?;
    validate_legacy_2024_content(content)
}

fn validate_legacy_2024_content(value: &Value) -> Result<(), Legacy2024WireError> {
    let content = value.as_object().ok_or(Legacy2024WireError(
        "exact MCP 2024-11-05 content entries must be objects",
    ))?;
    match content.get("type").and_then(Value::as_str) {
        Some("text") if content.get("text").is_some_and(Value::is_string) => {}
        Some("image")
            if content.get("data").is_some_and(Value::is_string)
                && content.get("mimeType").is_some_and(Value::is_string) => {}
        Some("resource") => {
            let resource = content.get("resource").ok_or(Legacy2024WireError(
                "embedded resource content requires resource data",
            ))?;
            validate_legacy_2024_resource_contents(resource)?;
        }
        _ => {
            return Err(Legacy2024WireError(
                "exact MCP 2024-11-05 content must be text, image, or resource",
            ));
        }
    }
    if let Some(annotations) = content.get("annotations") {
        validate_legacy_2024_annotations(annotations)?;
    }
    Ok(())
}

fn validate_legacy_2024_resource_contents(value: &Value) -> Result<(), Legacy2024WireError> {
    let resource = value.as_object().ok_or(Legacy2024WireError(
        "exact MCP 2024-11-05 resource contents must be objects",
    ))?;
    if !resource.get("uri").is_some_and(Value::is_string)
        || !resource.get("mimeType").is_none_or(Value::is_string)
        || !(resource.get("text").is_some_and(Value::is_string)
            || resource.get("blob").is_some_and(Value::is_string))
    {
        return Err(Legacy2024WireError(
            "exact MCP 2024-11-05 resource contents require string uri and text or blob data",
        ));
    }
    Ok(())
}

/// Validates the required exact-2024 parameter members for the methods used by
/// the server adapter. JSON-RPC itself already guarantees an object whenever
/// parameters are present; this function adds the pinned method-level shape.
pub fn validate_legacy_2024_11_05_method_params(
    method: &str,
    params: Option<&Value>,
) -> Result<(), Legacy2024WireError> {
    reject_final_metadata_from_legacy_params(params)?;
    match method {
        TOOLS_CALL => {
            let params = required_params_object(method, params)?;
            required_string(params, "name", "tools/call")?;
            optional_object(params, "arguments", "tools/call")
        }
        RESOURCES_READ | RESOURCES_SUBSCRIBE | RESOURCES_UNSUBSCRIBE => {
            let params = required_params_object(method, params)?;
            required_string(params, "uri", method)
        }
        PROMPTS_GET => {
            let params = required_params_object(method, params)?;
            required_string(params, "name", "prompts/get")?;
            let Some(arguments) = params.get("arguments") else {
                return Ok(());
            };
            let arguments = arguments.as_object().ok_or(Legacy2024WireError(
                "prompts/get arguments must be an object",
            ))?;
            if arguments.values().all(Value::is_string) {
                Ok(())
            } else {
                Err(Legacy2024WireError(
                    "prompts/get arguments must map names to strings",
                ))
            }
        }
        COMPLETION_COMPLETE => {
            let params = required_params_object(method, params)?;
            let argument =
                params
                    .get("argument")
                    .and_then(Value::as_object)
                    .ok_or(Legacy2024WireError(
                        "completion/complete requires an argument object",
                    ))?;
            required_string(argument, "name", "completion/complete argument")?;
            required_string(argument, "value", "completion/complete argument")?;
            let reference =
                params
                    .get("ref")
                    .and_then(Value::as_object)
                    .ok_or(Legacy2024WireError(
                        "completion/complete requires a reference object",
                    ))?;
            match reference.get("type").and_then(Value::as_str) {
                Some("ref/prompt") => required_string(reference, "name", "prompt reference"),
                Some("ref/resource") => required_string(reference, "uri", "resource reference"),
                _ => Err(Legacy2024WireError(
                    "completion/complete reference must be an exact prompt or resource reference",
                )),
            }
        }
        LOGGING_SET_LEVEL => {
            let params = required_params_object(method, params)?;
            let level = params
                .get("level")
                .and_then(Value::as_str)
                .ok_or(Legacy2024WireError(
                    "logging/setLevel requires a string level",
                ))?;
            if matches!(
                level,
                "alert"
                    | "critical"
                    | "debug"
                    | "emergency"
                    | "error"
                    | "info"
                    | "notice"
                    | "warning"
            ) {
                Ok(())
            } else {
                Err(Legacy2024WireError(
                    "logging/setLevel requires an exact MCP 2024-11-05 level",
                ))
            }
        }
        NOTIFICATIONS_CANCELLED => {
            let params = required_params_object(method, params)?;
            if !params.get("requestId").is_some_and(legacy_2024_request_id) {
                return Err(Legacy2024WireError(
                    "notifications/cancelled requires a non-null string or integer requestId",
                ));
            }
            if params.get("reason").is_none_or(Value::is_string) {
                Ok(())
            } else {
                Err(Legacy2024WireError(
                    "notifications/cancelled reason must be a string",
                ))
            }
        }
        NOTIFICATIONS_PROGRESS => {
            let params = required_params_object(method, params)?;
            let token = params.get("progressToken");
            if !token.is_some_and(legacy_2024_request_id)
                || !params.get("progress").is_some_and(Value::is_number)
                || !params.get("total").is_none_or(Value::is_number)
            {
                return Err(Legacy2024WireError(
                    "notifications/progress requires exact token, progress, and optional total members",
                ));
            }
            Ok(())
        }
        SAMPLING_CREATE_MESSAGE => {
            let params = required_params_object(method, params)?;
            validate_legacy_2024_sampling_create_message(params)
        }
        NOTIFICATIONS_MESSAGE => {
            let params = required_params_object(method, params)?;
            if !params.contains_key("data") {
                return Err(Legacy2024WireError(
                    "notifications/message requires a data member",
                ));
            }
            let level = params
                .get("level")
                .and_then(Value::as_str)
                .ok_or(Legacy2024WireError(
                    "notifications/message requires a string level",
                ))?;
            if !matches!(
                level,
                "alert"
                    | "critical"
                    | "debug"
                    | "emergency"
                    | "error"
                    | "info"
                    | "notice"
                    | "warning"
            ) || !params.get("logger").is_none_or(Value::is_string)
            {
                return Err(Legacy2024WireError(
                    "notifications/message requires exact level and optional string logger",
                ));
            }
            Ok(())
        }
        NOTIFICATIONS_RESOURCES_UPDATED => {
            let params = required_params_object(method, params)?;
            required_string(params, "uri", "notifications/resources/updated")
        }
        TOOLS_LIST | RESOURCES_LIST | RESOURCES_TEMPLATES_LIST | PROMPTS_LIST => {
            validate_legacy_2024_cursor_params(params, method)
        }
        NOTIFICATIONS_ROOTS_LIST_CHANGED
        | NOTIFICATIONS_INITIALIZED
        | NOTIFICATIONS_PROMPTS_LIST_CHANGED
        | NOTIFICATIONS_RESOURCES_LIST_CHANGED
        | NOTIFICATIONS_TOOLS_LIST_CHANGED => validate_legacy_2024_metadata_params(params, false),
        ROOTS_LIST | PING => validate_legacy_2024_metadata_params(params, true),
        INITIALIZE => validate_legacy_2024_initialize(method, params),
        _ => Err(Legacy2024WireError(
            "method is not part of exact MCP 2024-11-05",
        )),
    }
}

/// Rejects final-era protocol metadata before it can be interpreted as open
/// legacy application metadata.
///
/// The exact 2024 envelope permits application-defined `_meta` entries, but
/// the final-era reserved names select different request semantics. Keeping
/// that distinction at raw admission prevents either adapter direction from
/// silently crossing eras.
fn reject_final_metadata_from_legacy_params(
    params: Option<&Value>,
) -> Result<(), Legacy2024WireError> {
    if params.is_some_and(crate::result::has_final_only_metadata) {
        Err(Legacy2024WireError(
            "final protocol metadata cannot be represented by exact MCP 2024-11-05",
        ))
    } else {
        Ok(())
    }
}

fn required_params_object<'a>(
    method: &str,
    params: Option<&'a Value>,
) -> Result<&'a serde_json::Map<String, Value>, Legacy2024WireError> {
    params
        .and_then(Value::as_object)
        .ok_or(Legacy2024WireError(match method {
            TOOLS_CALL => "tools/call requires object params",
            RESOURCES_READ => "resources/read requires object params",
            RESOURCES_SUBSCRIBE => "resources/subscribe requires object params",
            RESOURCES_UNSUBSCRIBE => "resources/unsubscribe requires object params",
            PROMPTS_GET => "prompts/get requires object params",
            COMPLETION_COMPLETE => "completion/complete requires object params",
            LOGGING_SET_LEVEL => "logging/setLevel requires object params",
            NOTIFICATIONS_CANCELLED => "notifications/cancelled requires object params",
            NOTIFICATIONS_PROGRESS => "notifications/progress requires object params",
            SAMPLING_CREATE_MESSAGE => "sampling/createMessage requires object params",
            NOTIFICATIONS_MESSAGE => "notifications/message requires object params",
            NOTIFICATIONS_RESOURCES_UPDATED => {
                "notifications/resources/updated requires object params"
            }
            _ => "exact MCP 2024-11-05 method requires object params",
        }))
}

fn validate_legacy_2024_sampling_create_message(
    params: &serde_json::Map<String, Value>,
) -> Result<(), Legacy2024WireError> {
    let messages = params
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(Legacy2024WireError(
            "sampling/createMessage requires a messages array",
        ))?;
    if !params
        .get("maxTokens")
        .is_some_and(legacy_2024_json_integer)
    {
        return Err(Legacy2024WireError(
            "sampling/createMessage requires integer maxTokens",
        ));
    }
    for message in messages {
        validate_legacy_2024_sampling_message(message)?;
    }
    if !params
        .get("includeContext")
        .is_none_or(|value| matches!(value.as_str(), Some("allServers" | "none" | "thisServer")))
    {
        return Err(Legacy2024WireError(
            "sampling/createMessage includeContext must be allServers, none, or thisServer",
        ));
    }
    if !params.get("systemPrompt").is_none_or(Value::is_string)
        || !params.get("temperature").is_none_or(Value::is_number)
        || !params.get("stopSequences").is_none_or(|value| {
            value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string))
        })
        || !params.get("metadata").is_none_or(Value::is_object)
    {
        return Err(Legacy2024WireError(
            "sampling/createMessage optional fields must match the exact 2024-11-05 shapes",
        ));
    }
    if let Some(model_preferences) = params.get("modelPreferences") {
        validate_legacy_2024_model_preferences(model_preferences)?;
    }
    Ok(())
}

fn validate_legacy_2024_sampling_message(value: &Value) -> Result<(), Legacy2024WireError> {
    let message = value.as_object().ok_or(Legacy2024WireError(
        "sampling/createMessage messages must contain objects",
    ))?;
    if !matches!(
        message.get("role").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        return Err(Legacy2024WireError(
            "sampling/createMessage messages require an exact user or assistant role",
        ));
    }
    let content = message
        .get("content")
        .and_then(Value::as_object)
        .ok_or(Legacy2024WireError(
            "sampling/createMessage messages require text or image content",
        ))?;
    match content.get("type").and_then(Value::as_str) {
        Some("text") if content.get("text").is_some_and(Value::is_string) => {}
        Some("image")
            if content.get("data").is_some_and(Value::is_string)
                && content.get("mimeType").is_some_and(Value::is_string) => {}
        _ => {
            return Err(Legacy2024WireError(
                "sampling/createMessage messages require exact text or image content",
            ));
        }
    }
    if let Some(annotations) = content.get("annotations") {
        validate_legacy_2024_annotations(annotations)?;
    }
    Ok(())
}

fn validate_legacy_2024_annotations(value: &Value) -> Result<(), Legacy2024WireError> {
    let annotations = value.as_object().ok_or(Legacy2024WireError(
        "sampling content annotations must be an object",
    ))?;
    if !annotations.get("audience").is_none_or(|value| {
        value.as_array().is_some_and(|audience| {
            audience
                .iter()
                .all(|role| matches!(role.as_str(), Some("user" | "assistant")))
        })
    }) {
        return Err(Legacy2024WireError(
            "sampling content annotation audience must contain exact roles",
        ));
    }
    if !annotations.get("priority").is_none_or(|value| {
        value
            .as_f64()
            .is_some_and(|priority| (0.0..=1.0).contains(&priority))
    }) {
        return Err(Legacy2024WireError(
            "sampling content annotation priority must be a number from zero through one",
        ));
    }
    Ok(())
}

fn validate_legacy_2024_model_preferences(value: &Value) -> Result<(), Legacy2024WireError> {
    let preferences = value.as_object().ok_or(Legacy2024WireError(
        "sampling/createMessage modelPreferences must be an object",
    ))?;
    for member in ["costPriority", "speedPriority", "intelligencePriority"] {
        if !preferences.get(member).is_none_or(|value| {
            value
                .as_f64()
                .is_some_and(|priority| (0.0..=1.0).contains(&priority))
        }) {
            return Err(Legacy2024WireError(
                "sampling/createMessage model preference priorities must be numbers from zero through one",
            ));
        }
    }
    if !preferences.get("hints").is_none_or(|value| {
        value.as_array().is_some_and(|hints| {
            hints.iter().all(|hint| {
                hint.as_object()
                    .is_some_and(|hint| hint.get("name").is_none_or(Value::is_string))
            })
        })
    }) {
        return Err(Legacy2024WireError(
            "sampling/createMessage model preference hints must be objects with optional string names",
        ));
    }
    Ok(())
}

fn optional_params_object(params: Option<&Value>, method: &str) -> Result<(), Legacy2024WireError> {
    if params.is_none_or(Value::is_object) {
        Ok(())
    } else {
        Err(Legacy2024WireError(match method {
            PING => "ping params must be an object when present",
            _ => "exact MCP 2024-11-05 params must be an object when present",
        }))
    }
}

/// Validates optional exact-2024 parameter objects with a pagination cursor.
fn validate_legacy_2024_cursor_params(
    params: Option<&Value>,
    method: &str,
) -> Result<(), Legacy2024WireError> {
    optional_params_object(params, method)?;
    let Some(params) = params else {
        return Ok(());
    };
    if params
        .as_object()
        .and_then(|params| params.get("cursor"))
        .is_none_or(Value::is_string)
    {
        Ok(())
    } else {
        Err(Legacy2024WireError(
            "exact MCP 2024-11-05 cursor must be a string when present",
        ))
    }
}

fn validate_legacy_2024_metadata_params(
    params: Option<&Value>,
    permits_progress_token: bool,
) -> Result<(), Legacy2024WireError> {
    optional_params_object(params, "metadata")?;
    let Some(params) = params else {
        return Ok(());
    };
    let params = params.as_object().ok_or(Legacy2024WireError(
        "exact MCP 2024-11-05 params must be an object when present",
    ))?;
    let Some(meta) = params.get("_meta") else {
        return Ok(());
    };
    let meta = meta.as_object().ok_or(Legacy2024WireError(
        "exact MCP 2024-11-05 _meta must be an object",
    ))?;
    if !permits_progress_token || meta.get("progressToken").is_none_or(legacy_2024_request_id) {
        Ok(())
    } else {
        Err(Legacy2024WireError(
            "exact MCP 2024-11-05 progressToken must be a string or integer",
        ))
    }
}

fn required_string(
    object: &serde_json::Map<String, Value>,
    member: &str,
    subject: &str,
) -> Result<(), Legacy2024WireError> {
    if object.get(member).is_some_and(Value::is_string) {
        Ok(())
    } else {
        Err(Legacy2024WireError(match (subject, member) {
            ("tools/call", "name") => "tools/call requires a string name",
            ("prompts/get", "name") => "prompts/get requires a string name",
            ("completion/complete argument", "name") => {
                "completion/complete argument requires a string name"
            }
            ("completion/complete argument", "value") => {
                "completion/complete argument requires a string value"
            }
            ("prompt reference", "name") => "prompt reference requires a string name",
            ("resource reference", "uri") => "resource reference requires a string uri",
            ("resources/read", "uri") => "resources/read requires a string uri",
            ("resources/subscribe", "uri") => "resources/subscribe requires a string uri",
            ("resources/unsubscribe", "uri") => "resources/unsubscribe requires a string uri",
            ("notifications/resources/updated", "uri") => {
                "notifications/resources/updated requires a string uri"
            }
            _ => "exact MCP 2024-11-05 method requires a string member",
        }))
    }
}

fn optional_object(
    object: &serde_json::Map<String, Value>,
    member: &str,
    subject: &str,
) -> Result<(), Legacy2024WireError> {
    if object.get(member).is_none_or(Value::is_object) {
        Ok(())
    } else {
        Err(Legacy2024WireError(match subject {
            "tools/call" => "tools/call arguments must be an object",
            _ => "exact MCP 2024-11-05 optional member must be an object",
        }))
    }
}

/// Decodes one exact MCP 2024-11-05 JSON-RPC envelope before any lifecycle or
/// dispatch work.  Top-level batches, modern method literals, invalid IDs, and
/// 2025-11-25 initialization are rejected at this pure raw-admission boundary.
pub fn decode_legacy_2024_11_05_envelope(
    value: Value,
) -> Result<Legacy2024Envelope, Legacy2024WireError> {
    decode_legacy_2024_11_05_envelope_classified(value).map_err(|error| match error {
        Legacy2024EnvelopeError::Envelope(error)
        | Legacy2024EnvelopeError::Method(error)
        | Legacy2024EnvelopeError::MethodParams(error) => error,
    })
}

/// One exact-2024 admission failure, split by JSON-RPC error taxonomy.
///
/// Envelope-structure failures map to Invalid Request (-32600); a
/// structurally valid JSON-RPC envelope naming a method outside the exact
/// 2024-11-05 inventory maps to Method Not Found (-32601); a valid envelope
/// whose method-owned params content is malformed maps to Invalid Params
/// (-32602). Envelope admission runs first, so a doubly-invalid frame
/// reports its envelope failure.
#[derive(Debug)]
pub enum Legacy2024EnvelopeError {
    /// The JSON-RPC envelope itself is not an exact MCP 2024-11-05 frame.
    Envelope(Legacy2024WireError),
    /// The envelope is a valid JSON-RPC frame, but its method name is not
    /// part of exact MCP 2024-11-05 (JSON-RPC 2.0 Method Not Found).
    Method(Legacy2024WireError),
    /// The envelope is valid but the method's params content is malformed.
    MethodParams(Legacy2024WireError),
}

/// Decodes one exact-2024 envelope, classifying failures by taxonomy.
pub fn decode_legacy_2024_11_05_envelope_classified(
    value: Value,
) -> Result<Legacy2024Envelope, Legacy2024EnvelopeError> {
    let object = value.as_object().ok_or(Legacy2024EnvelopeError::Envelope(
        Legacy2024WireError(
            "MCP 2024-11-05 requires one top-level JSON-RPC object; batch arrays are unsupported",
        ),
    ))?;
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "jsonrpc must be exactly 2.0",
        )));
    }

    let has_method = object.contains_key("method");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_method && (has_result || has_error) {
        return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "JSON-RPC method is mutually exclusive with result and error",
        )));
    }
    if has_result && has_error {
        return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "MCP 2024-11-05 response envelopes require exactly one of result or error",
        )));
    }
    if !has_method && object.contains_key("params") {
        return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "JSON-RPC params is only permitted on request and notification envelopes",
        )));
    }

    if let Some(method_value) = object.get("method") {
        let method_name = method_value
            .as_str()
            .ok_or(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
                "JSON-RPC method must be a string",
            )))?;
        // An unrecognized method on an otherwise valid JSON-RPC frame is
        // JSON-RPC 2.0 Method Not Found (-32601), not Invalid Request: the
        // envelope structure is sound, the method simply is not available
        // in exact MCP 2024-11-05.
        let method =
            legacy_2024_11_05_method(method_name).ok_or(Legacy2024EnvelopeError::Method(
                Legacy2024WireError("method is not part of exact MCP 2024-11-05"),
            ))?;
        let params = object.get("params").cloned();
        if params.as_ref().is_some_and(|params| !params.is_object()) {
            return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
                "JSON-RPC params must be an object when present",
            )));
        }

        return match method.envelope {
            Legacy2024EnvelopeKind::Request => {
                let id = object
                    .get("id")
                    .cloned()
                    .ok_or(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
                        "MCP 2024-11-05 request envelopes require a non-null string or integer id",
                    )))?;
                if !legacy_2024_request_id(&id) {
                    return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
                        "MCP 2024-11-05 request envelopes require a non-null string or integer id",
                    )));
                }
                // Initialize params validation is the exact-era gate, so its
                // failures stay envelope-class (-32600); other methods'
                // params-content failures are Invalid Params.
                validate_legacy_2024_11_05_method_params(method.name, params.as_ref()).map_err(
                    if method.name == INITIALIZE {
                        Legacy2024EnvelopeError::Envelope
                    } else {
                        Legacy2024EnvelopeError::MethodParams
                    },
                )?;
                Ok(Legacy2024Envelope::Request { method, id, params })
            }
            Legacy2024EnvelopeKind::Notification => {
                if object.contains_key("id") {
                    return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
                        "MCP 2024-11-05 notification envelopes must omit id",
                    )));
                }
                validate_legacy_2024_11_05_method_params(method.name, params.as_ref()).map_err(
                    if method.name == INITIALIZE {
                        Legacy2024EnvelopeError::Envelope
                    } else {
                        Legacy2024EnvelopeError::MethodParams
                    },
                )?;
                Ok(Legacy2024Envelope::Notification { method, params })
            }
        };
    }

    let id = object
        .get("id")
        .cloned()
        .ok_or(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "MCP 2024-11-05 response envelopes require a non-null string or integer id",
        )))?;
    if !legacy_2024_request_id(&id) {
        return Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "MCP 2024-11-05 response envelopes require a non-null string or integer id",
        )));
    }
    match (object.get("result"), object.get("error")) {
        (Some(result), None)
            if result.is_object()
                && result.get("resultType").is_none()
                && !crate::result::has_final_only_metadata(result) =>
        {
            Ok(Legacy2024Envelope::Response {
                id,
                result: result.clone(),
            })
        }
        (Some(result), None) if result.is_object() => {
            Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
                "final result members cannot be represented by exact MCP 2024-11-05",
            )))
        }
        (Some(_), None) => Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "MCP 2024-11-05 response result must be an object",
        ))),
        (None, Some(error)) if valid_legacy_2024_error(error) => Ok(Legacy2024Envelope::Error {
            id,
            error: error.clone(),
        }),
        (None, Some(_)) => Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "MCP 2024-11-05 error envelopes require integer code and string message",
        ))),
        _ => Err(Legacy2024EnvelopeError::Envelope(Legacy2024WireError(
            "MCP 2024-11-05 response envelopes require exactly one of result or error",
        ))),
    }
}

fn legacy_2024_request_id(value: &Value) -> bool {
    value.is_string() || legacy_2024_json_integer(value)
}

fn legacy_2024_json_integer(value: &Value) -> bool {
    value
        .as_number()
        .is_some_and(|number| JsonInteger::try_from_number(number.clone()).is_ok())
}

fn valid_legacy_2024_error(value: &Value) -> bool {
    value.as_object().is_some_and(|error| {
        error.get("code").is_some_and(legacy_2024_json_integer)
            && error.get("message").is_some_and(Value::is_string)
    })
}

fn validate_legacy_2024_initialize(
    method: &str,
    params: Option<&Value>,
) -> Result<(), Legacy2024WireError> {
    if method != INITIALIZE {
        return Ok(());
    }
    let params = params
        .and_then(Value::as_object)
        .ok_or(Legacy2024WireError(
            "MCP 2024-11-05 initialize requires object params",
        ))?;
    if !params.get("protocolVersion").is_some_and(Value::is_string) {
        return Err(Legacy2024WireError(
            "initialize protocolVersion must be a string",
        ));
    }
    let client_info =
        params
            .get("clientInfo")
            .and_then(Value::as_object)
            .ok_or(Legacy2024WireError(
                "MCP 2024-11-05 initialize requires clientInfo object",
            ))?;
    if !client_info.get("name").is_some_and(Value::is_string)
        || !client_info.get("version").is_some_and(Value::is_string)
    {
        return Err(Legacy2024WireError(
            "MCP 2024-11-05 initialize clientInfo requires string name and version",
        ));
    }
    let capabilities = params
        .get("capabilities")
        .cloned()
        .ok_or(Legacy2024WireError(
            "MCP 2024-11-05 initialize requires client capabilities",
        ))?;
    decode_legacy_2024_11_05_client_capabilities(capabilities)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn initialize_wire() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"sampling": {}, "roots": {"listChanged": true}},
                "clientInfo": {"name": "exact-legacy-client", "version": "1.0.0"}
            }
        })
    }

    #[test]
    fn lifecycle_and_tool_constants_match_mcp_spec() {
        // Lifecycle: the initialized notification is `notifications/initialized`,
        // NOT bare `initialized`. https://modelcontextprotocol.io/specification
        assert_eq!(INITIALIZE, "initialize");
        assert_eq!(NOTIFICATIONS_INITIALIZED, "notifications/initialized");
        assert_eq!(SERVER_DISCOVER, "server/discover");

        assert_eq!(TOOLS_LIST, "tools/list");
        assert_eq!(TOOLS_CALL, "tools/call");
        assert_eq!(RESOURCES_LIST, "resources/list");
        assert_eq!(RESOURCES_TEMPLATES_LIST, "resources/templates/list");
        assert_eq!(RESOURCES_READ, "resources/read");
        assert_eq!(PROMPTS_LIST, "prompts/list");
        assert_eq!(PROMPTS_GET, "prompts/get");
        assert_eq!(LOGGING_SET_LEVEL, "logging/setLevel");
        assert_eq!(NOTIFICATIONS_CANCELLED, "notifications/cancelled");
        assert_eq!(NOTIFICATIONS_MESSAGE, "notifications/message");
        assert_eq!(PING, "ping");
    }

    #[test]
    fn final_2026_method_inventory_positive() {
        let expected = [
            "server/discover",
            "completion/complete",
            "prompts/get",
            "prompts/list",
            "resources/list",
            "resources/templates/list",
            "resources/read",
            "subscriptions/listen",
            "tools/call",
            "tools/list",
            "notifications/cancelled",
            "notifications/progress",
            "notifications/message",
            "notifications/resources/updated",
            "notifications/resources/list_changed",
            "notifications/tools/list_changed",
            "notifications/prompts/list_changed",
            "notifications/subscriptions/acknowledged",
        ];
        let actual: Vec<_> = FINAL_2026_07_28_METHODS
            .iter()
            .map(|method| method.name)
            .collect();

        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 18);
        assert_eq!(
            final_2026_07_28_method(SUBSCRIPTIONS_LISTEN),
            Some(&Final2026Method {
                name: SUBSCRIPTIONS_LISTEN,
                direction: Final2026Direction::ClientToServer,
                envelope: Final2026EnvelopeKind::Request,
            })
        );
        assert_eq!(
            final_2026_07_28_method(NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED),
            Some(&Final2026Method {
                name: NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED,
                direction: Final2026Direction::ServerToClient,
                envelope: Final2026EnvelopeKind::Notification,
            })
        );
    }

    #[test]
    fn final_2026_notification_direction_admission_is_exact() {
        let cancelled = final_2026_07_28_method(NOTIFICATIONS_CANCELLED)
            .expect("cancelled belongs to the final method table");
        assert!(cancelled.admits_notification_from(Final2026Peer::Client));
        assert!(cancelled.admits_notification_from(Final2026Peer::Server));

        let acknowledged = final_2026_07_28_method(NOTIFICATIONS_SUBSCRIPTIONS_ACKNOWLEDGED)
            .expect("subscription acknowledgement belongs to the final method table");
        assert!(acknowledged.admits_notification_from(Final2026Peer::Server));
        assert!(
            !acknowledged.admits_notification_from(Final2026Peer::Client),
            "changing only the sender rejects the server-only notification"
        );

        let discover = final_2026_07_28_method(SERVER_DISCOVER)
            .expect("server/discover belongs to the final method table");
        assert!(
            !discover.admits_notification_from(Final2026Peer::Client),
            "a client-to-server request never enters either notification union"
        );
    }

    #[test]
    fn final_2026_method_inventory_cross_era_and_unknown_negatives() {
        assert!(
            final_2026_07_28_method(RESOURCES_SUBSCRIBE).is_none(),
            "changing only the method from final subscriptions/listen to the legacy subscription RPC must reject it"
        );
        assert!(
            final_2026_07_28_method("com.example/unknown").is_none(),
            "an unregistered method must not enter the closed final core table"
        );
        assert!(
            legacy_2024_11_05_method(SUBSCRIPTIONS_LISTEN).is_none(),
            "the modern subscription method must not bleed into the exact legacy table"
        );
    }

    #[test]
    fn leg_01_schema_parity_positive() {
        use fastmcp_core::crypto::sha256_bounded;

        let schema = legacy_2024_11_05_schema().unwrap();
        assert_eq!(
            LEGACY_2024_11_05_SCHEMA_SHA256,
            "61cea2392d4f284092d09bc84b9ac488c0d5618ac2b38a56942fc5b99fd960ce"
        );
        let expected_digest: [u8; 32] = std::array::from_fn(|index| {
            u8::from_str_radix(
                &LEGACY_2024_11_05_SCHEMA_SHA256[index * 2..index * 2 + 2],
                16,
            )
            .expect("published schema digest must contain hexadecimal byte pairs")
        });
        let digest = sha256_bounded(LEGACY_2024_11_05_SCHEMA_JSON.as_bytes(), 128 * 1024)
            .expect("pinned schema must fit its byte bound");
        assert_eq!(
            digest.as_bytes(),
            &expected_digest,
            "embedded schema bytes must match the full published SHA-256"
        );
        assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
        assert_eq!(
            schema["definitions"]["InitializeRequest"]["properties"]["method"]["const"],
            INITIALIZE
        );
        assert_eq!(
            schema["definitions"]["ClientCapabilities"]["properties"].get("elicitation"),
            None
        );
        assert!(LEGACY_2024_11_05_SCHEMA_JSON.as_bytes().starts_with(b"{\n"));

        let crlf_schema = LEGACY_2024_11_05_SCHEMA_JSON.replace('\n', "\r\n");
        let crlf_parsed: Value =
            serde_json::from_str(&crlf_schema).expect("CRLF conversion must preserve valid JSON");
        assert_eq!(&crlf_parsed, schema);
        let crlf_digest = sha256_bounded(crlf_schema.as_bytes(), 128 * 1024)
            .expect("CRLF schema copy must fit the same byte bound");
        assert_ne!(
            crlf_digest.as_bytes(),
            &expected_digest,
            "equivalent JSON with changed line endings must fail the pinned byte digest"
        );
    }

    #[test]
    fn leg_01_initialize_open_extensions_and_protocol_version_positive() {
        let mut wire = initialize_wire();
        wire["params"]["protocolVersion"] = json!("2025-11-25");
        wire["params"]["capabilities"]["elicitation"] = json!({"form": {}});
        assert!(matches!(
            decode_legacy_2024_11_05_envelope(wire).unwrap(),
            Legacy2024Envelope::Request { method, .. } if method.name == INITIALIZE
        ));
    }

    #[test]
    fn leg_01_initialize_open_extensions_and_protocol_version_planted_negative() {
        let mut wire = initialize_wire();
        wire["params"]["protocolVersion"] = Value::Bool(true);
        wire["params"]["capabilities"]["elicitation"] = json!({"form": {}});
        assert_eq!(
            decode_legacy_2024_11_05_envelope(wire)
                .unwrap_err()
                .reason(),
            "initialize protocolVersion must be a string"
        );
    }

    #[test]
    fn leg_01_method_inventory_positive() {
        let expected = [
            "initialize",
            "notifications/initialized",
            "ping",
            "tools/list",
            "tools/call",
            "resources/list",
            "resources/templates/list",
            "resources/read",
            "resources/subscribe",
            "resources/unsubscribe",
            "prompts/list",
            "prompts/get",
            "logging/setLevel",
            "completion/complete",
            "sampling/createMessage",
            "roots/list",
            "notifications/cancelled",
            "notifications/progress",
            "notifications/roots/list_changed",
            "notifications/message",
            "notifications/prompts/list_changed",
            "notifications/resources/list_changed",
            "notifications/resources/updated",
            "notifications/tools/list_changed",
        ];
        let actual: Vec<_> = LEGACY_2024_11_05_METHODS
            .iter()
            .map(|method| method.name)
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 24);
        assert_eq!(
            legacy_2024_11_05_method(SAMPLING_CREATE_MESSAGE)
                .unwrap()
                .capability,
            Some(Legacy2024Capability::ClientSampling)
        );
        assert_eq!(
            legacy_2024_11_05_method(NOTIFICATIONS_RESOURCES_UPDATED)
                .unwrap()
                .capability,
            Some(Legacy2024Capability::ServerResourcesSubscribe)
        );
    }

    #[test]
    fn leg_01_method_inventory_planted_negative() {
        let mut wire = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"});
        wire["method"] = json!("elicitation/create");
        assert_eq!(
            decode_legacy_2024_11_05_envelope(wire)
                .unwrap_err()
                .reason(),
            "method is not part of exact MCP 2024-11-05"
        );
    }

    #[test]
    fn leg_01_server_to_client_ping_params_positive() {
        let ping: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"server-ping","method":"ping","params":{"_meta":{"progressToken":922337203685477580812345678901234567890}}}"#,
        )
        .expect("huge mathematical integer ping token is valid JSON");

        assert!(matches!(
            decode_legacy_2024_11_05_envelope(ping).unwrap(),
            Legacy2024Envelope::Request { method, .. } if method.name == PING
        ));
    }

    #[test]
    fn leg_01_server_to_client_ping_params_planted_negative() {
        let ping: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"server-ping","method":"ping","params":{"_meta":{"progressToken":922337203685477580812345678901234567890.5}}}"#,
        )
        .expect("fractional ping token is valid JSON");

        assert_eq!(
            decode_legacy_2024_11_05_envelope(ping)
                .unwrap_err()
                .reason(),
            "exact MCP 2024-11-05 progressToken must be a string or integer"
        );
    }

    #[test]
    fn leg_01_integer_token_params_positive() {
        let huge: Value = serde_json::from_str("922337203685477580812345678901234567890")
            .expect("huge mathematical integer token is valid JSON");
        let mut cancelled = json!({
            "jsonrpc": "2.0",
            "method": NOTIFICATIONS_CANCELLED,
            "params": {"requestId": 0}
        });
        cancelled["params"]["requestId"] = huge.clone();
        assert!(decode_legacy_2024_11_05_envelope(cancelled).is_ok());

        let mut progress = json!({
            "jsonrpc": "2.0",
            "method": NOTIFICATIONS_PROGRESS,
            "params": {"progressToken": 0, "progress": 1}
        });
        progress["params"]["progressToken"] = huge;
        assert!(decode_legacy_2024_11_05_envelope(progress).is_ok());
    }

    #[test]
    fn leg_01_integer_token_params_planted_negative() {
        let fractional: Value = serde_json::from_str("922337203685477580812345678901234567890.5")
            .expect("fractional token is valid JSON");
        let mut cancelled = json!({
            "jsonrpc": "2.0",
            "method": NOTIFICATIONS_CANCELLED,
            "params": {"requestId": 0}
        });
        cancelled["params"]["requestId"] = fractional.clone();
        assert_eq!(
            decode_legacy_2024_11_05_envelope(cancelled)
                .unwrap_err()
                .reason(),
            "notifications/cancelled requires a non-null string or integer requestId"
        );

        let mut progress = json!({
            "jsonrpc": "2.0",
            "method": NOTIFICATIONS_PROGRESS,
            "params": {"progressToken": 0, "progress": 1}
        });
        progress["params"]["progressToken"] = fractional;
        assert_eq!(
            decode_legacy_2024_11_05_envelope(progress)
                .unwrap_err()
                .reason(),
            "notifications/progress requires exact token, progress, and optional total members"
        );
    }

    #[test]
    fn leg_01_cursor_params_positive() {
        for method in [
            TOOLS_LIST,
            RESOURCES_LIST,
            RESOURCES_TEMPLATES_LIST,
            PROMPTS_LIST,
        ] {
            let request = json!({
                "jsonrpc": "2.0",
                "id": "cursor-request",
                "method": method,
                "params": {"cursor": "opaque-cursor"}
            });
            assert!(
                decode_legacy_2024_11_05_envelope(request).is_ok(),
                "{method}"
            );
        }
    }

    #[test]
    fn leg_01_cursor_params_planted_negative() {
        for method in [
            TOOLS_LIST,
            RESOURCES_LIST,
            RESOURCES_TEMPLATES_LIST,
            PROMPTS_LIST,
        ] {
            let request = json!({
                "jsonrpc": "2.0",
                "id": "cursor-request",
                "method": method,
                "params": {"cursor": false}
            });
            assert_eq!(
                decode_legacy_2024_11_05_envelope(request)
                    .unwrap_err()
                    .reason(),
                "exact MCP 2024-11-05 cursor must be a string when present",
                "{method}"
            );
        }
    }

    #[test]
    fn leg_01_metadata_params_positive() {
        for method in [
            NOTIFICATIONS_INITIALIZED,
            NOTIFICATIONS_ROOTS_LIST_CHANGED,
            NOTIFICATIONS_PROMPTS_LIST_CHANGED,
            NOTIFICATIONS_RESOURCES_LIST_CHANGED,
            NOTIFICATIONS_TOOLS_LIST_CHANGED,
        ] {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": {"_meta": {"vendor": true}}
            });
            assert!(
                decode_legacy_2024_11_05_envelope(notification).is_ok(),
                "{method}"
            );
        }
        let roots = json!({
            "jsonrpc": "2.0",
            "id": "roots-request",
            "method": ROOTS_LIST,
            "params": {"_meta": {"progressToken": "roots-progress"}}
        });
        assert!(decode_legacy_2024_11_05_envelope(roots).is_ok());
    }

    #[test]
    fn leg_01_metadata_params_planted_negative() {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": NOTIFICATIONS_INITIALIZED,
            "params": {"_meta": false}
        });
        assert_eq!(
            decode_legacy_2024_11_05_envelope(notification)
                .unwrap_err()
                .reason(),
            "exact MCP 2024-11-05 _meta must be an object"
        );

        let roots = json!({
            "jsonrpc": "2.0",
            "id": "roots-request",
            "method": ROOTS_LIST,
            "params": {"_meta": {"progressToken": false}}
        });
        assert_eq!(
            decode_legacy_2024_11_05_envelope(roots)
                .unwrap_err()
                .reason(),
            "exact MCP 2024-11-05 progressToken must be a string or integer"
        );
    }

    #[test]
    fn leg_01_legacy_application_metadata_and_progress_token_remain_admitted() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "legacy-ping",
            "method": PING,
            "params": {
                "_meta": {
                    "com.example/application": {"opaque": true},
                    "progressToken": "legacy-progress"
                }
            }
        });

        assert!(matches!(
            decode_legacy_2024_11_05_envelope_classified(request),
            Ok(Legacy2024Envelope::Request { method, .. }) if method.name == PING
        ));
    }

    #[test]
    fn leg_01_final_reserved_metadata_is_rejected_from_legacy_request_params() {
        let accepted = json!({
            "jsonrpc": "2.0",
            "id": "legacy-list",
            "method": TOOLS_LIST,
            "params": {"_meta": {"com.example/application": true}}
        });
        assert!(decode_legacy_2024_11_05_envelope_classified(accepted.clone()).is_ok());

        for member in [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/serverInfo",
            "io.modelcontextprotocol/subscriptionId",
        ] {
            let mut rejected = accepted.clone();
            rejected["params"]["_meta"][member] = json!({});
            assert!(matches!(
                decode_legacy_2024_11_05_envelope_classified(rejected),
                Err(Legacy2024EnvelopeError::MethodParams(error))
                    if error.reason()
                        == "final protocol metadata cannot be represented by exact MCP 2024-11-05"
            ));
        }
    }

    #[test]
    fn leg_01_final_result_members_are_rejected_from_legacy_responses() {
        let accepted = json!({
            "jsonrpc": "2.0",
            "id": 19,
            "result": {
                "legacy": true,
                "_meta": {"com.example/application": true}
            }
        });
        assert!(decode_legacy_2024_11_05_envelope_classified(accepted.clone()).is_ok());

        let mut result_type = accepted.clone();
        result_type["result"]["resultType"] = json!("complete");
        assert!(matches!(
            decode_legacy_2024_11_05_envelope_classified(result_type),
            Err(Legacy2024EnvelopeError::Envelope(error))
                if error.reason()
                    == "final result members cannot be represented by exact MCP 2024-11-05"
        ));

        for member in [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/serverInfo",
            "io.modelcontextprotocol/subscriptionId",
        ] {
            let mut rejected = accepted.clone();
            rejected["result"]["_meta"][member] = json!({});
            assert!(matches!(
                decode_legacy_2024_11_05_envelope_classified(rejected),
                Err(Legacy2024EnvelopeError::Envelope(error))
                    if error.reason()
                        == "final result members cannot be represented by exact MCP 2024-11-05"
            ));
        }
    }

    #[test]
    fn leg_01_legacy_tool_results_preserve_application_metadata_only() {
        let accepted = json!({
            "content": [{"type": "text", "text": "legacy"}],
            "_meta": {"com.example/application": true}
        });
        assert_eq!(
            translate_legacy_2024_result(TOOLS_CALL, accepted.clone()),
            Ok(accepted.clone())
        );

        let mut rejected = accepted;
        rejected["_meta"]["io.modelcontextprotocol/serverInfo"] = json!({});
        assert_eq!(
            translate_legacy_2024_result(TOOLS_CALL, rejected)
                .expect_err("final-only result metadata cannot cross into exact 2024"),
            Legacy2024WireError(
                "final protocol metadata cannot be represented by exact MCP 2024-11-05"
            )
        );
    }

    #[test]
    fn leg_01_request_and_response_shapes_are_mutually_exclusive() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "request",
            "method": PING,
            "params": {}
        });
        assert!(matches!(
            decode_legacy_2024_11_05_envelope_classified(request),
            Ok(Legacy2024Envelope::Request { method, .. }) if method.name == PING
        ));

        let response = json!({
            "jsonrpc": "2.0",
            "id": "response",
            "result": {"legacy": true}
        });
        assert!(matches!(
            decode_legacy_2024_11_05_envelope_classified(response),
            Ok(Legacy2024Envelope::Response { id, .. }) if id == json!("response")
        ));

        for rejected in [
            json!({
                "jsonrpc": "2.0",
                "id": "mixed-null-method",
                "method": null,
                "result": {
                    "legacy": true,
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": "mixed-ping",
                "method": PING,
                "result": {"legacy": true}
            }),
        ] {
            assert!(matches!(
                decode_legacy_2024_11_05_envelope_classified(rejected),
                Err(Legacy2024EnvelopeError::Envelope(error))
                    if error.reason()
                        == "JSON-RPC method is mutually exclusive with result and error"
            ));
        }

        let result_and_error = json!({
            "jsonrpc": "2.0",
            "id": "mixed-response",
            "result": {},
            "error": {"code": -32603, "message": "failed"}
        });
        assert!(matches!(
            decode_legacy_2024_11_05_envelope_classified(result_and_error),
            Err(Legacy2024EnvelopeError::Envelope(error))
                if error.reason()
                    == "MCP 2024-11-05 response envelopes require exactly one of result or error"
        ));

        let response_with_params = json!({
            "jsonrpc": "2.0",
            "id": "response-with-params",
            "result": {"legacy": true},
            "params": {}
        });
        assert!(matches!(
            decode_legacy_2024_11_05_envelope_classified(response_with_params),
            Err(Legacy2024EnvelopeError::Envelope(error))
                if error.reason()
                    == "JSON-RPC params is only permitted on request and notification envelopes"
        ));
    }

    #[test]
    fn leg_01_sampling_max_tokens_arbitrary_width_positive() {
        let request: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"sampling-request","method":"sampling/createMessage","params":{"messages":[],"maxTokens":922337203685477580812345678901234567890}}"#,
        )
        .expect("huge mathematical integer maxTokens is valid JSON");
        assert!(decode_legacy_2024_11_05_envelope(request).is_ok());
    }

    #[test]
    fn leg_01_sampling_max_tokens_arbitrary_width_planted_negative() {
        let request: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"sampling-request","method":"sampling/createMessage","params":{"messages":[],"maxTokens":922337203685477580812345678901234567890.5}}"#,
        )
        .expect("fractional maxTokens is valid JSON");
        assert_eq!(
            decode_legacy_2024_11_05_envelope(request)
                .unwrap_err()
                .reason(),
            "sampling/createMessage requires integer maxTokens"
        );
    }

    #[test]
    fn leg_01_envelopes_positive() {
        assert!(matches!(
            decode_legacy_2024_11_05_envelope(initialize_wire()).unwrap(),
            Legacy2024Envelope::Request { method, id, .. } if method.name == INITIALIZE && id == json!(1)
        ));
        assert!(matches!(
            decode_legacy_2024_11_05_envelope(json!({"jsonrpc":"2.0", "id":"reply", "result": {}})).unwrap(),
            Legacy2024Envelope::Response { id, result } if id == json!("reply") && result == json!({})
        ));
        assert!(matches!(
            decode_legacy_2024_11_05_envelope(json!({"jsonrpc":"2.0", "method":"notifications/initialized"})).unwrap(),
            Legacy2024Envelope::Notification { method, .. } if method.name == NOTIFICATIONS_INITIALIZED
        ));
    }

    #[test]
    fn leg_01_error_codes_preserve_integral_exponent_and_huge_lexemes() {
        for raw_code in ["-3.2603e4", "340282366920938463463374607431768211457"] {
            let wire: Value = serde_json::from_str(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":{raw_code},"message":"failed"}}}}"#
            ))
            .expect("integral error-code wire must parse");
            // serde_json canonicalizes exponent spellings while parsing the
            // wire into a Value ("-3.2603e4" arrives as "-3.2603e+4"); the
            // envelope decoder's obligation is to admit the parsed code
            // without further lexeme or precision loss.
            let parsed_code = wire["error"]["code"]
                .as_number()
                .expect("error code parses as a JSON number")
                .as_str()
                .to_owned();

            let Legacy2024Envelope::Error { error, .. } = decode_legacy_2024_11_05_envelope(wire)
                .expect("exact inbound error code must be admitted")
            else {
                panic!("error wire must decode as an error envelope");
            };
            assert_eq!(
                error["code"]
                    .as_number()
                    .expect("error code remains a JSON number")
                    .as_str(),
                parsed_code
            );
        }
    }

    #[test]
    fn leg_01_error_codes_reject_fractional_near_miss() {
        let wire: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603.5,"message":"failed"}}"#,
        )
        .expect("fractional error-code wire must parse");

        assert_eq!(
            decode_legacy_2024_11_05_envelope(wire)
                .expect_err("fractional error code must not enter the exact envelope")
                .reason(),
            "MCP 2024-11-05 error envelopes require integer code and string message"
        );
    }

    #[test]
    fn leg_01_top_level_batch_array_planted_negative() {
        let single = Value::Array(vec![initialize_wire()]);
        let mixed = Value::Array(vec![
            initialize_wire(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ]);
        let expected =
            "MCP 2024-11-05 requires one top-level JSON-RPC object; batch arrays are unsupported";
        assert_eq!(
            decode_legacy_2024_11_05_envelope(single)
                .unwrap_err()
                .reason(),
            expected
        );
        assert_eq!(
            decode_legacy_2024_11_05_envelope(mixed)
                .unwrap_err()
                .reason(),
            expected
        );
    }

    #[test]
    fn leg_01_client_capability_members_positive() {
        for member in ["experimental", "sampling", "roots"] {
            let mut wire = initialize_wire();
            wire["params"]["capabilities"][member] = json!({});
            assert!(decode_legacy_2024_11_05_envelope(wire).is_ok(), "{member}");
        }
    }

    #[test]
    fn leg_01_client_capability_members_planted_negative() {
        for member in ["experimental", "sampling", "roots"] {
            let mut wire = initialize_wire();
            wire["params"]["capabilities"][member] = Value::Null;
            assert_eq!(
                decode_legacy_2024_11_05_envelope(wire)
                    .unwrap_err()
                    .reason(),
                "MCP 2024-11-05 client capability members must be objects when present",
                "{member}"
            );
        }
    }

    #[test]
    fn leg_01_a_positive() {
        let server_capabilities = validate_legacy_2024_11_05_initialize_result(&json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": {"name": "exact-legacy-server", "version": "1.0.0"},
            "capabilities": {
                "logging": {}, "tools": {"listChanged": true},
                "resources": {"subscribe": true, "listChanged": true},
                "prompts": {"listChanged": true}
            }
        }))
        .unwrap();
        assert!(server_capabilities.resources.unwrap().subscribe);
    }

    #[test]
    fn leg_01_a_planted_negative() {
        let mut wire = initialize_wire();
        wire["id"] = Value::Null;
        assert_eq!(
            decode_legacy_2024_11_05_envelope(wire)
                .unwrap_err()
                .reason(),
            "MCP 2024-11-05 request envelopes require a non-null string or integer id"
        );
    }
}
