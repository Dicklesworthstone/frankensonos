//! MCP protocol types.
//!
//! Core types used in MCP communication.

use std::collections::BTreeMap;
use std::fmt;

use crate::common_types::{
    AbsoluteUri, Annotations, ContentBlock, Implementation, JsonInteger, OpenMetadata, RawIcon,
    SamplingContentBlock,
};
use crate::extensions::MCP_APPS_HTML_MIME_TYPE;
use crate::messages::{FinalCallToolResult, FinalCoreResult};
use crate::result::{MAX_RESULT_CONTAINER_MEMBERS, MAX_RESULT_ENCODED_BYTES};
use base64::Engine as _;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// MCP protocol version.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Server capabilities advertised during initialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Tool-related capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    /// Resource-related capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    /// Prompt-related capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    /// Logging capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
    /// Argument-completion capability (`completion/complete`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completions: Option<CompletionsCapability>,
    /// Background tasks capability (Docket/SEP-1686).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<TasksCapability>,
    /// Experimental, non-standard capabilities that the server supports.
    ///
    /// Schema: `type: object`, `additionalProperties: JSONObject`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<BTreeMap<String, serde_json::Value>>,
    /// Optional MCP extensions that the server supports.
    ///
    /// Keys are extension identifiers and values are per-extension settings
    /// objects; an empty object indicates support with no settings. Schema:
    /// `type: object`, `additionalProperties: JSONObject`.
    ///
    /// `serde_json::Value` rather than a nested map, symmetrical with
    /// `ClientCapabilities`: `additionalProperties` is UNSET on this type in
    /// the schema, i.e. open, so it must not gain `deny_unknown_fields`, and a
    /// non-object value must stay representable so consumers reject it at a
    /// typed boundary instead of failing the whole deserialization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<BTreeMap<String, serde_json::Value>>,
}

/// Empty object advertised when `completion/complete` is installed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionsCapability {}

/// Tool capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsCapability {
    /// Whether the server supports tool list changes.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
}

/// Resource capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourcesCapability {
    /// Whether the server supports resource subscriptions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subscribe: bool,
    /// Whether the server supports resource list changes.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
}

/// Prompt capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptsCapability {
    /// Whether the server supports prompt list changes.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
}

/// Logging capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingCapability {}

/// Client capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Sampling capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingCapability>,
    /// Elicitation capability (user input requests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<ElicitationCapability>,
    /// Roots capability (filesystem roots).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    /// Experimental, non-standard capabilities that the client supports.
    ///
    /// Schema: `type: object`, `additionalProperties: JSONObject`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<BTreeMap<String, serde_json::Value>>,
    /// Optional MCP extensions that the client supports.
    ///
    /// Keys are extension identifiers (for example
    /// `io.modelcontextprotocol/oauth-client-credentials`) and values are
    /// per-extension settings objects; an empty object indicates support with
    /// no settings. Schema: `type: object`, `additionalProperties: JSONObject`.
    ///
    /// The value type is deliberately `serde_json::Value` rather than a nested
    /// map, matching `ServerDiscoverCapabilities::members` and the `additional`
    /// fields in this file. `ClientCapabilities` has `additionalProperties`
    /// UNSET in the schema, i.e. open, so this must not gain
    /// `deny_unknown_fields`, and a non-object value must stay representable so
    /// consumers can reject it at a typed boundary instead of failing the whole
    /// deserialization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<BTreeMap<String, serde_json::Value>>,
}

/// Sampling capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamplingCapability {}

/// Capability for form mode elicitation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormElicitationCapability {}

/// Capability for URL mode elicitation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UrlElicitationCapability {}

/// Elicitation capability.
///
/// Clients must support at least one mode (form or url).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElicitationCapability {
    /// Present if the client supports form mode elicitation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form: Option<FormElicitationCapability>,
    /// Present if the client supports URL mode elicitation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<UrlElicitationCapability>,
}

impl ElicitationCapability {
    /// Creates a form-mode elicitation capability.
    #[must_use]
    pub fn form() -> Self {
        Self {
            form: Some(FormElicitationCapability {}),
            url: None,
        }
    }

    /// Creates a URL-mode elicitation capability.
    #[must_use]
    pub fn url() -> Self {
        Self {
            form: None,
            url: Some(UrlElicitationCapability {}),
        }
    }

    /// Creates an elicitation capability supporting both modes.
    #[must_use]
    pub fn both() -> Self {
        Self {
            form: Some(FormElicitationCapability {}),
            url: Some(UrlElicitationCapability {}),
        }
    }

    /// Returns true if form mode is supported.
    #[must_use]
    pub fn supports_form(&self) -> bool {
        self.form.is_some()
    }

    /// Returns true if URL mode is supported.
    #[must_use]
    pub fn supports_url(&self) -> bool {
        self.url.is_some()
    }
}

/// Roots capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootsCapability {
    /// Whether the client supports list changes notifications.
    #[serde(
        rename = "listChanged",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
}

/// A root definition representing a filesystem location.
///
/// Roots define the boundaries of where servers can operate within the filesystem,
/// allowing them to understand which directories and files they have access to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    /// Unique identifier for the root. Must be a `file://` URI.
    pub uri: String,
    /// Optional human-readable name for display purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Root {
    /// Creates a new root with the given URI.
    #[must_use]
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
        }
    }

    /// Creates a new root with a name.
    #[must_use]
    pub fn with_name(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: Some(name.into()),
        }
    }
}

/// Server information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
}

/// Client information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Client version.
    pub version: String,
}

impl ClientInfo {
    /// Projects this exact-2024 name/version pair into a final Implementation.
    ///
    /// Empty name or version is replaced with a nonempty fallback so modern
    /// request `_meta` can always carry a typed identity object.
    #[must_use]
    pub fn to_implementation(&self) -> Implementation {
        let name = if self.name.is_empty() {
            "unknown"
        } else {
            self.name.as_str()
        };
        let version = if self.version.is_empty() {
            "0"
        } else {
            self.version.as_str()
        };
        Implementation::try_new(name, version).expect("the fallback client identity is nonempty")
    }
}

// ============================================================================
// Icon Metadata
// ============================================================================

/// Icon metadata for visual representation of components.
///
/// Icons provide visual representation for tools, resources, and prompts
/// in client UIs. All fields are optional to support various use cases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Icon {
    /// URL or data URI for the icon.
    ///
    /// Can be:
    /// - HTTP/HTTPS URL: `https://example.com/icon.png`
    /// - Data URI: `data:image/png;base64,iVBORw0KGgo...`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,

    /// MIME type of the icon (e.g., "image/png", "image/svg+xml").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,

    /// Size hints for the icon (e.g., "32x32", "16x16 32x32 64x64").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<String>,
}

impl Icon {
    /// Creates a new icon with just a source URL.
    #[must_use]
    pub fn new(src: impl Into<String>) -> Self {
        Self {
            src: Some(src.into()),
            mime_type: None,
            sizes: None,
        }
    }

    /// Creates a new icon with source and MIME type.
    #[must_use]
    pub fn with_mime_type(src: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            src: Some(src.into()),
            mime_type: Some(mime_type.into()),
            sizes: None,
        }
    }

    /// Creates a new icon with all fields.
    #[must_use]
    pub fn full(
        src: impl Into<String>,
        mime_type: impl Into<String>,
        sizes: impl Into<String>,
    ) -> Self {
        Self {
            src: Some(src.into()),
            mime_type: Some(mime_type.into()),
            sizes: Some(sizes.into()),
        }
    }

    /// Returns true if this icon has a source.
    #[must_use]
    pub fn has_src(&self) -> bool {
        self.src.is_some()
    }

    /// Returns true if the source is a data URI.
    #[must_use]
    pub fn is_data_uri(&self) -> bool {
        self.src.as_ref().is_some_and(|s| s.starts_with("data:"))
    }
}

// ============================================================================
// Component Definitions
// ============================================================================

/// Tool annotations for additional metadata.
///
/// These annotations provide hints about tool behavior to help clients
/// make informed decisions about tool usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// Whether the tool may cause destructive side effects.
    /// True means the tool modifies external state (e.g., deleting files).
    /// Serialized as the MCP-spec `destructiveHint` field.
    #[serde(rename = "destructiveHint", skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    /// Whether the tool is idempotent (safe to retry without side effects).
    /// True means calling the tool multiple times has the same effect as calling it once.
    /// Serialized as the MCP-spec `idempotentHint` field.
    #[serde(rename = "idempotentHint", skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<bool>,
    /// Whether the tool is read-only (has no side effects).
    /// True means the tool only reads data without modifying anything.
    /// Serialized as the MCP-spec `readOnlyHint` field.
    #[serde(rename = "readOnlyHint", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Whether the tool interacts with an "open world" of external entities.
    /// Per the MCP spec `openWorldHint` is a boolean: `true` if the tool may reach
    /// external/unknown systems, `false` if it operates over a closed/local domain.
    #[serde(rename = "openWorldHint", skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    /// Creates a new empty annotations struct.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the destructive annotation.
    #[must_use]
    pub fn destructive(mut self, value: bool) -> Self {
        self.destructive = Some(value);
        self
    }

    /// Sets the idempotent annotation.
    #[must_use]
    pub fn idempotent(mut self, value: bool) -> Self {
        self.idempotent = Some(value);
        self
    }

    /// Sets the read_only annotation.
    #[must_use]
    pub fn read_only(mut self, value: bool) -> Self {
        self.read_only = Some(value);
        self
    }

    /// Sets the open_world_hint annotation.
    ///
    /// Per the MCP spec `openWorldHint` is a boolean: `true` if the tool interacts
    /// with an open world of external entities, `false` for a closed/local domain.
    #[must_use]
    pub fn open_world_hint(mut self, value: bool) -> Self {
        self.open_world_hint = Some(value);
        self
    }

    /// Returns true if any annotation is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.destructive.is_none()
            && self.idempotent.is_none()
            && self.read_only.is_none()
            && self.open_world_hint.is_none()
    }
}

/// Tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// Tool description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Input schema (JSON Schema).
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    /// Output schema (JSON Schema) describing the tool's result structure.
    #[serde(rename = "outputSchema", skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Icon for visual representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
    /// Component version (semver-like string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Tags for filtering and organization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Tool annotations providing behavioral hints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Resource definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    /// Resource URI.
    pub uri: String,
    /// Resource name.
    pub name: String,
    /// Resource description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Icon for visual representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
    /// Component version (semver-like string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Tags for filtering and organization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Resource template definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceTemplate {
    /// URI template (RFC 6570).
    #[serde(
        rename = "uriTemplate",
        serialize_with = "serialize_resource_uri_template",
        deserialize_with = "deserialize_resource_uri_template"
    )]
    pub uri_template: String,
    /// Template name.
    pub name: String,
    /// Template description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Icon for visual representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
    /// Component version (semver-like string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Tags for filtering and organization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn deserialize_resource_uri_template<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    crate::UriTemplate::parse(&value)
        .map(|_| value)
        .map_err(D::Error::custom)
}

fn serialize_resource_uri_template<S>(value: &String, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    crate::UriTemplate::parse(value).map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(value)
}

/// Prompt definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    /// Prompt name.
    pub name: String,
    /// Prompt description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Prompt arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
    /// Icon for visual representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<Icon>,
    /// Component version (semver-like string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Tags for filtering and organization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Prompt argument definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    /// Argument name.
    pub name: String,
    /// Argument description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the argument is required.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

// ============================================================================
// Final component definitions
// ============================================================================

/// Shared final component identity fields.
///
/// The final protocol separates a stable programmatic `name` from an optional
/// user-facing `title`. Legacy component definitions deliberately remain
/// separate because their icon, version, and tag members are not final wire
/// members.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalBaseMetadata {
    /// Programmatic component identifier.
    pub name: String,
    /// Optional human-facing display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Final tool annotations, including the final display-title hint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalToolAnnotations {
    /// Optional display title, lower priority than the enclosing tool title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the tool may perform destructive updates.
    #[serde(
        rename = "destructiveHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub destructive: Option<bool>,
    /// Whether repeated calls are idempotent.
    #[serde(
        rename = "idempotentHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub idempotent: Option<bool>,
    /// Whether the tool is read-only.
    #[serde(
        rename = "readOnlyHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub read_only: Option<bool>,
    /// Whether the tool may interact with an open world.
    #[serde(
        rename = "openWorldHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub open_world_hint: Option<bool>,
}

// ============================================================================
// MCP Apps metadata, lifecycle, and result projection
// ============================================================================

/// Nested `_meta` member reserved by the MCP Apps protocol.
pub const MCP_APPS_UI_METADATA_KEY: &str = "ui";
/// Deprecated flat metadata member that this final-only surface rejects.
pub const MCP_APPS_DEPRECATED_RESOURCE_URI_METADATA_KEY: &str = "ui/resourceUri";
/// Maximum members in a closed nested MCP Apps tool `ui` metadata object.
pub const MAX_MCP_APPS_UI_METADATA_MEMBERS: usize = 2;
/// Maximum audience entries retained by one Apps tool visibility declaration.
pub const MAX_MCP_APPS_TOOL_VISIBILITY_ENTRIES: usize = 128;
/// Maximum origins retained by one Apps CSP directive.
pub const MAX_MCP_APPS_CSP_DOMAINS_PER_DIRECTIVE: usize = 128;
/// Maximum UTF-8 bytes retained for one Apps CSP origin or host-selected domain.
pub const MAX_MCP_APPS_CSP_DOMAIN_BYTES: usize = 2_048;

/// A tool audience declared in nested MCP Apps metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAppsToolVisibility {
    /// The model can discover and invoke the tool.
    Model,
    /// The rendered App can invoke the tool through its later bridge runtime.
    App,
}

/// A Host-selected way to present an App View.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAppsDisplayMode {
    /// The View appears in normal document flow.
    Inline,
    /// The View occupies the host's full display surface.
    Fullscreen,
    /// The View is presented picture-in-picture.
    Pip,
}

/// Closed Apps metadata attached to a final `Tool` under `_meta.ui`.
///
/// The resource URI is intentionally typed as an exact authority-form
/// `ui://` URI. Security configuration belongs to resource metadata and is
/// intentionally not part of this non-security protocol slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpAppsToolMetadata {
    /// UI resource rendered when this tool is invoked, when declared.
    pub resource_uri: Option<AbsoluteUri>,
    /// Optional explicit audiences; absence retains the Apps default of both.
    pub visibility: Option<Vec<McpAppsToolVisibility>>,
}

impl McpAppsToolMetadata {
    /// Creates validated closed Apps tool metadata.
    pub fn try_new(
        resource_uri: Option<AbsoluteUri>,
        visibility: Option<Vec<McpAppsToolVisibility>>,
    ) -> Result<Self, McpAppsMetadataError> {
        if resource_uri
            .as_ref()
            .is_some_and(|resource_uri| !resource_uri.as_str().starts_with("ui://"))
        {
            return Err(McpAppsMetadataError::ResourceUriMustUseUiPrefix);
        }
        if visibility
            .as_ref()
            .is_some_and(|visibility| visibility.len() > MAX_MCP_APPS_TOOL_VISIBILITY_ENTRIES)
        {
            return Err(McpAppsMetadataError::TooManyToolVisibilityEntries);
        }
        Ok(Self {
            resource_uri,
            visibility,
        })
    }

    /// Returns the effective visibility without changing absent versus present
    /// wire state.
    #[must_use]
    pub fn effective_visibility(&self) -> &[McpAppsToolVisibility] {
        const DEFAULT_VISIBILITY: [McpAppsToolVisibility; 2] =
            [McpAppsToolVisibility::Model, McpAppsToolVisibility::App];
        self.visibility.as_deref().unwrap_or(&DEFAULT_VISIBILITY)
    }

    /// Produces a standalone final `_meta` object containing this exact nested
    /// Apps member.
    pub fn to_open_metadata(&self) -> Result<OpenMetadata, McpAppsMetadataError> {
        let value =
            serde_json::to_value(self).map_err(|_| McpAppsMetadataError::InvalidToolMetadata)?;
        OpenMetadata::try_from_entries([(MCP_APPS_UI_METADATA_KEY.to_owned(), value)])
            .map_err(|_| McpAppsMetadataError::InvalidToolMetadata)
    }

    /// Merges this typed `ui` member into existing final open metadata.
    pub fn merge_into(
        &self,
        metadata: &OpenMetadata,
    ) -> Result<OpenMetadata, McpAppsMetadataError> {
        reject_deprecated_mcp_apps_metadata(metadata)?;
        if metadata.entries().contains_key(MCP_APPS_UI_METADATA_KEY) {
            return Err(McpAppsMetadataError::UiMetadataAlreadyPresent);
        }
        let mut entries = metadata.entries().clone();
        entries.insert(
            MCP_APPS_UI_METADATA_KEY.to_owned(),
            serde_json::to_value(self).map_err(|_| McpAppsMetadataError::InvalidToolMetadata)?,
        );
        OpenMetadata::try_from_entries(entries)
            .map_err(|_| McpAppsMetadataError::InvalidToolMetadata)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct McpAppsToolMetadataWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_uri: Option<AbsoluteUri>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visibility: Option<Vec<McpAppsToolVisibility>>,
}

impl Serialize for McpAppsToolMetadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Self::try_new(self.resource_uri.clone(), self.visibility.clone())
            .map_err(serde::ser::Error::custom)?;
        McpAppsToolMetadataWire {
            resource_uri: self.resource_uri.clone(),
            visibility: self.visibility.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for McpAppsToolMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = McpAppsToolMetadataWire::deserialize(deserializer)?;
        Self::try_new(wire.resource_uri, wire.visibility).map_err(serde::de::Error::custom)
    }
}

/// Bounded CSP origins declared by an Apps resource.
///
/// These declarations remain requests to the host. They do not authorize a
/// network connection, nested frame, or base URI without host-side policy.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpAppsResourceCsp {
    /// Origins for network requests (`connect-src`).
    pub connect_domains: Option<Vec<String>>,
    /// Origins for static resources (`img-src`, `script-src`, and related directives).
    pub resource_domains: Option<Vec<String>>,
    /// Origins allowed for nested frames (`frame-src`).
    pub frame_domains: Option<Vec<String>>,
    /// Origins allowed as document base URIs (`base-uri`).
    pub base_uri_domains: Option<Vec<String>>,
}

impl McpAppsResourceCsp {
    /// Creates a bounded CSP declaration without granting any host authority.
    pub fn try_new(
        connect_domains: Option<Vec<String>>,
        resource_domains: Option<Vec<String>>,
        frame_domains: Option<Vec<String>>,
        base_uri_domains: Option<Vec<String>>,
    ) -> Result<Self, McpAppsMetadataError> {
        for domains in [
            connect_domains.as_deref(),
            resource_domains.as_deref(),
            frame_domains.as_deref(),
            base_uri_domains.as_deref(),
        ] {
            validate_mcp_apps_domains(domains)?;
        }
        Ok(Self {
            connect_domains,
            resource_domains,
            frame_domains,
            base_uri_domains,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpAppsResourceCspWire {
    #[serde(
        rename = "connectDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    connect: Option<Vec<String>>,
    #[serde(
        rename = "resourceDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    resources: Option<Vec<String>>,
    #[serde(
        rename = "frameDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    frames: Option<Vec<String>>,
    #[serde(
        rename = "baseUriDomains",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    base_uris: Option<Vec<String>>,
}

impl Serialize for McpAppsResourceCsp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Self::try_new(
            self.connect_domains.clone(),
            self.resource_domains.clone(),
            self.frame_domains.clone(),
            self.base_uri_domains.clone(),
        )
        .map_err(serde::ser::Error::custom)?;
        McpAppsResourceCspWire {
            connect: self.connect_domains.clone(),
            resources: self.resource_domains.clone(),
            frames: self.frame_domains.clone(),
            base_uris: self.base_uri_domains.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for McpAppsResourceCsp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = McpAppsResourceCspWire::deserialize(deserializer)?;
        Self::try_new(wire.connect, wire.resources, wire.frames, wire.base_uris)
            .map_err(serde::de::Error::custom)
    }
}

/// An empty-object Apps sandbox permission marker.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAppsResourcePermission {}

/// Optional sandbox permissions requested by an Apps resource.
///
/// Presence requests a host permission; absence does not. A host may further
/// restrict or reject every requested permission.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAppsResourcePermissions {
    /// Camera permission marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<McpAppsResourcePermission>,
    /// Microphone permission marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<McpAppsResourcePermission>,
    /// Geolocation permission marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geolocation: Option<McpAppsResourcePermission>,
    /// Clipboard-write permission marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_write: Option<McpAppsResourcePermission>,
}

/// Closed Apps rendering metadata attached to a `Resource` under `_meta.ui`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpAppsResourceMetadata {
    /// Optional CSP declarations for the rendered view.
    pub csp: Option<McpAppsResourceCsp>,
    /// Optional sandbox permission requests.
    pub permissions: Option<McpAppsResourcePermissions>,
    /// Optional host-defined dedicated view domain.
    pub domain: Option<String>,
    /// Whether the View prefers a visible host-provided border and background.
    pub prefers_border: Option<bool>,
}

impl McpAppsResourceMetadata {
    /// Creates closed non-security resource presentation metadata.
    #[must_use]
    pub const fn new(prefers_border: Option<bool>) -> Self {
        Self {
            csp: None,
            permissions: None,
            domain: None,
            prefers_border,
        }
    }

    /// Creates bounded resource rendering metadata with all currently stable
    /// Apps fields. The domain is retained as host-defined opaque data.
    pub fn try_new(
        csp: Option<McpAppsResourceCsp>,
        permissions: Option<McpAppsResourcePermissions>,
        domain: Option<String>,
        prefers_border: Option<bool>,
    ) -> Result<Self, McpAppsMetadataError> {
        if domain
            .as_deref()
            .is_some_and(|domain| domain.is_empty() || domain.len() > MAX_MCP_APPS_CSP_DOMAIN_BYTES)
        {
            return Err(McpAppsMetadataError::InvalidDomain);
        }
        Ok(Self {
            csp,
            permissions,
            domain,
            prefers_border,
        })
    }

    /// Produces a standalone final `_meta` object containing this exact nested
    /// Apps member.
    pub fn to_open_metadata(&self) -> Result<OpenMetadata, McpAppsMetadataError> {
        let value = serde_json::to_value(self)
            .map_err(|_| McpAppsMetadataError::InvalidResourceMetadata)?;
        OpenMetadata::try_from_entries([(MCP_APPS_UI_METADATA_KEY.to_owned(), value)])
            .map_err(|_| McpAppsMetadataError::InvalidResourceMetadata)
    }

    /// Merges this typed `ui` member into existing final open metadata.
    pub fn merge_into(
        &self,
        metadata: &OpenMetadata,
    ) -> Result<OpenMetadata, McpAppsMetadataError> {
        reject_deprecated_mcp_apps_metadata(metadata)?;
        if metadata.entries().contains_key(MCP_APPS_UI_METADATA_KEY) {
            return Err(McpAppsMetadataError::UiMetadataAlreadyPresent);
        }
        let mut entries = metadata.entries().clone();
        entries.insert(
            MCP_APPS_UI_METADATA_KEY.to_owned(),
            serde_json::to_value(self)
                .map_err(|_| McpAppsMetadataError::InvalidResourceMetadata)?,
        );
        OpenMetadata::try_from_entries(entries)
            .map_err(|_| McpAppsMetadataError::InvalidResourceMetadata)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct McpAppsResourceMetadataWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    csp: Option<McpAppsResourceCsp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permissions: Option<McpAppsResourcePermissions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefers_border: Option<bool>,
}

impl Serialize for McpAppsResourceMetadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        McpAppsResourceMetadataWire {
            csp: self.csp.clone(),
            permissions: self.permissions.clone(),
            domain: self.domain.clone(),
            prefers_border: self.prefers_border,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for McpAppsResourceMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = McpAppsResourceMetadataWire::deserialize(deserializer)?;
        Self::try_new(wire.csp, wire.permissions, wire.domain, wire.prefers_border)
            .map_err(serde::de::Error::custom)
    }
}

/// A validated association between a tool's nested Apps metadata and an HTML
/// UI resource in the final catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpAppsResourceBinding {
    /// Exact authority-form `ui://` resource URI selected by the tool.
    pub resource_uri: AbsoluteUri,
    /// The tool's effective Apps visibility.
    pub visibility: Vec<McpAppsToolVisibility>,
}

impl McpAppsResourceBinding {
    /// Derives a binding only when the tool declares a nested Apps resource URI.
    pub fn from_tool(tool: &FinalTool) -> Result<Option<Self>, McpAppsMetadataError> {
        let Some(metadata) = tool.mcp_apps_metadata()? else {
            return Ok(None);
        };
        let visibility = metadata.effective_visibility().to_vec();
        let Some(resource_uri) = metadata.resource_uri else {
            return Ok(None);
        };
        Ok(Some(Self {
            resource_uri,
            visibility,
        }))
    }

    /// Verifies that a catalog resource is the exact HTML resource selected by
    /// this binding. Resource presentation metadata remains optional.
    pub fn validate_resource(
        &self,
        resource: &FinalResource,
    ) -> Result<(), McpAppsResourceBindingError> {
        let _ = resource
            .mcp_apps_metadata()
            .map_err(McpAppsResourceBindingError::Metadata)?;
        if resource.uri != self.resource_uri {
            return Err(McpAppsResourceBindingError::UriMismatch);
        }
        if resource.mime_type.as_deref() != Some(MCP_APPS_HTML_MIME_TYPE) {
            return Err(McpAppsResourceBindingError::HtmlMimeTypeRequired);
        }
        Ok(())
    }
}

/// A View lifecycle phase. This pure protocol state machine does not send or
/// receive any `ui/*` RPC message; a future bridge runtime owns that work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsViewLifecycle {
    /// No initialization attempt has been admitted.
    New,
    /// One initialization request is reserved and awaiting its response.
    InitializeInFlight,
    /// Initialization succeeded and the initialized notification is due.
    AwaitingInitialized,
    /// The View may receive ordinary application traffic.
    Active,
    /// Terminal teardown has begun.
    Closing,
    /// Terminal teardown is complete.
    Closed,
}

impl Default for McpAppsViewLifecycle {
    fn default() -> Self {
        Self::New
    }
}

impl McpAppsViewLifecycle {
    /// Reserves the single legal initialization attempt.
    pub fn begin_initialize(&mut self) -> Result<(), McpAppsLifecycleError> {
        self.transition(Self::New, Self::InitializeInFlight, "initialize")
    }

    /// Commits a successful initialization response.
    pub fn initialization_succeeded(&mut self) -> Result<(), McpAppsLifecycleError> {
        self.transition(
            Self::InitializeInFlight,
            Self::AwaitingInitialized,
            "initialize response",
        )
    }

    /// Atomically rolls a failed initialization back before exposure.
    pub fn initialization_failed(&mut self) -> Result<(), McpAppsLifecycleError> {
        self.transition(Self::InitializeInFlight, Self::New, "initialize rollback")
    }

    /// Admits the sole initialized notification and enables application traffic.
    pub fn admit_initialized(&mut self) -> Result<(), McpAppsLifecycleError> {
        self.transition(
            Self::AwaitingInitialized,
            Self::Active,
            "initialized notification",
        )
    }

    /// Begins one terminal teardown from every non-terminal phase.
    pub fn begin_closing(&mut self) -> Result<(), McpAppsLifecycleError> {
        match *self {
            Self::New | Self::InitializeInFlight | Self::AwaitingInitialized | Self::Active => {
                *self = Self::Closing;
                Ok(())
            }
            Self::Closing | Self::Closed => Err(McpAppsLifecycleError::InvalidTransition {
                from: *self,
                operation: "begin closing",
            }),
        }
    }

    /// Completes one terminal teardown.
    pub fn finish_closing(&mut self) -> Result<(), McpAppsLifecycleError> {
        self.transition(Self::Closing, Self::Closed, "finish closing")
    }

    /// Returns whether ordinary Host/View application traffic is legal.
    #[must_use]
    pub const fn permits_application_traffic(self) -> bool {
        matches!(self, Self::Active)
    }

    fn transition(
        &mut self,
        expected: Self,
        next: Self,
        operation: &'static str,
    ) -> Result<(), McpAppsLifecycleError> {
        if *self != expected {
            return Err(McpAppsLifecycleError::InvalidTransition {
                from: *self,
                operation,
            });
        }
        *self = next;
        Ok(())
    }
}

/// One validated Apps-side projection of a complete final `tools/call` result.
///
/// This projection intentionally accepts only the complete final result
/// branch. Tasks and MRTR input-required branches remain outside the Apps
/// bridge until an explicit composition contract is implemented.
#[derive(Clone, Debug, PartialEq)]
pub struct McpAppsToolResult {
    /// Complete final content projected without normalization.
    pub content: Vec<ContentBlock>,
    /// Tool-level error indicator retained exactly.
    pub is_error: bool,
    /// Optional structured output, including an explicitly present JSON null.
    pub structured_content: Option<serde_json::Value>,
}

impl McpAppsToolResult {
    /// Constructs a bounded Apps result projection.
    pub fn try_new(
        content: Vec<ContentBlock>,
        is_error: bool,
        structured_content: Option<serde_json::Value>,
    ) -> Result<Self, McpAppsResultProjectionError> {
        if content.len() > MAX_RESULT_CONTAINER_MEMBERS {
            return Err(McpAppsResultProjectionError::ResultTooLarge);
        }
        let result = Self {
            content,
            is_error,
            structured_content,
        };
        let encoded = serde_json::to_vec(&McpAppsToolResultWire::from(&result))
            .map_err(|_| McpAppsResultProjectionError::ResultTooLarge)?;
        if encoded.len() > MAX_RESULT_ENCODED_BYTES {
            return Err(McpAppsResultProjectionError::ResultTooLarge);
        }
        Ok(result)
    }

    /// Projects one fully validated final `tools/call` payload exactly once.
    pub fn from_final_call_tool_result(
        result: &FinalCallToolResult,
    ) -> Result<Self, McpAppsResultProjectionError> {
        Self::try_new(
            result.content.clone(),
            result.is_error,
            result.structured_content.clone(),
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct McpAppsToolResultWire {
    content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_apps_present_json_value")]
    structured_content: Option<serde_json::Value>,
}

fn deserialize_apps_present_json_value<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

impl From<&McpAppsToolResult> for McpAppsToolResultWire {
    fn from(result: &McpAppsToolResult) -> Self {
        Self {
            content: result.content.clone(),
            is_error: result.is_error,
            structured_content: result.structured_content.clone(),
        }
    }
}

impl Serialize for McpAppsToolResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Self::try_new(
            self.content.clone(),
            self.is_error,
            self.structured_content.clone(),
        )
        .map_err(serde::ser::Error::custom)?;
        McpAppsToolResultWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for McpAppsToolResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = McpAppsToolResultWire::deserialize(deserializer)?;
        Self::try_new(wire.content, wire.is_error, wire.structured_content)
            .map_err(serde::de::Error::custom)
    }
}

/// Projects the complete `tools/call` branch while rejecting deferred Tasks and
/// MRTR branches before any Apps result is produced.
pub fn project_final_core_tools_call_result(
    result: &FinalCoreResult,
) -> Result<McpAppsToolResult, McpAppsResultProjectionError> {
    match result {
        FinalCoreResult::ToolsCall { result, .. } => {
            McpAppsToolResult::from_final_call_tool_result(&result.payload)
        }
        #[cfg(feature = "tasks")]
        FinalCoreResult::ToolsCallTask { .. } => {
            Err(McpAppsResultProjectionError::TasksUnsupported)
        }
        FinalCoreResult::ToolsCallInputRequired { .. } => {
            Err(McpAppsResultProjectionError::MrtrUnsupported)
        }
        _ => Err(McpAppsResultProjectionError::NotToolsCall),
    }
}

/// Metadata validation failures specific to the closed Apps `ui` member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsMetadataError {
    /// The old flat `ui/resourceUri` key is forbidden on the final surface.
    DeprecatedFlatResourceUri,
    /// The nested `ui` member was not an object.
    UiMetadataMustBeObject,
    /// A tool `ui` object did not satisfy its closed schema.
    InvalidToolMetadata,
    /// A resource `ui` object did not satisfy its closed schema.
    InvalidResourceMetadata,
    /// A resource binding URI must start with the exact `ui://` prefix.
    ResourceUriMustUseUiPrefix,
    /// A tool visibility declaration carried more than its bounded number of entries.
    TooManyToolVisibilityEntries,
    /// One CSP directive carried more than its bounded number of origins.
    TooManyCspDomains,
    /// One CSP origin was empty or exceeded its bounded byte allowance.
    InvalidCspDomain,
    /// The host-defined Apps domain was empty or exceeded its bounded allowance.
    InvalidDomain,
    /// A merge would overwrite a pre-existing nested `ui` member.
    UiMetadataAlreadyPresent,
}

impl fmt::Display for McpAppsMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeprecatedFlatResourceUri => formatter.write_str(
                "deprecated flat MCP Apps metadata key ui/resourceUri is forbidden; use _meta.ui.resourceUri",
            ),
            Self::UiMetadataMustBeObject => {
                formatter.write_str("MCP Apps _meta.ui must be an object")
            }
            Self::InvalidToolMetadata => {
                formatter.write_str("MCP Apps tool _meta.ui does not satisfy its closed schema")
            }
            Self::InvalidResourceMetadata => formatter
                .write_str("MCP Apps resource _meta.ui does not satisfy its closed schema"),
            Self::ResourceUriMustUseUiPrefix => {
                formatter.write_str("MCP Apps resourceUri must start with ui://")
            }
            Self::TooManyToolVisibilityEntries => {
                formatter.write_str("MCP Apps tool visibility exceeds its entry limit")
            }
            Self::TooManyCspDomains => {
                formatter.write_str("MCP Apps CSP directive exceeds its origin limit")
            }
            Self::InvalidCspDomain => {
                formatter.write_str("MCP Apps CSP origin is empty or exceeds its byte limit")
            }
            Self::InvalidDomain => {
                formatter.write_str("MCP Apps domain is empty or exceeds its byte limit")
            }
            Self::UiMetadataAlreadyPresent => {
                formatter.write_str("MCP Apps _meta already contains a ui member")
            }
        }
    }
}

impl std::error::Error for McpAppsMetadataError {}

/// A catalog resource did not satisfy one validated Apps binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsResourceBindingError {
    /// The candidate resource URI differs from the tool's exact binding URI.
    UriMismatch,
    /// The candidate resource is not an Apps HTML resource.
    HtmlMimeTypeRequired,
    /// Resource metadata was not a valid closed Apps metadata object.
    Metadata(McpAppsMetadataError),
}

impl fmt::Display for McpAppsResourceBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UriMismatch => {
                formatter.write_str("MCP Apps resource URI differs from tool binding")
            }
            Self::HtmlMimeTypeRequired => {
                formatter.write_str("MCP Apps bound resource must use text/html;profile=mcp-app")
            }
            Self::Metadata(error) => write!(formatter, "MCP Apps resource metadata: {error}"),
        }
    }
}

impl std::error::Error for McpAppsResourceBindingError {}

/// Illegal transition in the pure Apps View lifecycle state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsLifecycleError {
    /// The requested operation is not legal from the retained lifecycle phase.
    InvalidTransition {
        /// Current lifecycle phase.
        from: McpAppsViewLifecycle,
        /// Name of the rejected operation.
        operation: &'static str,
    },
}

impl fmt::Display for McpAppsLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, operation } => {
                write!(
                    formatter,
                    "MCP Apps lifecycle cannot {operation} from {from:?}"
                )
            }
        }
    }
}

impl std::error::Error for McpAppsLifecycleError {}

/// Apps result projection failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAppsResultProjectionError {
    /// The projected result exceeds the final result bounds.
    ResultTooLarge,
    /// A Tasks-backed `tools/call` result has no Apps result composition.
    TasksUnsupported,
    /// An MRTR `input_required` tools/call result has no Apps result composition.
    MrtrUnsupported,
    /// Only the final `tools/call` result family can be projected.
    NotToolsCall,
}

impl fmt::Display for McpAppsResultProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResultTooLarge => {
                formatter.write_str("MCP Apps result exceeds final result bounds")
            }
            Self::TasksUnsupported => {
                formatter.write_str("MCP Apps does not project Tasks-backed tool results")
            }
            Self::MrtrUnsupported => {
                formatter.write_str("MCP Apps does not project MRTR input-required tool results")
            }
            Self::NotToolsCall => {
                formatter.write_str("MCP Apps projects only final tools/call results")
            }
        }
    }
}

impl std::error::Error for McpAppsResultProjectionError {}

fn validate_mcp_apps_domains(domains: Option<&[String]>) -> Result<(), McpAppsMetadataError> {
    let Some(domains) = domains else {
        return Ok(());
    };
    if domains.len() > MAX_MCP_APPS_CSP_DOMAINS_PER_DIRECTIVE {
        return Err(McpAppsMetadataError::TooManyCspDomains);
    }
    if domains
        .iter()
        .any(|domain| domain.is_empty() || domain.len() > MAX_MCP_APPS_CSP_DOMAIN_BYTES)
    {
        return Err(McpAppsMetadataError::InvalidCspDomain);
    }
    Ok(())
}

fn reject_deprecated_mcp_apps_metadata(
    metadata: &OpenMetadata,
) -> Result<(), McpAppsMetadataError> {
    if metadata
        .entries()
        .contains_key(MCP_APPS_DEPRECATED_RESOURCE_URI_METADATA_KEY)
    {
        Err(McpAppsMetadataError::DeprecatedFlatResourceUri)
    } else {
        Ok(())
    }
}

fn parse_mcp_apps_tool_metadata(
    metadata: &OpenMetadata,
) -> Result<Option<McpAppsToolMetadata>, McpAppsMetadataError> {
    reject_deprecated_mcp_apps_metadata(metadata)?;
    let Some(value) = metadata.entries().get(MCP_APPS_UI_METADATA_KEY) else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err(McpAppsMetadataError::UiMetadataMustBeObject);
    }
    let metadata = serde_json::from_value(value.clone())
        .map_err(|_| McpAppsMetadataError::InvalidToolMetadata)?;
    Ok(Some(metadata))
}

fn parse_mcp_apps_resource_metadata(
    metadata: &OpenMetadata,
) -> Result<Option<McpAppsResourceMetadata>, McpAppsMetadataError> {
    reject_deprecated_mcp_apps_metadata(metadata)?;
    let Some(value) = metadata.entries().get(MCP_APPS_UI_METADATA_KEY) else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err(McpAppsMetadataError::UiMetadataMustBeObject);
    }
    let metadata = serde_json::from_value(value.clone())
        .map_err(|_| McpAppsMetadataError::InvalidResourceMetadata)?;
    Ok(Some(metadata))
}

fn serialize_final_tool_metadata<S>(
    metadata: &Option<OpenMetadata>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if let Some(metadata) = metadata {
        parse_mcp_apps_tool_metadata(metadata).map_err(serde::ser::Error::custom)?;
    }
    metadata.serialize(serializer)
}

fn deserialize_final_tool_metadata<'de, D>(
    deserializer: D,
) -> Result<Option<OpenMetadata>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let metadata = Option::<OpenMetadata>::deserialize(deserializer)?;
    if let Some(metadata) = &metadata {
        parse_mcp_apps_tool_metadata(metadata).map_err(serde::de::Error::custom)?;
    }
    Ok(metadata)
}

fn serialize_final_resource_metadata<S>(
    metadata: &Option<OpenMetadata>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if let Some(metadata) = metadata {
        parse_mcp_apps_resource_metadata(metadata).map_err(serde::ser::Error::custom)?;
    }
    metadata.serialize(serializer)
}

fn deserialize_final_resource_metadata<'de, D>(
    deserializer: D,
) -> Result<Option<OpenMetadata>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let metadata = Option::<OpenMetadata>::deserialize(deserializer)?;
    if let Some(metadata) = &metadata {
        parse_mcp_apps_resource_metadata(metadata).map_err(serde::de::Error::custom)?;
    }
    Ok(metadata)
}

/// Exact final `Tool` model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalTool {
    /// Programmatic component identifier.
    pub name: String,
    /// Optional human-facing display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional sized icon collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<RawIcon>>,
    /// Required JSON Schema object for tool input.
    #[serde(
        rename = "inputSchema",
        deserialize_with = "deserialize_final_tool_input_schema"
    )]
    pub input_schema: serde_json::Value,
    /// Optional JSON Schema object for structured tool output.
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_final_json_object"
    )]
    pub output_schema: Option<serde_json::Value>,
    /// Optional behavioral and display hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<FinalToolAnnotations>,
    /// Optional final metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_final_tool_metadata",
        deserialize_with = "deserialize_final_tool_metadata"
    )]
    pub meta: Option<OpenMetadata>,
}

impl FinalTool {
    /// Reads and validates the tool's optional closed `_meta.ui` Apps member.
    pub fn mcp_apps_metadata(&self) -> Result<Option<McpAppsToolMetadata>, McpAppsMetadataError> {
        self.meta
            .as_ref()
            .map_or(Ok(None), parse_mcp_apps_tool_metadata)
    }

    /// Derives the optional exact Apps resource binding declared by this tool.
    pub fn mcp_apps_resource_binding(
        &self,
    ) -> Result<Option<McpAppsResourceBinding>, McpAppsMetadataError> {
        McpAppsResourceBinding::from_tool(self)
    }
}

/// Exact final `Resource` model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalResource {
    /// Exact resource URI.
    pub uri: AbsoluteUri,
    /// Programmatic resource identifier.
    pub name: String,
    /// Optional human-facing display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional sized icon collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<RawIcon>>,
    /// Optional resource MIME type.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional raw content size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<JsonInteger>,
    /// Optional client-facing annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
    /// Optional final metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_final_resource_metadata",
        deserialize_with = "deserialize_final_resource_metadata"
    )]
    pub meta: Option<OpenMetadata>,
}

impl FinalResource {
    /// Reads and validates the resource's optional closed `_meta.ui` Apps
    /// presentation member.
    pub fn mcp_apps_metadata(
        &self,
    ) -> Result<Option<McpAppsResourceMetadata>, McpAppsMetadataError> {
        self.meta
            .as_ref()
            .map_or(Ok(None), parse_mcp_apps_resource_metadata)
    }
}

/// Exact final `ResourceTemplate` model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalResourceTemplate {
    /// RFC 6570 resource URI template.
    #[serde(
        rename = "uriTemplate",
        serialize_with = "serialize_final_resource_uri_template",
        deserialize_with = "deserialize_final_resource_uri_template"
    )]
    pub uri_template: String,
    /// Programmatic template identifier.
    pub name: String,
    /// Optional human-facing display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional sized icon collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<RawIcon>>,
    /// Optional MIME type for resources matched by the template.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional client-facing annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
    /// Optional final metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
}

fn deserialize_final_resource_uri_template<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    crate::UriTemplate::parse(&value)
        .map(|_| value)
        .map_err(D::Error::custom)
}

fn serialize_final_resource_uri_template<S>(
    value: &String,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    crate::UriTemplate::parse(value).map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(value)
}

/// Exact final prompt-argument model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalPromptArgument {
    /// Programmatic argument identifier.
    pub name: String,
    /// Optional human-facing display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether an argument is required; absence remains distinct from false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// Exact final `Prompt` model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalPrompt {
    /// Programmatic prompt identifier.
    pub name: String,
    /// Optional human-facing display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional sized icon collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<RawIcon>>,
    /// Optional prompt arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<FinalPromptArgument>>,
    /// Optional final metadata.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
}

fn deserialize_final_tool_input_schema<'de, D>(
    deserializer: D,
) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let Some(object) = value.as_object() else {
        return Err(D::Error::custom("final tool inputSchema must be an object"));
    };
    if object.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err(D::Error::custom(
            "final tool inputSchema must declare type object",
        ));
    }
    Ok(value)
}

fn deserialize_optional_final_json_object<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(D::Error::custom(
            "final tool outputSchema must be an object",
        ));
    }
    Ok(Some(value))
}

/// Legacy 2024 metadata, retained exactly as an open JSON object.
///
/// The 2024-11-05 schema permits arbitrary JSON members in result `_meta`
/// objects. This intentionally remains distinct from the final-era
/// [`OpenMetadata`] policy.
pub type LegacyMetadata = BTreeMap<String, serde_json::Value>;

/// Exact 2024-11-05 content blocks carried in legacy results.
///
/// The legacy `Content` surface remains available to 2026 adapters. This
/// separate wire model preserves the annotations, `_meta`, and open members
/// that the checked-in 2024 schema permits, while deliberately excluding audio
/// from the legacy content union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LegacyContent {
    /// Text content.
    Text {
        /// The text content.
        text: String,
        /// Optional presentation annotations.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        /// Other schema-allowed content members, including an open `_meta` value.
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, serde_json::Value>,
    },
    /// Image content.
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type (e.g., `image/png`).
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// Optional presentation annotations.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        /// Other schema-allowed content members, including an open `_meta` value.
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, serde_json::Value>,
    },
    /// An embedded resource.
    Resource {
        /// Resource contents.
        resource: LegacyResourceContent,
        /// Optional presentation annotations.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        /// Other schema-allowed content members, including an open `_meta` value.
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, serde_json::Value>,
    },
}

/// Exact 2024-11-05 resource contents carried by legacy results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LegacyResourceContent {
    /// Text resource contents.
    Text {
        /// Resource URI.
        uri: String,
        /// Resource text.
        text: String,
        /// Optional MIME type.
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        /// Other schema-allowed resource members, including an open `_meta` value.
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, serde_json::Value>,
    },
    /// Binary resource contents.
    Blob {
        /// Resource URI.
        uri: String,
        /// Base64-encoded resource data.
        blob: String,
        /// Optional MIME type.
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        /// Other schema-allowed resource members, including an open `_meta` value.
        #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
        additional: BTreeMap<String, serde_json::Value>,
    },
}

/// Content types used by the broader server and 2026 adapter surfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Content {
    /// Text content.
    Text {
        /// The text content.
        text: String,
    },
    /// Image content.
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type (e.g., "image/png").
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Audio content.
    Audio {
        /// Base64-encoded audio data.
        data: String,
        /// MIME type (e.g., "audio/wav").
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Resource content.
    Resource {
        /// The resource being referenced.
        resource: ResourceContent,
    },
}

impl Content {
    /// Creates text content.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Creates image content from base64-encoded data.
    #[must_use]
    pub fn image_base64(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// Creates image content from raw bytes (base64-encodes internally).
    #[must_use]
    pub fn image_bytes(bytes: impl AsRef<[u8]>, mime_type: impl Into<String>) -> Self {
        let data = base64::engine::general_purpose::STANDARD.encode(bytes.as_ref());
        Self::image_base64(data, mime_type)
    }

    /// Creates audio content from base64-encoded data.
    #[must_use]
    pub fn audio_base64(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Audio {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// Creates audio content from raw bytes (base64-encodes internally).
    #[must_use]
    pub fn audio_bytes(bytes: impl AsRef<[u8]>, mime_type: impl Into<String>) -> Self {
        let data = base64::engine::general_purpose::STANDARD.encode(bytes.as_ref());
        Self::audio_base64(data, mime_type)
    }

    /// Creates an embedded resource content with text payload.
    #[must_use]
    pub fn resource_text(
        uri: impl Into<String>,
        mime_type: Option<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::Resource {
            resource: ResourceContent {
                uri: uri.into(),
                mime_type,
                text: Some(text.into()),
                blob: None,
            },
        }
    }

    /// Creates an embedded resource content with base64 blob payload.
    #[must_use]
    pub fn resource_blob_base64(
        uri: impl Into<String>,
        mime_type: Option<String>,
        blob: impl Into<String>,
    ) -> Self {
        Self::Resource {
            resource: ResourceContent {
                uri: uri.into(),
                mime_type,
                text: None,
                blob: Some(blob.into()),
            },
        }
    }

    /// Creates an embedded resource content with raw bytes payload (base64-encodes internally).
    #[must_use]
    pub fn resource_blob_bytes(
        uri: impl Into<String>,
        mime_type: Option<String>,
        bytes: impl AsRef<[u8]>,
    ) -> Self {
        let blob = base64::engine::general_purpose::STANDARD.encode(bytes.as_ref());
        Self::resource_blob_base64(uri, mime_type, blob)
    }
}

/// Resource content in a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceContent {
    /// Resource URI.
    pub uri: String,
    /// MIME type.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Text content (if text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Binary content (if blob, base64).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// Role in prompt messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// User role.
    User,
    /// Assistant role.
    Assistant,
}

/// Exact 2024-11-05 prompt messages carried in legacy results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyPromptMessage {
    /// Message role.
    pub role: Role,
    /// Exact legacy message content.
    pub content: LegacyContent,
    /// Other schema-allowed message members, including an open `_meta` value.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, serde_json::Value>,
}

/// A message in a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessage {
    /// Message role.
    pub role: Role,
    /// Message content.
    pub content: Content,
}

// ============================================================================
// Background Tasks (Docket/SEP-1686)
// ============================================================================

/// Task identifier.
///
/// Unique identifier for background tasks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    /// Creates a new random task ID.
    #[must_use]
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self(format!("task-{timestamp:x}"))
    }

    /// Creates a task ID from a string.
    #[must_use]
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the task ID as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for TaskId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for TaskId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Task is queued but not yet started.
    Pending,
    /// Task is currently running.
    Running,
    /// Task completed successfully.
    Completed,
    /// Task failed with an error.
    Failed,
    /// Task was cancelled.
    Cancelled,
}

impl TaskStatus {
    /// Returns true if the task is in a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    /// Returns true if the task is still active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, TaskStatus::Pending | TaskStatus::Running)
    }
}

/// Information about a background task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    /// Unique task identifier.
    pub id: TaskId,
    /// Task type (identifies the kind of work).
    #[serde(rename = "taskType")]
    pub task_type: String,
    /// Current status.
    pub status: TaskStatus,
    /// Progress (0.0 to 1.0, if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Progress message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Task creation timestamp (ISO 8601).
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Task start timestamp (ISO 8601), if started.
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Task completion timestamp (ISO 8601), if completed.
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Error message if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Task result payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Task identifier.
    pub id: TaskId,
    /// Whether the task succeeded.
    pub success: bool,
    /// Result data (if successful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Error message (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Task capability for server capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksCapability {
    /// Whether the server supports task list changes notifications.
    #[serde(
        default,
        rename = "listChanged",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub list_changed: bool,
}

// ============================================================================
// Sampling Protocol Types
// ============================================================================

/// Message content for sampling requests.
///
/// Can contain text, images, or tool-related content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SamplingContent {
    /// Text content.
    Text {
        /// The text content.
        text: String,
    },
    /// Image content.
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type (e.g., "image/png").
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// A message in a sampling conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingMessage {
    /// Message role (user or assistant).
    pub role: Role,
    /// Message content.
    pub content: SamplingContent,
}

impl SamplingMessage {
    /// Creates a new user message with text content.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: SamplingContent::Text { text: text.into() },
        }
    }

    /// Creates a new assistant message with text content.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: SamplingContent::Text { text: text.into() },
        }
    }
}

/// Final sampling-specific content block.
pub type FinalSamplingMessageContentBlock = SamplingContentBlock;

/// Exact final sampling-message content, preserving one block versus an array
/// of blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FinalSamplingMessageContent {
    /// One sampled content block.
    Block(FinalSamplingMessageContentBlock),
    /// Multiple sampled content blocks.
    Blocks(Vec<FinalSamplingMessageContentBlock>),
}

/// A final sampling conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalSamplingMessage {
    /// Sender role.
    pub role: Role,
    /// Exact final sampling content shape.
    pub content: FinalSamplingMessageContent,
    /// Optional metadata retained on the final wire.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<OpenMetadata>,
}

/// Tool-selection mode for final sampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FinalToolChoiceMode {
    /// The model decides whether to use tools.
    Auto,
    /// The model must use at least one tool.
    Required,
    /// The model must not use tools.
    None,
}

/// Tool-selection controls for final sampling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalToolChoice {
    /// Optional mode; absence keeps the wire default of `auto` distinct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<FinalToolChoiceMode>,
}

/// Model preferences for sampling requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPreferences {
    /// Hints for model selection (model names or patterns).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<ModelHint>,
    /// Priority for cost (0.0 = lowest priority, 1.0 = highest).
    #[serde(rename = "costPriority", skip_serializing_if = "Option::is_none")]
    pub cost_priority: Option<f64>,
    /// Priority for speed (0.0 = lowest priority, 1.0 = highest).
    #[serde(rename = "speedPriority", skip_serializing_if = "Option::is_none")]
    pub speed_priority: Option<f64>,
    /// Priority for intelligence (0.0 = lowest priority, 1.0 = highest).
    #[serde(
        rename = "intelligencePriority",
        skip_serializing_if = "Option::is_none"
    )]
    pub intelligence_priority: Option<f64>,
}

/// A hint for model selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHint {
    /// Model name or pattern.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Stop reason for sampling responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// End of natural turn.
    #[default]
    EndTurn,
    /// Hit stop sequence.
    StopSequence,
    /// Hit max tokens limit.
    MaxTokens,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ========================================================================
    // ServerCapabilities Tests
    // ========================================================================

    #[test]
    fn server_capabilities_default_serialization() {
        let caps = ServerCapabilities::default();
        let value = serde_json::to_value(&caps).expect("serialize");
        // All None fields should be omitted
        assert_eq!(value, json!({}));
    }

    #[test]
    fn server_capabilities_full_serialization() {
        let caps = ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
            resources: Some(ResourcesCapability {
                subscribe: true,
                list_changed: true,
            }),
            prompts: Some(PromptsCapability { list_changed: true }),
            logging: Some(LoggingCapability {}),
            completions: Some(CompletionsCapability {}),
            tasks: Some(TasksCapability { list_changed: true }),
            ..Default::default()
        };
        let value = serde_json::to_value(&caps).expect("serialize");
        assert_eq!(value["tools"]["listChanged"], true);
        assert_eq!(value["resources"]["subscribe"], true);
        assert_eq!(value["resources"]["listChanged"], true);
        assert_eq!(value["prompts"]["listChanged"], true);
        assert!(value.get("logging").is_some());
        assert!(value.get("completions").is_some());
        assert_eq!(value["tasks"]["listChanged"], true);
    }

    #[test]
    fn server_capabilities_partial_serialization() {
        let caps = ServerCapabilities {
            tools: Some(ToolsCapability::default()),
            ..Default::default()
        };
        let value = serde_json::to_value(&caps).expect("serialize");
        assert!(value.get("tools").is_some());
        assert!(value.get("resources").is_none());
        assert!(value.get("prompts").is_none());
        assert!(value.get("logging").is_none());
        assert!(value.get("tasks").is_none());
    }

    #[test]
    fn server_capabilities_round_trip() {
        let caps = ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
            resources: Some(ResourcesCapability {
                subscribe: false,
                list_changed: true,
            }),
            prompts: None,
            logging: Some(LoggingCapability {}),
            completions: None,
            tasks: None,
            ..Default::default()
        };
        let json_str = serde_json::to_string(&caps).expect("serialize");
        let deserialized: ServerCapabilities =
            serde_json::from_str(&json_str).expect("deserialize");
        assert!(deserialized.tools.is_some());
        assert!(deserialized.tools.as_ref().unwrap().list_changed);
        assert!(deserialized.resources.is_some());
        assert!(!deserialized.resources.as_ref().unwrap().subscribe);
        assert!(deserialized.prompts.is_none());
        assert!(deserialized.logging.is_some());
        assert!(deserialized.tasks.is_none());
    }

    // ========================================================================
    // ToolsCapability Tests
    // ========================================================================

    #[test]
    fn tools_capability_default_omits_false() {
        let cap = ToolsCapability::default();
        let value = serde_json::to_value(&cap).expect("serialize");
        // list_changed defaults to false and should be omitted
        assert!(value.get("listChanged").is_none());
    }

    #[test]
    fn tools_capability_list_changed() {
        let cap = ToolsCapability { list_changed: true };
        let value = serde_json::to_value(&cap).expect("serialize");
        assert_eq!(value["listChanged"], true);
    }

    // ========================================================================
    // ResourcesCapability Tests
    // ========================================================================

    #[test]
    fn resources_capability_default() {
        let cap = ResourcesCapability::default();
        let value = serde_json::to_value(&cap).expect("serialize");
        assert!(value.get("subscribe").is_none());
        assert!(value.get("listChanged").is_none());
    }

    #[test]
    fn resources_capability_full() {
        let cap = ResourcesCapability {
            subscribe: true,
            list_changed: true,
        };
        let value = serde_json::to_value(&cap).expect("serialize");
        assert_eq!(value["subscribe"], true);
        assert_eq!(value["listChanged"], true);
    }

    // ========================================================================
    // ClientCapabilities Tests
    // ========================================================================

    #[test]
    fn client_capabilities_default_serialization() {
        let caps = ClientCapabilities::default();
        let value = serde_json::to_value(&caps).expect("serialize");
        assert_eq!(value, json!({}));
    }

    #[test]
    fn client_capabilities_full_serialization() {
        let caps = ClientCapabilities {
            sampling: Some(SamplingCapability {}),
            elicitation: Some(ElicitationCapability::both()),
            roots: Some(RootsCapability { list_changed: true }),
            ..Default::default()
        };
        let value = serde_json::to_value(&caps).expect("serialize");
        assert!(value.get("sampling").is_some());
        assert!(value.get("elicitation").is_some());
        assert_eq!(value["roots"]["listChanged"], true);
    }

    #[test]
    fn client_capabilities_round_trip() {
        let caps = ClientCapabilities {
            sampling: Some(SamplingCapability {}),
            elicitation: None,
            roots: Some(RootsCapability {
                list_changed: false,
            }),
            ..Default::default()
        };
        let json_str = serde_json::to_string(&caps).expect("serialize");
        let deserialized: ClientCapabilities =
            serde_json::from_str(&json_str).expect("deserialize");
        assert!(deserialized.sampling.is_some());
        assert!(deserialized.elicitation.is_none());
        assert!(deserialized.roots.is_some());
    }

    // ========================================================================
    // ElicitationCapability Tests
    // ========================================================================

    #[test]
    fn elicitation_capability_form_only() {
        let cap = ElicitationCapability::form();
        assert!(cap.supports_form());
        assert!(!cap.supports_url());
        let value = serde_json::to_value(&cap).expect("serialize");
        assert!(value.get("form").is_some());
        assert!(value.get("url").is_none());
    }

    #[test]
    fn elicitation_capability_url_only() {
        let cap = ElicitationCapability::url();
        assert!(!cap.supports_form());
        assert!(cap.supports_url());
    }

    #[test]
    fn elicitation_capability_both() {
        let cap = ElicitationCapability::both();
        assert!(cap.supports_form());
        assert!(cap.supports_url());
    }

    // ========================================================================
    // ServerInfo / ClientInfo Tests
    // ========================================================================

    #[test]
    fn server_info_serialization() {
        let info = ServerInfo {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
        };
        let value = serde_json::to_value(&info).expect("serialize");
        assert_eq!(value["name"], "test-server");
        assert_eq!(value["version"], "1.0.0");
    }

    #[test]
    fn client_info_serialization() {
        let info = ClientInfo {
            name: "test-client".to_string(),
            version: "0.1.0".to_string(),
        };
        let value = serde_json::to_value(&info).expect("serialize");
        assert_eq!(value["name"], "test-client");
        assert_eq!(value["version"], "0.1.0");
    }

    // ========================================================================
    // Icon Tests
    // ========================================================================

    #[test]
    fn icon_new() {
        let icon = Icon::new("https://example.com/icon.png");
        assert!(icon.has_src());
        assert!(!icon.is_data_uri());
        assert!(icon.mime_type.is_none());
        assert!(icon.sizes.is_none());
    }

    #[test]
    fn icon_with_mime_type() {
        let icon = Icon::with_mime_type("https://example.com/icon.png", "image/png");
        assert!(icon.has_src());
        assert_eq!(icon.mime_type, Some("image/png".to_string()));
    }

    #[test]
    fn icon_full() {
        let icon = Icon::full("https://example.com/icon.png", "image/png", "32x32");
        assert_eq!(icon.src, Some("https://example.com/icon.png".to_string()));
        assert_eq!(icon.mime_type, Some("image/png".to_string()));
        assert_eq!(icon.sizes, Some("32x32".to_string()));
    }

    #[test]
    fn icon_data_uri() {
        let icon = Icon::new("data:image/png;base64,iVBORw0KGgo=");
        assert!(icon.is_data_uri());
    }

    #[test]
    fn icon_default_no_src() {
        let icon = Icon::default();
        assert!(!icon.has_src());
        assert!(!icon.is_data_uri());
    }

    #[test]
    fn icon_serialization() {
        let icon = Icon::full(
            "https://example.com/icon.svg",
            "image/svg+xml",
            "16x16 32x32",
        );
        let value = serde_json::to_value(&icon).expect("serialize");
        assert_eq!(value["src"], "https://example.com/icon.svg");
        assert_eq!(value["mimeType"], "image/svg+xml");
        assert_eq!(value["sizes"], "16x16 32x32");
    }

    #[test]
    fn icon_serialization_omits_none_fields() {
        let icon = Icon::new("https://example.com/icon.png");
        let value = serde_json::to_value(&icon).expect("serialize");
        assert!(value.get("src").is_some());
        assert!(value.get("mimeType").is_none());
        assert!(value.get("sizes").is_none());
    }

    #[test]
    fn icon_equality() {
        let a = Icon::new("https://example.com/icon.png");
        let b = Icon::new("https://example.com/icon.png");
        let c = Icon::new("https://example.com/other.png");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn legacy_icon_rejects_final_theme_without_mutating_accepted_wire() {
        let accepted = json!({
            "src": "https://example.com/icon.svg",
            "mimeType": "image/svg+xml",
            "sizes": "48x48"
        });
        let legacy: Icon = serde_json::from_value(accepted.clone()).expect("legacy icon");
        let baseline = accepted.clone();
        let mut planted = accepted.clone();
        planted["theme"] = json!("dark");
        assert!(
            serde_json::from_value::<Icon>(planted).is_err(),
            "the final-only theme field must not be silently discarded by the legacy icon"
        );
        assert_eq!(
            accepted, baseline,
            "the rejected one-field final addition cannot mutate accepted legacy wire state"
        );
        assert_eq!(serde_json::to_value(legacy).expect("legacy wire"), accepted);
    }

    // ========================================================================
    // Content Tests
    // ========================================================================

    #[test]
    fn content_text_serialization() {
        let content = Content::Text {
            text: "Hello, world!".to_string(),
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "text");
        assert_eq!(value["text"], "Hello, world!");
    }

    #[test]
    fn content_image_serialization() {
        let content = Content::Image {
            data: "iVBORw0KGgo=".to_string(),
            mime_type: "image/png".to_string(),
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "image");
        assert_eq!(value["data"], "iVBORw0KGgo=");
        assert_eq!(value["mimeType"], "image/png");
    }

    #[test]
    fn content_audio_serialization() {
        let content = Content::Audio {
            data: "UklGRg==".to_string(),
            mime_type: "audio/wav".to_string(),
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "audio");
        assert_eq!(value["data"], "UklGRg==");
        assert_eq!(value["mimeType"], "audio/wav");
    }

    #[test]
    fn content_resource_serialization() {
        let content = Content::Resource {
            resource: ResourceContent {
                uri: "file://config.json".to_string(),
                mime_type: Some("application/json".to_string()),
                text: Some("{\"key\": \"value\"}".to_string()),
                blob: None,
            },
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "resource");
        assert_eq!(value["resource"]["uri"], "file://config.json");
        assert_eq!(value["resource"]["mimeType"], "application/json");
        assert_eq!(value["resource"]["text"], "{\"key\": \"value\"}");
        assert!(value["resource"].get("blob").is_none());
    }

    #[test]
    fn content_text_deserialization() {
        let json = json!({"type": "text", "text": "Hello!"});
        let content: Content = serde_json::from_value(json).expect("deserialize");
        let text = match content {
            Content::Text { text } => Some(text),
            _ => None,
        };
        assert_eq!(text.as_deref(), Some("Hello!"));
    }

    #[test]
    fn content_image_deserialization() {
        let json = json!({"type": "image", "data": "abc123", "mimeType": "image/jpeg"});
        let content: Content = serde_json::from_value(json).expect("deserialize");
        let (data, mime_type) = match content {
            Content::Image { data, mime_type } => (Some(data), Some(mime_type)),
            _ => (None, None),
        };
        assert_eq!(data.as_deref(), Some("abc123"));
        assert_eq!(mime_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn content_audio_deserialization() {
        let json = json!({"type": "audio", "data": "abc123", "mimeType": "audio/mpeg"});
        let content: Content = serde_json::from_value(json).expect("deserialize");
        let (data, mime_type) = match content {
            Content::Audio { data, mime_type } => (Some(data), Some(mime_type)),
            _ => (None, None),
        };
        assert_eq!(data.as_deref(), Some("abc123"));
        assert_eq!(mime_type.as_deref(), Some("audio/mpeg"));
    }

    #[test]
    fn legacy_content_rejects_final_metadata_without_mutating_accepted_wire() {
        let accepted = json!({"type": "text", "text": "legacy text"});
        let legacy: Content = serde_json::from_value(accepted.clone()).expect("legacy content");
        let baseline = accepted.clone();
        let mut planted = accepted.clone();
        planted["_meta"] = json!({"com.example/renderHint": true});
        assert!(
            serde_json::from_value::<Content>(planted).is_err(),
            "the final-only content metadata must not be silently discarded by legacy content"
        );
        assert_eq!(
            accepted, baseline,
            "the rejected one-field final metadata addition cannot mutate accepted legacy wire state"
        );
        assert_eq!(serde_json::to_value(legacy).expect("legacy wire"), accepted);
    }

    // ========================================================================
    // ResourceContent Tests
    // ========================================================================

    #[test]
    fn resource_content_text_serialization() {
        let rc = ResourceContent {
            uri: "file://readme.md".to_string(),
            mime_type: Some("text/markdown".to_string()),
            text: Some("# Hello".to_string()),
            blob: None,
        };
        let value = serde_json::to_value(&rc).expect("serialize");
        assert_eq!(value["uri"], "file://readme.md");
        assert_eq!(value["mimeType"], "text/markdown");
        assert_eq!(value["text"], "# Hello");
        assert!(value.get("blob").is_none());
    }

    #[test]
    fn resource_content_blob_serialization() {
        let rc = ResourceContent {
            uri: "file://image.png".to_string(),
            mime_type: Some("image/png".to_string()),
            text: None,
            blob: Some("base64data".to_string()),
        };
        let value = serde_json::to_value(&rc).expect("serialize");
        assert_eq!(value["uri"], "file://image.png");
        assert!(value.get("text").is_none());
        assert_eq!(value["blob"], "base64data");
    }

    #[test]
    fn resource_content_minimal() {
        let rc = ResourceContent {
            uri: "file://test".to_string(),
            mime_type: None,
            text: None,
            blob: None,
        };
        let value = serde_json::to_value(&rc).expect("serialize");
        assert_eq!(value["uri"], "file://test");
        assert!(value.get("mimeType").is_none());
        assert!(value.get("text").is_none());
        assert!(value.get("blob").is_none());
    }

    // ========================================================================
    // Role Tests
    // ========================================================================

    #[test]
    fn role_serialization() {
        assert_eq!(serde_json::to_value(Role::User).unwrap(), "user");
        assert_eq!(serde_json::to_value(Role::Assistant).unwrap(), "assistant");
    }

    #[test]
    fn role_deserialization() {
        let user: Role = serde_json::from_value(json!("user")).expect("deserialize");
        assert_eq!(user, Role::User);
        let assistant: Role = serde_json::from_value(json!("assistant")).expect("deserialize");
        assert_eq!(assistant, Role::Assistant);
    }

    // ========================================================================
    // PromptMessage Tests
    // ========================================================================

    #[test]
    fn prompt_message_serialization() {
        let msg = PromptMessage {
            role: Role::User,
            content: Content::Text {
                text: "Tell me a joke".to_string(),
            },
        };
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "Tell me a joke");
    }

    #[test]
    fn prompt_message_assistant() {
        let msg = PromptMessage {
            role: Role::Assistant,
            content: Content::Text {
                text: "Here's a joke...".to_string(),
            },
        };
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["role"], "assistant");
    }

    // ========================================================================
    // PromptArgument Tests
    // ========================================================================

    #[test]
    fn prompt_argument_required() {
        let arg = PromptArgument {
            name: "language".to_string(),
            description: Some("Target language".to_string()),
            required: true,
        };
        let value = serde_json::to_value(&arg).expect("serialize");
        assert_eq!(value["name"], "language");
        assert_eq!(value["description"], "Target language");
        assert_eq!(value["required"], true);
    }

    #[test]
    fn prompt_argument_optional_omits_false() {
        let arg = PromptArgument {
            name: "style".to_string(),
            description: None,
            required: false,
        };
        let value = serde_json::to_value(&arg).expect("serialize");
        assert_eq!(value["name"], "style");
        assert!(value.get("description").is_none());
        // required=false should be omitted
        assert!(value.get("required").is_none());
    }

    #[test]
    fn prompt_argument_deserialization_defaults() {
        let json = json!({"name": "arg1"});
        let arg: PromptArgument = serde_json::from_value(json).expect("deserialize");
        assert_eq!(arg.name, "arg1");
        assert!(arg.description.is_none());
        assert!(!arg.required);
    }

    // ========================================================================
    // Tool Definition Tests
    // ========================================================================

    #[test]
    fn tool_minimal_serialization() {
        let tool = Tool {
            name: "add".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        };
        let value = serde_json::to_value(&tool).expect("serialize");
        assert_eq!(value["name"], "add");
        assert_eq!(value["inputSchema"]["type"], "object");
        assert!(value.get("description").is_none());
        assert!(value.get("outputSchema").is_none());
        assert!(value.get("icon").is_none());
        assert!(value.get("version").is_none());
        assert!(value.get("tags").is_none());
        assert!(value.get("annotations").is_none());
    }

    #[test]
    fn tool_full_serialization() {
        let tool = Tool {
            name: "compute".to_string(),
            description: Some("Runs a computation".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": { "x": { "type": "number" } },
                "required": ["x"]
            }),
            output_schema: Some(json!({"type": "number"})),
            icon: Some(Icon::new("https://example.com/icon.png")),
            version: Some("2.1.0".to_string()),
            tags: vec!["math".to_string(), "compute".to_string()],
            annotations: Some(ToolAnnotations::new().read_only(true).idempotent(true)),
        };
        let value = serde_json::to_value(&tool).expect("serialize");
        assert_eq!(value["name"], "compute");
        assert_eq!(value["description"], "Runs a computation");
        assert!(value["inputSchema"]["properties"]["x"].is_object());
        assert_eq!(value["outputSchema"]["type"], "number");
        assert_eq!(value["icon"]["src"], "https://example.com/icon.png");
        assert_eq!(value["version"], "2.1.0");
        assert_eq!(value["tags"], json!(["math", "compute"]));
        assert_eq!(value["annotations"]["readOnlyHint"], true);
        assert_eq!(value["annotations"]["idempotentHint"], true);
    }

    #[test]
    fn tool_round_trip() {
        let json = json!({
            "name": "greet",
            "description": "Greets the user",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}},
            "outputSchema": {"type": "string"},
            "version": "1.0.0",
            "tags": ["greeting"],
            "annotations": {"readOnlyHint": true}
        });
        let tool: Tool = serde_json::from_value(json.clone()).expect("deserialize");
        assert_eq!(tool.name, "greet");
        assert_eq!(tool.version, Some("1.0.0".to_string()));
        assert_eq!(tool.tags, vec!["greeting"]);
        assert!(tool.annotations.as_ref().unwrap().read_only.unwrap());
        let re_serialized = serde_json::to_value(&tool).expect("re-serialize");
        assert_eq!(re_serialized["name"], json["name"]);
    }

    // ========================================================================
    // Resource Definition Tests
    // ========================================================================

    #[test]
    fn resource_minimal_serialization() {
        let resource = Resource {
            uri: "file://test.txt".to_string(),
            name: "Test File".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let value = serde_json::to_value(&resource).expect("serialize");
        assert_eq!(value["uri"], "file://test.txt");
        assert_eq!(value["name"], "Test File");
        assert!(value.get("description").is_none());
        assert!(value.get("mimeType").is_none());
    }

    #[test]
    fn resource_full_round_trip() {
        let json = json!({
            "uri": "file://config.json",
            "name": "Config",
            "description": "Application configuration",
            "mimeType": "application/json",
            "version": "3.0.0",
            "tags": ["config", "json"]
        });
        let resource: Resource = serde_json::from_value(json).expect("deserialize");
        assert_eq!(resource.uri, "file://config.json");
        assert_eq!(resource.mime_type, Some("application/json".to_string()));
        assert_eq!(resource.tags, vec!["config", "json"]);
    }

    // ========================================================================
    // ResourceTemplate Tests
    // ========================================================================

    #[test]
    fn resource_template_serialization() {
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "File Reader".to_string(),
            description: Some("Read any file".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        };
        let value = serde_json::to_value(&template).expect("serialize");
        assert_eq!(value["uriTemplate"], "file://{path}");
        assert_eq!(value["name"], "File Reader");
        assert_eq!(value["description"], "Read any file");
        assert_eq!(value["mimeType"], "text/plain");
    }

    #[test]
    fn resource_template_peer_admission_rejects_malformed_and_oversized_templates() {
        let accepted_wire = json!({
            "uriTemplate": "file://{path}",
            "name": "File Reader"
        });
        let accepted: ResourceTemplate = serde_json::from_value(accepted_wire.clone())
            .expect("a legal exact-2024 resource template remains admissible");
        assert_eq!(
            serde_json::to_value(accepted).expect("admitted template serializes"),
            accepted_wire
        );

        let mut malformed = accepted_wire.clone();
        malformed["uriTemplate"] = json!("file://{path");
        assert!(
            serde_json::from_value::<ResourceTemplate>(malformed).is_err(),
            "changing only the closing brace rejects malformed peer input"
        );

        let mut oversized = accepted_wire;
        oversized["uriTemplate"] = json!(format!(
            "mcp://{}",
            "x".repeat(crate::MAX_URI_TEMPLATE_BYTES)
        ));
        assert!(
            serde_json::from_value::<ResourceTemplate>(oversized).is_err(),
            "changing only the template length beyond the protocol bound rejects peer input"
        );
    }

    // ========================================================================
    // Prompt Definition Tests
    // ========================================================================

    #[test]
    fn prompt_with_arguments() {
        let prompt = Prompt {
            name: "translate".to_string(),
            description: Some("Translate text".to_string()),
            arguments: vec![
                PromptArgument {
                    name: "text".to_string(),
                    description: Some("Text to translate".to_string()),
                    required: true,
                },
                PromptArgument {
                    name: "language".to_string(),
                    description: Some("Target language".to_string()),
                    required: true,
                },
                PromptArgument {
                    name: "style".to_string(),
                    description: None,
                    required: false,
                },
            ],
            icon: None,
            version: None,
            tags: vec![],
        };
        let value = serde_json::to_value(&prompt).expect("serialize");
        assert_eq!(value["name"], "translate");
        let args = value["arguments"].as_array().expect("arguments array");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0]["name"], "text");
        assert_eq!(args[0]["required"], true);
        assert_eq!(args[2]["name"], "style");
        // required=false should be omitted
        assert!(args[2].get("required").is_none());
    }

    #[test]
    fn prompt_empty_arguments_omitted() {
        let prompt = Prompt {
            name: "simple".to_string(),
            description: None,
            arguments: vec![],
            icon: None,
            version: None,
            tags: vec![],
        };
        let value = serde_json::to_value(&prompt).expect("serialize");
        assert!(value.get("arguments").is_none());
    }

    // ========================================================================
    // TaskId Tests
    // ========================================================================

    #[test]
    fn task_id_new_has_prefix() {
        let id = TaskId::new();
        assert!(id.as_str().starts_with("task-"));
    }

    #[test]
    fn task_id_from_string() {
        let id = TaskId::from_string("task-abc123");
        assert_eq!(id.as_str(), "task-abc123");
    }

    #[test]
    fn task_id_display() {
        let id = TaskId::from_string("task-xyz");
        assert_eq!(format!("{id}"), "task-xyz");
    }

    #[test]
    fn task_id_from_impls() {
        let from_string: TaskId = "my-task".to_string().into();
        assert_eq!(from_string.as_str(), "my-task");

        let from_str: TaskId = "another-task".into();
        assert_eq!(from_str.as_str(), "another-task");
    }

    #[test]
    fn task_id_serialization() {
        let id = TaskId::from_string("task-1");
        let value = serde_json::to_value(&id).expect("serialize");
        assert_eq!(value, "task-1");

        let deserialized: TaskId = serde_json::from_value(json!("task-2")).expect("deserialize");
        assert_eq!(deserialized.as_str(), "task-2");
    }

    #[test]
    fn task_id_equality() {
        let a = TaskId::from_string("task-1");
        let b = TaskId::from_string("task-1");
        let c = TaskId::from_string("task-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ========================================================================
    // TaskStatus Tests
    // ========================================================================

    #[test]
    fn task_status_is_terminal() {
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
    }

    #[test]
    fn task_status_is_active() {
        assert!(TaskStatus::Pending.is_active());
        assert!(TaskStatus::Running.is_active());
        assert!(!TaskStatus::Completed.is_active());
        assert!(!TaskStatus::Failed.is_active());
        assert!(!TaskStatus::Cancelled.is_active());
    }

    #[test]
    fn task_status_serialization() {
        assert_eq!(
            serde_json::to_value(TaskStatus::Pending).unwrap(),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(TaskStatus::Running).unwrap(),
            "running"
        );
        assert_eq!(
            serde_json::to_value(TaskStatus::Completed).unwrap(),
            "completed"
        );
        assert_eq!(serde_json::to_value(TaskStatus::Failed).unwrap(), "failed");
        assert_eq!(
            serde_json::to_value(TaskStatus::Cancelled).unwrap(),
            "cancelled"
        );
    }

    #[test]
    fn task_status_deserialization() {
        assert_eq!(
            serde_json::from_value::<TaskStatus>(json!("pending")).unwrap(),
            TaskStatus::Pending
        );
        assert_eq!(
            serde_json::from_value::<TaskStatus>(json!("running")).unwrap(),
            TaskStatus::Running
        );
        assert_eq!(
            serde_json::from_value::<TaskStatus>(json!("completed")).unwrap(),
            TaskStatus::Completed
        );
        assert_eq!(
            serde_json::from_value::<TaskStatus>(json!("failed")).unwrap(),
            TaskStatus::Failed
        );
        assert_eq!(
            serde_json::from_value::<TaskStatus>(json!("cancelled")).unwrap(),
            TaskStatus::Cancelled
        );
    }

    // ========================================================================
    // TaskInfo Tests
    // ========================================================================

    #[test]
    fn task_info_serialization() {
        let info = TaskInfo {
            id: TaskId::from_string("task-1"),
            task_type: "compute".to_string(),
            status: TaskStatus::Running,
            progress: Some(0.5),
            message: Some("Processing...".to_string()),
            created_at: "2026-01-28T00:00:00Z".to_string(),
            started_at: Some("2026-01-28T00:01:00Z".to_string()),
            completed_at: None,
            error: None,
        };
        let value = serde_json::to_value(&info).expect("serialize");
        assert_eq!(value["id"], "task-1");
        assert_eq!(value["taskType"], "compute");
        assert_eq!(value["status"], "running");
        assert_eq!(value["progress"], 0.5);
        assert_eq!(value["message"], "Processing...");
        assert_eq!(value["createdAt"], "2026-01-28T00:00:00Z");
        assert_eq!(value["startedAt"], "2026-01-28T00:01:00Z");
        assert!(value.get("completedAt").is_none());
        assert!(value.get("error").is_none());
    }

    #[test]
    fn task_info_minimal() {
        let json = json!({
            "id": "task-2",
            "taskType": "demo",
            "status": "pending",
            "createdAt": "2026-01-28T00:00:00Z"
        });
        let info: TaskInfo = serde_json::from_value(json).expect("deserialize");
        assert_eq!(info.id.as_str(), "task-2");
        assert_eq!(info.status, TaskStatus::Pending);
        assert!(info.progress.is_none());
        assert!(info.message.is_none());
    }

    // ========================================================================
    // TaskResult Tests
    // ========================================================================

    #[test]
    fn task_result_success() {
        let result = TaskResult {
            id: TaskId::from_string("task-1"),
            success: true,
            data: Some(json!({"value": 42})),
            error: None,
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["id"], "task-1");
        assert_eq!(value["success"], true);
        assert_eq!(value["data"]["value"], 42);
        assert!(value.get("error").is_none());
    }

    #[test]
    fn task_result_failure() {
        let result = TaskResult {
            id: TaskId::from_string("task-2"),
            success: false,
            data: None,
            error: Some("computation failed".to_string()),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(value["success"], false);
        assert!(value.get("data").is_none());
        assert_eq!(value["error"], "computation failed");
    }

    // ========================================================================
    // SamplingContent Tests
    // ========================================================================

    #[test]
    fn sampling_content_text_serialization() {
        let content = SamplingContent::Text {
            text: "Hello".to_string(),
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "text");
        assert_eq!(value["text"], "Hello");
    }

    #[test]
    fn sampling_content_image_serialization() {
        let content = SamplingContent::Image {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        };
        let value = serde_json::to_value(&content).expect("serialize");
        assert_eq!(value["type"], "image");
        assert_eq!(value["data"], "base64data");
        assert_eq!(value["mimeType"], "image/png");
    }

    // ========================================================================
    // SamplingMessage Tests
    // ========================================================================

    #[test]
    fn sampling_message_user_constructor() {
        let msg = SamplingMessage::user("Hello!");
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "Hello!");
    }

    #[test]
    fn sampling_message_assistant_constructor() {
        let msg = SamplingMessage::assistant("Hi there!");
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"]["text"], "Hi there!");
    }

    // ========================================================================
    // ModelPreferences Tests
    // ========================================================================

    #[test]
    fn model_preferences_default() {
        let prefs = ModelPreferences::default();
        let value = serde_json::to_value(&prefs).expect("serialize");
        // All optional fields should be omitted
        assert!(value.get("hints").is_none());
        assert!(value.get("costPriority").is_none());
        assert!(value.get("speedPriority").is_none());
        assert!(value.get("intelligencePriority").is_none());
    }

    #[test]
    fn model_preferences_full() {
        let prefs = ModelPreferences {
            hints: vec![ModelHint {
                name: Some("claude-3".to_string()),
            }],
            cost_priority: Some(0.3),
            speed_priority: Some(0.5),
            intelligence_priority: Some(0.9),
        };
        let value = serde_json::to_value(&prefs).expect("serialize");
        assert_eq!(value["hints"][0]["name"], "claude-3");
        assert_eq!(value["costPriority"], 0.3);
        assert_eq!(value["speedPriority"], 0.5);
        assert_eq!(value["intelligencePriority"], 0.9);
    }

    // ========================================================================
    // StopReason Tests
    // ========================================================================

    #[test]
    fn stop_reason_serialization() {
        assert_eq!(
            serde_json::to_value(StopReason::EndTurn).unwrap(),
            "endTurn"
        );
        assert_eq!(
            serde_json::to_value(StopReason::StopSequence).unwrap(),
            "stopSequence"
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTokens).unwrap(),
            "maxTokens"
        );
    }

    #[test]
    fn stop_reason_deserialization() {
        assert_eq!(
            serde_json::from_value::<StopReason>(json!("endTurn")).unwrap(),
            StopReason::EndTurn
        );
        assert_eq!(
            serde_json::from_value::<StopReason>(json!("stopSequence")).unwrap(),
            StopReason::StopSequence
        );
        assert_eq!(
            serde_json::from_value::<StopReason>(json!("maxTokens")).unwrap(),
            StopReason::MaxTokens
        );
    }

    #[test]
    fn stop_reason_default() {
        assert_eq!(StopReason::default(), StopReason::EndTurn);
    }

    // ========================================================================
    // PROTOCOL_VERSION Test
    // ========================================================================

    #[test]
    fn protocol_version_value() {
        assert_eq!(PROTOCOL_VERSION, "2024-11-05");
    }

    // ========================================================================
    // ToolAnnotations Tests
    // ========================================================================

    #[test]
    fn tool_annotations_default_is_empty() {
        let ann = ToolAnnotations::new();
        assert!(ann.is_empty());
    }

    #[test]
    fn tool_annotations_builder_chain() {
        let ann = ToolAnnotations::new()
            .read_only(true)
            .idempotent(true)
            .destructive(false)
            .open_world_hint(false);

        assert_eq!(ann.read_only, Some(true));
        assert_eq!(ann.idempotent, Some(true));
        assert_eq!(ann.destructive, Some(false));
        assert_eq!(ann.open_world_hint, Some(false));
        assert!(!ann.is_empty());
    }

    #[test]
    fn tool_annotations_single_field_not_empty() {
        assert!(!ToolAnnotations::new().destructive(true).is_empty());
        assert!(!ToolAnnotations::new().idempotent(false).is_empty());
        assert!(!ToolAnnotations::new().read_only(true).is_empty());
        assert!(!ToolAnnotations::new().open_world_hint(true).is_empty());
    }

    #[test]
    fn tool_annotations_serialization_skips_none() {
        let ann = ToolAnnotations::new().read_only(true);
        let value = serde_json::to_value(&ann).expect("serialize");
        assert_eq!(value["readOnlyHint"], true);
        assert!(value.get("destructiveHint").is_none());
        assert!(value.get("idempotentHint").is_none());
        assert!(value.get("openWorldHint").is_none());
    }

    #[test]
    fn tool_annotations_round_trip() {
        let ann = ToolAnnotations::new()
            .destructive(true)
            .idempotent(false)
            .open_world_hint(true);
        let json_str = serde_json::to_string(&ann).expect("serialize");
        let deserialized: ToolAnnotations = serde_json::from_str(&json_str).expect("deserialize");
        assert_eq!(ann, deserialized);
    }

    // ========================================================================
    // Icon Tests
    // ========================================================================

    #[test]
    fn icon_is_data_uri_with_data_prefix() {
        let icon = Icon {
            src: Some("data:image/png;base64,iVBOR".to_string()),
            mime_type: None,
            sizes: None,
        };
        assert!(icon.is_data_uri());
    }

    #[test]
    fn icon_is_data_uri_without_data_prefix() {
        let icon = Icon {
            src: Some("https://example.com/icon.png".to_string()),
            mime_type: None,
            sizes: None,
        };
        assert!(!icon.is_data_uri());
    }

    #[test]
    fn icon_is_data_uri_no_src() {
        let icon = Icon {
            src: None,
            mime_type: None,
            sizes: None,
        };
        assert!(!icon.is_data_uri());
    }

    #[test]
    fn icon_is_data_uri_empty_string() {
        let icon = Icon {
            src: Some(String::new()),
            mime_type: None,
            sizes: None,
        };
        assert!(!icon.is_data_uri());
    }

    // ========================================================================
    // Content Binary Factory Tests
    // ========================================================================

    #[test]
    fn content_image_bytes_encodes_base64() {
        let bytes: &[u8] = &[0x89, 0x50, 0x4E, 0x47]; // PNG header
        let content = Content::image_bytes(bytes, "image/png");
        match &content {
            Content::Image { data, mime_type } => {
                assert_eq!(mime_type, "image/png");
                // Verify it's valid base64 that decodes back
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .expect("valid base64");
                assert_eq!(decoded, bytes);
            }
            _ => panic!("expected Image content"),
        }
    }

    #[test]
    fn content_image_bytes_empty() {
        let content = Content::image_bytes(&[] as &[u8], "image/png");
        match &content {
            Content::Image { data, .. } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .expect("valid base64");
                assert_eq!(decoded.len(), 0);
            }
            _ => panic!("expected Image content"),
        }
    }

    #[test]
    fn content_audio_bytes_encodes_base64() {
        let bytes: &[u8] = &[0x52, 0x49, 0x46, 0x46]; // RIFF header
        let content = Content::audio_bytes(bytes, "audio/wav");
        match &content {
            Content::Audio { data, mime_type } => {
                assert_eq!(mime_type, "audio/wav");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .expect("valid base64");
                assert_eq!(decoded, bytes);
            }
            _ => panic!("expected Audio content"),
        }
    }

    #[test]
    fn content_resource_blob_bytes_encodes_base64() {
        let bytes: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
        let content = Content::resource_blob_bytes(
            "blob://test",
            Some("application/octet-stream".into()),
            bytes,
        );
        match &content {
            Content::Resource {
                resource: ResourceContent { uri, blob, .. },
            } => {
                assert_eq!(uri, "blob://test");
                let blob_data = blob.as_ref().expect("should have blob");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(blob_data)
                    .expect("valid base64");
                assert_eq!(decoded, bytes);
            }
            _ => panic!("expected Resource content"),
        }
    }

    #[test]
    fn content_resource_blob_bytes_none_mime() {
        let content = Content::resource_blob_bytes("blob://test", None, &[1, 2, 3]);
        match &content {
            Content::Resource {
                resource: ResourceContent { mime_type, .. },
            } => {
                assert!(mime_type.is_none());
            }
            _ => panic!("expected Resource content"),
        }
    }

    // =========================================================================
    // Additional coverage tests (bd-1cd5)
    // =========================================================================

    #[test]
    fn root_new_constructor() {
        let root = Root::new("file:///home/user/project");
        assert_eq!(root.uri, "file:///home/user/project");
        assert!(root.name.is_none());
    }

    #[test]
    fn root_new_from_string() {
        let uri = String::from("file:///tmp");
        let root = Root::new(uri);
        assert_eq!(root.uri, "file:///tmp");
    }

    #[test]
    fn root_with_name_constructor() {
        let root = Root::with_name("file:///workspace", "My Project");
        assert_eq!(root.uri, "file:///workspace");
        assert_eq!(root.name.as_deref(), Some("My Project"));
    }

    #[test]
    fn root_with_name_from_strings() {
        let uri = String::from("file:///src");
        let name = String::from("Source");
        let root = Root::with_name(uri, name);
        assert_eq!(root.uri, "file:///src");
        assert_eq!(root.name.unwrap(), "Source");
    }

    #[test]
    fn content_text_constructor() {
        let content = Content::text("hello world");
        match &content {
            Content::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn content_text_from_string() {
        let s = String::from("owned string");
        let content = Content::text(s);
        match &content {
            Content::Text { text } => assert_eq!(text, "owned string"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn content_image_base64_constructor() {
        let content = Content::image_base64("aGVsbG8=", "image/png");
        match &content {
            Content::Image { data, mime_type } => {
                assert_eq!(data, "aGVsbG8=");
                assert_eq!(mime_type, "image/png");
            }
            _ => panic!("expected Image content"),
        }
    }

    #[test]
    fn content_audio_base64_constructor() {
        let content = Content::audio_base64("AAAA", "audio/mp3");
        match &content {
            Content::Audio { data, mime_type } => {
                assert_eq!(data, "AAAA");
                assert_eq!(mime_type, "audio/mp3");
            }
            _ => panic!("expected Audio content"),
        }
    }

    #[test]
    fn content_resource_text_constructor() {
        let content = Content::resource_text(
            "file:///readme.md",
            Some("text/markdown".to_string()),
            "# Hello",
        );
        match &content {
            Content::Resource { resource } => {
                assert_eq!(resource.uri, "file:///readme.md");
                assert_eq!(resource.mime_type.as_deref(), Some("text/markdown"));
                assert_eq!(resource.text.as_deref(), Some("# Hello"));
                assert!(resource.blob.is_none());
            }
            _ => panic!("expected Resource content"),
        }
    }

    #[test]
    fn content_resource_text_no_mime() {
        let content = Content::resource_text("file:///data.txt", None, "data");
        match &content {
            Content::Resource { resource } => {
                assert!(resource.mime_type.is_none());
                assert_eq!(resource.text.as_deref(), Some("data"));
            }
            _ => panic!("expected Resource content"),
        }
    }

    #[test]
    fn content_resource_blob_base64_constructor() {
        let content = Content::resource_blob_base64(
            "file:///image.png",
            Some("image/png".to_string()),
            "iVBOR",
        );
        match &content {
            Content::Resource { resource } => {
                assert_eq!(resource.uri, "file:///image.png");
                assert_eq!(resource.mime_type.as_deref(), Some("image/png"));
                assert!(resource.text.is_none());
                assert_eq!(resource.blob.as_deref(), Some("iVBOR"));
            }
            _ => panic!("expected Resource content"),
        }
    }

    #[test]
    fn content_resource_blob_base64_no_mime() {
        let content = Content::resource_blob_base64("file:///bin", None, "AQID");
        match &content {
            Content::Resource { resource } => {
                assert!(resource.mime_type.is_none());
                assert_eq!(resource.blob.as_deref(), Some("AQID"));
            }
            _ => panic!("expected Resource content"),
        }
    }

    #[test]
    fn final_models_and_sampling_blocks_round_trip_without_legacy_fields() {
        let tool_wire = json!({
            "name": "weather",
            "title": "Weather lookup",
            "description": "Looks up forecast data",
            "icons": [{"src": "https://example.test/weather.png"}],
            "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}},
            "annotations": {"title": "Forecast", "readOnlyHint": true},
            "_meta": {"com.example/source": "catalog"}
        });
        let tool: FinalTool = serde_json::from_value(tool_wire.clone())
            .expect("final tool accepts final metadata and icons array");
        assert_eq!(tool.title.as_deref(), Some("Weather lookup"));
        assert_eq!(
            serde_json::to_value(&tool).expect("final tool re-encodes"),
            tool_wire
        );

        let sampling_wire = json!({
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "call-1", "name": "weather", "input": {"city": "Boston"}},
                {"type": "tool_result", "toolUseId": "call-1", "content": [{"type": "text", "text": "sunny"}], "structuredContent": {"temperature": 22}}
            ],
            "_meta": {"com.example/turn": 4}
        });
        let message: FinalSamplingMessage = serde_json::from_value(sampling_wire.clone())
            .expect("final sampling admits tool-use and tool-result blocks");
        assert_eq!(
            serde_json::to_value(message).expect("final sampling re-encodes"),
            sampling_wire
        );
    }

    #[test]
    fn final_tool_rejects_one_legacy_icon_field_without_mutating_final_baseline() {
        let accepted = json!({
            "name": "weather",
            "inputSchema": {"type": "object"}
        });
        let baseline: FinalTool =
            serde_json::from_value(accepted.clone()).expect("final tool baseline");
        let mut planted = accepted.clone();
        planted["icon"] = json!({"src": "https://example.test/legacy.png"});
        assert!(
            serde_json::from_value::<FinalTool>(planted).is_err(),
            "only the legacy singular icon field changes the final model"
        );
        assert_eq!(
            serde_json::to_value(baseline).expect("baseline re-encodes"),
            accepted,
            "legacy-field rejection does not mutate final model state"
        );
    }

    #[test]
    fn final_tool_output_schema_distinguishes_absence_from_explicit_null() {
        let absent_wire = json!({
            "name": "weather",
            "inputSchema": {"type": "object"}
        });
        let absent: FinalTool =
            serde_json::from_value(absent_wire.clone()).expect("absent outputSchema is valid");
        assert!(absent.output_schema.is_none());
        assert_eq!(
            serde_json::to_value(absent).expect("absent outputSchema re-encodes"),
            absent_wire
        );

        let accepted_wire = json!({
            "name": "weather",
            "inputSchema": {"type": "object"},
            "outputSchema": {"type": "null"}
        });
        let accepted: FinalTool = serde_json::from_value(accepted_wire.clone())
            .expect("an object schema whose admitted instances are null is valid");

        let mut planted = accepted_wire.clone();
        planted["outputSchema"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<FinalTool>(planted).is_err(),
            "a present outputSchema must itself be an object, not JSON null"
        );
        assert_eq!(
            serde_json::to_value(accepted).expect("accepted outputSchema re-encodes"),
            accepted_wire,
            "rejecting the one-field null plant cannot mutate the accepted model"
        );
    }

    #[test]
    fn final_resource_size_preserves_arbitrary_width_and_rejects_fractional_values() {
        let accepted: serde_json::Value = serde_json::from_str(
            r#"{"uri":"file:///data.bin","name":"data","size":922337203685477580812345678901234567890}"#,
        )
        .expect("arbitrary-width resource size wire parses");
        let resource: FinalResource = serde_json::from_value(accepted.clone())
            .expect("arbitrary-width final resource size is accepted");
        assert_eq!(
            resource.size.as_ref().map(JsonInteger::as_str),
            Some("922337203685477580812345678901234567890")
        );
        assert_eq!(
            serde_json::to_value(resource).expect("arbitrary-width final resource re-encodes"),
            accepted,
            "the exact integer resource size lexeme round-trips"
        );

        let planted: serde_json::Value = serde_json::from_str(
            r#"{"uri":"file:///data.bin","name":"data","size":922337203685477580812345678901234567890.5}"#,
        )
        .expect("one-field fractional resource size wire parses");
        assert!(
            serde_json::from_value::<FinalResource>(planted).is_err(),
            "changing only the resource size to a fractional number rejects it"
        );
    }

    #[test]
    fn final_resource_template_enforces_the_uri_template_schema_format() {
        let accepted_wire = json!({
            "uriTemplate": "mcp://resources/{item:3}{?cursor,labels*}",
            "name": "resource-template"
        });
        let accepted: FinalResourceTemplate = serde_json::from_value(accepted_wire.clone())
            .expect("a final RFC 6570 Level 4 resource template is admitted");
        assert_eq!(
            accepted.uri_template, "mcp://resources/{item:3}{?cursor,labels*}",
            "typed final decoding preserves the accepted template spelling"
        );
        assert_eq!(
            serde_json::to_value(accepted).expect("accepted template re-encodes"),
            accepted_wire,
            "template validation does not normalize the final wire value"
        );

        let mut planted = accepted_wire;
        planted["uriTemplate"] = json!("mcp://resources/{item:0}");
        assert!(
            serde_json::from_value::<FinalResourceTemplate>(planted).is_err(),
            "changing only the prefix modifier to RFC 6570's forbidden zero rejects the template"
        );

        let locally_invalid = FinalResourceTemplate {
            uri_template: "mcp://resources/{item:0}".to_owned(),
            name: "resource-template".to_owned(),
            title: None,
            description: None,
            icons: None,
            mime_type: None,
            annotations: None,
            meta: None,
        };
        assert!(
            serde_json::to_value(locally_invalid).is_err(),
            "direct construction cannot serialize a URI template rejected at peer admission"
        );
    }

    #[test]
    fn apps_02_nested_tool_resource_metadata_and_result_projection_round_trip() {
        let tool_wire = json!({
            "name": "weather",
            "inputSchema": {"type": "object"},
            "_meta": {
                "ui": {
                    "resourceUri": "ui://weather/dashboard",
                    "visibility": ["model", "app"]
                },
                "com.example/catalog": "weather"
            }
        });
        let tool: FinalTool = serde_json::from_value(tool_wire.clone())
            .expect("a nested Apps tool resource binding is valid final metadata");
        let metadata = tool
            .mcp_apps_metadata()
            .expect("nested Apps tool metadata remains typed")
            .expect("the tool declares Apps metadata");
        assert_eq!(
            metadata.resource_uri.as_ref().map(AbsoluteUri::as_str),
            Some("ui://weather/dashboard")
        );
        assert_eq!(
            metadata.effective_visibility(),
            [McpAppsToolVisibility::Model, McpAppsToolVisibility::App]
        );
        assert_eq!(
            serde_json::to_value(&tool).expect("tool re-encodes exact nested metadata"),
            tool_wire
        );

        let resource_wire = json!({
            "uri": "ui://weather/dashboard",
            "name": "weather-dashboard",
            "mimeType": MCP_APPS_HTML_MIME_TYPE,
            "_meta": {"ui": {
                "csp": {
                    "connectDomains": ["https://api.weather.example"],
                    "resourceDomains": ["https://cdn.weather.example"],
                    "frameDomains": ["https://maps.weather.example"],
                    "baseUriDomains": ["https://cdn.weather.example"]
                },
                "permissions": {"geolocation": {}},
                "domain": "weather-view.host.example",
                "prefersBorder": true
            }}
        });
        let resource: FinalResource = serde_json::from_value(resource_wire.clone())
            .expect("an Apps HTML resource accepts nested presentation metadata");
        let resource_metadata = resource
            .mcp_apps_metadata()
            .expect("resource metadata remains typed")
            .expect("resource declares Apps presentation");
        assert_eq!(resource_metadata.prefers_border, Some(true));
        assert_eq!(
            resource_metadata
                .csp
                .as_ref()
                .and_then(|csp| csp.connect_domains.as_ref())
                .map(|domains| domains.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(vec!["https://api.weather.example"])
        );
        assert!(
            resource_metadata
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.geolocation.as_ref())
                .is_some()
        );
        assert_eq!(
            resource_metadata.domain.as_deref(),
            Some("weather-view.host.example")
        );
        tool.mcp_apps_resource_binding()
            .expect("tool metadata is valid")
            .expect("tool has an Apps binding")
            .validate_resource(&resource)
            .expect("the exact Apps HTML catalog resource satisfies the binding");
        assert_eq!(
            serde_json::to_value(&resource).expect("resource re-encodes exact nested metadata"),
            resource_wire
        );

        let final_result = FinalCallToolResult {
            content: vec![ContentBlock::text("sunny")],
            is_error: false,
            structured_content: Some(serde_json::Value::Null),
        };
        let projected = McpAppsToolResult::from_final_call_tool_result(&final_result)
            .expect("a complete final tools/call result projects into Apps content");
        let result_wire = json!({
            "content": [{"type": "text", "text": "sunny"}],
            "structuredContent": null
        });
        assert_eq!(
            serde_json::to_value(&projected).expect("Apps result serializes"),
            result_wire
        );
        assert_eq!(
            serde_json::from_value::<McpAppsToolResult>(result_wire)
                .expect("Apps result round-trips exactly"),
            projected
        );
    }

    #[test]
    fn apps_02_rejects_one_opaque_ui_uri_in_tool_metadata_construction_and_serde() {
        let accepted_uri = AbsoluteUri::parse("ui://opaque").expect("authority-form UI URI");
        let accepted = McpAppsToolMetadata::try_new(Some(accepted_uri), None)
            .expect("the exact UI URI is admitted");
        let accepted_wire = serde_json::json!({"resourceUri": "ui://opaque"});
        assert_eq!(
            serde_json::to_value(&accepted).expect("the admitted UI URI serializes"),
            accepted_wire,
        );

        let opaque_uri = AbsoluteUri::parse("ui:opaque").expect("opaque UI URI is absolute");
        assert_eq!(
            McpAppsToolMetadata::try_new(Some(opaque_uri.clone()), None),
            Err(McpAppsMetadataError::ResourceUriMustUseUiPrefix),
            "removing only the authority delimiter rejects constructor input"
        );
        let planted = McpAppsToolMetadata {
            resource_uri: Some(opaque_uri),
            visibility: None,
        };
        assert!(
            serde_json::to_value(&planted).is_err(),
            "direct construction cannot serialize an opaque ui: URI"
        );

        let mut opaque_wire = accepted_wire.clone();
        opaque_wire["resourceUri"] = serde_json::json!("ui:opaque");
        assert!(
            serde_json::from_value::<McpAppsToolMetadata>(opaque_wire).is_err(),
            "removing only the authority delimiter rejects deserialization"
        );
        assert_eq!(
            serde_json::to_value(&accepted).expect("rejected variants do not mutate the baseline"),
            accepted_wire,
        );
    }

    #[test]
    fn apps_02_rejects_only_deprecated_flat_resource_uri_metadata() {
        let accepted = json!({
            "name": "weather",
            "inputSchema": {"type": "object"},
            "_meta": {"ui": {"resourceUri": "ui://weather/dashboard"}}
        });
        let baseline: FinalTool = serde_json::from_value(accepted.clone())
            .expect("nested Apps resource metadata is the baseline");
        let mut planted = accepted.clone();
        let metadata = planted["_meta"]
            .as_object_mut()
            .expect("baseline metadata is an object");
        let nested = metadata
            .remove(MCP_APPS_UI_METADATA_KEY)
            .expect("baseline has nested Apps metadata");
        let resource_uri = nested["resourceUri"].clone();
        metadata.insert(
            MCP_APPS_DEPRECATED_RESOURCE_URI_METADATA_KEY.to_owned(),
            resource_uri,
        );

        assert!(
            serde_json::from_value::<FinalTool>(planted).is_err(),
            "only replacing nested ui.resourceUri with the deprecated flat key rejects the tool"
        );
        assert_eq!(
            serde_json::to_value(&baseline).expect("baseline remains serializable"),
            accepted,
            "the flat-key rejection cannot mutate the accepted nested binding"
        );
    }

    #[test]
    fn apps_02_rejects_one_csp_origin_beyond_the_bounded_directive_limit() {
        let accepted = json!({
            "uri": "ui://weather/dashboard",
            "name": "weather-dashboard",
            "mimeType": MCP_APPS_HTML_MIME_TYPE,
            "_meta": {"ui": {"csp": {
                "connectDomains": (0..MAX_MCP_APPS_CSP_DOMAINS_PER_DIRECTIVE)
                    .map(|index| format!("https://{index}.weather.example"))
                    .collect::<Vec<_>>()
            }}}
        });
        let baseline: FinalResource = serde_json::from_value(accepted.clone())
            .expect("a bounded Apps CSP declaration is valid");
        let mut planted = accepted.clone();
        planted["_meta"]["ui"]["csp"]["connectDomains"]
            .as_array_mut()
            .expect("bounded baseline has a CSP origin array")
            .push(json!(format!(
                "https://{MAX_MCP_APPS_CSP_DOMAINS_PER_DIRECTIVE}.weather.example"
            )));

        assert!(
            serde_json::from_value::<FinalResource>(planted).is_err(),
            "adding only one origin beyond the CSP directive bound rejects the resource metadata"
        );
        assert_eq!(
            serde_json::to_value(baseline).expect("bounded baseline re-encodes"),
            accepted,
            "the one-origin rejection cannot mutate the admitted Apps metadata"
        );
    }

    #[test]
    fn apps_02_direct_csp_construction_cannot_bypass_serialization_bounds() {
        let accepted = McpAppsResourceCsp {
            connect_domains: Some(
                (0..MAX_MCP_APPS_CSP_DOMAINS_PER_DIRECTIVE)
                    .map(|index| format!("https://{index}.weather.example"))
                    .collect(),
            ),
            ..McpAppsResourceCsp::default()
        };
        let accepted_wire = serde_json::to_value(&accepted)
            .expect("the direct CSP value at the directive bound serializes");

        let mut planted = accepted.clone();
        planted
            .connect_domains
            .as_mut()
            .expect("the direct CSP fixture has connect domains")
            .push(format!(
                "https://{MAX_MCP_APPS_CSP_DOMAINS_PER_DIRECTIVE}.weather.example"
            ));

        assert!(
            serde_json::to_value(&planted).is_err(),
            "adding only one domain beyond the bound rejects direct CSP serialization"
        );
        assert_eq!(
            serde_json::to_value(&accepted).expect("accepted direct CSP re-serializes"),
            accepted_wire,
            "a rejected direct CSP serialization cannot mutate the bounded baseline"
        );
    }

    #[test]
    fn apps_02_preserves_duplicate_and_ordered_visibility() {
        let accepted = json!({
            "name": "weather",
            "inputSchema": {"type": "object"},
            "_meta": {"ui": {"visibility": ["app", "model", "app"]}}
        });
        let tool: FinalTool = serde_json::from_value(accepted.clone())
            .expect("the stable Apps visibility array permits duplicates in wire order");
        let metadata = tool
            .mcp_apps_metadata()
            .expect("Apps metadata remains typed")
            .expect("the tool declares Apps metadata");
        assert_eq!(
            metadata.effective_visibility(),
            [
                McpAppsToolVisibility::App,
                McpAppsToolVisibility::Model,
                McpAppsToolVisibility::App,
            ],
            "Apps visibility preserves the received duplicate sequence"
        );
        assert_eq!(
            serde_json::to_value(&tool).expect("tool re-encodes"),
            accepted,
            "Apps visibility re-encodes duplicates in their received order"
        );
    }

    #[test]
    fn apps_02_bounds_tool_visibility_entries_without_changing_duplicates_or_order() {
        let accepted_visibility = (0..MAX_MCP_APPS_TOOL_VISIBILITY_ENTRIES)
            .map(|index| {
                if index % 2 == 0 {
                    McpAppsToolVisibility::App
                } else {
                    McpAppsToolVisibility::Model
                }
            })
            .collect::<Vec<_>>();
        let baseline = McpAppsToolMetadata::try_new(None, Some(accepted_visibility.clone()))
            .expect("the tool visibility entry bound is admitted");
        let baseline_wire =
            serde_json::to_value(&baseline).expect("bounded visibility metadata serializes");
        assert_eq!(
            baseline.effective_visibility(),
            accepted_visibility,
            "bounded visibility retains received duplicate entries in order"
        );

        let mut planted_visibility = accepted_visibility;
        planted_visibility.push(McpAppsToolVisibility::App);
        assert_eq!(
            McpAppsToolMetadata::try_new(None, Some(planted_visibility.clone())),
            Err(McpAppsMetadataError::TooManyToolVisibilityEntries),
            "adding only one visibility entry beyond the bound is rejected with the typed error"
        );
        let planted = McpAppsToolMetadata {
            resource_uri: None,
            visibility: Some(planted_visibility),
        };
        assert!(
            serde_json::to_value(&planted).is_err(),
            "direct construction cannot bypass the visibility entry bound during serialization"
        );
        assert_eq!(
            serde_json::to_value(&baseline).expect("bounded metadata re-serializes"),
            baseline_wire,
            "rejecting the one-entry plant cannot mutate the admitted metadata"
        );
    }

    #[test]
    fn apps_02_rejects_one_non_html_bound_resource_without_mutating_binding() {
        let tool: FinalTool = serde_json::from_value(json!({
            "name": "weather",
            "inputSchema": {"type": "object"},
            "_meta": {"ui": {"resourceUri": "ui://weather/dashboard"}}
        }))
        .expect("nested Apps binding is valid");
        let binding = tool
            .mcp_apps_resource_binding()
            .expect("tool metadata is valid")
            .expect("tool declares an Apps resource");
        let accepted = json!({
            "uri": "ui://weather/dashboard",
            "name": "weather-dashboard",
            "mimeType": MCP_APPS_HTML_MIME_TYPE
        });
        let baseline: FinalResource =
            serde_json::from_value(accepted.clone()).expect("Apps resource baseline decodes");
        let mut planted = accepted.clone();
        planted["mimeType"] = json!("text/plain");

        assert_eq!(binding.validate_resource(&baseline), Ok(()));
        let incompatible: FinalResource = serde_json::from_value(planted)
            .expect("only MIME type changes; resource remains a final resource");
        assert_eq!(
            binding.validate_resource(&incompatible),
            Err(McpAppsResourceBindingError::HtmlMimeTypeRequired),
            "only replacing the Apps HTML MIME type rejects the bound resource"
        );
        assert_eq!(
            serde_json::to_value(&baseline).expect("baseline resource re-encodes"),
            accepted,
            "the invalid resource cannot mutate the admitted binding target"
        );
    }

    #[test]
    fn apps_02_view_lifecycle_requires_initialization_before_activation() {
        assert_eq!(
            serde_json::to_value(McpAppsDisplayMode::Pip).expect("display mode serializes exactly"),
            json!("pip")
        );
        assert_eq!(
            serde_json::from_value::<McpAppsDisplayMode>(json!("fullscreen"))
                .expect("known display mode decodes"),
            McpAppsDisplayMode::Fullscreen
        );
        assert!(
            serde_json::from_value::<McpAppsDisplayMode>(json!("overlay")).is_err(),
            "only replacing a known display mode with an undeclared value rejects it"
        );

        let mut lifecycle = McpAppsViewLifecycle::default();
        assert!(!lifecycle.permits_application_traffic());
        assert_eq!(
            lifecycle.admit_initialized(),
            Err(McpAppsLifecycleError::InvalidTransition {
                from: McpAppsViewLifecycle::New,
                operation: "initialized notification",
            }),
            "only omitting the prior initialization transition rejects early activation"
        );
        assert_eq!(lifecycle, McpAppsViewLifecycle::New);

        lifecycle
            .begin_initialize()
            .expect("one initialization reservation is legal from New");
        lifecycle
            .initialization_succeeded()
            .expect("a successful initialization awaits exactly one notification");
        lifecycle
            .admit_initialized()
            .expect("the first initialized notification activates the View");
        assert!(lifecycle.permits_application_traffic());
        lifecycle
            .begin_closing()
            .expect("an active View can begin terminal teardown");
        lifecycle
            .finish_closing()
            .expect("a closing View reaches Closed exactly once");
        assert_eq!(lifecycle, McpAppsViewLifecycle::Closed);
    }

    #[test]
    fn apps_02_result_projection_rejects_one_unknown_wire_member() {
        let accepted = json!({
            "content": [{"type": "text", "text": "sunny"}],
            "isError": true
        });
        let baseline: McpAppsToolResult = serde_json::from_value(accepted.clone())
            .expect("bounded complete Apps tool result is valid");
        let mut planted = accepted.clone();
        planted["task"] = json!({"taskId": "deferred"});

        assert!(
            serde_json::from_value::<McpAppsToolResult>(planted).is_err(),
            "only adding a Tasks-shaped member rejects the complete Apps result projection"
        );
        assert_eq!(
            serde_json::to_value(&baseline).expect("baseline result re-encodes"),
            accepted,
            "the rejected Tasks-shaped member cannot mutate the complete Apps result"
        );
    }
}
