//! Request router for MCP servers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::Poll;
use std::time::Duration;

#[cfg(feature = "tasks")]
use crate::FinalTaskRuntime;
use crate::Session;
use crate::auth::AuthRequest;
use crate::bidirectional::{
    MrtrCompletedInputs, MrtrExchangeBinding, MrtrExchangeRegistry, MrtrInputKind,
    MrtrInputRequest, MrtrInputRequests, MrtrInputRequired, MrtrRetry,
};
use crate::handler::{
    BidirectionalSenders, BoxFuture, FinalMethodOutcome, FinalResourceReadCacheHintProvenance,
    FinalResourceUriUse, FinalToolOutcome, ProgressNotificationSender, ResourceUriUsePolicy,
    UriParams, empty_final_result_meta, encode_final_complete_result,
};
use crate::handler::{
    BoxedCompletionHandler, BoxedPromptHandler, BoxedResourceHandler, BoxedToolHandler,
    CompletionHandler, PromptHandler, ResourceHandler, ToolErrorKind, ToolHandler,
};
#[cfg(all(feature = "proxy", feature = "tasks"))]
use crate::proxy::ProxyFinalTaskRelay;
use crate::session::SessionPrincipalBinding;
use asupersync::cx::{ChildRegion, ChildRegionSpec};
#[cfg(test)]
use asupersync::time::wall_now;
use asupersync::types::Time;
use asupersync::{Budget, Cx, Outcome};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
#[cfg(test)]
use fastmcp_core::block_on;
use fastmcp_core::logging::{debug, targets, trace};
use fastmcp_core::{
    McpContext, McpError, McpErrorCode, McpOutcome, McpResult, PromptCaller, PromptGetResult,
    PromptMessageItem, PromptMessageRole, SessionState, sha256_bounded,
};
use fastmcp_protocol::common_types::{
    AbsoluteUri, Annotations, ContentBlock, EmbeddedResourceContents, OpenMetadata, RawIcon,
};
#[cfg(feature = "tasks")]
use fastmcp_protocol::extensions::OFFICIAL_TASKS_EXTENSION_ID;
use fastmcp_protocol::methods::COMPLETION_COMPLETE;
use fastmcp_protocol::protocol_policy::ProtocolEra;
use fastmcp_protocol::uri_template::{ReversibleResourceTemplate, UriTemplatePart};
use fastmcp_protocol::{
    AdmittedSchema, CacheScope, CacheTtl, CallToolParams, CallToolResult, CompleteResult, Content,
    CoreRequest, CoreResult, FinalCallToolParams, FinalCallToolResult, FinalCompletionParams,
    FinalCompletionReference, FinalCompletionResult, FinalCoreRequest, FinalCoreResult,
    FinalGetPromptParams, FinalGetPromptResult, FinalInputResponses, FinalListParams,
    FinalListPromptsResult, FinalListResourceTemplatesResult, FinalListResourcesResult,
    FinalListToolsResult, FinalPrompt, FinalPromptArgument, FinalReadResourceParams,
    FinalReadResourceResult, FinalResource, FinalResourceTemplate, FinalTool, FinalToolAnnotations,
    GetPromptParams, GetPromptResult, InitializeParams, InitializeResult, InputRequiredResult,
    JsonRpcRequest, LegacyCompletionParams, LegacyCompletionResult, LegacyContent,
    LegacyCoreRequest, LegacyPromptMessage, LegacyResourceContent, ListPromptsParams,
    ListPromptsResult, ListResourceTemplatesParams, ListResourceTemplatesResult,
    ListResourcesParams, ListResourcesResult, ListToolsParams, ListToolsResult, PROTOCOL_VERSION,
    ProgressMarker, Prompt, PromptMessage, ReadResourceParams, ReadResourceResult, Resource,
    ResourceContent, ResourceTemplate, Role, ServerBehavior, ServerBehaviorRegistry, TemplateValue,
    Tool, admit_final_schema, exact_json_to_serde, validate, validate_strict,
};

/// Type alias for a notification sender callback.
///
/// This callback is used to send notifications (like progress updates) back to the client
/// during request handling. The callback receives a JSON-RPC request (notification format).
pub type NotificationSender = Arc<dyn Fn(JsonRpcRequest) + Send + Sync>;

/// Application handler output before the exact legacy wire projection.
#[derive(serde::Serialize)]
pub(crate) struct ApplicationToolResult {
    content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
}

/// Allowlisted transport provenance attached to a sanitized inbound request.
///
/// This deliberately contains no peer address, headers, cookies, or
/// credentials. Transport implementations retain those raw values inside their
/// authentication boundary and pass only one of these validated facts to the
/// server dispatch layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundRequestTransport {
    /// Standard input/output framing.
    Stdio,
    /// Streamable HTTP request handling.
    Http,
    /// Server-sent events transport.
    Sse,
    /// WebSocket transport.
    WebSocket,
    /// In-process transport used by embeddings and tests.
    Memory,
}

/// Opaque custody for one transport's singleton `Authorization` field.
///
/// This deliberately has no `Debug` implementation and no raw-value getter.
/// Only server admission can convert it into an [`AuthRequest`], so extension
/// middleware and handlers cannot observe a native transport credential.
#[derive(Clone, Default)]
pub(crate) struct TransportAuthorization(Option<String>);

impl TransportAuthorization {
    /// Captures an already cardinality-validated native authorization field.
    #[must_use]
    pub(crate) fn from_singleton_header(value: Option<&str>) -> Self {
        Self(value.map(ToOwned::to_owned))
    }

    pub(crate) fn auth_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<&'a serde_json::Value>,
        request_id: u64,
    ) -> AuthRequest<'a> {
        AuthRequest {
            method,
            params,
            transport_authorization: self.0.as_deref(),
            request_id,
        }
    }
}

/// Sanitized, immutable ingress facts for one server dispatch.
///
/// The type intentionally has no `Clone`, `Serialize`, or `Debug`
/// implementation. In particular, it offers no channel for credentials or
/// arbitrary raw headers. A native transport may retain its singleton
/// authorization field in crate-private custody for server admission, but it
/// never exposes that field through this public context. The only header
/// fields it carries are a modern HTTP request's admitted `Mcp-Param-*`
/// mirrors, which the router compares with the resolved tool's annotated
/// arguments. The server creates a fresh request-scoped [`McpContext`] from
/// these facts for every dispatch.
pub struct InboundRequestContext {
    cx: Cx,
    request_id: u64,
    transport: InboundRequestTransport,
    state: Option<SessionState>,
    mrtr_continuation_cancellation: Option<fastmcp_core::McpRequestCancellation>,
    transport_authorization: TransportAuthorization,
    principal_binding: Option<SessionPrincipalBinding>,
    http_parameter_headers: Option<Arc<[(String, String)]>>,
}

impl InboundRequestContext {
    /// Creates sanitized facts after transport metadata validation has
    /// completed.
    #[must_use]
    pub fn new(cx: Cx, request_id: u64, transport: InboundRequestTransport) -> Self {
        Self {
            cx,
            request_id,
            transport,
            state: None,
            mrtr_continuation_cancellation: None,
            transport_authorization: TransportAuthorization::default(),
            principal_binding: None,
            http_parameter_headers: None,
        }
    }

    /// Creates sanitized facts for a request that belongs to one live modern
    /// transport connection. Durable connections contribute the opaque
    /// partition used to bind MRTR retries and the cancellation authority that
    /// makes retained continuations unusable after peer disconnect. An
    /// ephemeral HTTP partition proves transport ownership only; the router
    /// maps it to the operation- and principal-bound stateless domain so a
    /// later POST can resume without inventing a durable session.
    #[must_use]
    pub fn with_modern_connection(
        cx: Cx,
        request_id: u64,
        transport: InboundRequestTransport,
        connection: &ModernConnection,
    ) -> Self {
        Self::with_modern_connection_and_transport_authorization(
            cx,
            request_id,
            transport,
            connection,
            TransportAuthorization::default(),
        )
    }

    /// Creates sanitized modern connection facts while retaining the native
    /// authorization field solely for server admission.
    #[must_use]
    pub(crate) fn with_modern_connection_and_transport_authorization(
        cx: Cx,
        request_id: u64,
        transport: InboundRequestTransport,
        connection: &ModernConnection,
        transport_authorization: TransportAuthorization,
    ) -> Self {
        Self::with_modern_connection_context(
            cx,
            request_id,
            transport,
            &connection.request_context(),
            transport_authorization,
        )
    }

    #[must_use]
    pub(crate) fn with_modern_connection_context(
        cx: Cx,
        request_id: u64,
        transport: InboundRequestTransport,
        connection: &ModernConnectionRequestContext,
        transport_authorization: TransportAuthorization,
    ) -> Self {
        Self {
            cx,
            request_id,
            transport,
            state: Some(connection.state.clone()),
            mrtr_continuation_cancellation: Some(connection.continuation_cancellation.clone()),
            transport_authorization,
            principal_binding: Some(connection.principal_binding.clone()),
            http_parameter_headers: None,
        }
    }

    /// Installs the `Mcp-Param-*` fields modern HTTP admission received.
    /// Only the HTTP transport calls this; every other transport leaves them
    /// absent, so no parameter-header comparison is made for it.
    #[must_use]
    pub(crate) fn with_http_parameter_headers(mut self, headers: Arc<[(String, String)]>) -> Self {
        self.http_parameter_headers = Some(headers);
        self
    }

    /// Returns the transport's allowlisted provenance fact.
    #[must_use]
    pub const fn transport(&self) -> InboundRequestTransport {
        self.transport
    }

    /// Returns the request identity selected by the transport.
    #[must_use]
    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(crate) fn request_context(&self) -> McpContext {
        // Anonymous modern HTTP POSTs are request-local. Give each one a
        // fresh SessionState so disable_*/enable_* can mutate and publish
        // list_changed to live subscriptions/listen streams without inventing
        // a durable Mcp-Session-Id.
        let context = self.state.clone().map_or_else(
            || McpContext::with_state(self.cx.clone(), self.request_id, SessionState::new()),
            |state| McpContext::with_state(self.cx.clone(), self.request_id, state),
        );
        let context = match &self.http_parameter_headers {
            Some(headers) => context.with_http_parameter_headers(Arc::clone(headers)),
            None => context,
        };
        if self.mrtr_continuation_cancellation.is_some() {
            context.with_retained_continuation_owner()
        } else {
            context
        }
    }

    pub(crate) fn mrtr_continuation_cancellation(
        &self,
    ) -> Option<fastmcp_core::McpRequestCancellation> {
        self.mrtr_continuation_cancellation.clone()
    }

    pub(crate) fn auth_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<&'a serde_json::Value>,
    ) -> AuthRequest<'a> {
        self.transport_authorization
            .auth_request(method, params, self.request_id)
    }

    pub(crate) fn bind_or_verify_principal(&self, fingerprint: fastmcp_core::Sha256Digest) -> bool {
        self.principal_binding
            .as_ref()
            .is_none_or(|binding| binding.bind_or_verify(fingerprint))
    }

    pub(crate) fn verify_existing_principal(
        &self,
        fingerprint: fastmcp_core::Sha256Digest,
    ) -> bool {
        self.principal_binding
            .as_ref()
            .is_some_and(|binding| binding.verify_existing(fingerprint))
    }

    pub(crate) fn with_cx(mut self, cx: Cx) -> Self {
        self.cx = cx;
        self
    }
}

/// State and retained-continuation ownership for a modern transport boundary.
///
/// Durable stdio and custom-transport owners cancel their MRTR continuations
/// when disconnected or dropped. Ephemeral HTTP request owners are different:
/// their mutable state is request-local, while eligible router-owned opaque
/// continuations can survive that one POST for a later bound stateless retry.
/// Request contexts retain only clones of the state and cancellation capability.
pub struct ModernConnection {
    state: SessionState,
    continuation_cancellation: fastmcp_core::McpRequestCancellation,
    principal_binding: SessionPrincipalBinding,
}

#[derive(Clone)]
pub(crate) struct ModernConnectionRequestContext {
    state: SessionState,
    continuation_cancellation: fastmcp_core::McpRequestCancellation,
    principal_binding: SessionPrincipalBinding,
}

impl ModernConnection {
    /// Creates durable state and continuation ownership for one custom modern
    /// transport connection.
    #[must_use]
    pub fn new() -> Self {
        Self::with_state(SessionState::new())
    }

    /// One modern HTTP POST. The bag is request-local: disable/enable can
    /// publish `list_changed` without a durable `Mcp-Session-Id`, and the
    /// response cache must not treat it as a partition identity. Eligible
    /// MRTR state is owned separately by the router and bound to the resumed
    /// operation rather than to this ephemeral bag.
    pub(crate) fn new_request_local() -> Self {
        Self::with_state(SessionState::ephemeral())
    }

    fn with_state(state: SessionState) -> Self {
        Self {
            state,
            continuation_cancellation: fastmcp_core::McpRequestCancellation::new(),
            principal_binding: SessionPrincipalBinding::default(),
        }
    }

    /// Marks the connection as disconnected and cancels every continuation it
    /// previously minted.
    pub fn disconnect(&self) {
        self.continuation_cancellation.cancel();
    }

    pub(crate) fn is_ephemeral(&self) -> bool {
        self.state.is_ephemeral()
    }

    pub(crate) fn request_context(&self) -> ModernConnectionRequestContext {
        ModernConnectionRequestContext {
            state: self.state.clone(),
            continuation_cancellation: self.continuation_cancellation.clone(),
            principal_binding: self.principal_binding.clone(),
        }
    }
}

impl Default for ModernConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ModernConnection {
    fn drop(&mut self) {
        if !self.is_ephemeral() {
            self.disconnect();
        }
    }
}

/// Tag filtering parameters for list operations.
#[derive(Debug, Clone, Default)]
pub struct TagFilters<'a> {
    /// Only include components with ALL of these tags (AND logic).
    pub include: Option<&'a [String]>,
    /// Exclude components with ANY of these tags (OR logic).
    pub exclude: Option<&'a [String]>,
}

impl<'a> TagFilters<'a> {
    /// Creates tag filters from include and exclude vectors.
    pub fn new(include: Option<&'a Vec<String>>, exclude: Option<&'a Vec<String>>) -> Self {
        Self {
            include: include.map(|v| v.as_slice()),
            exclude: exclude.map(|v| v.as_slice()),
        }
    }

    /// Returns true if the given component tags pass the filter.
    ///
    /// - Include filter: component must have ALL include tags (AND logic)
    /// - Exclude filter: component is rejected if it has ANY exclude tag (OR logic)
    /// - Tag matching is case-insensitive
    pub fn matches(&self, component_tags: &[String]) -> bool {
        // Normalize component tags to lowercase for comparison
        let component_tags_lower: Vec<String> =
            component_tags.iter().map(|t| t.to_lowercase()).collect();

        // Include filter: must have ALL specified tags
        if let Some(include) = self.include {
            // Empty include array means no filter (all pass)
            if !include.is_empty() {
                for tag in include {
                    let tag_lower = tag.to_lowercase();
                    if !component_tags_lower.contains(&tag_lower) {
                        return false;
                    }
                }
            }
        }

        // Exclude filter: rejected if has ANY specified tag
        if let Some(exclude) = self.exclude {
            for tag in exclude {
                let tag_lower = tag.to_lowercase();
                if component_tags_lower.contains(&tag_lower) {
                    return false;
                }
            }
        }

        true
    }
}

fn decode_cursor_offset(cursor: Option<&str>) -> McpResult<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };

    let decoded = BASE64_STANDARD.decode(cursor).map_err(|_| {
        McpError::invalid_params("Invalid cursor (base64 decode failed)".to_string())
    })?;
    let v: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| McpError::invalid_params("Invalid cursor (JSON parse failed)".to_string()))?;
    let offset = v
        .get("offset")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| McpError::invalid_params("Invalid cursor (missing offset)".to_string()))?;

    usize::try_from(offset)
        .map_err(|_| McpError::invalid_params("Invalid cursor (offset too large)".to_string()))
}

/// The catalog a continuation cursor was minted for.
///
/// Paged list cursors bind the offset to this catalog discriminator, the
/// router catalog revision, the normalized query filters, and—for exact-2024
/// lists that filter session-disabled members—the visible-member digest so a
/// continuation cannot cross list methods, observe a changed catalog, replay
/// under different tag filters, or silently skip after a session disable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum FinalCatalogKind {
    Tools,
    Resources,
    ResourceTemplates,
    Prompts,
}

/// Canonical identity for the final list filters whose semantics affect a page.
///
/// Tag matching is case-insensitive and ignores duplicate/order differences,
/// so cursors bind those normalized semantics rather than incidental input
/// spelling. `None` and an explicitly empty tag array consequently identify
/// the same unfiltered catalog.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalCatalogQuery {
    include_tags: Vec<String>,
    exclude_tags: Vec<String>,
}

impl FinalCatalogQuery {
    fn from_final_list_params(params: &FinalListParams) -> Self {
        Self::from_tag_filters(
            params.include_tags.as_deref(),
            params.exclude_tags.as_deref(),
        )
    }

    fn from_tag_filters(include_tags: Option<&[String]>, exclude_tags: Option<&[String]>) -> Self {
        Self {
            include_tags: canonical_final_catalog_tags(include_tags),
            exclude_tags: canonical_final_catalog_tags(exclude_tags),
        }
    }
}

fn canonical_final_catalog_tags(tags: Option<&[String]>) -> Vec<String> {
    let Some(tags) = tags else {
        return Vec::new();
    };
    let mut canonical = tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>();
    canonical.sort_unstable();
    canonical.dedup();
    canonical
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalCatalogCursor {
    catalog: FinalCatalogKind,
    revision: u64,
    query: FinalCatalogQuery,
    offset: u64,
    /// Digest of the exact-2024 visible member keys after session filtering.
    ///
    /// Final catalogs are connection-independent and omit this member. A
    /// session-filtered list binds it so a later disable cannot reuse an
    /// offset into a shorter snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visibility_digest: Option<String>,
}

fn decode_final_catalog_cursor_offset(
    cursor: Option<&str>,
    expected_catalog: FinalCatalogKind,
    expected_revision: u64,
    expected_query: &FinalCatalogQuery,
    expected_visibility_digest: Option<&str>,
    catalog_length: usize,
) -> McpResult<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };

    let decoded = BASE64_STANDARD.decode(cursor).map_err(|_| {
        McpError::invalid_params("Invalid final catalog cursor (base64 decode failed)")
    })?;
    let cursor = serde_json::from_slice::<FinalCatalogCursor>(&decoded).map_err(|_| {
        McpError::invalid_params("Invalid final catalog cursor (JSON parse failed)")
    })?;
    if cursor.catalog != expected_catalog {
        return Err(McpError::invalid_params(
            "final catalog cursor belongs to another list method",
        ));
    }
    if cursor.revision != expected_revision {
        return Err(McpError::invalid_params(
            "final catalog cursor references a stale catalog revision",
        ));
    }
    if &cursor.query != expected_query {
        return Err(McpError::invalid_params(
            "final catalog cursor does not match the requested query filters",
        ));
    }
    if cursor.visibility_digest.as_deref() != expected_visibility_digest {
        return Err(McpError::invalid_params(
            "final catalog cursor does not match the requested session visibility",
        ));
    }
    let offset = usize::try_from(cursor.offset)
        .map_err(|_| McpError::invalid_params("Invalid final catalog cursor (offset too large)"))?;
    if offset >= catalog_length {
        return Err(McpError::invalid_params(
            "final catalog cursor offset is outside the requested catalog page",
        ));
    }
    Ok(offset)
}

fn parse_stateless_params<T: serde::de::DeserializeOwned>(
    params: Option<serde_json::Value>,
) -> McpResult<T> {
    let value = params.ok_or_else(|| McpError::invalid_params("Missing required parameters"))?;
    serde_json::from_value(value).map_err(|error| McpError::invalid_params(error.to_string()))
}

fn parse_stateless_params_or_default<T: serde::de::DeserializeOwned + Default>(
    params: Option<serde_json::Value>,
) -> McpResult<T> {
    match params {
        Some(value) => serde_json::from_value(value)
            .map_err(|error| McpError::invalid_params(error.to_string())),
        None => Ok(T::default()),
    }
}

/// Converts a completed stateless handler result through the final result
/// contract while preserving the original typed [`McpError`] on refusal.
fn encode_stateless_handler_result<T: serde::Serialize>(
    result: McpResult<T>,
) -> McpResult<serde_json::Value> {
    encode_final_complete_result(result?)
}

/// Encodes a handler-authored final tool result without reprojecting it through
/// the legacy result surface. This preserves the complete result's metadata
/// and inert open members under the protocol-owned final codec.
fn encode_final_tools_call_result(
    result: McpResult<CompleteResult<FinalCallToolResult>>,
) -> McpResult<serde_json::Value> {
    let result = result?;
    let encoded = CoreResult::Final(FinalCoreResult::ToolsCall {
        result,
        diagnostic: None,
    })
    .encode()
    .map_err(|error| McpError::internal_error(error.to_string()))?;
    serde_json::from_str(&encoded).map_err(McpError::from)
}

/// Encodes a framework-minted MRTR continuation without allowing a handler to
/// select or replay its opaque request state.
fn encode_mrtr_input_required_result(result: MrtrInputRequired) -> McpResult<serde_json::Value> {
    serde_json::to_value(result)
        .map_err(|_| McpError::internal_error("failed to encode MRTR input_required result"))
}

const MAX_MRTR_BINDING_BYTES: usize = 64 * 1024;
const MAX_MRTR_RAW_PARAMS_BYTES: usize = 256 * 1024;
const MAX_MRTR_RAW_INPUT_RESPONSES_BYTES: usize = 192 * 1024;
const MAX_MRTR_RAW_JSON_DEPTH: usize = 32;
const MAX_MRTR_RAW_JSON_VALUES: usize = 4_096;
const MRTR_REQUIRES_BOUND_MODERN_CONNECTION: &str =
    "MRTR-capable handlers require a bound modern connection";

struct MrtrRawJsonCounter {
    max_bytes: usize,
    bytes: usize,
}

impl Write for MrtrRawJsonCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.bytes);
        if buffer.len() > remaining {
            return Err(io::Error::other("MRTR JSON byte limit exceeded"));
        }
        self.bytes += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admit_mrtr_raw_json_value(value: &serde_json::Value, max_bytes: usize) -> McpResult<()> {
    fn count_values(value: &serde_json::Value, depth: usize, values: &mut usize) -> McpResult<()> {
        if depth > MAX_MRTR_RAW_JSON_DEPTH {
            return Err(McpError::invalid_params(
                "MRTR JSON exceeds its nesting limit",
            ));
        }
        *values = values.saturating_add(1);
        if *values > MAX_MRTR_RAW_JSON_VALUES {
            return Err(McpError::invalid_params(
                "MRTR JSON exceeds its value limit",
            ));
        }
        match value {
            serde_json::Value::Array(items) => {
                for value in items {
                    count_values(value, depth + 1, values)?;
                }
            }
            serde_json::Value::Object(members) => {
                for value in members.values() {
                    count_values(value, depth + 1, values)?;
                }
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
        Ok(())
    }

    let mut values = 0;
    count_values(value, 0, &mut values)?;
    let mut counter = MrtrRawJsonCounter {
        max_bytes,
        bytes: 0,
    };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| McpError::invalid_params("MRTR JSON exceeds its byte limit"))
}

enum FinalMrtrDispatch {
    Fresh,
    Resume(MrtrCompletedInputs),
    InputRequired(serde_json::Value),
}

fn mrtr_digest(value: &impl serde::Serialize) -> McpResult<[u8; 32]> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| McpError::invalid_params("invalid MRTR operation binding"))?;
    let digest = sha256_bounded(&bytes, MAX_MRTR_BINDING_BYTES)
        .map_err(|_| McpError::invalid_params("MRTR operation binding exceeds its limit"))?;
    Ok(*digest.as_bytes())
}

fn final_mrtr_binding(
    request_ctx: &McpContext,
    method: &'static str,
    target: String,
    arguments: &impl serde::Serialize,
) -> McpResult<Option<MrtrExchangeBinding>> {
    if target.len() > MAX_MRTR_BINDING_BYTES {
        return Err(McpError::invalid_params("MRTR target exceeds its limit"));
    }
    // A binding is only consumable where transport admission installed an
    // owner. Ephemeral modern HTTP state shares the stateless continuation
    // partition across POSTs so subsequent retries from the client can consume
    // the continuation within its expiration deadline.
    let Some(session_partition) = request_ctx.retained_continuation_partition() else {
        return Ok(None);
    };
    let is_stateless = request_ctx.session_is_ephemeral();
    let session_partition = if is_stateless {
        mrtr_digest(&"fastmcp-mrtr-stateless-partition-v1")?
    } else {
        session_partition
    };
    let principal_digest = if is_stateless {
        // Cross-POST state requires a provider-scoped owner when authentication
        // is present. A display subject or an anonymous AuthContext hash is not
        // durable identity authority; require verified owner authority before
        // allocating or accessing state. For unauthenticated ingress on servers
        // without an auth provider, bind to a consistent unauthenticated MRTR
        // principal partition digest.
        if let Some(auth) = request_ctx.auth() {
            auth.session_owner().map(|owner| *owner.as_bytes())
        } else {
            Some(mrtr_digest(&"fastmcp-mrtr-unauthenticated-principal-v1")?)
        }
    } else {
        request_ctx
            .auth()
            .map(|auth| {
                auth.session_owner()
                    .map(|owner| Ok(*owner.as_bytes()))
                    .unwrap_or_else(|| mrtr_digest(&auth))
            })
            .transpose()?
    };
    // Stable ownership survives token rotation, but it does not authorize a
    // continuation under changed grants. Scope grants form a set: ordering
    // and duplicates do not change authorization. Hash only verified facts,
    // separately from both owner identity and the operation arguments.
    let auth = request_ctx.auth();
    let mut grants: Vec<&str> = auth
        .as_ref()
        .map(|auth| auth.scopes.iter().map(String::as_str).collect())
        .unwrap_or_default();
    grants.sort_unstable();
    grants.dedup();
    let verified_grants_digest = mrtr_digest(&("fastmcp-mrtr-verified-grants-v1", grants))?;
    let binding = if is_stateless {
        MrtrExchangeBinding::stateless(
            method,
            target,
            mrtr_digest(arguments)?,
            session_partition,
            principal_digest,
            verified_grants_digest,
        )
    } else {
        MrtrExchangeBinding::new(
            method,
            target,
            mrtr_digest(arguments)?,
            session_partition,
            principal_digest,
            verified_grants_digest,
        )
    };
    Ok(Some(binding))
}

fn handler_mrtr_input_requests(
    request_ctx: &McpContext,
    result: &InputRequiredResult,
) -> McpResult<MrtrInputRequests> {
    let Some(input_requests) = result.input_requests() else {
        return Ok(MrtrInputRequests::default());
    };
    if input_requests.members().is_empty() {
        return Err(McpError::invalid_params(
            "MRTR inputRequests must not be empty",
        ));
    }
    MrtrInputRequests::new(
        input_requests
            .members()
            .iter()
            .map(|member| {
                let value = exact_json_to_serde(&member.value)
                    .map_err(|_| McpError::invalid_params("invalid MRTR input request"))?;
                let request = MrtrInputRequest::from_wire(&value)?;
                if request.kind() == MrtrInputKind::Sampling
                    && !request_ctx.client_supports_sampling()
                {
                    return Err(McpError::invalid_request(
                        "Final sampling is not advertised by the client",
                    ));
                }
                Ok((member.name.clone(), request))
            })
            .collect::<McpResult<Vec<_>>>()?,
    )
}

#[cfg(feature = "tasks")]
fn encode_final_task_result(
    result: fastmcp_protocol::tasks_extension::CreateTaskResult,
) -> McpResult<serde_json::Value> {
    let encoded = CoreResult::Final(FinalCoreResult::ToolsCallTask { result })
        .encode()
        .map_err(|error| McpError::internal_error(error.to_string()))?;
    serde_json::from_str(&encoded).map_err(McpError::from)
}

#[cfg(feature = "tasks")]
fn require_final_tasks_capability(metadata: &OpenMetadata) -> McpResult<()> {
    let declared = metadata
        .client_capabilities()
        .map_err(|_| McpError::invalid_params("invalid final client capabilities"))?
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(serde_json::Value::as_object)
        .and_then(|extensions| extensions.get(OFFICIAL_TASKS_EXTENSION_ID))
        .and_then(serde_json::Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    if declared {
        return Ok(());
    }

    Err(crate::missing_final_tasks_capability_error()?)
}

/// Admits the schemas a local tool declares for a final-dialect call.
///
/// The returned values own immutable copies of the declarations so one exact
/// admitted pair is used for both the pre-handler input check and the
/// post-handler output check. Legacy dispatch intentionally retains its raw
/// compatibility validators.
#[derive(Clone)]
struct FinalToolSchemas {
    input: Option<AdmittedSchema>,
    output: Option<AdmittedSchema>,
    errors: Option<FinalToolErrorStructuredContent>,
}

#[derive(Clone)]
struct FinalToolErrorStructuredContent {
    input_validation: serde_json::Value,
    handler: serde_json::Value,
}

const MAX_FINAL_TOOL_ERROR_STRUCTURED_CONTENT_BYTES: usize = 16 * 1024;

fn admit_final_tool_error_structured_content<H: ToolHandler + ?Sized>(
    handler: &H,
    output: &AdmittedSchema,
    kind: ToolErrorKind,
) -> McpResult<serde_json::Value> {
    let mapped = crate::catch_extension_unwind(|| {
        handler.final_tool_error_structured_content(kind)
    })
    .map_err(|_payload| {
        McpError::internal_error("tool error structured-content mapper panicked during admission")
    })?
    .ok_or_else(|| {
        McpError::internal_error(
            "tool declares outputSchema without a complete tool-error structured-content mapper",
        )
    })?;
    let encoded = serde_json::to_vec(&mapped).map_err(|_error| {
        McpError::internal_error("tool error structured-content mapper returned invalid JSON")
    })?;
    if encoded.len() > MAX_FINAL_TOOL_ERROR_STRUCTURED_CONTENT_BYTES {
        return Err(McpError::internal_error(
            "tool error structured-content mapper exceeded the registration limit",
        ));
    }
    if output.validate(&mapped).is_err() {
        return Err(McpError::internal_error(
            "tool error structured-content mapper does not satisfy outputSchema",
        ));
    }
    Ok(mapped)
}

fn admit_final_tool_schemas<H: ToolHandler + ?Sized>(
    upstream_schema_registered: bool,
    input_schema: &serde_json::Value,
    output_schema: Option<&serde_json::Value>,
    handler: &H,
) -> McpResult<FinalToolSchemas> {
    if upstream_schema_registered {
        // The upstream selected and already admitted this exact schema. In
        // particular, a proxy must retain a valid non-object JSON Schema
        // rather than treating it as a local framework schema or inventing a
        // `{}` error payload to satisfy it.
        return Ok(FinalToolSchemas {
            input: None,
            output: None,
            errors: None,
        });
    }
    if !input_schema.is_object() {
        return Err(McpError::internal_error(
            "tool declares a final input schema that is not an object",
        ));
    }
    if input_schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err(McpError::internal_error(
            "tool declares a final input schema without type object",
        ));
    }
    let input = fastmcp_protocol::http_headers::admit_final_tool_input_schema(input_schema.clone())
        .map_err(|_error| {
            McpError::internal_error("tool declares an invalid final input schema")
        })?;
    let output = output_schema
        .cloned()
        .map(|schema| {
            if !schema.is_object() {
                return Err(McpError::internal_error(
                    "tool declares a final output schema that is not an object",
                ));
            }
            admit_final_schema(schema).map_err(|_error| {
                McpError::internal_error("tool declares an invalid final output schema")
            })
        })
        .transpose()?;
    let errors = output
        .as_ref()
        .map(|output| -> McpResult<FinalToolErrorStructuredContent> {
            Ok(FinalToolErrorStructuredContent {
                input_validation: admit_final_tool_error_structured_content(
                    handler,
                    output,
                    ToolErrorKind::InputValidation,
                )?,
                handler: admit_final_tool_error_structured_content(
                    handler,
                    output,
                    ToolErrorKind::Handler,
                )?,
            })
        })
        .transpose()?;
    Ok(FinalToolSchemas {
        input: Some(input),
        output,
        errors,
    })
}

/// One immutable catalog snapshot committed together with its dispatch target.
/// No list or validation path re-invokes the handler's definition hooks.
struct AdmittedToolRegistration {
    handler: BoxedToolHandler,
    definition: Tool,
    final_registration: Option<AdmittedFinalToolRegistration>,
    legacy_enabled: bool,
    execution_mode: crate::ToolExecutionMode,
}

struct AdmittedFinalToolRegistration {
    final_definition: FinalTool,
    schemas: FinalToolSchemas,
    declares_final_tasks: bool,
}

/// Immutable final resource catalog data, including the legacy tag snapshot
/// used only for server-side list filtering.
struct AdmittedFinalResourceRegistration {
    definition: FinalResource,
    tags: Vec<String>,
    uri_use_policy: ResourceUriUsePolicy,
}

/// Immutable final prompt catalog data, including the legacy tag snapshot
/// used only for server-side list filtering.
struct AdmittedFinalPromptRegistration {
    definition: FinalPrompt,
    tags: Vec<String>,
    uri_use_policy: ResourceUriUsePolicy,
}

impl AdmittedToolRegistration {
    fn admit<H: ToolHandler + 'static>(
        handler: H,
        definition: Tool,
        legacy_enabled: bool,
        execution_mode: crate::ToolExecutionMode,
    ) -> McpResult<Self> {
        let (exact_final_definition, declares_final_tasks, upstream_schema_registered) =
            crate::catch_extension_unwind(|| {
                (
                    handler.final_definition(),
                    handler.declares_final_tasks(),
                    handler.upstream_final_tool_schema_registration().is_some(),
                )
            })
            .map_err(|_payload| {
                McpError::internal_error("tool metadata hook panicked during admission")
            })?;
        let final_definition = match exact_final_definition {
            Some(definition) => definition,
            None => {
                let (title, icons, metadata) = crate::catch_extension_unwind(|| {
                    (
                        handler.final_title().map(str::to_owned),
                        handler.final_icons().map(|icons| icons.to_vec()),
                        handler.final_metadata().cloned(),
                    )
                })
                .map_err(|_payload| {
                    McpError::internal_error("tool metadata hook panicked during admission")
                })?;
                FinalTool {
                    name: definition.name.clone(),
                    title,
                    description: definition.description.clone(),
                    input_schema: definition.input_schema.clone(),
                    output_schema: definition.output_schema.clone(),
                    annotations: definition
                        .annotations
                        .clone()
                        .map(project_final_tool_annotations),
                    icons,
                    meta: metadata,
                }
            }
        };
        if final_definition.name != definition.name {
            return Err(McpError::internal_error(
                "tool's exact final definition name differs from its legacy definition name",
            ));
        }
        let schemas = admit_final_tool_schemas(
            upstream_schema_registered,
            &final_definition.input_schema,
            final_definition.output_schema.as_ref(),
            &handler,
        )?;
        Ok(Self {
            handler: Box::new(handler),
            definition,
            final_registration: Some(AdmittedFinalToolRegistration {
                final_definition,
                schemas,
                declares_final_tasks,
            }),
            legacy_enabled,
            execution_mode,
        })
    }

    fn legacy_only<H: ToolHandler + 'static>(
        handler: H,
        definition: Tool,
        execution_mode: crate::ToolExecutionMode,
    ) -> Self {
        Self {
            handler: Box::new(handler),
            definition,
            final_registration: None,
            legacy_enabled: true,
            execution_mode,
        }
    }

    fn with_mounted_name(self, mounted_name: String) -> Self {
        use crate::handler::MountedToolHandler;

        let Self {
            handler,
            mut definition,
            mut final_registration,
            legacy_enabled,
            execution_mode,
        } = self;
        definition.name.clone_from(&mounted_name);
        if let Some(final_registration) = final_registration.as_mut() {
            final_registration
                .final_definition
                .name
                .clone_from(&mounted_name);
        }
        Self {
            handler: Box::new(MountedToolHandler::new(handler, mounted_name)),
            definition,
            final_registration,
            legacy_enabled,
            execution_mode,
        }
    }
}

/// Encodes a handler-authored final resource result without reprojecting it
/// through the legacy resource result surface. This preserves final embedded
/// resource fields, cache policy, metadata, and open members selected by the
/// handler.
fn encode_final_resources_read_result(
    result: McpResult<CompleteResult<FinalReadResourceResult>>,
) -> McpResult<serde_json::Value> {
    let result = result?;
    let encoded = CoreResult::Final(FinalCoreResult::ResourcesRead {
        result,
        diagnostic: None,
    })
    .encode()
    .map_err(|error| McpError::internal_error(error.to_string()))?;
    serde_json::from_str(&encoded).map_err(McpError::from)
}

/// Encodes a handler-authored final prompt result without reprojecting it
/// through the legacy prompt surface. This preserves final common content and
/// result metadata selected by the handler.
fn encode_final_prompts_get_result(
    result: McpResult<CompleteResult<FinalGetPromptResult>>,
) -> McpResult<serde_json::Value> {
    let result = result?;
    let encoded = CoreResult::Final(FinalCoreResult::PromptsGet {
        result,
        diagnostic: None,
    })
    .encode()
    .map_err(|error| McpError::internal_error(error.to_string()))?;
    serde_json::from_str(&encoded).map_err(McpError::from)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FinalCacheHintPolicy {
    list_ttl_ms: CacheTtl,
    resource_read_ttl_ms: CacheTtl,
    scope: CacheScope,
}

impl Default for FinalCacheHintPolicy {
    fn default() -> Self {
        Self {
            list_ttl_ms: CacheTtl::milliseconds(5 * 60 * 1_000),
            resource_read_ttl_ms: CacheTtl::milliseconds(60 * 60 * 1_000),
            scope: CacheScope::Private,
        }
    }
}

/// Encodes a server-authored final payload through the selected method's exact
/// `FinalCoreResult` composition. This preserves typed catalog and cache
/// fields instead of inserting unvalidated JSON after serialization.
fn encode_final_core_result<T>(
    result: McpResult<T>,
    select: impl FnOnce(CompleteResult<T>) -> FinalCoreResult,
) -> McpResult<serde_json::Value> {
    let result = CompleteResult::new(result?, empty_final_result_meta()?);
    let encoded = CoreResult::Final(select(result))
        .encode()
        .map_err(|error| McpError::internal_error(error.to_string()))?;
    serde_json::from_str(&encoded).map_err(McpError::from)
}

fn project_final_tool_annotations(
    annotations: fastmcp_protocol::ToolAnnotations,
) -> FinalToolAnnotations {
    FinalToolAnnotations {
        title: None,
        destructive: annotations.destructive,
        idempotent: annotations.idempotent,
        read_only: annotations.read_only,
        open_world_hint: annotations.open_world_hint,
    }
}

fn project_final_resource_catalog_entry(
    resource: Resource,
    title: Option<String>,
    icons: Option<Vec<RawIcon>>,
    annotations: Option<Annotations>,
    meta: Option<OpenMetadata>,
) -> McpResult<FinalResource> {
    let uri = AbsoluteUri::parse(resource.uri).map_err(|error| {
        McpError::internal_error(format!(
            "legacy resource URI cannot be projected into the final catalog: {error}",
        ))
    })?;
    Ok(FinalResource {
        uri,
        name: resource.name,
        title,
        description: resource.description,
        icons,
        mime_type: resource.mime_type,
        size: None,
        annotations,
        meta,
    })
}

const FINAL_RESOURCE_URI_USE_REJECTED: &str =
    "final resource URI is not admitted for this local use site";
const FINAL_RESOURCE_URI_USE_EMISSION_REJECTED: &str =
    "handler emitted a final resource URI that is not admitted for this local use site";

fn admit_final_resource_uri(
    policy: ResourceUriUsePolicy,
    uri: &AbsoluteUri,
    use_site: FinalResourceUriUse,
) -> McpResult<()> {
    policy
        .admits(uri, use_site)
        .then_some(())
        .ok_or_else(|| McpError::invalid_params(FINAL_RESOURCE_URI_USE_REJECTED))
}

fn admit_final_resource_template_uri(
    policy: ResourceUriUsePolicy,
    uri_template: &str,
) -> McpResult<()> {
    policy
        .admits_template(uri_template)
        .then_some(())
        .ok_or_else(|| McpError::invalid_params(FINAL_RESOURCE_URI_USE_REJECTED))
}

fn enforce_final_resource_uri_emission(
    policy: ResourceUriUsePolicy,
    uri: &AbsoluteUri,
    use_site: FinalResourceUriUse,
) -> McpResult<()> {
    policy
        .admits(uri, use_site)
        .then_some(())
        .ok_or_else(|| McpError::internal_error(FINAL_RESOURCE_URI_USE_EMISSION_REJECTED))
}

fn embedded_resource_uri(contents: &EmbeddedResourceContents) -> &AbsoluteUri {
    match contents {
        EmbeddedResourceContents::Text { uri, .. } | EmbeddedResourceContents::Blob { uri, .. } => {
            uri
        }
    }
}

fn admit_final_resource_read_outcome(
    policy: ResourceUriUsePolicy,
    outcome: &FinalMethodOutcome<FinalReadResourceResult>,
) -> McpResult<()> {
    let FinalMethodOutcome::Complete(result) = outcome else {
        return Ok(());
    };
    for contents in &result.payload.contents {
        enforce_final_resource_uri_emission(
            policy,
            embedded_resource_uri(contents),
            FinalResourceUriUse::ResourceReadContents,
        )?;
    }
    Ok(())
}

fn admit_final_prompt_content(
    policy: ResourceUriUsePolicy,
    content: &fastmcp_protocol::common_types::ContentBlock,
) -> McpResult<()> {
    match content {
        fastmcp_protocol::common_types::ContentBlock::ResourceLink { uri, .. } => {
            enforce_final_resource_uri_emission(
                policy,
                uri,
                FinalResourceUriUse::PromptResourceLink,
            )
        }
        fastmcp_protocol::common_types::ContentBlock::Resource { resource, .. } => {
            enforce_final_resource_uri_emission(
                policy,
                embedded_resource_uri(resource),
                FinalResourceUriUse::PromptEmbeddedResource,
            )
        }
        fastmcp_protocol::common_types::ContentBlock::Text { .. }
        | fastmcp_protocol::common_types::ContentBlock::Image { .. }
        | fastmcp_protocol::common_types::ContentBlock::Audio { .. } => Ok(()),
    }
}

fn admit_declared_final_prompt_arguments<S: AsRef<str>>(
    declared: &[FinalPromptArgument],
    supplied: impl IntoIterator<Item = S>,
) -> McpResult<()> {
    let supplied: HashSet<String> = supplied
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .collect();
    if declared
        .iter()
        .any(|argument| argument.required == Some(true) && !supplied.contains(&argument.name))
    {
        return Err(McpError::invalid_params("Missing required prompt argument"));
    }
    if supplied
        .iter()
        .any(|name| !declared.iter().any(|argument| &argument.name == name))
    {
        return Err(McpError::invalid_params("Unknown prompt argument"));
    }
    Ok(())
}

fn admit_final_prompt_outcome(
    policy: ResourceUriUsePolicy,
    outcome: &FinalMethodOutcome<FinalGetPromptResult>,
) -> McpResult<()> {
    let FinalMethodOutcome::Complete(result) = outcome else {
        return Ok(());
    };
    for message in &result.payload.messages {
        admit_final_prompt_content(policy, &message.content)?;
    }
    Ok(())
}

/// Freezes one resource's modern catalog entry during registration.
///
/// Discovery must not call application hooks: a catalog observed by a final
/// peer has to remain the one that was admitted alongside its dispatch target.
fn admit_final_resource_definition<H: ResourceHandler + ?Sized>(
    handler: &H,
    resource: &Resource,
    uri_use_policy: ResourceUriUsePolicy,
) -> McpResult<FinalResource> {
    if let Some(definition) =
        crate::catch_extension_unwind(|| handler.final_definition()).map_err(|_payload| {
            McpError::internal_error("resource final-definition hook panicked during admission")
        })?
    {
        if definition.uri.as_str() != resource.uri {
            return Err(McpError::internal_error(
                "resource exact final URI differs from its legacy definition URI",
            ));
        }
        admit_final_resource_uri(
            uri_use_policy,
            &definition.uri,
            FinalResourceUriUse::CatalogResource,
        )?;
        return Ok(definition);
    }
    let (title, icons, annotations, meta) = crate::catch_extension_unwind(|| {
        (
            handler.final_title().map(str::to_owned),
            handler.final_icons().map(|icons| icons.to_vec()),
            handler.final_annotations().cloned(),
            handler.final_metadata().cloned(),
        )
    })
    .map_err(|_payload| {
        McpError::internal_error("resource metadata hook panicked during admission")
    })?;
    let definition =
        project_final_resource_catalog_entry(resource.clone(), title, icons, annotations, meta)?;
    admit_final_resource_uri(
        uri_use_policy,
        &definition.uri,
        FinalResourceUriUse::CatalogResource,
    )?;
    Ok(definition)
}

/// Freezes one resource template's final catalog entry during registration.
fn admit_final_resource_template_definition<H: ResourceHandler + ?Sized>(
    handler: Option<&H>,
    template: &ResourceTemplate,
    uri_use_policy: ResourceUriUsePolicy,
) -> McpResult<FinalResourceTemplate> {
    if let Some(handler) = handler {
        if let Some(definition) =
            crate::catch_extension_unwind(|| handler.final_template_definition()).map_err(
                |_payload| {
                    McpError::internal_error(
                        "resource final-template-definition hook panicked during admission",
                    )
                },
            )?
        {
            if definition.uri_template != template.uri_template {
                return Err(McpError::internal_error(
                    "resource exact final template differs from its legacy template URI",
                ));
            }
            admit_final_resource_template_uri(uri_use_policy, &definition.uri_template)?;
            return Ok(definition);
        }
    }
    let (title, icons, annotations, meta) = match handler {
        Some(handler) => crate::catch_extension_unwind(|| {
            (
                handler.final_template_title().map(str::to_owned),
                handler.final_template_icons().map(|icons| icons.to_vec()),
                handler.final_template_annotations().cloned(),
                handler.final_template_metadata().cloned(),
            )
        })
        .map_err(|_payload| {
            McpError::internal_error("resource template metadata hook panicked during admission")
        })?,
        None => (None, None, None, None),
    };
    let definition = FinalResourceTemplate {
        uri_template: template.uri_template.clone(),
        name: template.name.clone(),
        title,
        description: template.description.clone(),
        icons,
        mime_type: template.mime_type.clone(),
        annotations,
        meta,
    };
    admit_final_resource_template_uri(uri_use_policy, &definition.uri_template)?;
    Ok(definition)
}

/// Freezes one prompt's final catalog entry during registration.
fn admit_final_prompt_definition<H: PromptHandler + ?Sized>(
    handler: &H,
    prompt: &Prompt,
) -> McpResult<FinalPrompt> {
    if let Some(definition) =
        crate::catch_extension_unwind(|| handler.final_definition()).map_err(|_payload| {
            McpError::internal_error("prompt final-definition hook panicked during admission")
        })?
    {
        if definition.name != prompt.name {
            return Err(McpError::internal_error(
                "prompt exact final name differs from its legacy definition name",
            ));
        }
        return Ok(definition);
    }
    let (title, icons, meta) = crate::catch_extension_unwind(|| {
        (
            handler.final_title().map(str::to_owned),
            handler.final_icons().map(|icons| icons.to_vec()),
            handler.final_metadata().cloned(),
        )
    })
    .map_err(|_payload| {
        McpError::internal_error("prompt metadata hook panicked during admission")
    })?;
    let arguments = (!prompt.arguments.is_empty()).then(|| {
        prompt
            .arguments
            .iter()
            .map(|argument| FinalPromptArgument {
                name: argument.name.clone(),
                title: None,
                description: argument.description.clone(),
                required: Some(argument.required),
            })
            .collect()
    });
    Ok(FinalPrompt {
        name: prompt.name.clone(),
        title,
        description: prompt.description.clone(),
        icons,
        arguments,
        meta,
    })
}

/// Converts handler-owned content into the exact legacy result union.
///
/// Audio is valid only in the broader handler surface, never in the exact
/// 2024-11-05 content union. Refuse it rather than emitting an invalid legacy
/// response or silently changing the content type.
fn legacy_content_from_handler(content: Content) -> McpResult<LegacyContent> {
    match content {
        Content::Text { text } => Ok(LegacyContent::Text {
            text,
            annotations: None,
            additional: BTreeMap::new(),
        }),
        Content::Image { data, mime_type } => Ok(LegacyContent::Image {
            data,
            mime_type,
            annotations: None,
            additional: BTreeMap::new(),
        }),
        Content::Resource { resource } => Ok(LegacyContent::Resource {
            resource: legacy_resource_content_from_handler(resource)?,
            annotations: None,
            additional: BTreeMap::new(),
        }),
        Content::Audio { .. } => Err(McpError::internal_error(
            "legacy 2024 result content does not support audio",
        )),
    }
}

fn legacy_contents_from_handler(content: Vec<Content>) -> McpResult<Vec<LegacyContent>> {
    content
        .into_iter()
        .map(legacy_content_from_handler)
        .collect()
}

/// Converts handler-owned resource content into an exact legacy result item.
fn legacy_resource_content_from_handler(
    resource: ResourceContent,
) -> McpResult<LegacyResourceContent> {
    match (resource.text, resource.blob) {
        (Some(text), None) => Ok(LegacyResourceContent::Text {
            uri: resource.uri,
            text,
            mime_type: resource.mime_type,
            additional: BTreeMap::new(),
        }),
        (None, Some(blob)) => Ok(LegacyResourceContent::Blob {
            uri: resource.uri,
            blob,
            mime_type: resource.mime_type,
            additional: BTreeMap::new(),
        }),
        _ => Err(McpError::internal_error(
            "legacy 2024 resource content requires exactly one text or blob payload",
        )),
    }
}

fn legacy_resource_contents_from_handler(
    contents: Vec<ResourceContent>,
) -> McpResult<Vec<LegacyResourceContent>> {
    contents
        .into_iter()
        .map(legacy_resource_content_from_handler)
        .collect()
}

fn legacy_prompt_messages_from_handler(
    messages: Vec<PromptMessage>,
) -> McpResult<Vec<LegacyPromptMessage>> {
    messages
        .into_iter()
        .map(|PromptMessage { role, content }| {
            Ok(LegacyPromptMessage {
                role,
                content: legacy_content_from_handler(content)?,
                additional: BTreeMap::new(),
            })
        })
        .collect()
}

/// Promotes exact legacy resource content into a final resource result.
///
/// Legacy open members remain inert but are retained verbatim. In particular,
/// an untyped legacy `_meta` value stays in `additional` rather than acquiring
/// final-era metadata authority during the cross-era projection.
fn promote_legacy_resource_content(
    resource: LegacyResourceContent,
) -> McpResult<EmbeddedResourceContents> {
    let (uri, content, mime_type, additional) = match resource {
        LegacyResourceContent::Text {
            uri,
            text,
            mime_type,
            additional,
        } => (
            uri,
            LegacyEmbeddedContent::Text(text),
            mime_type,
            additional,
        ),
        LegacyResourceContent::Blob {
            uri,
            blob,
            mime_type,
            additional,
        } => (
            uri,
            LegacyEmbeddedContent::Blob(blob),
            mime_type,
            additional,
        ),
    };
    let uri = AbsoluteUri::parse(uri).map_err(|error| {
        McpError::internal_error(format!(
            "legacy resource content cannot be projected into the final result: {error}",
        ))
    })?;

    match content {
        LegacyEmbeddedContent::Text(text) => Ok(EmbeddedResourceContents::Text {
            uri,
            text,
            mime_type,
            meta: None,
            additional,
        }),
        LegacyEmbeddedContent::Blob(blob) => Ok(EmbeddedResourceContents::Blob {
            uri,
            blob,
            mime_type,
            meta: None,
            additional,
        }),
    }
}

enum LegacyEmbeddedContent {
    Text(String),
    Blob(String),
}

fn legacy_read_resource_params(params: FinalReadResourceParams) -> ReadResourceParams {
    ReadResourceParams {
        uri: params.uri.as_str().to_owned(),
        meta: None,
    }
}

fn encode_cursor_offset(offset: usize) -> String {
    let payload = serde_json::json!({ "offset": offset });
    let bytes = serde_json::to_vec(&payload).expect("cursor state must serialize");
    BASE64_STANDARD.encode(bytes)
}

fn encode_final_catalog_cursor(
    catalog: FinalCatalogKind,
    revision: u64,
    query: &FinalCatalogQuery,
    offset: usize,
    visibility_digest: Option<&str>,
) -> String {
    let offset = u64::try_from(offset).expect("usize cursor offsets always fit u64");
    let payload = FinalCatalogCursor {
        catalog,
        revision,
        query: query.clone(),
        offset,
        visibility_digest: visibility_digest.map(str::to_owned),
    };
    let bytes = serde_json::to_vec(&payload).expect("final catalog cursor must serialize");
    BASE64_STANDARD.encode(bytes)
}

const MAX_CATALOG_VISIBILITY_DIGEST_BYTES: usize = 256 * 1024;

fn catalog_visibility_digest<'a, I>(keys: I) -> McpResult<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut material = Vec::new();
    for key in keys {
        let bytes = key.as_bytes();
        let len = u64::try_from(bytes.len()).map_err(|_| {
            McpError::internal_error("catalog visibility key exceeds digest length encoding")
        })?;
        material.extend_from_slice(&len.to_le_bytes());
        material.extend_from_slice(bytes);
    }
    let digest = sha256_bounded(&material, MAX_CATALOG_VISIBILITY_DIGEST_BYTES)
        .map_err(|_| McpError::internal_error("catalog visibility digest input too large"))?;
    Ok(BASE64_STANDARD.encode(digest.as_bytes()))
}

/// Pages an already-filtered catalog snapshot.
///
/// Filtering must precede cursor arithmetic so a legacy-only or tag-filtered
/// entry cannot create an empty modern page or shift a final peer's cursor.
/// Exact-2024 session-disabled filtering additionally binds
/// `visibility_digest` so a later disable cannot reuse an offset into a
/// shorter snapshot.
fn page_final_catalog<T: Clone>(
    items: Vec<T>,
    cursor: Option<&str>,
    page_size: Option<usize>,
    catalog: FinalCatalogKind,
    revision: u64,
    query: &FinalCatalogQuery,
    visibility_digest: Option<&str>,
) -> McpResult<(Vec<T>, Option<String>)> {
    let offset = decode_final_catalog_cursor_offset(
        cursor,
        catalog,
        revision,
        query,
        visibility_digest,
        items.len(),
    )?;
    let Some(page_size) = page_size else {
        return Ok((items, None));
    };
    let end = offset.saturating_add(page_size).min(items.len());
    Ok((
        items.get(offset..end).unwrap_or_default().to_vec(),
        (end < items.len())
            .then(|| encode_final_catalog_cursor(catalog, revision, query, end, visibility_digest)),
    ))
}

fn page_session_filtered_catalog<T: Clone>(
    items: Vec<T>,
    cursor: Option<&str>,
    page_size: Option<usize>,
    catalog: FinalCatalogKind,
    revision: u64,
    query: &FinalCatalogQuery,
    bind_session_visibility: bool,
    key: impl Fn(&T) -> &str,
) -> McpResult<(Vec<T>, Option<String>)> {
    let visibility_digest = bind_session_visibility
        .then(|| catalog_visibility_digest(items.iter().map(key)))
        .transpose()?;
    page_final_catalog(
        items,
        cursor,
        page_size,
        catalog,
        revision,
        query,
        visibility_digest.as_deref(),
    )
}

const SANITIZED_HANDLER_PANIC_MESSAGE: &str = "Internal server error";
static NEXT_HANDLER_INCIDENT_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum number of peer-controlled label bytes admitted to the log-key hash.
///
/// Labels longer than this retain their exact byte length in logs, but their
/// correlation key covers only this bounded prefix. This keeps observability
/// useful without allowing an attacker to turn debug logging into unbounded
/// hashing work.
const LOG_LABEL_HASH_INPUT_LIMIT: usize = 4 * 1024;
const LOG_LABEL_DIGEST_PREFIX_BYTES: usize = 8;

#[derive(Clone, Copy)]
struct SafeLogLabel {
    byte_len: usize,
    hashed_bytes: usize,
    digest_prefix: [u8; LOG_LABEL_DIGEST_PREFIX_BYTES],
}

impl std::fmt::Display for SafeLogLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bytes={},sha256_prefix=", self.byte_len)?;
        for byte in self.digest_prefix {
            write!(f, "{byte:02x}")?;
        }
        if self.hashed_bytes < self.byte_len {
            write!(f, ",hashed_prefix_bytes={}", self.hashed_bytes)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for SafeLogLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

fn safe_log_label(value: &str) -> SafeLogLabel {
    let bytes = value.as_bytes();
    let hashed_bytes = bytes.len().min(LOG_LABEL_HASH_INPUT_LIMIT);
    let bounded_prefix = &bytes[..hashed_bytes];
    let mut digest_prefix = [0_u8; LOG_LABEL_DIGEST_PREFIX_BYTES];
    if let Ok(digest) = sha256_bounded(bounded_prefix, LOG_LABEL_HASH_INPUT_LIMIT) {
        digest_prefix.copy_from_slice(&digest.as_bytes()[..LOG_LABEL_DIGEST_PREFIX_BYTES]);
    }

    SafeLogLabel {
        byte_len: bytes.len(),
        hashed_bytes,
        digest_prefix,
    }
}

fn duplicate_registration_error(component: &'static str, key: &str) -> McpError {
    McpError::invalid_request(format!(
        "{component} already exists; component_key={}",
        safe_log_label(key)
    ))
}

/// Returns whether `error` is a duplicate-registration refusal from
/// [`duplicate_registration_error`].
///
/// Prefixed `as_proxy_typed` may skip one colliding catalog member under
/// [`crate::DuplicateBehavior::Error`]. Other admission failures (schema,
/// metadata, panic-during-definition) must still fail the install.
pub(crate) fn is_duplicate_registration_error(error: &McpError) -> bool {
    error.code == McpErrorCode::InvalidRequest
        && error.message.contains(" already exists; component_key=")
}

fn compose_handler_budget(
    ambient: Budget,
    server_or_request: Budget,
    handler_timeout: Option<Duration>,
    now: Time,
) -> Budget {
    let inherited = ambient.meet(server_or_request);
    match handler_timeout {
        Some(timeout) if !timeout.is_zero() => inherited.tightened_by_timeout(now, timeout),
        Some(_) | None => inherited,
    }
}

fn budget_error(ctx: &McpContext) -> Option<McpError> {
    ctx.ensure_live().is_err().then(|| cancellation_error(ctx))
}

/// The error for a request that is no longer live. A handler whose own
/// checkpoint saw the deadline first, or a Cx the runtime cancelled for its
/// deadline, still ended on that deadline; only an explicit cancellation is
/// reported as a plain cancellation.
fn cancellation_error(ctx: &McpContext) -> McpError {
    if ctx.deadline_expired() {
        McpError::new(McpErrorCode::RequestCancelled, "Request timeout exceeded")
    } else {
        McpError::request_cancelled()
    }
}

fn handler_budget_error(ctx: &McpContext, budget: Budget) -> Option<McpError> {
    if budget.is_past_deadline(ctx.cx().now()) {
        return Some(McpError::new(
            McpErrorCode::RequestCancelled,
            "Request timeout exceeded",
        ));
    }
    budget_error(ctx)
}

/// Returns whether a pending child-context cancellation is currently visible.
///
/// `Cx::is_cancel_requested` reports the raw pending signal even inside an
/// asupersync cancellation mask. Probe a checkpoint only after that cheap bit
/// test so an active mask can defer cancellation without charging ordinary
/// requests (including a valid zero poll balance).
fn request_cx_cancellation_is_visible(request_cx: &Cx) -> bool {
    request_cx.is_cancel_requested() && request_cx.checkpoint().is_err()
}

/// Keeps custody of an admission result even if dispatch is dropped while
/// the scheduler is opening the region. `ChildRegionOpening` itself has no
/// drop cleanup in the pinned runtime. The short, parent-owned task consumes
/// that result and drops the region (requesting close) if its receiver left.
async fn open_modern_request_region(ctx: &McpContext) -> McpResult<Option<ChildRegion>> {
    let budget = ctx.budget();
    let admission = ctx.cx().spawn(move |admission_cx| async move {
        admission_cx
            .open_child_region(ChildRegionSpec::inherit().with_budget(budget))
            .await
    });
    let mut admission = match admission {
        Err(asupersync::runtime::state::SpawnError::RuntimeUnavailable) => {
            // Spawn conflates detached contexts with runtime teardown. Region
            // admission distinguishes them. After this spawn error both cases
            // resolve immediately, without an outstanding admission to abandon.
            return match ctx
                .cx()
                .open_child_region(ChildRegionSpec::inherit().with_budget(budget))
                .await
            {
                Err(asupersync::cx::ChildRegionError::NoRuntimeGateway) => Ok(None),
                Ok(region) => Ok(Some(region)),
                Err(_) => Err(McpError::internal_error(
                    "modern request runtime is no longer available",
                )),
            };
        }
        Err(_) => {
            return Err(McpError::internal_error(
                "modern request region admission could not be scheduled",
            ));
        }
        Ok(admission) => admission,
    };
    // poll_join does not charge/checkpoint the ambient context, unlike a
    // cancellable channel receive. Dropping this handle only detaches its
    // observation; publication drops a returned ChildRegion if the receiver
    // left. Do not create/drop a cancelling JoinFuture during admission.
    match std::future::poll_fn(|task_cx| admission.poll_join(task_cx)).await {
        Ok(Ok(region)) => Ok(Some(region)),
        Err(asupersync::runtime::JoinError::Cancelled(_)) => Err(McpError::request_cancelled()),
        Ok(Err(_)) | Err(_) => Err(McpError::internal_error(
            "modern request child region could not be opened",
        )),
    }
}

/// Runs declared blocking work without occupying the caller's async worker.
/// The enclosing region task retains the pool handle until actual completion,
/// including when cancellation arrives after the closure has started.
async fn run_modern_blocking_dispatch(
    request_cx: &Cx,
    dispatch: impl FnOnce() -> McpResult<serde_json::Value> + Send + 'static,
) -> McpResult<serde_json::Value> {
    let permit = crate::try_reserve_blocking_dispatch()
        .ok_or_else(|| McpError::internal_error("blocking dispatch capacity exhausted"))?;
    let (sender, mut receiver) = asupersync::channel::oneshot::channel();
    let task = crate::blocking_dispatch_pool().spawn(move || {
        let _permit = permit;
        // This is a pool thread, never the async driver, so a handler that
        // bridges an async operation here cannot starve the thread that would
        // complete it. Declaring that keeps bd-6rfrg's detection from rejecting
        // the path it is supposed to protect.
        let _lane = fastmcp_core::runtime::enter_blocking_lane();
        let result = crate::catch_extension_unwind(dispatch);
        let _ = sender.send_blocking(result);
    });
    let task = crate::BlockingTaskGuard(task);
    // A running synchronous closure cannot be preempted. Cancellation must
    // not turn its still-live effects into a completed request-region close.
    while !task.0.is_done() {
        asupersync::time::sleep(request_cx.now(), Duration::from_millis(1)).await;
        // Cancelled Sleep is immediately ready. Retain the blocking task,
        // but yield so its owner's bounded shutdown can still make progress.
        if request_cx.is_cancel_requested() {
            asupersync::runtime::yield_now().await;
        }
    }
    match receiver.try_recv() {
        Ok(Ok(result)) => result,
        Ok(Err(_payload)) => Err(sanitized_handler_panic(request_cx, "modern_dispatch")),
        Err(_) => Err(McpError::internal_error(
            "blocking dispatch ended without a result",
        )),
    }
}

fn sanitized_handler_panic(_request_lifetime: &Cx, handler_class: &'static str) -> McpError {
    let incident_id = NEXT_HANDLER_INCIDENT_ID.fetch_add(1, Ordering::Relaxed);
    log::error!(
        target: "fastmcp_rust::handler",
        "handler terminated unexpectedly; incident_id={incident_id}; class={handler_class}; detail=panic_payload_redacted"
    );
    McpError::internal_error(SANITIZED_HANDLER_PANIC_MESSAGE)
}

fn sanitized_handler_internal_error(
    _request_lifetime: &Cx,
    handler_class: &'static str,
) -> McpError {
    let incident_id = NEXT_HANDLER_INCIDENT_ID.fetch_add(1, Ordering::Relaxed);
    log::error!(
        target: "fastmcp_rust::handler",
        "handler returned an opaque internal failure; incident_id={incident_id}; class={handler_class}; detail=internal_error_redacted"
    );
    McpError::internal_error(SANITIZED_HANDLER_PANIC_MESSAGE)
}

fn sanitize_handler_error(cx: &Cx, handler_class: &'static str, error: McpError) -> McpError {
    if error.code == McpErrorCode::InternalError {
        sanitized_handler_internal_error(cx, handler_class)
    } else {
        error
    }
}

const fn is_framework_terminal_tool_error(code: McpErrorCode) -> bool {
    matches!(
        code,
        McpErrorCode::InternalError | McpErrorCode::RequestCancelled
    )
}

fn is_framework_terminal_final_tool_error(error: &McpError) -> bool {
    is_framework_terminal_tool_error(error.code)
        || crate::is_canonical_missing_required_client_capability_error(error)
}

fn read_handler_timeout(
    cx: &Cx,
    handler_class: &'static str,
    read: impl FnOnce() -> Option<Duration>,
) -> McpResult<Option<Duration>> {
    crate::catch_extension_unwind(read)
        .map_err(|_payload| sanitized_handler_panic(cx, handler_class))
}

/// Drives one handler future without entering the legacy blocking dispatcher.
///
/// The future stays inside its request-owned child task: timeout drops the
/// pending future, and dropping the parent task's join cancels the child before
/// the parent can complete. This helper deliberately receives the child Cx
/// separately from the framework context so modern handlers can propagate that
/// structured capability to their own nested work.
async fn run_handler_in_request<'a, T>(
    ctx: &'a McpContext,
    request_cx: &'a Cx,
    budget: Budget,
    handler_class: &'static str,
    make_future: impl FnOnce(&'a Cx) -> BoxFuture<'a, McpOutcome<T>>,
) -> McpResult<McpOutcome<T>> {
    if request_cx_cancellation_is_visible(request_cx) {
        return Err(cancellation_error(ctx));
    }
    if let Some(error) = handler_budget_error(ctx, budget) {
        return Err(error);
    }

    let future = crate::catch_extension_unwind(|| make_future(request_cx))
        .map_err(|_payload| sanitized_handler_panic(ctx.cx(), handler_class))?;
    let mut future = future;
    let poll_handler = std::future::poll_fn(|task_cx| {
        match crate::catch_extension_unwind(|| future.as_mut().poll(task_cx)) {
            Ok(Poll::Ready(outcome)) => Poll::Ready(Ok(outcome)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_payload) => Poll::Ready(Err(())),
        }
    });

    let outcome = match budget.deadline {
        Some(deadline) => match asupersync::time::timeout_at(deadline, poll_handler).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(())) => return Err(sanitized_handler_panic(ctx.cx(), handler_class)),
            Err(_elapsed) => {
                return Err(McpError::new(
                    McpErrorCode::RequestCancelled,
                    "Request timeout exceeded",
                ));
            }
        },
        None => match poll_handler.await {
            Ok(outcome) => outcome,
            Err(()) => return Err(sanitized_handler_panic(ctx.cx(), handler_class)),
        },
    };

    if request_cx_cancellation_is_visible(request_cx) {
        return Err(cancellation_error(ctx));
    }
    if let Some(error) = handler_budget_error(ctx, budget) {
        return Err(error);
    }
    Ok(outcome)
}

fn derive_handler_context(
    request_ctx: &McpContext,
    progress_marker: Option<ProgressMarker>,
    notification_sender: Option<&NotificationSender>,
    bidirectional_senders: Option<&BidirectionalSenders>,
    protocol_era: ProtocolEra,
) -> McpContext {
    trace!(
        target: targets::HANDLER,
        "Deriving handler context for request {}",
        request_ctx.request_id()
    );
    let mut handler_ctx = request_ctx
        .clone()
        .with_final_request_surface(matches!(protocol_era, ProtocolEra::Modern2026));

    // The owned modern dispatch path may already have installed a
    // request-scoped final-progress runtime. Preserve that reporter so its
    // marker, monotonic high-water mark, coalescing slot, and eventual
    // terminal finalization remain one request-owned authority.
    let reuse_installed_final_reporter =
        matches!(protocol_era, ProtocolEra::Modern2026) && handler_ctx.has_progress_reporter();

    if let (Some(marker), Some(sender)) = (progress_marker, notification_sender)
        && !reuse_installed_final_reporter
    {
        let sender = sender.clone();
        let reporter = match protocol_era {
            ProtocolEra::Legacy2024 => ProgressNotificationSender::new(marker, move |request| {
                sender(request);
            })
            .into_reporter(),
            ProtocolEra::Modern2026 => {
                ProgressNotificationSender::new_final(marker, move |request| {
                    sender(request);
                })
                .into_reporter()
            }
        };
        handler_ctx = handler_ctx.with_progress_reporter(reporter);
    }

    if let Some(sender) = notification_sender {
        let sender = sender.clone();
        handler_ctx =
            handler_ctx.with_log_sender(Arc::new(crate::handler::LogNotificationSender::new(
                move |request| {
                    sender(request);
                },
                protocol_era,
            )));
    }

    if let Some(senders) = bidirectional_senders {
        if let Some(ref sampling) = senders.sampling {
            handler_ctx = handler_ctx.with_sampling(sampling.clone());
        }
        if let Some(ref elicitation) = senders.elicitation {
            handler_ctx = handler_ctx.with_elicitation(elicitation.clone());
        }
        if let Some(ref roots) = senders.roots {
            handler_ctx = handler_ctx.with_roots_provider(roots.clone());
        }
    }

    handler_ctx
}

fn final_progress_marker(metadata: &OpenMetadata) -> McpResult<Option<ProgressMarker>> {
    metadata
        .get("progressToken")
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|_| {
                McpError::invalid_params("final progressToken must be a string or integer")
            })
        })
        .transpose()
}

/// Routes MCP requests to the appropriate handlers.
pub struct Router {
    tools: HashMap<String, AdmittedToolRegistration>,
    tool_order: Vec<String>,
    completion_handler: Option<BoxedCompletionHandler>,
    /// Whether the server-wide completion fallback is admitted for final dispatch.
    default_final_completion_enabled: bool,
    /// Legacy completion providers selected by exact prompt name.
    legacy_prompt_completion_handlers: HashMap<String, BoxedCompletionHandler>,
    /// Legacy completion providers selected by exact resource-template URI.
    legacy_resource_template_completion_handlers: HashMap<String, BoxedCompletionHandler>,
    /// Final completion providers selected by exact prompt name.
    final_prompt_completion_handlers: HashMap<String, BoxedCompletionHandler>,
    /// Final completion providers selected by exact resource-template URI.
    final_resource_template_completion_handlers: HashMap<String, BoxedCompletionHandler>,
    resources: HashMap<String, BoxedResourceHandler>,
    /// Static resources visible to exact MCP 2024-11-05 only.
    final_only_resources: HashSet<String>,
    /// Immutable final catalog entries. Absence means exact-legacy-only.
    final_resources: HashMap<String, AdmittedFinalResourceRegistration>,
    resource_order: Vec<String>,
    prompts: HashMap<String, BoxedPromptHandler>,
    /// Prompts visible to exact MCP 2024-11-05 only.
    final_only_prompts: HashSet<String>,
    /// Immutable final catalog entries. Absence means exact-legacy-only.
    final_prompts: HashMap<String, AdmittedFinalPromptRegistration>,
    prompt_order: Vec<String>,
    resource_templates: HashMap<String, ResourceTemplateEntry>,
    resource_template_order: Vec<String>,
    /// Pre-sorted template keys by specificity (most specific first).
    /// Updated whenever templates are added/modified.
    sorted_template_keys: Vec<String>,
    /// Whether to enforce strict input validation (reject extra properties).
    strict_input_validation: bool,
    /// Optional list page size for cursor-based pagination.
    ///
    /// When `None`, list methods return all items in a single response and
    /// `nextCursor` is always omitted.
    list_page_size: Option<usize>,
    /// Monotonic revision bound into every paged catalog continuation cursor.
    ///
    /// This advances after each successful catalog mutation so a retained
    /// cursor cannot observe a later catalog snapshot.
    final_catalog_revision: u64,
    /// Cache policy emitted on exact modern catalog and resource-read results.
    final_cache_hints: FinalCacheHintPolicy,
    /// Application-owned durable final Tasks runtime used only after the
    /// request metadata has admitted the official extension.
    #[cfg(feature = "tasks")]
    final_task_runtime: Option<FinalTaskRuntime>,
    /// One route-bound upstream final Tasks relay. This is distinct from the
    /// local runtime because upstream task IDs must never be recreated or
    /// translated into local task state.
    #[cfg(all(feature = "proxy", feature = "tasks"))]
    final_task_relay: Option<Arc<ProxyFinalTaskRelay>>,
    /// Bounded, process-local final request-state records for tool, resource,
    /// and prompt retries. A state is accepted only if this router issued it.
    mrtr_exchanges: Arc<MrtrExchangeRegistry>,
}

impl Router {
    /// Creates a new empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            tool_order: Vec::new(),
            completion_handler: None,
            default_final_completion_enabled: false,
            legacy_prompt_completion_handlers: HashMap::new(),
            legacy_resource_template_completion_handlers: HashMap::new(),
            final_prompt_completion_handlers: HashMap::new(),
            final_resource_template_completion_handlers: HashMap::new(),
            resources: HashMap::new(),
            final_only_resources: HashSet::new(),
            final_resources: HashMap::new(),
            resource_order: Vec::new(),
            prompts: HashMap::new(),
            final_only_prompts: HashSet::new(),
            final_prompts: HashMap::new(),
            prompt_order: Vec::new(),
            resource_templates: HashMap::new(),
            resource_template_order: Vec::new(),
            sorted_template_keys: Vec::new(),
            strict_input_validation: false,
            list_page_size: None,
            final_catalog_revision: 0,
            final_cache_hints: FinalCacheHintPolicy::default(),
            #[cfg(feature = "tasks")]
            final_task_runtime: None,
            #[cfg(all(feature = "proxy", feature = "tasks"))]
            final_task_relay: None,
            mrtr_exchanges: Arc::new(MrtrExchangeRegistry::new()),
        }
    }

    /// Returns the number of framework-minted MRTR exchanges for crate tests.
    #[cfg(test)]
    pub(crate) fn test_active_mrtr_exchange_count(&self) -> usize {
        self.mrtr_exchanges.active_len()
    }

    /// Closes stateless MRTR issuance and removes every retained stateless
    /// continuation when the owning HTTP listener begins shutdown.
    pub(crate) fn close_stateless_mrtr_exchanges(&self) -> usize {
        self.mrtr_exchanges.close_stateless()
    }

    #[cfg(feature = "tasks")]
    pub(crate) fn set_final_task_runtime(&mut self, runtime: Option<FinalTaskRuntime>) {
        self.final_task_runtime = runtime;
    }

    #[cfg(all(feature = "proxy", feature = "tasks"))]
    pub(crate) fn set_final_task_relay(&mut self, relay: Option<Arc<ProxyFinalTaskRelay>>) {
        self.final_task_relay = relay;
    }

    /// Sets the list pagination page size.
    ///
    /// When set, list methods (`tools/list`, `resources/list`,
    /// `resources/templates/list`, and `prompts/list`) will page results using
    /// opaque base64 cursors.
    pub fn set_list_page_size(&mut self, page_size: Option<usize>) {
        self.list_page_size = page_size.filter(|n| *n > 0);
    }

    fn advance_final_catalog_revision(&mut self) {
        self.final_catalog_revision = self
            .final_catalog_revision
            .checked_add(1)
            .expect("final catalog revision cannot overflow");
    }

    /// Sets the cache hints emitted by final catalog and resource-read
    /// responses. The default is a five-minute private catalog TTL and a
    /// one-hour private resource-read TTL.
    pub fn set_final_cache_hint_policy(
        &mut self,
        list_ttl_ms: CacheTtl,
        resource_read_ttl_ms: CacheTtl,
        scope: CacheScope,
    ) {
        self.final_cache_hints = FinalCacheHintPolicy {
            list_ttl_ms,
            resource_read_ttl_ms,
            scope,
        };
    }

    /// Returns the active final cache-hint policy as
    /// `(&list_ttl_ms, &resource_read_ttl_ms, scope)`.
    #[must_use]
    pub fn final_cache_hint_policy(&self) -> (&CacheTtl, &CacheTtl, CacheScope) {
        (
            &self.final_cache_hints.list_ttl_ms,
            &self.final_cache_hints.resource_read_ttl_ms,
            self.final_cache_hints.scope,
        )
    }

    /// Sets whether to use strict input validation.
    ///
    /// When enabled, tool input validation will reject any properties not
    /// explicitly defined in the tool's input schema (enforces `additionalProperties: false`).
    ///
    /// When disabled (default), extra properties are allowed unless the schema
    /// explicitly sets `additionalProperties: false`.
    pub fn set_strict_input_validation(&mut self, strict: bool) {
        self.strict_input_validation = strict;
    }

    /// Returns whether strict input validation is enabled.
    #[must_use]
    pub fn strict_input_validation(&self) -> bool {
        self.strict_input_validation
    }

    /// Rebuilds the sorted template keys vector.
    /// Called after any modification to resource_templates.
    fn rebuild_sorted_template_keys(&mut self) {
        self.sorted_template_keys = self.resource_templates.keys().cloned().collect();
        self.sorted_template_keys.sort_by(|a, b| {
            let entry_a = &self.resource_templates[a];
            let entry_b = &self.resource_templates[b];
            let (a_literals, a_literal_segments, a_segments) = entry_a.specificity;
            let (b_literals, b_literal_segments, b_segments) = entry_b.specificity;
            b_literals
                .cmp(&a_literals)
                .then(b_literal_segments.cmp(&a_literal_segments))
                .then(b_segments.cmp(&a_segments))
                .then_with(|| a.cmp(b))
        });
    }

    /// Rejects a modern handler template that could select an already-admitted
    /// modern resource route. Dispatch must never resolve such a collision by
    /// registration order or by the specificity tie-breaker.
    fn reject_final_template_collisions(
        &self,
        candidate: &ReversibleResourceTemplate,
        replacing_source: Option<&str>,
    ) -> McpResult<()> {
        for uri in self.final_resources.keys() {
            if candidate
                .match_uri(uri)
                .map_err(|_| McpError::invalid_params("resource template match admission failed"))?
                .is_some()
            {
                return Err(McpError::invalid_params(
                    "resource template collides with an exact final resource",
                ));
            }
        }

        for (source, entry) in &self.resource_templates {
            if replacing_source == Some(source.as_str()) {
                continue;
            }
            let Some(existing) = entry.matcher.as_ref() else {
                continue;
            };
            if reversible_templates_may_overlap(candidate, existing)? {
                return Err(McpError::invalid_params(
                    "resource template collides with an admitted final resource template",
                ));
            }
        }

        Ok(())
    }

    /// Rejects a modern exact resource whose byte-exact URI is matched by an
    /// already-admitted modern resource template.
    fn reject_final_exact_resource_template_collisions(&self, uri: &str) -> McpResult<()> {
        for entry in self.resource_templates.values() {
            let Some(matcher) = entry.matcher.as_ref() else {
                continue;
            };
            if matcher
                .match_uri(uri)
                .map_err(|_| McpError::invalid_params("resource template match admission failed"))?
                .is_some()
            {
                return Err(McpError::invalid_params(
                    "exact final resource collides with an admitted resource template",
                ));
            }
        }

        Ok(())
    }

    /// Adds a tool handler.
    ///
    /// If a tool with the same name already exists, it will be replaced.
    /// Use [`add_tool_with_behavior`](Self::add_tool_with_behavior) for
    /// finer control over duplicate handling.
    pub fn add_tool<H: ToolHandler + 'static>(&mut self, handler: H) -> McpResult<()> {
        self.add_tool_with_behavior(handler, crate::DuplicateBehavior::Replace)
    }

    /// Adds an intentionally exact-2024-only tool handler.
    ///
    /// This is the explicit escape hatch for a legacy definition that cannot
    /// satisfy final schema admission. The tool remains available to exact
    /// MCP 2024-11-05 list and call routes, but is absent from every modern
    /// catalog and modern dispatch lookup. Ordinary [`Self::add_tool`] never
    /// falls back to this path.
    pub fn add_legacy_tool<H: ToolHandler + 'static>(&mut self, handler: H) -> McpResult<()> {
        self.add_legacy_tool_with_behavior(handler, crate::DuplicateBehavior::Replace)
    }

    /// Adds a tool handler with specified duplicate behavior.
    ///
    /// Returns `Err` if duplicate policy rejects the name or if the candidate's
    /// immutable definition, schemas, final metadata, or required error mapper
    /// cannot be admitted. Every error is returned before catalog mutation.
    pub fn add_tool_with_behavior<H: ToolHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_tool_registration_with_behavior(handler, behavior, true, true, false)
    }

    /// Adds an exact-final-only tool with duplicate handling.
    pub(crate) fn add_final_tool_with_behavior<H: ToolHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_tool_registration_with_behavior(handler, behavior, true, false, false)
    }

    /// Adds a final-only tool carrying validated MCP Apps metadata.
    ///
    /// This is deliberately narrower than ordinary tool registration: callers
    /// must first install the Apps extension through the builder, and the tool
    /// is never projected into exact MCP 2024-11-05 discovery or dispatch.
    pub(crate) fn add_mcp_apps_tool_with_behavior<H: ToolHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_tool_registration_with_behavior(handler, behavior, true, false, true)
    }

    /// Adds an intentionally exact-2024-only tool with duplicate policy.
    ///
    /// The definition is snapshotted before mutation, but no final definition,
    /// schema, metadata, or error-mapper hook is read. This prevents an
    /// explicitly legacy-only registration from accidentally claiming modern
    /// support while retaining the same duplicate semantics as ordinary tools.
    pub fn add_legacy_tool_with_behavior<H: ToolHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_tool_registration_with_behavior(handler, behavior, false, true, false)
    }

    fn add_tool_registration_with_behavior<H: ToolHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
        admit_final: bool,
        legacy_enabled: bool,
        allow_mcp_apps: bool,
    ) -> Result<(), McpError> {
        let def = crate::catch_extension_unwind(|| handler.definition()).map_err(|_payload| {
            McpError::internal_error("tool definition hook panicked during admission")
        })?;
        let name = &def.name;

        let existed = self.tools.contains_key(name);
        if existed {
            match behavior {
                crate::DuplicateBehavior::Error => {
                    return Err(duplicate_registration_error("Tool", name));
                }
                crate::DuplicateBehavior::Warn => {
                    log::warn!(
                        target: "fastmcp_rust::router",
                        "tool already exists, keeping original; tool_key={}",
                        safe_log_label(name)
                    );
                    return Ok(());
                }
                crate::DuplicateBehavior::Replace => {
                    log::debug!(
                        target: "fastmcp_rust::router",
                        "replacing tool; tool_key={}",
                        safe_log_label(name)
                    );
                    // Fall through to insert
                }
                crate::DuplicateBehavior::Ignore => {
                    return Ok(());
                }
            }
        }

        // Admission must finish before any map or ordering mutation. In
        // particular, a rejected replacement must retain the prior handler
        // and its admitted schemas for both protocol eras.
        let execution_mode =
            crate::catch_extension_unwind(|| handler.execution_mode()).map_err(|_payload| {
                McpError::internal_error("tool execution-mode hook panicked during admission")
            })?;
        let name = def.name.clone();
        let admitted = if admit_final {
            AdmittedToolRegistration::admit(handler, def, legacy_enabled, execution_mode)?
        } else {
            AdmittedToolRegistration::legacy_only(handler, def, execution_mode)
        };
        if let Some(final_registration) = admitted.final_registration.as_ref() {
            self.validate_mcp_apps_tool_admission(
                &final_registration.final_definition,
                allow_mcp_apps,
            )?;
        }
        self.tools.insert(name.clone(), admitted);
        if !existed {
            self.tool_order.push(name);
        }
        self.advance_final_catalog_revision();
        Ok(())
    }

    /// Keeps Apps metadata on its explicit, negotiated, exact-final path.
    fn validate_mcp_apps_tool_admission(
        &self,
        tool: &FinalTool,
        allow_mcp_apps: bool,
    ) -> McpResult<()> {
        let metadata = tool.mcp_apps_metadata().map_err(|error| {
            McpError::invalid_request(format!("invalid MCP Apps tool metadata: {error}"))
        })?;
        if metadata.is_some() && !allow_mcp_apps {
            return Err(McpError::invalid_request(
                "MCP Apps tool metadata requires ServerBuilder::mcp_apps_tool",
            ));
        }
        self.validate_mcp_apps_tool_resource_binding(tool)
    }

    /// Verifies that a final tool's optional Apps UI binding selects a
    /// registered final Apps HTML resource before either catalog is mutated.
    fn validate_mcp_apps_tool_resource_binding(&self, tool: &FinalTool) -> McpResult<()> {
        let binding = tool.mcp_apps_resource_binding().map_err(|error| {
            McpError::invalid_request(format!("invalid MCP Apps tool metadata: {error}"))
        })?;
        let Some(binding) = binding else {
            return Ok(());
        };
        let resource = self
            .final_resources
            .get(binding.resource_uri.as_str())
            .ok_or_else(|| {
                McpError::invalid_request(format!(
                    "MCP Apps UI resource is not registered: {}",
                    binding.resource_uri.as_str()
                ))
            })?;
        binding
            .validate_resource(&resource.definition)
            .map_err(|error| {
                McpError::invalid_request(format!("invalid MCP Apps UI resource binding: {error}"))
            })
    }

    /// Prevents replacement of one final Apps resource with a definition that
    /// would invalidate an already-admitted tool binding.
    fn validate_mcp_apps_resource_bindings(&self, resource: &FinalResource) -> McpResult<()> {
        for tool in self.tools.values() {
            let Some(final_registration) = tool.final_registration.as_ref() else {
                continue;
            };
            let binding = final_registration
                .final_definition
                .mcp_apps_resource_binding()
                .map_err(|error| {
                    McpError::invalid_request(format!("invalid MCP Apps tool metadata: {error}"))
                })?;
            if let Some(binding) = binding
                && binding.resource_uri == resource.uri
            {
                binding.validate_resource(resource).map_err(|error| {
                    McpError::invalid_request(format!(
                        "invalid MCP Apps UI resource replacement: {error}"
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Prevents generic resource registration from projecting an Apps View
    /// into exact MCP 2024-11-05. The typed provider is admitted only through
    /// the builder's negotiated final-only registration path.
    fn validate_mcp_apps_ui_resource_admission(
        &self,
        resource: &FinalResource,
        allow_mcp_apps: bool,
    ) -> McpResult<()> {
        let requires_mcp_apps = Self::mcp_apps_ui_resource_requires_special_admission(resource)?;
        if !requires_mcp_apps {
            return Ok(());
        }
        if !allow_mcp_apps {
            return Err(McpError::invalid_request(
                "MCP Apps UI resources require ServerBuilder::mcp_apps_ui_resource",
            ));
        }
        let is_apps_html = resource.uri.as_str().starts_with("ui://")
            && resource.mime_type.as_deref() == Some(fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE);
        if !is_apps_html {
            return Err(McpError::invalid_request(
                "MCP Apps UI resources must use a ui:// URI and the MCP Apps HTML MIME type",
            ));
        }
        Ok(())
    }

    fn mcp_apps_ui_resource_requires_special_admission(
        resource: &FinalResource,
    ) -> McpResult<bool> {
        let metadata = resource.mcp_apps_metadata().map_err(|error| {
            McpError::invalid_request(format!("invalid MCP Apps resource metadata: {error}"))
        })?;
        let is_apps_html = resource.uri.as_str().starts_with("ui://")
            && resource.mime_type.as_deref() == Some(fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE);
        Ok(is_apps_html || metadata.is_some())
    }

    /// Returns whether this router contains a final-only MCP Apps component.
    ///
    /// Builder-level composition uses this inventory before consuming a child
    /// server. Invalid retained final metadata is treated as Apps-bound so a
    /// malformed child cannot bypass the destination's Apps opt-in gate.
    #[must_use]
    pub(crate) fn has_mcp_apps_bound_components(&self) -> bool {
        self.final_resources.values().any(|registration| {
            Self::mcp_apps_ui_resource_requires_special_admission(&registration.definition)
                .unwrap_or(true)
        }) || self.tools.values().any(|registration| {
            registration
                .final_registration
                .as_ref()
                .is_some_and(|final_registration| {
                    final_registration
                        .final_definition
                        .mcp_apps_metadata()
                        .map_or(true, |metadata| metadata.is_some())
                })
        })
    }

    /// Registers the handler for `completion/complete`.
    ///
    /// This is the server-wide fallback for final completion dispatch and the
    /// sole route for exact MCP 2024-11-05. A final provider registered for a
    /// specific prompt or resource template takes precedence. Re-registering
    /// replaces the prior fallback, matching ordinary component registration
    /// semantics.
    pub fn add_completion_handler<H: CompletionHandler + 'static>(&mut self, handler: H) {
        self.completion_handler = Some(Box::new(handler));
        self.default_final_completion_enabled = true;
    }

    /// Registers a completion handler for exact MCP 2024-11-05 dispatch only.
    pub fn add_legacy_completion_handler<H: CompletionHandler + 'static>(&mut self, handler: H) {
        self.completion_handler = Some(Box::new(handler));
        self.default_final_completion_enabled = false;
    }

    /// Registers a legacy completion provider for one exact prompt name.
    ///
    /// This route takes precedence over the legacy server-wide fallback and is
    /// removed atomically when `Replace` admits a new prompt at the same name.
    pub(crate) fn add_legacy_prompt_completion_handler<H: CompletionHandler + 'static>(
        &mut self,
        prompt_name: impl Into<String>,
        handler: H,
    ) {
        self.legacy_prompt_completion_handlers
            .insert(prompt_name.into(), Box::new(handler));
    }

    /// Registers a legacy completion provider for one exact resource-template URI.
    ///
    /// This route takes precedence over the legacy server-wide fallback and is
    /// removed atomically when `Replace` admits a new template at the same URI.
    pub(crate) fn add_legacy_resource_template_completion_handler<
        H: CompletionHandler + 'static,
    >(
        &mut self,
        uri_template: impl Into<String>,
        handler: H,
    ) {
        self.legacy_resource_template_completion_handlers
            .insert(uri_template.into(), Box::new(handler));
    }

    /// Registers a final completion provider for one exact prompt name.
    ///
    /// The provider is selected only after final prompt and argument admission
    /// succeeds. It never changes exact MCP 2024-11-05's server-wide route.
    pub fn add_prompt_completion_handler<H: CompletionHandler + 'static>(
        &mut self,
        prompt_name: impl Into<String>,
        handler: H,
    ) {
        self.final_prompt_completion_handlers
            .insert(prompt_name.into(), Box::new(handler));
    }

    /// Registers a final completion provider for one exact resource-template URI.
    ///
    /// The provider is selected only after the resource template and requested
    /// template variable have been admitted for final dispatch.
    pub fn add_resource_template_completion_handler<H: CompletionHandler + 'static>(
        &mut self,
        uri_template: impl Into<String>,
        handler: H,
    ) {
        self.final_resource_template_completion_handlers
            .insert(uri_template.into(), Box::new(handler));
    }

    /// Returns whether a `completion/complete` handler is installed.
    #[must_use]
    pub fn has_completion_handler(&self) -> bool {
        self.completion_handler.is_some()
            || !self.legacy_prompt_completion_handlers.is_empty()
            || !self.legacy_resource_template_completion_handlers.is_empty()
            || !self.final_prompt_completion_handlers.is_empty()
            || !self.final_resource_template_completion_handlers.is_empty()
    }

    fn has_final_completion_handler(&self) -> bool {
        self.default_final_completion_enabled || self.has_admitted_final_completion_provider()
    }

    fn has_admitted_final_completion_provider(&self) -> bool {
        self.final_prompt_completion_handlers
            .keys()
            .any(|name| self.final_prompts.contains_key(name))
            || self
                .final_resource_template_completion_handlers
                .keys()
                .any(|uri| {
                    self.resource_templates
                        .get(uri)
                        .is_some_and(|entry| entry.final_definition.is_some())
                })
    }

    /// Adds a resource handler.
    ///
    /// If a resource with the same URI already exists, it will be replaced.
    /// Use [`add_resource_with_behavior`](Self::add_resource_with_behavior) for
    /// finer control over duplicate handling.
    pub fn add_resource<H: ResourceHandler + 'static>(&mut self, handler: H) {
        if let Err(error) = self.add_resource_registration_with_behavior(
            handler,
            crate::DuplicateBehavior::Replace,
            true,
            true,
            false,
        ) {
            log::warn!(
                target: "fastmcp_rust::router",
                "rejected resource registration; code={:?}",
                error.code
            );
        }
    }

    /// Adds an intentionally exact-2024-only resource handler.
    pub fn add_legacy_resource<H: ResourceHandler + 'static>(&mut self, handler: H) {
        if let Err(error) = self.add_resource_registration_with_behavior(
            handler,
            crate::DuplicateBehavior::Replace,
            false,
            true,
            false,
        ) {
            log::warn!(
                target: "fastmcp_rust::router",
                "rejected exact-2024-only resource registration; code={:?}",
                error.code
            );
        }
    }

    /// Adds a resource handler with specified duplicate behavior.
    ///
    /// Returns `Err` if behavior is [`crate::DuplicateBehavior::Error`] and the
    /// resource URI already exists.
    pub fn add_resource_with_behavior<H: ResourceHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_resource_registration_with_behavior(handler, behavior, true, true, false)
    }

    /// Adds an exact-final-only resource or resource template with duplicate handling.
    pub(crate) fn add_final_resource_with_behavior<H: ResourceHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_resource_registration_with_behavior(handler, behavior, true, false, false)
    }

    /// Adds one final-only MCP Apps HTML resource after builder-level Apps opt-in.
    pub(crate) fn add_mcp_apps_ui_resource_with_behavior<H: ResourceHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_resource_registration_with_behavior(handler, behavior, true, false, true)
    }

    /// Adds an exact-2024-only resource handler with duplicate handling.
    pub fn add_legacy_resource_with_behavior<H: ResourceHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_resource_registration_with_behavior(handler, behavior, false, true, false)
    }

    fn add_resource_registration_with_behavior<H: ResourceHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
        admit_final: bool,
        legacy_enabled: bool,
        allow_mcp_apps: bool,
    ) -> Result<(), McpError> {
        let (template, def) =
            crate::catch_extension_unwind(|| (handler.template(), handler.definition())).map_err(
                |_payload| {
                    McpError::internal_error("resource definition hook panicked during admission")
                },
            )?;

        // Check for duplicates
        let key = match template.as_ref() {
            Some(template) => template.uri_template.clone(),
            None => def.uri.clone(),
        };

        let exists = if template.is_some() {
            self.resource_templates.contains_key(&key)
        } else {
            self.resources.contains_key(&key)
        };

        if exists {
            match behavior {
                crate::DuplicateBehavior::Error => {
                    return Err(duplicate_registration_error("Resource", &key));
                }
                crate::DuplicateBehavior::Warn => {
                    log::warn!(
                        target: "fastmcp_rust::router",
                        "resource already exists, keeping original; resource_key={}",
                        safe_log_label(&key)
                    );
                    return Ok(());
                }
                crate::DuplicateBehavior::Replace => {
                    log::debug!(
                        target: "fastmcp_rust::router",
                        "replacing resource; resource_key={}",
                        safe_log_label(&key)
                    );
                    // Fall through to insert
                }
                crate::DuplicateBehavior::Ignore => {
                    return Ok(());
                }
            }
        }

        if let Some(template) = template {
            let legacy_matcher = if legacy_enabled {
                match admit_legacy_resource_template(&template.uri_template) {
                    Ok(matcher) => Some(matcher),
                    Err(error) if !admit_final => return Err(error),
                    Err(_) => None,
                }
            } else {
                None
            };
            let (matcher, specificity) = if admit_final {
                let (matcher, specificity) = admit_resource_template(&template.uri_template)?;
                self.reject_final_template_collisions(
                    &matcher,
                    Some(template.uri_template.as_str()),
                )?;
                (Some(matcher), specificity)
            } else {
                let matcher = legacy_matcher.as_ref().ok_or_else(|| {
                    McpError::internal_error(
                        "exact-2024 resource template admission lost its matcher",
                    )
                })?;
                (None, matcher.specificity())
            };
            let uri_use_policy = if admit_final {
                ResourceUriUsePolicy::from_client_direct_https(
                    crate::catch_extension_unwind(|| handler.final_client_direct_https()).map_err(
                        |_payload| {
                            McpError::internal_error(
                                "resource URI-use policy hook panicked during admission",
                            )
                        },
                    )?,
                )
            } else {
                ResourceUriUsePolicy::server_mediated()
            };
            let final_definition = admit_final
                .then(|| {
                    admit_final_resource_template_definition(
                        Some(&handler),
                        &template,
                        uri_use_policy,
                    )
                })
                .transpose()?;
            let boxed: BoxedResourceHandler = Box::new(handler);
            let is_new = !self.resource_templates.contains_key(&template.uri_template);
            if !is_new {
                self.legacy_resource_template_completion_handlers
                    .remove(&template.uri_template);
                self.final_resource_template_completion_handlers
                    .remove(&template.uri_template);
            }
            let entry = ResourceTemplateEntry {
                matcher,
                specificity,
                template: template.clone(),
                handler: Some(boxed),
                final_definition,
                uri_use_policy,
                legacy_enabled: legacy_matcher.is_some(),
                legacy_matcher,
            };
            self.resource_templates
                .insert(template.uri_template.clone(), entry);
            if is_new {
                self.resource_template_order.push(template.uri_template);
            }
            self.rebuild_sorted_template_keys();
        } else {
            let uri_use_policy = if admit_final {
                ResourceUriUsePolicy::from_client_direct_https(
                    crate::catch_extension_unwind(|| handler.final_client_direct_https()).map_err(
                        |_payload| {
                            McpError::internal_error(
                                "resource URI-use policy hook panicked during admission",
                            )
                        },
                    )?,
                )
            } else {
                ResourceUriUsePolicy::server_mediated()
            };
            let final_definition = admit_final
                .then(|| admit_final_resource_definition(&handler, &def, uri_use_policy))
                .transpose()?;
            if let Some(final_definition) = final_definition.as_ref() {
                self.validate_mcp_apps_ui_resource_admission(final_definition, allow_mcp_apps)?;
                self.validate_mcp_apps_resource_bindings(final_definition)?;
                self.reject_final_exact_resource_template_collisions(&def.uri)?;
            }
            let boxed: BoxedResourceHandler = Box::new(handler);
            let is_new = !self.resources.contains_key(&def.uri);
            self.resources.insert(def.uri.clone(), boxed);
            if legacy_enabled {
                self.final_only_resources.remove(&def.uri);
            } else {
                self.final_only_resources.insert(def.uri.clone());
            }
            match final_definition {
                Some(final_definition) => {
                    self.final_resources.insert(
                        def.uri.clone(),
                        AdmittedFinalResourceRegistration {
                            definition: final_definition,
                            tags: def.tags.clone(),
                            uri_use_policy,
                        },
                    );
                }
                None => {
                    self.final_resources.remove(&def.uri);
                }
            }
            if is_new {
                self.resource_order.push(def.uri);
            }
        }

        self.advance_final_catalog_revision();
        Ok(())
    }

    /// Adds a resource template definition.
    ///
    /// If a template with the same URI template already exists, its definition
    /// is replaced while any registered handler is retained. Use
    /// [`add_resource_template_with_behavior`](Self::add_resource_template_with_behavior)
    /// for finer control over duplicate handling.
    pub fn add_resource_template(&mut self, template: ResourceTemplate) {
        let key = template.uri_template.clone();
        if let Err(error) =
            self.add_resource_template_with_behavior(template, crate::DuplicateBehavior::Replace)
        {
            log::warn!(
                target: "fastmcp_rust::router",
                "rejected resource template definition; template_key={}; code={:?}",
                safe_log_label(&key),
                error.code
            );
        }
    }

    /// Adds an exact-2024-only resource template definition.
    pub fn add_legacy_resource_template(&mut self, template: ResourceTemplate) {
        let key = template.uri_template.clone();
        if let Err(error) = self.add_resource_template_registration_with_behavior(
            template,
            crate::DuplicateBehavior::Replace,
            false,
        ) {
            log::warn!(
                target: "fastmcp_rust::router",
                "rejected exact-2024-only resource template; template_key={}; code={:?}",
                safe_log_label(&key),
                error.code
            );
        }
    }

    /// Adds a resource template definition with specified duplicate behavior.
    ///
    /// Replacing a definition retains an existing handler registered for the
    /// same URI template. Returns `Err` when behavior is
    /// [`crate::DuplicateBehavior::Error`] and the URI template already exists.
    pub fn add_resource_template_with_behavior(
        &mut self,
        template: ResourceTemplate,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_resource_template_registration_with_behavior(template, behavior, true)
    }

    /// Adds an exact-2024-only resource template with duplicate handling.
    pub fn add_legacy_resource_template_with_behavior(
        &mut self,
        template: ResourceTemplate,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_resource_template_registration_with_behavior(template, behavior, false)
    }

    fn add_resource_template_registration_with_behavior(
        &mut self,
        template: ResourceTemplate,
        behavior: crate::DuplicateBehavior,
        admit_final: bool,
    ) -> Result<(), McpError> {
        let key = template.uri_template.clone();
        let existed = self.resource_templates.contains_key(&key);

        if existed {
            match behavior {
                crate::DuplicateBehavior::Error => {
                    return Err(duplicate_registration_error("Resource template", &key));
                }
                crate::DuplicateBehavior::Warn => {
                    log::warn!(
                        target: "fastmcp_rust::router",
                        "resource template already exists, keeping original; template_key={}",
                        safe_log_label(&key)
                    );
                    return Ok(());
                }
                crate::DuplicateBehavior::Replace => {
                    log::debug!(
                        target: "fastmcp_rust::router",
                        "replacing resource template definition; template_key={}",
                        safe_log_label(&key)
                    );
                }
                crate::DuplicateBehavior::Ignore => return Ok(()),
            }
        }

        let legacy_matcher = match admit_legacy_resource_template(&key) {
            Ok(matcher) => Some(matcher),
            Err(error) if !admit_final => return Err(error),
            Err(_) => None,
        };
        let (matcher, specificity) = if admit_final {
            let (matcher, specificity) = admit_resource_template(&key)?;
            self.reject_final_template_collisions(&matcher, Some(key.as_str()))?;
            (Some(matcher), specificity)
        } else {
            let matcher = legacy_matcher.as_ref().ok_or_else(|| {
                McpError::internal_error("exact-2024 resource template admission lost its matcher")
            })?;
            (None, matcher.specificity())
        };
        let uri_use_policy = ResourceUriUsePolicy::server_mediated();
        let final_definition = admit_final
            .then(|| {
                admit_final_resource_template_definition::<dyn ResourceHandler>(
                    None,
                    &template,
                    uri_use_policy,
                )
            })
            .transpose()?;
        if existed {
            self.legacy_resource_template_completion_handlers
                .remove(&key);
            self.final_resource_template_completion_handlers
                .remove(&key);
        }
        let needs_rebuild = match self.resource_templates.get_mut(&key) {
            Some(existing) => {
                existing.template = template;
                existing.matcher = matcher;
                existing.specificity = specificity;
                existing.final_definition = final_definition;
                existing.uri_use_policy = uri_use_policy;
                existing.legacy_enabled = legacy_matcher.is_some();
                existing.legacy_matcher = legacy_matcher;
                false // Key already exists, order unchanged
            }
            None => {
                self.resource_templates.insert(
                    key.clone(),
                    ResourceTemplateEntry {
                        matcher,
                        specificity,
                        template,
                        handler: None,
                        final_definition,
                        uri_use_policy,
                        legacy_enabled: legacy_matcher.is_some(),
                        legacy_matcher,
                    },
                );
                true // New key added, need to rebuild
            }
        };
        if needs_rebuild {
            self.resource_template_order.push(key);
            self.rebuild_sorted_template_keys();
        }
        self.advance_final_catalog_revision();
        Ok(())
    }

    /// Adds a prompt handler.
    ///
    /// If a prompt with the same name already exists, it will be replaced.
    /// Use [`add_prompt_with_behavior`](Self::add_prompt_with_behavior) for
    /// finer control over duplicate handling.
    pub fn add_prompt<H: PromptHandler + 'static>(&mut self, handler: H) {
        if let Err(error) = self.add_prompt_registration_with_behavior(
            handler,
            crate::DuplicateBehavior::Replace,
            true,
            true,
        ) {
            log::warn!(
                target: "fastmcp_rust::router",
                "rejected prompt registration; code={:?}",
                error.code
            );
        }
    }

    /// Adds an intentionally exact-2024-only prompt handler.
    pub fn add_legacy_prompt<H: PromptHandler + 'static>(&mut self, handler: H) {
        if let Err(error) = self.add_prompt_registration_with_behavior(
            handler,
            crate::DuplicateBehavior::Replace,
            false,
            true,
        ) {
            log::warn!(
                target: "fastmcp_rust::router",
                "rejected exact-2024-only prompt registration; code={:?}",
                error.code
            );
        }
    }

    /// Adds a prompt handler with specified duplicate behavior.
    ///
    /// Returns `Err` if behavior is [`crate::DuplicateBehavior::Error`] and the
    /// prompt name already exists.
    pub fn add_prompt_with_behavior<H: PromptHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_prompt_registration_with_behavior(handler, behavior, true, true)
    }

    /// Adds an exact-final-only prompt with duplicate handling.
    pub(crate) fn add_final_prompt_with_behavior<H: PromptHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_prompt_registration_with_behavior(handler, behavior, true, false)
    }

    /// Adds an exact-2024-only prompt with duplicate handling.
    pub fn add_legacy_prompt_with_behavior<H: PromptHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
    ) -> Result<(), McpError> {
        self.add_prompt_registration_with_behavior(handler, behavior, false, true)
    }

    fn add_prompt_registration_with_behavior<H: PromptHandler + 'static>(
        &mut self,
        handler: H,
        behavior: crate::DuplicateBehavior,
        admit_final: bool,
        legacy_enabled: bool,
    ) -> Result<(), McpError> {
        let def = crate::catch_extension_unwind(|| handler.definition()).map_err(|_payload| {
            McpError::internal_error("prompt definition hook panicked during admission")
        })?;
        let name = &def.name;

        let existed = self.prompts.contains_key(name);
        if existed {
            match behavior {
                crate::DuplicateBehavior::Error => {
                    return Err(duplicate_registration_error("Prompt", name));
                }
                crate::DuplicateBehavior::Warn => {
                    log::warn!(
                        target: "fastmcp_rust::router",
                        "prompt already exists, keeping original; prompt_key={}",
                        safe_log_label(name)
                    );
                    return Ok(());
                }
                crate::DuplicateBehavior::Replace => {
                    log::debug!(
                        target: "fastmcp_rust::router",
                        "replacing prompt; prompt_key={}",
                        safe_log_label(name)
                    );
                    // Fall through to insert
                }
                crate::DuplicateBehavior::Ignore => {
                    return Ok(());
                }
            }
        }

        let uri_use_policy = if admit_final {
            ResourceUriUsePolicy::from_client_direct_https(
                crate::catch_extension_unwind(|| handler.final_client_direct_https()).map_err(
                    |_payload| {
                        McpError::internal_error(
                            "prompt URI-use policy hook panicked during admission",
                        )
                    },
                )?,
            )
        } else {
            ResourceUriUsePolicy::server_mediated()
        };
        let final_definition = admit_final
            .then(|| admit_final_prompt_definition(&handler, &def))
            .transpose()?;
        if existed {
            self.legacy_prompt_completion_handlers.remove(&def.name);
            self.final_prompt_completion_handlers.remove(&def.name);
        }
        self.prompts.insert(def.name.clone(), Box::new(handler));
        if legacy_enabled {
            self.final_only_prompts.remove(&def.name);
        } else {
            self.final_only_prompts.insert(def.name.clone());
        }
        match final_definition {
            Some(final_definition) => {
                self.final_prompts.insert(
                    def.name.clone(),
                    AdmittedFinalPromptRegistration {
                        definition: final_definition,
                        tags: def.tags.clone(),
                        uri_use_policy,
                    },
                );
            }
            None => {
                self.final_prompts.remove(&def.name);
            }
        }
        if !existed {
            self.prompt_order.push(def.name);
        }
        self.advance_final_catalog_revision();
        Ok(())
    }

    /// Returns all tool definitions.
    #[must_use]
    pub fn tools(&self) -> Vec<Tool> {
        self.tool_order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .filter(|entry| entry.legacy_enabled)
            .map(|entry| entry.definition.clone())
            .collect()
    }

    /// Returns tool definitions filtered by session state and tags.
    ///
    /// Tools that have been disabled in the session state will not be included.
    /// If tag filters are provided, tools must match the include/exclude criteria.
    #[must_use]
    pub fn tools_filtered(
        &self,
        session_state: Option<&SessionState>,
        tag_filters: Option<&TagFilters<'_>>,
    ) -> Vec<Tool> {
        self.tool_order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .filter_map(|entry| {
                if !entry.legacy_enabled {
                    return None;
                }
                let def = &entry.definition;
                // Check session state filter
                if let Some(state) = session_state {
                    if !state.is_tool_enabled(&def.name) {
                        return None;
                    }
                }
                // Check tag filters
                if let Some(filters) = tag_filters {
                    if !filters.matches(&def.tags) {
                        return None;
                    }
                }
                Some(def.clone())
            })
            .collect()
    }

    /// Returns all resource definitions.
    #[must_use]
    pub fn resources(&self) -> Vec<Resource> {
        self.resource_order
            .iter()
            .filter_map(|uri| self.resources.get(uri))
            .filter(|handler| {
                let uri = handler.definition().uri;
                !self.final_only_resources.contains(&uri)
            })
            .map(|h| h.definition())
            .collect()
    }

    /// Returns resource definitions filtered by session state and tags.
    ///
    /// Resources that have been disabled in the session state will not be included.
    /// If tag filters are provided, resources must match the include/exclude criteria.
    #[must_use]
    pub fn resources_filtered(
        &self,
        session_state: Option<&SessionState>,
        tag_filters: Option<&TagFilters<'_>>,
    ) -> Vec<Resource> {
        self.resource_order
            .iter()
            .filter_map(|uri| self.resources.get(uri))
            .filter_map(|h| {
                let def = h.definition();
                if self.final_only_resources.contains(&def.uri) {
                    return None;
                }
                // Check session state filter
                if let Some(state) = session_state {
                    if !state.is_resource_enabled(&def.uri) {
                        return None;
                    }
                }
                // Check tag filters
                if let Some(filters) = tag_filters {
                    if !filters.matches(&def.tags) {
                        return None;
                    }
                }
                Some(def)
            })
            .collect()
    }

    /// Returns all resource templates.
    #[must_use]
    pub fn resource_templates(&self) -> Vec<ResourceTemplate> {
        self.resource_template_order
            .iter()
            .filter_map(|t| self.resource_templates.get(t))
            .filter(|entry| entry.legacy_enabled)
            .map(|entry| entry.template.clone())
            .collect()
    }

    /// Returns resource templates filtered by session state and tags.
    ///
    /// Templates that have been disabled in the session state will not be included.
    /// If tag filters are provided, templates must match the include/exclude criteria.
    #[must_use]
    pub fn resource_templates_filtered(
        &self,
        session_state: Option<&SessionState>,
        tag_filters: Option<&TagFilters<'_>>,
    ) -> Vec<ResourceTemplate> {
        self.resource_template_order
            .iter()
            .filter_map(|t| self.resource_templates.get(t))
            .filter_map(|entry| {
                if !entry.legacy_enabled {
                    return None;
                }
                // Check session state filter
                if let Some(state) = session_state {
                    if !state.is_resource_enabled(&entry.template.uri_template) {
                        return None;
                    }
                }
                // Check tag filters
                if let Some(filters) = tag_filters {
                    if !filters.matches(&entry.template.tags) {
                        return None;
                    }
                }
                Some(entry.template.clone())
            })
            .collect()
    }

    /// Returns all prompt definitions.
    #[must_use]
    pub fn prompts(&self) -> Vec<Prompt> {
        self.prompt_order
            .iter()
            .filter_map(|name| self.prompts.get(name))
            .filter(|handler| {
                let name = handler.definition().name;
                !self.final_only_prompts.contains(&name)
            })
            .map(|h| h.definition())
            .collect()
    }

    /// Returns prompt definitions filtered by session state and tags.
    ///
    /// Prompts that have been disabled in the session state will not be included.
    /// If tag filters are provided, prompts must match the include/exclude criteria.
    #[must_use]
    pub fn prompts_filtered(
        &self,
        session_state: Option<&SessionState>,
        tag_filters: Option<&TagFilters<'_>>,
    ) -> Vec<Prompt> {
        self.prompt_order
            .iter()
            .filter_map(|name| self.prompts.get(name))
            .filter_map(|h| {
                let def = h.definition();
                if self.final_only_prompts.contains(&def.name) {
                    return None;
                }
                // Check session state filter
                if let Some(state) = session_state {
                    if !state.is_prompt_enabled(&def.name) {
                        return None;
                    }
                }
                // Check tag filters
                if let Some(filters) = tag_filters {
                    if !filters.matches(&def.tags) {
                        return None;
                    }
                }
                Some(def)
            })
            .collect()
    }

    /// Returns the number of registered tools.
    #[must_use]
    pub fn tools_count(&self) -> usize {
        self.tools.len()
    }

    /// Returns the number of registered resources.
    #[must_use]
    pub fn resources_count(&self) -> usize {
        self.resources.len()
    }

    /// Returns the number of registered resource templates.
    #[must_use]
    pub fn resource_templates_count(&self) -> usize {
        self.resource_templates.len()
    }

    /// Returns the number of registered prompts.
    #[must_use]
    pub fn prompts_count(&self) -> usize {
        self.prompts.len()
    }

    /// Returns the immutable behavior registry for final server discovery.
    ///
    /// This records only APIs backed by this router's installed catalog. The
    /// The server composition supplies final subscription execution and
    /// publishes catalog/resource changes through its request-owned listener
    /// registry. This router records the catalog branches that can therefore
    /// be selected by those final filters; it still does not advertise the
    /// removed logging-request emitter.
    #[must_use]
    pub(crate) fn server_discovery_behavior_registry(&self) -> ServerBehaviorRegistry {
        let mut behaviors = Vec::with_capacity(11);
        behaviors.push(ServerBehavior::SubscriptionsListen);
        if self.has_final_completion_handler() {
            behaviors.push(ServerBehavior::CompletionComplete);
        }
        if self
            .tool_order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .any(|entry| entry.final_registration.is_some())
        {
            behaviors.push(ServerBehavior::ToolsList);
            behaviors.push(ServerBehavior::ToolsListChangedNotification);
        }
        if !self.final_resources.is_empty()
            || self
                .resource_template_order
                .iter()
                .filter_map(|key| self.resource_templates.get(key))
                .any(|entry| entry.final_definition.is_some())
        {
            behaviors.push(ServerBehavior::ResourcesList);
            behaviors.push(ServerBehavior::ResourcesListChangedNotification);
        }
        if !self.final_resources.is_empty() {
            behaviors.push(ServerBehavior::ResourceUpdateDelivery);
        }
        if !self.final_prompts.is_empty() {
            behaviors.push(ServerBehavior::PromptsList);
            behaviors.push(ServerBehavior::PromptsListChangedNotification);
        }
        ServerBehaviorRegistry::from_behaviors(behaviors)
    }

    /// Gets a tool handler by name.
    #[must_use]
    pub fn get_tool(&self, name: &str) -> Option<&BoxedToolHandler> {
        self.tools.get(name).map(|entry| &entry.handler)
    }

    /// Returns whether an admitted exact-2024 request selects a handler frozen
    /// as async at registration and therefore stays on the caller's runtime.
    pub(crate) fn legacy_request_uses_transport_owned_async_dispatch(
        &self,
        request: &JsonRpcRequest,
    ) -> bool {
        if request.method != "tools/call" {
            return false;
        }
        let Some(params) = request.params.clone() else {
            return false;
        };
        let Ok(params) = serde_json::from_value::<CallToolParams>(params) else {
            return false;
        };
        self.tools.get(&params.name).is_some_and(|registration| {
            registration.legacy_enabled
                && registration.execution_mode == crate::ToolExecutionMode::Async
        })
    }

    /// Gets a resource handler by URI.
    #[must_use]
    pub fn get_resource(&self, uri: &str) -> Option<&BoxedResourceHandler> {
        self.resources.get(uri)
    }

    /// Gets a resource template by URI template.
    #[must_use]
    pub fn get_resource_template(&self, uri_template: &str) -> Option<&ResourceTemplate> {
        self.resource_templates
            .get(uri_template)
            .map(|entry| &entry.template)
    }

    /// Returns true if a resource exists for the given URI (static or template match).
    #[must_use]
    pub fn resource_exists(&self, uri: &str) -> bool {
        self.resolve_resource(uri).is_some()
    }

    /// Forwards a session `resources/subscribe` onto the resolved handler.
    pub(crate) fn notify_resource_subscribed(&self, ctx: &McpContext, uri: &str) -> McpResult<()> {
        if let Some(resolved) = self.resolve_resource(uri) {
            resolved.handler.on_subscribe(ctx, uri)?;
        }
        Ok(())
    }

    /// Forwards a session `resources/unsubscribe` onto the resolved handler.
    pub(crate) fn notify_resource_unsubscribed(
        &self,
        ctx: &McpContext,
        uri: &str,
    ) -> McpResult<()> {
        if let Some(resolved) = self.resolve_resource(uri) {
            resolved.handler.on_unsubscribe(ctx, uri)?;
        }
        Ok(())
    }

    /// Awaits the resolved handler's subscription hook on the request runtime.
    pub(crate) async fn notify_resource_subscribed_async(
        &self,
        ctx: &McpContext,
        uri: &str,
    ) -> McpResult<()> {
        ctx.ensure_live()?;
        if let Some(resolved) = self.resolve_resource(uri) {
            resolved.handler.on_subscribe_async(ctx, uri).await?;
        }
        ctx.ensure_live()?;
        Ok(())
    }

    /// Awaits the resolved handler's unsubscription hook on the request runtime.
    pub(crate) async fn notify_resource_unsubscribed_async(
        &self,
        ctx: &McpContext,
        uri: &str,
    ) -> McpResult<()> {
        ctx.ensure_live()?;
        if let Some(resolved) = self.resolve_resource(uri) {
            resolved.handler.on_unsubscribe_async(ctx, uri).await?;
        }
        ctx.ensure_live()?;
        Ok(())
    }

    fn resolve_resource(&self, uri: &str) -> Option<ResolvedResource<'_>> {
        self.resolve_resource_for_era(uri, None)
    }

    /// Resolves a registered tool visible in one requested protocol era.
    ///
    /// A final-only tool (Apps-linked, task-capable) is reachable only in the
    /// modern era; a legacy-enabled tool is reachable in the exact-2024 era.
    #[cfg(test)]
    fn resolve_tool_for_era(
        &self,
        name: &str,
        era: Option<ProtocolEra>,
    ) -> Option<&AdmittedToolRegistration> {
        let registration = self.tools.get(name)?;
        let visible = era.is_none_or(|era| match era {
            ProtocolEra::Modern2026 => registration.final_registration.is_some(),
            ProtocolEra::Legacy2024 => registration.legacy_enabled,
        });
        visible.then_some(registration)
    }

    /// Resolves a resource that is visible in one requested protocol era.
    ///
    /// A static resource takes precedence only when it is visible to that
    /// era. Otherwise a resource-template instance from the requested era
    /// must remain reachable; registrations intentionally keep the two
    /// catalogs separate.
    fn resolve_resource_for_era(
        &self,
        uri: &str,
        era: Option<ProtocolEra>,
    ) -> Option<ResolvedResource<'_>> {
        if let Some(handler) = self.resources.get(uri) {
            let resolved = ResolvedResource {
                handler,
                params: UriParams::new(),
                final_enabled: self.final_resources.contains_key(uri),
                legacy_enabled: !self.final_only_resources.contains(uri),
                uri_use_policy: self
                    .final_resources
                    .get(uri)
                    .map_or_else(ResourceUriUsePolicy::server_mediated, |entry| {
                        entry.uri_use_policy
                    }),
            };
            if era.is_none_or(|era| resolved.is_enabled_in(era)) {
                return Some(resolved);
            }
        }

        // Use pre-sorted template keys to avoid sorting on every lookup
        'templates: for key in &self.sorted_template_keys {
            let entry = &self.resource_templates[key];
            let Some(handler) = entry.handler.as_ref() else {
                continue;
            };
            let legacy_params = || {
                entry
                    .legacy_matcher
                    .as_ref()
                    .and_then(|matcher| matcher.matches(uri))
            };
            let final_params = || {
                let values = entry.matcher.as_ref()?.match_uri(uri).ok()??;
                let mut params = UriParams::with_capacity(values.len());
                for (name, value) in values {
                    let TemplateValue::Scalar(value) = value else {
                        return None;
                    };
                    params.insert(name, value);
                }
                Some(params)
            };
            let Some(params) = (match era {
                Some(ProtocolEra::Legacy2024) => legacy_params(),
                Some(ProtocolEra::Modern2026) => final_params(),
                None => final_params().or_else(legacy_params),
            }) else {
                continue 'templates;
            };
            let resolved = ResolvedResource {
                handler,
                params,
                final_enabled: entry.final_definition.is_some(),
                legacy_enabled: entry.legacy_enabled,
                uri_use_policy: entry.uri_use_policy,
            };
            if era.is_none_or(|era| resolved.is_enabled_in(era)) {
                return Some(resolved);
            }
        }

        None
    }

    /// Gets a prompt handler by name.
    #[must_use]
    pub fn get_prompt(&self, name: &str) -> Option<&BoxedPromptHandler> {
        self.prompts.get(name)
    }

    // ========================================================================
    // Request Dispatch Methods
    // ========================================================================

    /// Handles the initialize request.
    pub fn handle_initialize(
        &self,
        request_ctx: &McpContext,
        session: &mut Session,
        params: InitializeParams,
        instructions: Option<&str>,
    ) -> McpResult<InitializeResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        debug!(
            target: targets::SESSION,
            "preparing session initialization; client_key={}",
            safe_log_label(&params.client_info.name)
        );

        // Initialize the session
        session.initialize(
            params.client_info,
            params.capabilities,
            PROTOCOL_VERSION.to_string(),
        );

        Ok(InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: session.server_capabilities().clone(),
            server_info: session.server_info().clone(),
            instructions: instructions.map(String::from),
        })
    }

    /// Dispatches one exact legacy `completion/complete` request.
    ///
    /// This route decodes through the dual-era core contract before invoking
    /// the installed handler. In particular, a final `_meta` object remains a
    /// cross-era error even though the legacy parameter shape is otherwise
    /// intentionally open.
    pub(crate) async fn dispatch_legacy_completion(
        &self,
        request_ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<serde_json::Value> {
        self.dispatch_legacy_completion_in_request(request_ctx, request_ctx.cx(), request)
            .await
    }

    pub(crate) async fn dispatch_legacy_completion_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        request: &JsonRpcRequest,
    ) -> McpResult<serde_json::Value> {
        if request.method != COMPLETION_COMPLETE {
            return Err(McpError::method_not_found(&request.method));
        }

        let request = CoreRequest::decode(
            ProtocolEra::Legacy2024,
            COMPLETION_COMPLETE,
            request.params.as_ref(),
        )
        .map_err(|error| McpError::invalid_params(error.to_string()))?;
        let CoreRequest::Legacy(LegacyCoreRequest::Completion(params)) = request else {
            return Err(McpError::internal_error(
                "legacy completion dispatch selected another core request",
            ));
        };

        serde_json::to_value(
            self.handle_completion_legacy_in_request(request_ctx, request_cx, params)
                .await?,
        )
        .map_err(McpError::from)
    }

    /// Handles one exact MCP 2024-11-05 completion request.
    ///
    /// Completion providers retain the legacy freedom to resolve references
    /// and arguments outside the local catalog. Session-disabled and
    /// final-only targets remain inaccessible through this discovery surface.
    pub async fn handle_completion_legacy(
        &self,
        request_ctx: &McpContext,
        params: LegacyCompletionParams,
    ) -> McpResult<LegacyCompletionResult> {
        self.handle_completion_legacy_in_request(request_ctx, request_ctx.cx(), params)
            .await
    }

    pub(crate) async fn handle_completion_legacy_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: LegacyCompletionParams,
    ) -> McpResult<LegacyCompletionResult> {
        let dispatch_started_at = request_ctx.cx().now();
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let target_handler = match &params.reference {
            fastmcp_protocol::LegacyCompletionReference::Prompt { name } => {
                if !request_ctx.is_prompt_enabled(name) || self.final_only_prompts.contains(name) {
                    return Err(McpError::invalid_params(
                        "completion prompt reference is not available",
                    ));
                }
                self.legacy_prompt_completion_handlers.get(name)
            }
            fastmcp_protocol::LegacyCompletionReference::Resource { uri } => {
                if !request_ctx.is_resource_enabled(uri)
                    || self.final_only_resources.contains(uri)
                    || self
                        .resource_templates
                        .get(uri)
                        .is_some_and(|entry| !entry.legacy_enabled)
                {
                    return Err(McpError::invalid_params(
                        "completion resource reference is not available",
                    ));
                }
                self.legacy_resource_template_completion_handlers.get(uri)
            }
        };
        let handler = target_handler
            .or(self.completion_handler.as_ref())
            .ok_or_else(|| McpError::method_not_found(COMPLETION_COMPLETE))?;
        let handler_ctx =
            derive_handler_context(request_ctx, None, None, None, ProtocolEra::Legacy2024);
        let handler_timeout =
            read_handler_timeout(request_ctx.cx(), "completion_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let handler_ctx = handler_ctx.with_operation_deadline(effective_budget.deadline);
        let outcome = run_handler_in_request(
            &handler_ctx,
            request_cx,
            effective_budget,
            "completion",
            |child_cx| handler.complete_legacy_async_in_request(&handler_ctx, child_cx, params),
        )
        .await?;

        let completion = match outcome {
            Outcome::Ok(completion) => {
                if completion.values.len() > fastmcp_protocol::MAX_COMPLETION_VALUES {
                    return Err(McpError::internal_error(
                        "completion handler returned more than 100 values",
                    ));
                }
                completion
            }
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(
                    request_ctx.cx(),
                    "completion",
                    error,
                ));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(request_ctx.cx(), "completion"));
            }
        };

        Ok(LegacyCompletionResult {
            completion,
            meta: None,
        })
    }

    async fn handle_completion_final_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalCompletionParams,
    ) -> McpResult<FinalCompletionResult> {
        let dispatch_started_at = request_ctx.cx().now();
        if request_cx_cancellation_is_visible(request_cx) {
            return Err(McpError::request_cancelled());
        }
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        if !self.has_final_completion_handler() {
            return Err(McpError::method_not_found(COMPLETION_COMPLETE));
        }

        let provider_handler = match &params.reference {
            FinalCompletionReference::Prompt { name }
            | FinalCompletionReference::PromptWithTitle { name, .. } => {
                // Completion is a reference-discovery surface as well as an
                // invocation surface. Keep a component disabled for this
                // modern connection indistinguishable from an unregistered
                // prompt, before consulting its schema or a provider.
                if !request_ctx.is_prompt_enabled(name) {
                    return Err(McpError::invalid_params(
                        "completion prompt reference is not registered",
                    ));
                }
                let prompt = self.final_prompts.get(name).ok_or_else(|| {
                    McpError::invalid_params("completion prompt reference is not registered")
                })?;
                if !prompt
                    .definition
                    .arguments
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|argument| argument.name == params.argument.name)
                {
                    return Err(McpError::invalid_params(
                        "completion argument is not declared by the referenced target",
                    ));
                }
                self.final_prompt_completion_handlers.get(name)
            }
            FinalCompletionReference::Resource { uri } => {
                // Resource-template completion must use the same individual-request
                // admission gate as resources/read. Catalog listing is deliberately
                // connection-independent, so never fall through to the global
                // completion provider for a disabled template.
                if !request_ctx.is_resource_enabled(uri) {
                    return Err(McpError::invalid_params(
                        "completion resource reference is not registered",
                    ));
                }
                let template = self
                    .resource_templates
                    .get(uri)
                    .filter(|entry| entry.final_definition.is_some())
                    .ok_or_else(|| {
                        McpError::invalid_params("completion resource reference is not registered")
                    })?;
                let template = fastmcp_protocol::UriTemplate::parse(
                    &template.template.uri_template,
                )
                .map_err(|_| {
                    McpError::internal_error(
                        "admitted completion resource template is no longer valid",
                    )
                })?;
                if !template.parts().iter().any(|part| {
                    matches!(part, UriTemplatePart::Expression(expression)
                        if expression
                            .variables()
                            .iter()
                            .any(|variable| variable.name() == params.argument.name))
                }) {
                    return Err(McpError::invalid_params(
                        "completion argument is not declared by the referenced target",
                    ));
                }
                self.final_resource_template_completion_handlers.get(uri)
            }
        };

        let handler = match provider_handler {
            Some(handler) => handler,
            None if self.default_final_completion_enabled => self
                .completion_handler
                .as_ref()
                .ok_or_else(|| McpError::method_not_found(COMPLETION_COMPLETE))?,
            None => {
                return Err(McpError::invalid_params(
                    "no final completion provider is registered for the referenced target",
                ));
            }
        };
        let handler_ctx =
            derive_handler_context(request_ctx, None, None, None, ProtocolEra::Modern2026);
        let handler_timeout =
            read_handler_timeout(request_ctx.cx(), "completion_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let handler_ctx = handler_ctx.with_operation_deadline(effective_budget.deadline);
        let outcome = run_handler_in_request(
            &handler_ctx,
            request_cx,
            effective_budget,
            "completion",
            |child_cx| handler.complete_final_async_in_request(&handler_ctx, child_cx, params),
        )
        .await?;

        let completion = match outcome {
            Outcome::Ok(completion) => {
                if completion.values.len() > fastmcp_protocol::MAX_COMPLETION_VALUES {
                    return Err(McpError::internal_error(
                        "completion handler returned more than 100 values",
                    ));
                }
                completion.validate().map_err(McpError::internal_error)?;
                completion
            }
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(
                    request_ctx.cx(),
                    "completion",
                    error,
                ));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(request_ctx.cx(), "completion"));
            }
        };

        Ok(FinalCompletionResult { completion })
    }

    /// Dispatches a modern request through the transport-neutral final router.
    ///
    /// This is the modern server-side routing seam. It deliberately has no
    /// `Session` argument: final catalog pages and cursors are independent of
    /// connection state. A supplied modern connection context still controls
    /// individual request admission. Durable contexts bind MRTR retries to
    /// their exact partition; an ephemeral HTTP partition is only an
    /// eligibility token that this router replaces with its stateless domain.
    /// Every successful response is re-emitted through the final
    /// complete-result contract. State-bearing lifecycle methods and exact
    /// 2024-11-05 wire results stay on the legacy adapter rather than
    /// acquiring accidental modern semantics.
    pub(crate) async fn dispatch_stateless(
        &self,
        request_ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<serde_json::Value> {
        let continuation_cancellation = fastmcp_core::McpRequestCancellation::new();
        self.dispatch_stateless_with_continuation_cancellation(
            request_ctx,
            request,
            &continuation_cancellation,
        )
        .await
    }

    /// Dispatches a modern request with the exact admitted `params` source.
    ///
    /// Callers that received an ingress raw-parameter sidecar must use this
    /// entry point so final MRTR retries retain ordered response entries and
    /// reject duplicate keys before registry admission.
    pub(crate) async fn dispatch_stateless_with_raw_params(
        &self,
        request_ctx: &McpContext,
        request: &JsonRpcRequest,
        raw_params: Option<&str>,
    ) -> McpResult<serde_json::Value> {
        let continuation_cancellation = fastmcp_core::McpRequestCancellation::new();
        self.dispatch_stateless_with_continuation_cancellation_and_raw_params(
            request_ctx,
            request,
            raw_params,
            &continuation_cancellation,
        )
        .await
    }

    pub(crate) async fn dispatch_stateless_with_continuation_cancellation(
        &self,
        request_ctx: &McpContext,
        request: &JsonRpcRequest,
        continuation_cancellation: &fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        self.dispatch_stateless_with_continuation_cancellation_and_raw_params(
            request_ctx,
            request,
            None,
            continuation_cancellation,
        )
        .await
    }

    /// Dispatches with connection-owned continuation cancellation and the
    /// exact admitted parameter source retained by transport ingress.
    pub(crate) async fn dispatch_stateless_with_continuation_cancellation_and_raw_params(
        &self,
        request_ctx: &McpContext,
        request: &JsonRpcRequest,
        raw_params: Option<&str>,
        continuation_cancellation: &fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        self.dispatch_stateless_in_request(
            request_ctx,
            request_ctx.cx(),
            request,
            raw_params,
            continuation_cancellation,
        )
        .await
    }

    /// Dispatches one modern request in a request-owned structured child task.
    ///
    /// With a runtime-backed context, the caller owns a child region containing
    /// the handler task and descendants spawned through the explicit in-request
    /// Cx. Returning awaits their quiescent close; dropping the future requests
    /// close without claiming that cleanup has already finished. Request-local
    /// cancellation affects this subtree, never sibling requests. Direct callers
    /// without a runtime gateway retain the inline dispatch path.
    pub(crate) async fn dispatch_stateless_owned(
        self: Arc<Self>,
        request_ctx: McpContext,
        request: JsonRpcRequest,
    ) -> McpResult<serde_json::Value> {
        self.dispatch_stateless_owned_with_continuation_cancellation(
            request_ctx,
            request,
            fastmcp_core::McpRequestCancellation::new(),
        )
        .await
    }

    /// Dispatches one modern request with the continuation owner selected by
    /// its transport connection. The owner is deliberately distinct from the
    /// request cancellation: an `input_required` response ends one JSON-RPC
    /// request normally, while its retry remains valid until its connection
    /// disconnects or the continuation expires.
    pub(crate) async fn dispatch_stateless_owned_with_continuation_cancellation(
        self: Arc<Self>,
        request_ctx: McpContext,
        request: JsonRpcRequest,
        continuation_cancellation: fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        self.dispatch_stateless_owned_with_continuation_cancellation_and_raw_params(
            request_ctx,
            request,
            None,
            continuation_cancellation,
        )
        .await
    }

    /// Dispatches an owned modern request with its retained raw-parameter
    /// sidecar. The sidecar is owned because a request child may outlive the
    /// transport frame reader that admitted it.
    pub(crate) async fn dispatch_stateless_owned_with_continuation_cancellation_and_raw_params(
        self: Arc<Self>,
        request_ctx: McpContext,
        request: JsonRpcRequest,
        raw_params: Option<Arc<str>>,
        continuation_cancellation: fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        if let Some(error) = budget_error(&request_ctx) {
            return Err(error);
        }

        let Some(region) = open_modern_request_region(&request_ctx).await? else {
            return self
                .dispatch_stateless_in_request(
                    &request_ctx,
                    request_ctx.cx(),
                    &request,
                    raw_params.as_deref(),
                    &continuation_cancellation,
                )
                .await;
        };
        let join_cx = request_ctx.cx().clone();
        let dispatch_ctx = request_ctx.clone();
        let blocking = request.method == "tools/call"
            && request
                .params
                .as_ref()
                .and_then(|params| params.get("name"))
                .and_then(serde_json::Value::as_str)
                .and_then(|name| self.tools.get(name))
                .is_some_and(|registration| {
                    registration.execution_mode == crate::ToolExecutionMode::Blocking
                });
        let task = region.cx().spawn(move |child_cx| async move {
            let dispatch_ctx = dispatch_ctx.with_request_cx(child_cx.clone());
            if blocking {
                let blocking_cx = child_cx.clone();
                return run_modern_blocking_dispatch(&child_cx, move || {
                    crate::poll_on_cx(
                        &blocking_cx,
                        self.dispatch_stateless_in_request(
                            &dispatch_ctx,
                            &blocking_cx,
                            &request,
                            raw_params.as_deref(),
                            &continuation_cancellation,
                        ),
                    )
                })
                .await;
            }
            self.dispatch_stateless_in_request(
                &dispatch_ctx,
                &child_cx,
                &request,
                raw_params.as_deref(),
                &continuation_cancellation,
            )
            .await
        });
        let mut task = match task {
            Ok(task) => task,
            Err(_) => {
                region.close().await.map_err(|_| {
                    McpError::internal_error("modern request child region could not be closed")
                })?;
                return Err(McpError::internal_error(
                    "request-owned modern dispatch could not be scheduled",
                ));
            }
        };

        let mut cancelled = std::pin::pin!(request_ctx.request_cancelled());
        let mut join = std::pin::pin!(task.join(&join_cx));
        let mut cancel_forwarded = false;
        let wait = std::future::poll_fn(|task_cx| {
            // Joining alone does not observe caller cancellation in the pinned
            // runtime. Arm the request token as well so even a parked handler
            // receives subtree cancellation without a polling loop.
            // The context wait also wakes when a shared framework mask exits.
            // Its completion linearizes visibility: rechecking a later mask
            // here would lose that observation and leave no waiter to wake.
            if !cancel_forwarded
                && (cancelled.as_mut().poll(task_cx).is_ready()
                    || (request_cx_cancellation_is_visible(&join_cx)
                        && budget_error(&request_ctx).is_some()))
            {
                let _ = region.cancel(asupersync::CancelReason::user("modern request cancelled"));
                cancel_forwarded = true;
            }
            join.as_mut().poll(task_cx)
        });
        let joined = match request_ctx.budget().deadline {
            Some(deadline) => asupersync::time::timeout_at(deadline, wait)
                .await
                .map_err(|_| {
                    McpError::new(McpErrorCode::RequestCancelled, "Request timeout exceeded")
                }),
            None => Ok(wait.await),
        };
        // Close cancels and drains remaining descendants on success, error,
        // panic, and deadline expiry. Keeping this wait inside dispatch also
        // lets the HTTP shutdown owner retain nonquiescent work as debt.
        region.close().await.map_err(|_| {
            McpError::internal_error("modern request child region could not be closed")
        })?;
        match joined? {
            Ok(result) => result,
            Err(asupersync::runtime::JoinError::Panicked(_payload)) => {
                Err(sanitized_handler_panic(&join_cx, "modern_dispatch"))
            }
            Err(_error) => Err(McpError::request_cancelled()),
        }
    }

    async fn dispatch_stateless_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        request: &JsonRpcRequest,
        raw_params: Option<&str>,
        continuation_cancellation: &fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        if request_cx_cancellation_is_visible(request_cx) {
            return Err(McpError::request_cancelled());
        }
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let params = request.params.as_ref();
        let result = match request.method.as_str() {
            // Connection health-check. Session dispatch already answers `{}`.
            // Stateless HTTP needs the same check without adding ping to
            // FINAL_2026_07_28_METHODS / FinalCoreRequest.
            "ping" => serde_json::json!({}),
            COMPLETION_COMPLETE => {
                let request =
                    CoreRequest::decode(ProtocolEra::Modern2026, COMPLETION_COMPLETE, params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::Completion(params)) = request else {
                    return Err(McpError::internal_error(
                        "modern completion dispatch selected another core request",
                    ));
                };
                encode_stateless_handler_result(
                    self.handle_completion_final_in_request(request_ctx, request_cx, params)
                        .await,
                )?
            }
            "tools/list" => {
                let request = CoreRequest::decode(ProtocolEra::Modern2026, "tools/list", params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::ToolsList(params)) = request else {
                    return Err(McpError::internal_error(
                        "modern tools/list dispatch selected another core request",
                    ));
                };
                encode_final_core_result(
                    self.handle_final_tools_list(request_ctx, params),
                    |result| FinalCoreResult::ToolsList {
                        result,
                        diagnostic: None,
                    },
                )?
            }
            "tools/call" => {
                self.admit_final_mrtr_response_map(params)?;
                let request = CoreRequest::decode_with_raw_params(
                    ProtocolEra::Modern2026,
                    "tools/call",
                    params,
                    raw_params,
                )
                .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::ToolsCall(params)) = request else {
                    return Err(McpError::internal_error(
                        "modern tools/call dispatch selected another core request",
                    ));
                };
                let binding = final_mrtr_binding(
                    request_ctx,
                    "tools/call",
                    params.name.clone(),
                    &params.arguments,
                )?;
                match self.resolve_final_mrtr_retry(
                    params.request_state.as_deref(),
                    params.input_responses.as_ref(),
                    binding.as_ref(),
                )? {
                    FinalMrtrDispatch::InputRequired(result) => result,
                    FinalMrtrDispatch::Fresh => {
                        self.dispatch_final_tools_call(
                            request_ctx,
                            request_cx,
                            params,
                            binding,
                            None,
                            continuation_cancellation,
                        )
                        .await?
                    }
                    FinalMrtrDispatch::Resume(resume_inputs) => {
                        self.dispatch_final_tools_call(
                            request_ctx,
                            request_cx,
                            params,
                            binding,
                            Some(resume_inputs),
                            continuation_cancellation,
                        )
                        .await?
                    }
                }
            }
            "resources/list" => {
                let request =
                    CoreRequest::decode(ProtocolEra::Modern2026, "resources/list", params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::ResourcesList(params)) = &request else {
                    return Err(McpError::internal_error(
                        "modern resources/list dispatch selected another core request",
                    ));
                };
                encode_final_core_result(
                    self.handle_final_resources_list(request_ctx, params.clone()),
                    |result| FinalCoreResult::ResourcesList {
                        result,
                        diagnostic: None,
                    },
                )?
            }
            "resources/templates/list" => {
                let request = CoreRequest::decode(
                    ProtocolEra::Modern2026,
                    "resources/templates/list",
                    params,
                )
                .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::ResourceTemplatesList(params)) = &request
                else {
                    return Err(McpError::internal_error(
                        "modern resources/templates/list dispatch selected another core request",
                    ));
                };
                encode_final_core_result(
                    self.handle_final_resource_templates_list(request_ctx, params.clone()),
                    |result| FinalCoreResult::ResourceTemplatesList {
                        result,
                        diagnostic: None,
                    },
                )?
            }
            "resources/read" => {
                self.admit_final_mrtr_response_map(params)?;
                let request = CoreRequest::decode_with_raw_params(
                    ProtocolEra::Modern2026,
                    "resources/read",
                    params,
                    raw_params,
                )
                .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::ResourcesRead(params)) = &request else {
                    return Err(McpError::internal_error(
                        "modern resources/read dispatch selected another core request",
                    ));
                };
                let binding = final_mrtr_binding(
                    request_ctx,
                    "resources/read",
                    params.uri.as_str().to_owned(),
                    &(),
                )?;
                match self.resolve_final_mrtr_retry(
                    params.request_state.as_deref(),
                    params.input_responses.as_ref(),
                    binding.as_ref(),
                )? {
                    FinalMrtrDispatch::InputRequired(result) => result,
                    FinalMrtrDispatch::Fresh => {
                        self.dispatch_final_resources_read(
                            request_ctx,
                            request_cx,
                            params.clone(),
                            binding,
                            None,
                            continuation_cancellation,
                        )
                        .await?
                    }
                    FinalMrtrDispatch::Resume(resume_inputs) => {
                        self.dispatch_final_resources_read(
                            request_ctx,
                            request_cx,
                            params.clone(),
                            binding,
                            Some(resume_inputs),
                            continuation_cancellation,
                        )
                        .await?
                    }
                }
            }
            "prompts/list" => {
                let request = CoreRequest::decode(ProtocolEra::Modern2026, "prompts/list", params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::PromptsList(params)) = &request else {
                    return Err(McpError::internal_error(
                        "modern prompts/list dispatch selected another core request",
                    ));
                };
                encode_final_core_result(
                    self.handle_final_prompts_list(request_ctx, params.clone()),
                    |result| FinalCoreResult::PromptsList {
                        result,
                        diagnostic: None,
                    },
                )?
            }
            "prompts/get" => {
                self.admit_final_mrtr_response_map(params)?;
                let request = CoreRequest::decode_with_raw_params(
                    ProtocolEra::Modern2026,
                    "prompts/get",
                    params,
                    raw_params,
                )
                .map_err(|error| McpError::invalid_params(error.to_string()))?;
                let CoreRequest::Final(FinalCoreRequest::PromptsGet(params)) = request else {
                    return Err(McpError::internal_error(
                        "modern prompts/get dispatch selected another core request",
                    ));
                };
                let binding = final_mrtr_binding(
                    request_ctx,
                    "prompts/get",
                    params.name.clone(),
                    &params.arguments,
                )?;
                match self.resolve_final_mrtr_retry(
                    params.request_state.as_deref(),
                    params.input_responses.as_ref(),
                    binding.as_ref(),
                )? {
                    FinalMrtrDispatch::InputRequired(result) => result,
                    FinalMrtrDispatch::Fresh => {
                        self.dispatch_final_prompts_get(
                            request_ctx,
                            request_cx,
                            params,
                            binding,
                            None,
                            continuation_cancellation,
                        )
                        .await?
                    }
                    FinalMrtrDispatch::Resume(resume_inputs) => {
                        self.dispatch_final_prompts_get(
                            request_ctx,
                            request_cx,
                            params,
                            binding,
                            Some(resume_inputs),
                            continuation_cancellation,
                        )
                        .await?
                    }
                }
            }
            _ => return Err(McpError::method_not_found(&request.method)),
        };

        if request_cx_cancellation_is_visible(request_cx) {
            return Err(McpError::request_cancelled());
        }
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        Ok(result)
    }

    /// Admits bounded raw retry values before final parameter decoding clones
    /// them into method-specific fields or materializes `inputResponses`.
    fn admit_final_mrtr_response_map(&self, params: Option<&serde_json::Value>) -> McpResult<()> {
        let Some(params) = params else {
            return Ok(());
        };
        admit_mrtr_raw_json_value(params, MAX_MRTR_RAW_PARAMS_BYTES)?;
        let Some(input_responses) = params
            .as_object()
            .and_then(|members| members.get("inputResponses"))
        else {
            return Ok(());
        };
        admit_mrtr_raw_json_value(input_responses, MAX_MRTR_RAW_INPUT_RESPONSES_BYTES)?;
        let Some(input_responses) = input_responses.as_object() else {
            return Ok(());
        };
        if input_responses.len() > self.mrtr_exchanges.max_inputs_per_round() {
            return Err(McpError::invalid_params(
                "MRTR inputResponses exceeds the configured bound",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "tasks")]
    fn admit_final_task_tool(&self, metadata: &OpenMetadata) -> McpResult<&FinalTaskRuntime> {
        require_final_tasks_capability(metadata)?;
        let runtime = self.final_task_runtime.as_ref().ok_or_else(|| {
            McpError::internal_error("task-capable tool requires an installed final Tasks runtime")
        })?;
        runtime.ensure_task_service_ready()?;
        Ok(runtime)
    }

    fn issue_final_mrtr_input_required(
        &self,
        request_ctx: &McpContext,
        continuation_cancellation: fastmcp_core::McpRequestCancellation,
        binding: MrtrExchangeBinding,
        handler_result: InputRequiredResult,
    ) -> McpResult<serde_json::Value> {
        // The handler may describe the input it needs, but it never controls
        // downstream requestState. Retain its state privately for this exact
        // conversation; only admitted resume inputs can return it to a proxy.
        // Open result siblings are not forwarded across this boundary.
        let input_requests = handler_mrtr_input_requests(request_ctx, &handler_result)?;
        let required = self.mrtr_exchanges.issue_bound(
            continuation_cancellation,
            binding,
            input_requests,
            handler_result.request_state().map(str::to_owned),
        )?;
        encode_mrtr_input_required_result(required)
    }

    /// Resolves a final embedded-input retry before its method handler runs.
    ///
    /// Only a framework-issued state whose immutable operation binding still
    /// matches may yield resume inputs. A complete retry passes those typed
    /// values into the handler's resume-aware final hook exactly once.
    fn resolve_final_mrtr_retry(
        &self,
        request_state: Option<&str>,
        input_responses: Option<&FinalInputResponses>,
        binding: Option<&MrtrExchangeBinding>,
    ) -> McpResult<FinalMrtrDispatch> {
        match (request_state, input_responses) {
            (None, None) => Ok(FinalMrtrDispatch::Fresh),
            (Some(request_state), None) => {
                let binding = binding.ok_or_else(|| {
                    McpError::invalid_params("MRTR retries require session state")
                })?;
                match self
                    .mrtr_exchanges
                    .accept_state_only_bound(request_state, binding)?
                {
                    MrtrRetry::Complete(inputs) => Ok(FinalMrtrDispatch::Resume(inputs)),
                    MrtrRetry::InputRequired(_) => Err(McpError::internal_error(
                        "state-only MRTR retry cannot issue further input requests",
                    )),
                }
            }
            (Some(request_state), Some(input_responses)) => {
                let binding = binding.ok_or_else(|| {
                    McpError::invalid_params("MRTR retries require session state")
                })?;
                match self.mrtr_exchanges.accept_final_input_responses_bound(
                    request_state,
                    binding,
                    input_responses,
                )? {
                    MrtrRetry::Complete(inputs) => Ok(FinalMrtrDispatch::Resume(inputs)),
                    MrtrRetry::InputRequired(result) => encode_mrtr_input_required_result(result)
                        .map(FinalMrtrDispatch::InputRequired),
                }
            }
            _ => Err(McpError::invalid_params(
                "final MRTR inputResponses require requestState",
            )),
        }
    }

    async fn dispatch_final_tools_call(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalCallToolParams,
        binding: Option<MrtrExchangeBinding>,
        resume_inputs: Option<MrtrCompletedInputs>,
        continuation_cancellation: &fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        #[cfg(feature = "tasks")]
        let request_metadata = params.meta.clone();
        let session_state = request_ctx
            .session_state()
            .cloned()
            .unwrap_or_else(SessionState::new);
        let outcome = self
            .handle_tools_call_final_in_request(
                request_ctx,
                request_cx,
                params,
                session_state,
                None,
                None,
                resume_inputs.as_ref(),
            )
            .await?;
        match outcome {
            FinalToolOutcome::Complete(result) => encode_final_tools_call_result(Ok(result)),
            FinalToolOutcome::InputRequired(result) => {
                let binding = binding.ok_or_else(|| {
                    McpError::internal_error(
                        "MRTR input_required requires session state to bind retries",
                    )
                })?;
                self.issue_final_mrtr_input_required(
                    request_ctx,
                    continuation_cancellation.clone(),
                    binding,
                    result,
                )
            }
            #[cfg(feature = "tasks")]
            FinalToolOutcome::CreateTask {
                work_descriptor,
                status_message,
            } => {
                #[cfg(all(feature = "proxy", feature = "tasks"))]
                if let Some(relay) = self.final_task_relay.as_ref() {
                    // A relayed task is already durable upstream. Decode its
                    // private carrier only after the downstream capability
                    // gate, retain the route-bound snapshot for controls, and
                    // emit the exact upstream handle without local creation.
                    require_final_tasks_capability(&request_metadata)?;
                    if let Some(result) = relay.admit_carried_task(&work_descriptor)? {
                        return encode_final_task_result(result);
                    }
                    return Err(McpError::internal_error(
                        "a proxy final Tasks relay received a non-relayed CreateTask outcome",
                    ));
                }
                // A handler's declaration means it may return CreateTask; it
                // does not turn its Complete or InputRequired outcomes into
                // Tasks operations. Admit only the branch that can mutate the
                // Tasks store, immediately before that mutation.
                let runtime = self.admit_final_task_tool(&request_metadata)?;
                encode_final_task_result(runtime.create_task_for_request(
                    request_ctx,
                    work_descriptor,
                    status_message,
                )?)
            }
        }
    }

    async fn dispatch_final_resources_read(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalReadResourceParams,
        binding: Option<MrtrExchangeBinding>,
        resume_inputs: Option<MrtrCompletedInputs>,
        continuation_cancellation: &fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        let session_state = request_ctx
            .session_state()
            .cloned()
            .unwrap_or_else(SessionState::new);
        match self
            .handle_resources_read_final_in_request(
                request_ctx,
                request_cx,
                params,
                session_state,
                None,
                None,
                resume_inputs.as_ref(),
            )
            .await?
        {
            FinalMethodOutcome::Complete(result) => encode_final_resources_read_result(Ok(result)),
            FinalMethodOutcome::InputRequired(result) => {
                let binding = binding.ok_or_else(|| {
                    McpError::internal_error(
                        "MRTR input_required requires session state to bind retries",
                    )
                })?;
                self.issue_final_mrtr_input_required(
                    request_ctx,
                    continuation_cancellation.clone(),
                    binding,
                    result,
                )
            }
        }
    }

    async fn dispatch_final_prompts_get(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalGetPromptParams,
        binding: Option<MrtrExchangeBinding>,
        resume_inputs: Option<MrtrCompletedInputs>,
        continuation_cancellation: &fastmcp_core::McpRequestCancellation,
    ) -> McpResult<serde_json::Value> {
        let session_state = request_ctx
            .session_state()
            .cloned()
            .unwrap_or_else(SessionState::new);
        match self
            .handle_prompts_get_final_in_request(
                request_ctx,
                request_cx,
                params,
                session_state,
                None,
                None,
                resume_inputs.as_ref(),
            )
            .await?
        {
            FinalMethodOutcome::Complete(result) => encode_final_prompts_get_result(Ok(result)),
            FinalMethodOutcome::InputRequired(result) => {
                let binding = binding.ok_or_else(|| {
                    McpError::internal_error(
                        "MRTR input_required requires session state to bind retries",
                    )
                })?;
                self.issue_final_mrtr_input_required(
                    request_ctx,
                    continuation_cancellation.clone(),
                    binding,
                    result,
                )
            }
        }
    }

    /// Handles the tools/list request.
    ///
    /// If session_state is provided, disabled tools will be filtered out.
    /// If include_tags/exclude_tags are provided, tools are filtered by tags.
    pub fn handle_tools_list(
        &self,
        request_ctx: &McpContext,
        params: ListToolsParams,
        session_state: Option<&SessionState>,
    ) -> McpResult<ListToolsResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let tag_filters =
            TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let tag_filters = if params.include_tags.is_some() || params.exclude_tags.is_some() {
            Some(&tag_filters)
        } else {
            None
        };
        let tools =
            crate::catch_extension_unwind(|| self.tools_filtered(session_state, tag_filters))
                .map_err(|_payload| sanitized_handler_panic(request_ctx.cx(), "tool_definition"))?;
        let Some(page_size) = self.list_page_size else {
            return Ok(ListToolsResult {
                tools,
                next_cursor: None,
            });
        };
        let query = FinalCatalogQuery::from_tag_filters(
            params.include_tags.as_deref(),
            params.exclude_tags.as_deref(),
        );
        let (tools, next_cursor) = page_session_filtered_catalog(
            tools,
            params.cursor.as_deref(),
            Some(page_size),
            FinalCatalogKind::Tools,
            self.final_catalog_revision,
            &query,
            session_state.is_some(),
            |tool| tool.name.as_str(),
        )?;
        Ok(ListToolsResult { tools, next_cursor })
    }

    /// Handles a final tools/list request using only final-admitted entries.
    /// Normal registration admits schemas before committing its handler, so a
    /// malformed candidate cannot influence a modern catalog page or cursor.
    fn handle_final_tools_list(
        &self,
        request_ctx: &McpContext,
        params: FinalListParams,
    ) -> McpResult<FinalListToolsResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let query = FinalCatalogQuery::from_final_list_params(&params);
        let tag_filters =
            TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let tag_filters = if params.include_tags.is_some() || params.exclude_tags.is_some() {
            Some(&tag_filters)
        } else {
            None
        };
        let tools = crate::catch_extension_unwind(|| {
            self.tool_order
                .iter()
                .filter_map(|name| self.tools.get(name))
                .filter(|entry| entry.final_registration.is_some())
                .map(|entry| entry.definition.clone())
                .filter(|tool| tag_filters.is_none_or(|filters| filters.matches(&tool.tags)))
                .collect::<Vec<_>>()
        })
        .map_err(|_payload| sanitized_handler_panic(request_ctx.cx(), "tool_definition"))?;

        let (tools, next_cursor) = page_final_catalog(
            tools,
            params.cursor.as_deref(),
            self.list_page_size,
            FinalCatalogKind::Tools,
            self.final_catalog_revision,
            &query,
            None,
        )?;
        let result = ListToolsResult { tools, next_cursor };
        self.project_final_tools_list(request_ctx, result, self.final_cache_hints.clone())
    }

    fn project_final_tools_list(
        &self,
        _request_ctx: &McpContext,
        result: ListToolsResult,
        cache_hints: FinalCacheHintPolicy,
    ) -> McpResult<FinalListToolsResult> {
        let tools = result
            .tools
            .into_iter()
            .map(|tool| {
                let entry = self.tools.get(&tool.name).ok_or_else(|| {
                    McpError::internal_error("listed tool is absent from the router catalog")
                })?;
                let final_registration = entry.final_registration.as_ref().ok_or_else(|| {
                    McpError::internal_error(
                        "legacy-only tool reached the final catalog projection",
                    )
                })?;

                Ok(final_registration.final_definition.clone())
            })
            .collect::<McpResult<Vec<_>>>()?;
        Ok(FinalListToolsResult {
            tools,
            next_cursor: result.next_cursor,
            ttl_ms: cache_hints.list_ttl_ms,
            cache_scope: cache_hints.scope,
        })
    }

    fn handle_final_resources_list(
        &self,
        request_ctx: &McpContext,
        params: FinalListParams,
    ) -> McpResult<FinalListResourcesResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        let query = FinalCatalogQuery::from_final_list_params(&params);
        let filters = TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let filters =
            (params.include_tags.is_some() || params.exclude_tags.is_some()).then_some(filters);
        let resources = self
            .resource_order
            .iter()
            .filter_map(|uri| self.final_resources.get(uri).map(|entry| (uri, entry)))
            .filter(|(_, entry)| {
                filters
                    .as_ref()
                    .is_none_or(|filters| filters.matches(&entry.tags))
            })
            .map(|(_, entry)| {
                admit_final_resource_uri(
                    entry.uri_use_policy,
                    &entry.definition.uri,
                    FinalResourceUriUse::CatalogResource,
                )?;
                Ok(entry.definition.clone())
            })
            .collect::<McpResult<Vec<_>>>()?;
        let (resources, next_cursor) = page_final_catalog(
            resources,
            params.cursor.as_deref(),
            self.list_page_size,
            FinalCatalogKind::Resources,
            self.final_catalog_revision,
            &query,
            None,
        )?;
        Ok(FinalListResourcesResult {
            resources,
            next_cursor,
            ttl_ms: self.final_cache_hints.list_ttl_ms.clone(),
            cache_scope: self.final_cache_hints.scope,
        })
    }

    fn handle_final_resource_templates_list(
        &self,
        request_ctx: &McpContext,
        params: FinalListParams,
    ) -> McpResult<FinalListResourceTemplatesResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        let query = FinalCatalogQuery::from_final_list_params(&params);
        let filters = TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let filters =
            (params.include_tags.is_some() || params.exclude_tags.is_some()).then_some(filters);
        let resource_templates = self
            .resource_template_order
            .iter()
            .filter_map(|key| self.resource_templates.get(key).map(|entry| (key, entry)))
            .filter_map(|(_, entry)| {
                entry
                    .final_definition
                    .as_ref()
                    .map(|definition| (definition, &entry.template.tags, entry.uri_use_policy))
            })
            .filter(|(_, tags, _)| filters.as_ref().is_none_or(|filters| filters.matches(tags)))
            .map(|(definition, _, uri_use_policy)| {
                admit_final_resource_template_uri(uri_use_policy, &definition.uri_template)?;
                Ok(definition.clone())
            })
            .collect::<McpResult<Vec<_>>>()?;
        let (resource_templates, next_cursor) = page_final_catalog(
            resource_templates,
            params.cursor.as_deref(),
            self.list_page_size,
            FinalCatalogKind::ResourceTemplates,
            self.final_catalog_revision,
            &query,
            None,
        )?;
        Ok(FinalListResourceTemplatesResult {
            resource_templates,
            next_cursor,
            ttl_ms: self.final_cache_hints.list_ttl_ms.clone(),
            cache_scope: self.final_cache_hints.scope,
        })
    }

    fn handle_final_prompts_list(
        &self,
        request_ctx: &McpContext,
        params: FinalListParams,
    ) -> McpResult<FinalListPromptsResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        let query = FinalCatalogQuery::from_final_list_params(&params);
        let filters = TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let filters =
            (params.include_tags.is_some() || params.exclude_tags.is_some()).then_some(filters);
        let prompts = self
            .prompt_order
            .iter()
            .filter_map(|name| self.final_prompts.get(name).map(|entry| (name, entry)))
            .filter(|(_, entry)| {
                filters
                    .as_ref()
                    .is_none_or(|filters| filters.matches(&entry.tags))
            })
            .map(|(_, entry)| entry.definition.clone())
            .collect();
        let (prompts, next_cursor) = page_final_catalog(
            prompts,
            params.cursor.as_deref(),
            self.list_page_size,
            FinalCatalogKind::Prompts,
            self.final_catalog_revision,
            &query,
            None,
        )?;
        Ok(FinalListPromptsResult {
            prompts,
            next_cursor,
            ttl_ms: self.final_cache_hints.list_ttl_ms.clone(),
            cache_scope: self.final_cache_hints.scope,
        })
    }

    /// Handles the tools/call request.
    ///
    /// # Arguments
    ///
    /// * `request_ctx` - Request authority for cancellation, identity, auth, and accounting
    /// * `params` - The tool call parameters including tool name and arguments
    /// * `session_state` - Session state for per-session storage
    /// * `notification_sender` - Optional callback for sending progress notifications
    /// * `bidirectional_senders` - Optional senders for sampling/elicitation
    pub async fn handle_tools_call(
        &self,
        request_ctx: &McpContext,
        params: CallToolParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<CallToolResult> {
        self.handle_tools_call_in_request(
            request_ctx,
            request_ctx.cx(),
            params,
            session_state,
            notification_sender,
            bidirectional_senders,
        )
        .await
    }

    pub(crate) async fn handle_tools_call_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: CallToolParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<CallToolResult> {
        let result = self
            .handle_application_tools_call_in_request(
                request_ctx,
                request_cx,
                params,
                session_state,
                notification_sender,
                bidirectional_senders,
            )
            .await?;
        Ok(CallToolResult {
            content: legacy_contents_from_handler(result.content)?,
            is_error: result.is_error,
            meta: None,
            additional: BTreeMap::new(),
        })
    }

    pub(crate) async fn handle_application_tools_call_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: CallToolParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<ApplicationToolResult> {
        debug!(
            target: targets::HANDLER,
            "calling modern tool; tool_key={}; arguments_present={}",
            safe_log_label(&params.name),
            params.arguments.is_some()
        );

        let dispatch_started_at = request_ctx.cx().now();
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        if !session_state.is_tool_enabled(&params.name) {
            return Err(McpError::new(
                McpErrorCode::MethodNotFound,
                format!("Tool '{}' is disabled for this session", params.name),
            ));
        }

        let entry = self
            .tools
            .get(&params.name)
            .ok_or_else(|| McpError::method_not_found(&format!("tool: {}", params.name)))?;
        if !entry.legacy_enabled {
            return Err(McpError::method_not_found(&format!(
                "tool: {}",
                params.name
            )));
        }
        let handler = &entry.handler;
        let arguments = params.arguments.unwrap_or_else(|| serde_json::json!({}));
        let validation_result = if self.strict_input_validation {
            validate_strict(&entry.definition.input_schema, &arguments)
        } else {
            validate(&entry.definition.input_schema, &arguments)
        };
        if let Err(validation_errors) = validation_result {
            let error_messages: Vec<String> = validation_errors
                .iter()
                .map(|error| format!("{}: {}", error.path, error.message))
                .collect();
            return Err(McpError::invalid_params(format!(
                "Input validation failed: {}",
                error_messages.join("; ")
            )));
        }

        let progress_marker = params
            .meta
            .as_ref()
            .and_then(|meta| meta.progress_marker.clone());
        let ctx = derive_handler_context(
            request_ctx,
            progress_marker,
            notification_sender,
            bidirectional_senders,
            ProtocolEra::Legacy2024,
        );
        let handler_timeout =
            read_handler_timeout(request_ctx.cx(), "tool_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let ctx = ctx.with_operation_deadline(effective_budget.deadline);
        let outcome =
            run_handler_in_request(&ctx, request_cx, effective_budget, "tool", |child_cx| {
                handler.call_async_in_request(&ctx, child_cx, arguments)
            })
            .await?;

        match outcome {
            Outcome::Ok(content) => Ok(ApplicationToolResult {
                content,
                is_error: false,
            }),
            Outcome::Err(error) => {
                let error = sanitize_handler_error(request_ctx.cx(), "tool", error);
                if is_framework_terminal_tool_error(error.code) {
                    return Err(error);
                }
                Ok(ApplicationToolResult {
                    content: vec![Content::Text {
                        text: error.message,
                    }],
                    is_error: true,
                })
            }
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => Err(sanitized_handler_panic(request_ctx.cx(), "tool")),
        }
    }

    /// Handles one final MCP 2026-07-28 `tools/call` request.
    ///
    /// Legacy dispatch remains on [`Self::handle_tools_call`], including its
    /// exact `CallToolResult` behavior. Final dispatch calls the final handler
    /// hook directly and encodes the returned complete result with the typed
    /// core result codec.
    async fn handle_tools_call_final_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalCallToolParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
        resume_inputs: Option<&MrtrCompletedInputs>,
    ) -> McpResult<FinalToolOutcome> {
        debug!(
            target: targets::HANDLER,
            "calling final tool; tool_key={}; arguments_present={}",
            safe_log_label(&params.name),
            !params.arguments.is_absent()
        );

        let dispatch_started_at = request_ctx.cx().now();
        if request_cx_cancellation_is_visible(request_cx) {
            return Err(McpError::request_cancelled());
        }
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        let progress_marker = final_progress_marker(&params.meta)?;
        // Modern connection state is attached to `request_ctx`. Stateless
        // callers still supply the explicit `session_state`, so both views
        // must admit the component before a final handler (or an MRTR state)
        // can be reached.
        if !session_state.is_tool_enabled(&params.name)
            || !request_ctx.is_tool_enabled(&params.name)
        {
            return Err(McpError::new(
                McpErrorCode::MethodNotFound,
                format!("Tool '{}' is disabled for this session", params.name),
            ));
        }

        let entry = self
            .tools
            .get(&params.name)
            .ok_or_else(|| McpError::invalid_params(format!("Unknown tool: {}", params.name)))?;
        let final_registration = entry
            .final_registration
            .as_ref()
            .ok_or_else(|| McpError::invalid_params(format!("Unknown tool: {}", params.name)))?;
        let handler = &entry.handler;
        if handler.declares_final_mrtr() && request_ctx.retained_continuation_partition().is_none()
        {
            return Err(McpError::invalid_params(
                MRTR_REQUIRES_BOUND_MODERN_CONNECTION,
            ));
        }
        let input_schema = final_registration.schemas.input.as_ref();
        let output_schema = final_registration.schemas.output.as_ref();
        // FinalArguments deserialization rejects an explicit null before this
        // point, so `arguments` is here always Absent or a typed value.
        #[cfg(feature = "tasks")]
        let declares_final_tasks = final_registration.declares_final_tasks;
        let arguments = params
            .arguments
            .into_value()
            .unwrap_or_else(|| serde_json::json!({}));
        // HTTP-05: recognized `Mcp-Param-*` mirrors are compared after the
        // tool is resolved, so unknown and disabled tools keep their own
        // errors, and before argument validation or any handler effect.
        if let Some(headers) = request_ctx.http_parameter_headers() {
            crate::http_admission::validate_tool_parameter_headers(
                &final_registration.final_definition.input_schema,
                &arguments,
                headers,
            )?;
        }
        let input_validation_failed = match input_schema {
            Some(schema) => {
                let validation = if self.strict_input_validation {
                    validate_strict(schema.schema(), &arguments)
                } else {
                    schema.validate(&arguments)
                };
                validation.is_err()
            }
            None if self.strict_input_validation => {
                // Upstream/proxy tools skip local schema admission so a valid
                // non-object catalog schema can be retained without inventing
                // an error payload. Gateway strict mode still refuses
                // additionalProperties against that catalog input schema.
                validate_strict(
                    &final_registration.final_definition.input_schema,
                    &arguments,
                )
                .is_err()
            }
            None => false,
        };
        if input_validation_failed {
            let mut result = crate::handler::promote_legacy_tool_content(vec![Content::text(
                "Tool arguments do not match the declared input schema.",
            )])?;
            result.payload.is_error = true;
            result.payload.structured_content = final_registration
                .schemas
                .errors
                .as_ref()
                .map(|errors| errors.input_validation.clone());
            return Ok(FinalToolOutcome::Complete(result));
        }

        let ctx = derive_handler_context(
            request_ctx,
            progress_marker,
            notification_sender,
            bidirectional_senders,
            ProtocolEra::Modern2026,
        );
        let handler_timeout =
            read_handler_timeout(request_ctx.cx(), "tool_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let ctx = ctx.with_operation_deadline(effective_budget.deadline);
        let outcome =
            run_handler_in_request(&ctx, request_cx, effective_budget, "tool", |child_cx| {
                // MRTR-aware handlers receive every call through the resuming
                // hook: None marks the initial invocation, Some the admitted
                // retry. The default resuming hook forwards to the plain
                // final hook, so MRTR-unaware handlers are unaffected.
                handler.call_final_outcome_async_resuming_in_request(
                    &ctx,
                    child_cx,
                    arguments,
                    resume_inputs,
                )
            })
            .await?;

        match outcome {
            Outcome::Ok(result) => {
                match &result {
                    FinalToolOutcome::Complete(result) => {
                        if let Some(output_schema) = output_schema {
                            let structured_content = result
                                .payload
                                .structured_content
                                .as_ref()
                                .ok_or_else(|| {
                                    McpError::internal_error(
                                        "tool output is missing structuredContent required by the declared output schema",
                                    )
                                })?;
                            if output_schema.validate(structured_content).is_err() {
                                return Err(McpError::internal_error(
                                    "tool output does not match the declared output schema",
                                ));
                            }
                        }
                    }
                    #[cfg(feature = "tasks")]
                    FinalToolOutcome::CreateTask { .. } if !declares_final_tasks => {
                        return Err(McpError::invalid_request(
                            "tool returned CreateTask without declaring final Tasks capability",
                        ));
                    }
                    FinalToolOutcome::InputRequired(_) => {}
                    #[cfg(feature = "tasks")]
                    FinalToolOutcome::CreateTask { .. } => {}
                }
                Ok(result)
            }
            Outcome::Err(error) => {
                let error = sanitize_handler_error(request_ctx.cx(), "tool", error);
                if is_framework_terminal_final_tool_error(&error) {
                    return Err(error);
                }
                let mut result =
                    crate::handler::promote_legacy_tool_content(vec![Content::Text {
                        text: error.message,
                    }])?;
                result.payload.is_error = true;
                result.payload.structured_content = final_registration
                    .schemas
                    .errors
                    .as_ref()
                    .map(|errors| errors.handler.clone());
                Ok(FinalToolOutcome::Complete(result))
            }
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => Err(sanitized_handler_panic(request_ctx.cx(), "tool")),
        }
    }

    /// Handles the resources/list request.
    ///
    /// If session_state is provided, disabled resources will be filtered out.
    /// If include_tags/exclude_tags are provided, resources are filtered by tags.
    pub fn handle_resources_list(
        &self,
        request_ctx: &McpContext,
        params: ListResourcesParams,
        session_state: Option<&SessionState>,
    ) -> McpResult<ListResourcesResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let tag_filters =
            TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let tag_filters = if params.include_tags.is_some() || params.exclude_tags.is_some() {
            Some(&tag_filters)
        } else {
            None
        };
        let resources = crate::catch_extension_unwind(|| {
            self.resources_filtered(session_state, tag_filters)
        })
        .map_err(|_payload| sanitized_handler_panic(request_ctx.cx(), "resource_definition"))?;
        let Some(page_size) = self.list_page_size else {
            return Ok(ListResourcesResult {
                resources,
                next_cursor: None,
            });
        };
        let query = FinalCatalogQuery::from_tag_filters(
            params.include_tags.as_deref(),
            params.exclude_tags.as_deref(),
        );
        let (resources, next_cursor) = page_session_filtered_catalog(
            resources,
            params.cursor.as_deref(),
            Some(page_size),
            FinalCatalogKind::Resources,
            self.final_catalog_revision,
            &query,
            session_state.is_some(),
            |resource| resource.uri.as_str(),
        )?;
        Ok(ListResourcesResult {
            resources,
            next_cursor,
        })
    }

    /// Handles the resources/templates/list request.
    ///
    /// If session_state is provided, disabled resource templates will be filtered out.
    /// If include_tags/exclude_tags are provided, templates are filtered by tags.
    pub fn handle_resource_templates_list(
        &self,
        request_ctx: &McpContext,
        params: ListResourceTemplatesParams,
        session_state: Option<&SessionState>,
    ) -> McpResult<ListResourceTemplatesResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let tag_filters =
            TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let tag_filters = if params.include_tags.is_some() || params.exclude_tags.is_some() {
            Some(&tag_filters)
        } else {
            None
        };
        let templates = self.resource_templates_filtered(session_state, tag_filters);
        let Some(page_size) = self.list_page_size else {
            return Ok(ListResourceTemplatesResult {
                resource_templates: templates,
                next_cursor: None,
            });
        };
        let query = FinalCatalogQuery::from_tag_filters(
            params.include_tags.as_deref(),
            params.exclude_tags.as_deref(),
        );
        let (resource_templates, next_cursor) = page_session_filtered_catalog(
            templates,
            params.cursor.as_deref(),
            Some(page_size),
            FinalCatalogKind::ResourceTemplates,
            self.final_catalog_revision,
            &query,
            session_state.is_some(),
            |template| template.uri_template.as_str(),
        )?;
        Ok(ListResourceTemplatesResult {
            resource_templates,
            next_cursor,
        })
    }

    /// Handles the resources/read request.
    ///
    /// # Arguments
    ///
    /// * `request_ctx` - Request authority for cancellation, identity, auth, and accounting
    /// * `params` - The resource read parameters including URI
    /// * `session_state` - Session state for per-session storage
    /// * `notification_sender` - Optional callback for sending progress notifications
    /// * `bidirectional_senders` - Optional senders for sampling/elicitation
    pub async fn handle_resources_read(
        &self,
        request_ctx: &McpContext,
        params: &ReadResourceParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<ReadResourceResult> {
        self.handle_resources_read_in_request(
            request_ctx,
            request_ctx.cx(),
            params,
            session_state,
            notification_sender,
            bidirectional_senders,
        )
        .await
    }

    pub(crate) async fn handle_resources_read_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: &ReadResourceParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<ReadResourceResult> {
        debug!(
            target: targets::HANDLER,
            "reading modern resource; resource_key={}",
            safe_log_label(&params.uri)
        );

        let dispatch_started_at = request_ctx.cx().now();
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        if !session_state.is_resource_enabled(&params.uri) {
            return Err(McpError::new(
                McpErrorCode::ResourceNotFound,
                format!("Resource '{}' is disabled for this session", params.uri),
            ));
        }

        let resolved = self
            .resolve_resource_for_era(&params.uri, Some(ProtocolEra::Legacy2024))
            .ok_or_else(|| McpError::resource_not_found(&params.uri))?;
        if !resolved.legacy_enabled {
            return Err(McpError::resource_not_found(&params.uri));
        }
        let progress_marker = params
            .meta
            .as_ref()
            .and_then(|meta| meta.progress_marker.clone());
        let ctx = derive_handler_context(
            request_ctx,
            progress_marker,
            notification_sender,
            bidirectional_senders,
            ProtocolEra::Legacy2024,
        );
        let handler_timeout = read_handler_timeout(request_ctx.cx(), "resource_timeout", || {
            resolved.handler.timeout()
        })?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let ctx = ctx.with_operation_deadline(effective_budget.deadline);
        let outcome =
            run_handler_in_request(&ctx, request_cx, effective_budget, "resource", |child_cx| {
                resolved.handler.read_async_with_uri_in_request(
                    &ctx,
                    child_cx,
                    &params.uri,
                    &resolved.params,
                )
            })
            .await?;

        let contents = match outcome {
            Outcome::Ok(contents) => contents,
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(request_ctx.cx(), "resource", error));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(request_ctx.cx(), "resource"));
            }
        };

        Ok(ReadResourceResult {
            contents: legacy_resource_contents_from_handler(contents)?,
            meta: None,
            additional: BTreeMap::new(),
        })
    }

    /// Handles one final MCP 2026-07-28 `resources/read` request.
    ///
    /// Legacy dispatch remains on [`Self::handle_resources_read`], including
    /// its exact `ReadResourceResult` shape. Final dispatch calls the final
    /// handler hook directly so embedded resource metadata, open fields, and
    /// cache hints are not projected through the legacy resource surface.
    async fn handle_resources_read_final_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalReadResourceParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
        resume_inputs: Option<&MrtrCompletedInputs>,
    ) -> McpResult<FinalMethodOutcome<FinalReadResourceResult>> {
        let progress_marker = final_progress_marker(&params.meta)?;
        let uri = params.uri.as_str();
        debug!(
            target: targets::HANDLER,
            "reading final resource; resource_key={}",
            safe_log_label(uri)
        );

        let dispatch_started_at = request_ctx.cx().now();
        if request_cx_cancellation_is_visible(request_cx) {
            return Err(McpError::request_cancelled());
        }
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        // See the corresponding final tools/call check: a modern connection's
        // durable component state lives on the request context.
        if !session_state.is_resource_enabled(uri) || !request_ctx.is_resource_enabled(uri) {
            return Err(McpError::new(
                McpErrorCode::ResourceNotFound,
                format!("Resource '{uri}' is disabled for this session"),
            ));
        }

        let resolved = self
            .resolve_resource_for_era(uri, Some(ProtocolEra::Modern2026))
            .ok_or_else(|| {
                McpError::with_data(
                    McpErrorCode::InvalidParams,
                    "Resource not found",
                    serde_json::json!({"uri": uri}),
                )
            })?;
        if !resolved.final_enabled {
            return Err(McpError::invalid_params(
                "resource is registered only for exact MCP 2024-11-05 dispatch",
            ));
        }
        if resolved.handler.declares_final_mrtr()
            && request_ctx.retained_continuation_partition().is_none()
        {
            return Err(McpError::invalid_params(
                MRTR_REQUIRES_BOUND_MODERN_CONNECTION,
            ));
        }
        admit_final_resource_uri(
            resolved.uri_use_policy,
            &params.uri,
            FinalResourceUriUse::ResourceReadTarget,
        )?;
        let cache_hint_provenance = crate::catch_extension_unwind(|| {
            resolved.handler.final_resource_read_cache_hint_provenance()
        })
        .map_err(|_payload| {
            McpError::internal_error("resource cache-hint provenance hook panicked during dispatch")
        })?;
        let ctx = derive_handler_context(
            request_ctx,
            progress_marker,
            notification_sender,
            bidirectional_senders,
            ProtocolEra::Modern2026,
        );
        let handler_timeout = read_handler_timeout(request_ctx.cx(), "resource_timeout", || {
            resolved.handler.timeout()
        })?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let ctx = ctx.with_operation_deadline(effective_budget.deadline);
        let outcome =
            run_handler_in_request(&ctx, request_cx, effective_budget, "resource", |child_cx| {
                if let Some(resume_inputs) = resume_inputs {
                    resolved
                        .handler
                        .read_final_outcome_async_with_uri_resuming_in_request(
                            &ctx,
                            child_cx,
                            uri,
                            &resolved.params,
                            Some(resume_inputs),
                        )
                } else {
                    resolved
                        .handler
                        .read_final_outcome_async_with_uri_in_request(
                            &ctx,
                            child_cx,
                            uri,
                            &resolved.params,
                        )
                }
            })
            .await?;

        match outcome {
            Outcome::Ok(mut result) => {
                admit_final_resource_read_outcome(resolved.uri_use_policy, &result)?;
                // Provenance, not equality with a wire value, determines
                // whether router policy owns these hints. An explicit final
                // handler may intentionally choose the same values as the
                // legacy bridge and must still retain them unchanged.
                if let FinalMethodOutcome::Complete(complete) = &mut result
                    && cache_hint_provenance == FinalResourceReadCacheHintProvenance::RouterPolicy
                {
                    complete.payload.ttl_ms = self.final_cache_hints.resource_read_ttl_ms.clone();
                    complete.payload.cache_scope = self.final_cache_hints.scope;
                }
                Ok(result)
            }
            Outcome::Err(error) => Err(sanitize_handler_error(request_ctx.cx(), "resource", error)),
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                Err(sanitized_handler_panic(request_ctx.cx(), "resource"))
            }
        }
    }

    /// Handles the prompts/list request.
    ///
    /// If session_state is provided, disabled prompts will be filtered out.
    /// If include_tags/exclude_tags are provided, prompts are filtered by tags.
    pub fn handle_prompts_list(
        &self,
        request_ctx: &McpContext,
        params: ListPromptsParams,
        session_state: Option<&SessionState>,
    ) -> McpResult<ListPromptsResult> {
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }

        let tag_filters =
            TagFilters::new(params.include_tags.as_ref(), params.exclude_tags.as_ref());
        let tag_filters = if params.include_tags.is_some() || params.exclude_tags.is_some() {
            Some(&tag_filters)
        } else {
            None
        };
        let prompts =
            crate::catch_extension_unwind(|| self.prompts_filtered(session_state, tag_filters))
                .map_err(|_payload| {
                    sanitized_handler_panic(request_ctx.cx(), "prompt_definition")
                })?;
        let Some(page_size) = self.list_page_size else {
            return Ok(ListPromptsResult {
                prompts,
                next_cursor: None,
            });
        };
        let query = FinalCatalogQuery::from_tag_filters(
            params.include_tags.as_deref(),
            params.exclude_tags.as_deref(),
        );
        let (prompts, next_cursor) = page_session_filtered_catalog(
            prompts,
            params.cursor.as_deref(),
            Some(page_size),
            FinalCatalogKind::Prompts,
            self.final_catalog_revision,
            &query,
            session_state.is_some(),
            |prompt| prompt.name.as_str(),
        )?;
        Ok(ListPromptsResult {
            prompts,
            next_cursor,
        })
    }

    /// Handles the prompts/get request.
    ///
    /// # Arguments
    ///
    /// * `request_ctx` - Request authority for cancellation, identity, auth, and accounting
    /// * `params` - The prompt get parameters including name and arguments
    /// * `session_state` - Session state for per-session storage
    /// * `notification_sender` - Optional callback for sending progress notifications
    /// * `bidirectional_senders` - Optional senders for sampling/elicitation
    pub async fn handle_prompts_get(
        &self,
        request_ctx: &McpContext,
        params: GetPromptParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<GetPromptResult> {
        self.handle_prompts_get_in_request(
            request_ctx,
            request_ctx.cx(),
            params,
            session_state,
            notification_sender,
            bidirectional_senders,
        )
        .await
    }

    pub(crate) async fn handle_prompts_get_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: GetPromptParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
    ) -> McpResult<GetPromptResult> {
        debug!(
            target: targets::HANDLER,
            "getting modern prompt; prompt_key={}; arguments_present={}",
            safe_log_label(&params.name),
            params.arguments.is_some()
        );

        let dispatch_started_at = request_ctx.cx().now();
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        if !session_state.is_prompt_enabled(&params.name) {
            return Err(McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt '{}' is disabled for this session", params.name),
            ));
        }

        let handler = self.prompts.get(&params.name).ok_or_else(|| {
            McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt not found: {}", params.name),
            )
        })?;
        if self.final_only_prompts.contains(&params.name) {
            return Err(McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt not found: {}", params.name),
            ));
        }
        let description = crate::catch_extension_unwind(|| handler.definition().description)
            .map_err(|_payload| sanitized_handler_panic(request_ctx.cx(), "prompt_definition"))?;
        let progress_marker = params
            .meta
            .as_ref()
            .and_then(|meta| meta.progress_marker.clone());
        let ctx = derive_handler_context(
            request_ctx,
            progress_marker,
            notification_sender,
            bidirectional_senders,
            ProtocolEra::Legacy2024,
        );
        let handler_timeout =
            read_handler_timeout(request_ctx.cx(), "prompt_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let ctx = ctx.with_operation_deadline(effective_budget.deadline);
        let arguments = params.arguments.unwrap_or_default();
        let outcome =
            run_handler_in_request(&ctx, request_cx, effective_budget, "prompt", |child_cx| {
                handler.get_async_in_request(&ctx, child_cx, arguments)
            })
            .await?;

        let messages = match outcome {
            Outcome::Ok(messages) => messages,
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(request_ctx.cx(), "prompt", error));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(request_ctx.cx(), "prompt"));
            }
        };

        Ok(GetPromptResult {
            description,
            messages: legacy_prompt_messages_from_handler(messages)?,
            meta: None,
            additional: BTreeMap::new(),
        })
    }

    /// Handles one final MCP 2026-07-28 `prompts/get` request.
    ///
    /// Legacy dispatch remains on [`Self::handle_prompts_get`], including its
    /// exact `GetPromptResult` projection. Final dispatch calls the final
    /// handler hook directly so handler-authored final content and complete
    /// result metadata never pass through the legacy prompt surface.
    async fn handle_prompts_get_final_in_request(
        &self,
        request_ctx: &McpContext,
        request_cx: &Cx,
        params: FinalGetPromptParams,
        session_state: SessionState,
        notification_sender: Option<&NotificationSender>,
        bidirectional_senders: Option<&BidirectionalSenders>,
        resume_inputs: Option<&MrtrCompletedInputs>,
    ) -> McpResult<FinalMethodOutcome<FinalGetPromptResult>> {
        debug!(
            target: targets::HANDLER,
            "getting final prompt; prompt_key={}; arguments_present={}",
            safe_log_label(&params.name),
            !params.arguments.is_absent()
        );

        let dispatch_started_at = request_ctx.cx().now();
        if request_cx_cancellation_is_visible(request_cx) {
            return Err(McpError::request_cancelled());
        }
        if let Some(error) = budget_error(request_ctx) {
            return Err(error);
        }
        let progress_marker = final_progress_marker(&params.meta)?;
        // See the corresponding final tools/call check: a modern connection's
        // durable component state lives on the request context.
        if !session_state.is_prompt_enabled(&params.name)
            || !request_ctx.is_prompt_enabled(&params.name)
        {
            return Err(McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt '{}' is disabled for this session", params.name),
            ));
        }

        let handler = self
            .prompts
            .get(&params.name)
            .ok_or_else(|| McpError::invalid_params(format!("Unknown prompt: {}", params.name)))?;
        let final_registration = self.final_prompts.get(&params.name).ok_or_else(|| {
            McpError::invalid_params("prompt is registered only for exact MCP 2024-11-05 dispatch")
        })?;
        if handler.declares_final_mrtr() && request_ctx.retained_continuation_partition().is_none()
        {
            return Err(McpError::invalid_params(
                MRTR_REQUIRES_BOUND_MODERN_CONNECTION,
            ));
        }
        // FinalArguments deserialization rejects an explicit null before this
        // point, so `arguments` is here always Absent or a typed value.
        let arguments = params.arguments.into_value().unwrap_or_default();
        admit_declared_final_prompt_arguments(
            final_registration
                .definition
                .arguments
                .as_deref()
                .unwrap_or_default(),
            arguments.keys(),
        )?;
        let ctx = derive_handler_context(
            request_ctx,
            progress_marker,
            notification_sender,
            bidirectional_senders,
            ProtocolEra::Modern2026,
        );
        let handler_timeout =
            read_handler_timeout(request_ctx.cx(), "prompt_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            request_ctx.cx().budget(),
            request_ctx.budget(),
            handler_timeout,
            dispatch_started_at,
        );
        let ctx = ctx.with_operation_deadline(effective_budget.deadline);
        let arguments = arguments.into_iter().collect();
        let outcome =
            run_handler_in_request(&ctx, request_cx, effective_budget, "prompt", |child_cx| {
                if let Some(resume_inputs) = resume_inputs {
                    handler.get_final_outcome_async_resuming_in_request(
                        &ctx,
                        child_cx,
                        arguments,
                        Some(resume_inputs),
                    )
                } else {
                    handler.get_final_outcome_async_in_request(&ctx, child_cx, arguments)
                }
            })
            .await?;

        match outcome {
            Outcome::Ok(result) => {
                admit_final_prompt_outcome(final_registration.uri_use_policy, &result)?;
                Ok(result)
            }
            Outcome::Err(error) => Err(sanitize_handler_error(request_ctx.cx(), "prompt", error)),
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => Err(sanitized_handler_panic(request_ctx.cx(), "prompt")),
        }
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Mount/Composition Support
// ============================================================================

/// Result of a mount operation.
#[derive(Debug, Default)]
pub struct MountResult {
    /// Number of tools mounted.
    pub tools: usize,
    /// Number of resources mounted.
    pub resources: usize,
    /// Number of resource templates mounted.
    pub resource_templates: usize,
    /// Number of prompts mounted.
    pub prompts: usize,
    /// Any warnings generated during mounting (e.g., name conflicts).
    pub warnings: Vec<String>,
    /// Errors that caused the mount operation to be rejected.
    ///
    /// A rejected mount does not mutate the destination router.
    pub errors: Vec<String>,
}

impl MountResult {
    /// Returns true if any components were mounted.
    #[must_use]
    pub fn has_components(&self) -> bool {
        self.tools > 0 || self.resources > 0 || self.resource_templates > 0 || self.prompts > 0
    }

    /// Returns true if mounting was not rejected.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }

    fn merge(&mut self, other: Self) {
        self.tools += other.tools;
        self.resources += other.resources;
        self.resource_templates += other.resource_templates;
        self.prompts += other.prompts;
        self.warnings.extend(other.warnings);
        self.errors.extend(other.errors);
    }
}

#[derive(Clone, Copy)]
enum MountSelection {
    All,
    Tools,
    Resources,
    Prompts,
}

impl MountSelection {
    const fn includes_tools(self) -> bool {
        matches!(self, Self::All | Self::Tools)
    }

    const fn includes_resources(self) -> bool {
        matches!(self, Self::All | Self::Resources)
    }

    const fn includes_prompts(self) -> bool {
        matches!(self, Self::All | Self::Prompts)
    }
}

impl Router {
    /// Applies a prefix to a name or URI.
    fn apply_prefix(name: &str, prefix: Option<&str>) -> String {
        match prefix {
            Some(p) if !p.is_empty() => format!("{}/{}", p, name),
            _ => name.to_string(),
        }
    }

    /// Returns whether mounting preserves component keys exactly.
    ///
    /// `Some("")` is deliberately equivalent to no prefix: [`Self::apply_prefix`]
    /// leaves every key byte-for-byte unchanged in both cases. Final route
    /// projection and collision admission must use this rule rather than
    /// distinguishing the two `Option` representations.
    fn prefix_preserves_keys(prefix: Option<&str>) -> bool {
        prefix.is_none_or(str::is_empty)
    }

    /// Validates a prefix string.
    ///
    /// Prefixes must be alphanumeric plus underscores and hyphens,
    /// and cannot contain slashes.
    fn validate_prefix(prefix: &str) -> Result<(), String> {
        if prefix.is_empty() {
            return Ok(());
        }
        if prefix.contains('/') {
            return Err("Invalid mount prefix: slashes are not permitted".to_string());
        }
        // Allow alphanumeric, underscore, hyphen
        for ch in prefix.chars() {
            if !ch.is_alphanumeric() && ch != '_' && ch != '-' {
                return Err(
                    "Invalid mount prefix: invalid characters are not permitted".to_string()
                );
            }
        }
        Ok(())
    }

    fn mount_preflight(
        &self,
        other: &Self,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
        selection: MountSelection,
    ) -> MountResult {
        let mut result = MountResult::default();

        if let Some(prefix) = prefix {
            if let Err(error) = Self::validate_prefix(prefix) {
                // Keep the warning for callers of the original API while also
                // recording the rejection as a real failure.
                result.warnings.push(error.clone());
                result.errors.push(error);
                return result;
            }
        }

        if behavior == crate::DuplicateBehavior::Error {
            let mut conflicts = Vec::new();
            if selection.includes_tools() {
                for name in other.tools.keys() {
                    let mounted_name = Self::apply_prefix(name, prefix);
                    if self.tools.contains_key(&mounted_name) {
                        conflicts.push(("Tool", mounted_name));
                    }
                }
            }
            if selection.includes_resources() {
                for uri in other.resources.keys() {
                    let mounted_uri = Self::apply_prefix(uri, prefix);
                    if self.resources.contains_key(&mounted_uri) {
                        conflicts.push(("Resource", mounted_uri));
                    }
                }
                for uri_template in other.resource_templates.keys() {
                    let mounted_uri_template = Self::apply_prefix(uri_template, prefix);
                    if self.resource_templates.contains_key(&mounted_uri_template) {
                        conflicts.push(("Resource template", mounted_uri_template));
                    }
                }
            }
            if selection.includes_prompts() {
                for name in other.prompts.keys() {
                    let mounted_name = Self::apply_prefix(name, prefix);
                    if self.prompts.contains_key(&mounted_name) {
                        conflicts.push(("Prompt", mounted_name));
                    }
                }
            }

            conflicts.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
            result
                .errors
                .extend(conflicts.into_iter().map(|(kind, key)| {
                    format!(
                        "Mount rejected because {kind} already exists; component_key={}",
                        safe_log_label(&key)
                    )
                }));
        }

        self.preflight_final_resource_route_collisions(
            other,
            prefix,
            behavior,
            selection,
            &mut result,
        );
        self.preflight_mcp_apps_mount_bindings(other, prefix, behavior, selection, &mut result);
        result
    }

    /// Projects the final resource routes that a mount would leave in the
    /// destination, then refuses ambiguous exact/template or template/template
    /// languages before consuming any source handlers. A nonempty prefix
    /// produces legacy-only resources, so it cannot introduce a final route
    /// collision.
    fn preflight_final_resource_route_collisions(
        &self,
        other: &Self,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
        selection: MountSelection,
        result: &mut MountResult,
    ) {
        if !selection.includes_resources() {
            return;
        }

        let mut final_resources: HashSet<String> = self.final_resources.keys().cloned().collect();
        let mut final_templates: HashMap<String, ReversibleResourceTemplate> = self
            .resource_templates
            .iter()
            .filter_map(|(uri_template, entry)| {
                entry
                    .final_definition
                    .as_ref()
                    .zip(entry.matcher.as_ref())
                    .map(|(_, matcher)| (uri_template.clone(), matcher.clone()))
            })
            .collect();

        for uri in other.resources.keys() {
            let mounted_uri = Self::apply_prefix(uri, prefix);
            let replaces_destination = !self.resources.contains_key(&mounted_uri)
                || behavior == crate::DuplicateBehavior::Replace;
            if !replaces_destination {
                continue;
            }
            if Self::prefix_preserves_keys(prefix) && other.final_resources.contains_key(uri) {
                final_resources.insert(mounted_uri);
            } else {
                final_resources.remove(&mounted_uri);
            }
        }

        for (uri_template, entry) in &other.resource_templates {
            let mounted_uri_template = Self::apply_prefix(uri_template, prefix);
            let replaces_destination = !self.resource_templates.contains_key(&mounted_uri_template)
                || behavior == crate::DuplicateBehavior::Replace;
            if !replaces_destination {
                continue;
            }
            if Self::prefix_preserves_keys(prefix)
                && let Some(matcher) = entry
                    .final_definition
                    .as_ref()
                    .zip(entry.matcher.as_ref())
                    .map(|(_, matcher)| matcher.clone())
            {
                final_templates.insert(mounted_uri_template, matcher);
            } else {
                final_templates.remove(&mounted_uri_template);
            }
        }

        let mut template_routes: Vec<_> = final_templates.iter().collect();
        template_routes.sort_unstable_by_key(|(uri, _)| *uri);
        let mut exact_routes: Vec<_> = final_resources.iter().collect();
        exact_routes.sort_unstable();
        let mut collisions = Vec::new();

        for (template_uri, matcher) in &template_routes {
            for exact_uri in &exact_routes {
                match matcher.match_uri(exact_uri) {
                    Ok(Some(_)) => collisions.push(format!(
                        "Mount rejected because a final resource template collides with an exact final resource; template_key={}; resource_key={}",
                        safe_log_label(template_uri),
                        safe_log_label(exact_uri)
                    )),
                    Ok(None) => {}
                    Err(_) => collisions.push(format!(
                        "Mount rejected because a final resource template lost match admission; template_key={}",
                        safe_log_label(template_uri)
                    )),
                }
            }
        }

        for (index, (left_uri, left)) in template_routes.iter().enumerate() {
            for (right_uri, right) in template_routes.iter().skip(index + 1) {
                match reversible_templates_may_overlap(left, right) {
                    Ok(true) => collisions.push(format!(
                        "Mount rejected because final resource template languages collide; left_template_key={}; right_template_key={}",
                        safe_log_label(left_uri),
                        safe_log_label(right_uri)
                    )),
                    Ok(false) => {}
                    Err(_) => collisions.push(format!(
                        "Mount rejected because a final resource template lost match admission; template_key={}",
                        safe_log_label(left_uri)
                    )),
                }
            }
        }

        collisions.sort_unstable();
        collisions.dedup();
        result.errors.extend(collisions);
    }

    fn preflight_mcp_apps_mount_bindings(
        &self,
        other: &Self,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
        selection: MountSelection,
        result: &mut MountResult,
    ) {
        let mut final_resources: HashMap<String, FinalResource> = self
            .final_resources
            .iter()
            .map(|(uri, registration)| (uri.clone(), registration.definition.clone()))
            .collect();
        let mut final_tools: HashMap<String, FinalTool> = self
            .tools
            .iter()
            .filter_map(|(name, registration)| {
                registration
                    .final_registration
                    .as_ref()
                    .map(|final_registration| {
                        (name.clone(), final_registration.final_definition.clone())
                    })
            })
            .collect();

        if selection.includes_resources() {
            for uri in other.resources.keys() {
                let mounted_uri = Self::apply_prefix(uri, prefix);
                let replacing = !self.resources.contains_key(&mounted_uri)
                    || matches!(behavior, crate::DuplicateBehavior::Replace);
                if !replacing {
                    continue;
                }
                let source_final = other.final_resources.get(uri);
                if !Self::prefix_preserves_keys(prefix)
                    && source_final.is_some_and(|registration| {
                        Self::mcp_apps_ui_resource_requires_special_admission(
                            &registration.definition,
                        )
                        .unwrap_or(true)
                    })
                {
                    result.errors.push(format!(
                        "Mount rejected because a final-only MCP Apps UI resource cannot be prefixed; resource_key={}",
                        safe_log_label(uri)
                    ));
                    continue;
                }
                if Self::prefix_preserves_keys(prefix) {
                    if let Some(registration) = source_final {
                        final_resources.insert(mounted_uri, registration.definition.clone());
                    } else {
                        final_resources.remove(&mounted_uri);
                    }
                } else {
                    final_resources.remove(&mounted_uri);
                }
            }
        }

        if selection.includes_tools() {
            for (name, registration) in &other.tools {
                let mounted_name = Self::apply_prefix(name, prefix);
                let replacing = !self.tools.contains_key(&mounted_name)
                    || matches!(behavior, crate::DuplicateBehavior::Replace);
                if !replacing {
                    continue;
                }
                if let Some(final_registration) = registration.final_registration.as_ref() {
                    let mut definition = final_registration.final_definition.clone();
                    definition.name.clone_from(&mounted_name);
                    final_tools.insert(mounted_name, definition);
                } else {
                    final_tools.remove(&mounted_name);
                }
            }
        }

        for (name, tool) in final_tools {
            let binding = match tool.mcp_apps_resource_binding() {
                Ok(binding) => binding,
                Err(error) => {
                    result.errors.push(format!(
                        "Mount rejected because tool has invalid MCP Apps metadata; tool_key={}; error={error}",
                        safe_log_label(&name)
                    ));
                    continue;
                }
            };
            let Some(binding) = binding else {
                continue;
            };
            let Some(resource) = final_resources.get(binding.resource_uri.as_str()) else {
                result.errors.push(format!(
                    "Mount rejected because an MCP Apps tool binding has no final HTML resource; tool_key={}; resource_uri={}",
                    safe_log_label(&name),
                    binding.resource_uri.as_str()
                ));
                continue;
            };
            if let Err(error) = binding.validate_resource(resource) {
                result.errors.push(format!(
                    "Mount rejected because an MCP Apps tool binding is invalid; tool_key={}; error={error}",
                    safe_log_label(&name)
                ));
            }
        }
    }

    fn should_mount_duplicate(
        behavior: crate::DuplicateBehavior,
        kind: &'static str,
        key: &str,
        result: &mut MountResult,
    ) -> bool {
        match behavior {
            crate::DuplicateBehavior::Error => {
                result.errors.push(format!(
                    "Mount rejected because {kind} already exists; component_key={}",
                    safe_log_label(key)
                ));
                false
            }
            crate::DuplicateBehavior::Warn => {
                result.warnings.push(format!(
                    "{kind} already exists, keeping original; component_key={}",
                    safe_log_label(key)
                ));
                false
            }
            crate::DuplicateBehavior::Replace => {
                result.warnings.push(format!(
                    "{kind} already exists, replacing original; component_key={}",
                    safe_log_label(key)
                ));
                true
            }
            crate::DuplicateBehavior::Ignore => false,
        }
    }

    fn completion_prompt_keys(&self) -> HashSet<String> {
        self.prompts
            .keys()
            .chain(self.final_prompts.keys())
            .cloned()
            .collect()
    }

    fn remap_mounted_completion_handler(
        handler: crate::handler::BoxedCompletionHandler,
        original: String,
        mounted: String,
        for_prompt: bool,
    ) -> crate::handler::BoxedCompletionHandler {
        if original == mounted {
            return handler;
        }
        if for_prompt {
            Box::new(crate::handler::MountedCompletionHandler::for_prompt(
                handler, mounted, original,
            ))
        } else {
            Box::new(
                crate::handler::MountedCompletionHandler::for_resource_template(
                    handler, mounted, original,
                ),
            )
        }
    }

    fn remap_mounted_fallback_completion_handler(
        handler: crate::handler::BoxedCompletionHandler,
        prompt_prefix: Option<&str>,
        template_prefix: Option<&str>,
    ) -> crate::handler::BoxedCompletionHandler {
        if Self::prefix_preserves_keys(prompt_prefix)
            && Self::prefix_preserves_keys(template_prefix)
        {
            return handler;
        }
        Box::new(crate::handler::PrefixedFallbackCompletionHandler::new(
            handler,
            prompt_prefix,
            template_prefix,
        ))
    }

    fn mount_completion_providers_from(
        &mut self,
        prompt_prefix: Option<&str>,
        template_prefix: Option<&str>,
        legacy_prompt_completion_handlers: HashMap<String, crate::handler::BoxedCompletionHandler>,
        legacy_resource_template_completion_handlers: HashMap<
            String,
            crate::handler::BoxedCompletionHandler,
        >,
        final_prompt_completion_handlers: HashMap<String, crate::handler::BoxedCompletionHandler>,
        final_resource_template_completion_handlers: HashMap<
            String,
            crate::handler::BoxedCompletionHandler,
        >,
        completion_handler: Option<crate::handler::BoxedCompletionHandler>,
        default_final_completion_enabled: bool,
        preexisting_prompts: &HashSet<String>,
        preexisting_templates: &HashSet<String>,
        behavior: crate::DuplicateBehavior,
    ) {
        let replace = behavior == crate::DuplicateBehavior::Replace;
        for (name, handler) in legacy_prompt_completion_handlers {
            let mounted = Self::apply_prefix(&name, prompt_prefix);
            if !self.prompts.contains_key(&mounted) {
                continue;
            }
            if preexisting_prompts.contains(&mounted) && !replace {
                continue;
            }
            self.legacy_prompt_completion_handlers.insert(
                mounted.clone(),
                Self::remap_mounted_completion_handler(handler, name, mounted, true),
            );
        }
        for (name, handler) in final_prompt_completion_handlers {
            let mounted = Self::apply_prefix(&name, prompt_prefix);
            if !self.prompts.contains_key(&mounted) && !self.final_prompts.contains_key(&mounted) {
                continue;
            }
            if preexisting_prompts.contains(&mounted) && !replace {
                continue;
            }
            self.final_prompt_completion_handlers.insert(
                mounted.clone(),
                Self::remap_mounted_completion_handler(handler, name, mounted, true),
            );
        }
        for (uri, handler) in legacy_resource_template_completion_handlers {
            let mounted = Self::apply_prefix(&uri, template_prefix);
            if !self.resource_templates.contains_key(&mounted) {
                continue;
            }
            if preexisting_templates.contains(&mounted) && !replace {
                continue;
            }
            self.legacy_resource_template_completion_handlers.insert(
                mounted.clone(),
                Self::remap_mounted_completion_handler(handler, uri, mounted, false),
            );
        }
        for (uri, handler) in final_resource_template_completion_handlers {
            let mounted = Self::apply_prefix(&uri, template_prefix);
            if !self.resource_templates.contains_key(&mounted) {
                continue;
            }
            if preexisting_templates.contains(&mounted) && !replace {
                continue;
            }
            self.final_resource_template_completion_handlers.insert(
                mounted.clone(),
                Self::remap_mounted_completion_handler(handler, uri, mounted, false),
            );
        }
        if let Some(handler) = completion_handler {
            if self.completion_handler.is_none() || replace {
                self.completion_handler = Some(Self::remap_mounted_fallback_completion_handler(
                    handler,
                    prompt_prefix,
                    template_prefix,
                ));
                self.default_final_completion_enabled = default_final_completion_enabled;
            }
        } else if self.completion_handler.is_none() {
            self.default_final_completion_enabled |= default_final_completion_enabled;
        }
    }

    /// Mounts all handlers from another router with an optional prefix.
    ///
    /// This consumes the source router and moves its handlers into this router.
    /// Names/URIs are prefixed with `prefix/` if a prefix is provided.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut main_router = Router::new();
    /// let db_router = Router::new();
    /// // ... add handlers to db_router ...
    ///
    /// main_router.mount(db_router, Some("db"));
    /// // Tool "query" becomes "db/query"
    /// ```
    pub fn mount(&mut self, other: Router, prefix: Option<&str>) -> MountResult {
        self.mount_with_behavior(other, prefix, crate::DuplicateBehavior::Replace)
    }

    /// Mounts all handlers using the specified duplicate behavior.
    ///
    /// Prefix validation happens before any destination mutation. With
    /// [`crate::DuplicateBehavior::Error`], every selected component is
    /// preflighted and any conflict rejects the entire mount atomically.
    pub fn mount_with_behavior(
        &mut self,
        other: Router,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        let preflight = self.mount_preflight(&other, prefix, behavior, MountSelection::All);
        if !preflight.is_success() {
            return preflight;
        }

        let mut result = preflight;
        let preexisting_prompts = self.completion_prompt_keys();
        let preexisting_templates = self
            .resource_templates
            .keys()
            .cloned()
            .collect::<HashSet<_>>();

        let Router {
            tools,
            tool_order,
            resources,
            final_only_resources,
            final_resources,
            resource_order,
            prompts,
            final_only_prompts,
            final_prompts,
            prompt_order,
            resource_templates,
            resource_template_order,
            completion_handler,
            default_final_completion_enabled,
            legacy_prompt_completion_handlers,
            legacy_resource_template_completion_handlers,
            final_prompt_completion_handlers,
            final_resource_template_completion_handlers,
            ..
        } = other;

        // Mount tools
        result.merge(self.mount_tools_from(tools, tool_order, prefix, behavior));

        // Mount resources
        result.merge(self.mount_resources_from(
            resources,
            final_only_resources,
            final_resources,
            resource_order,
            prefix,
            behavior,
        ));

        // Mount resource templates
        result.merge(self.mount_resource_templates_from(
            resource_templates,
            resource_template_order,
            prefix,
            behavior,
        ));

        // Mount prompts
        result.merge(self.mount_prompts_from(
            prompts,
            final_only_prompts,
            final_prompts,
            prompt_order,
            prefix,
            behavior,
        ));
        self.mount_completion_providers_from(
            prefix,
            prefix,
            legacy_prompt_completion_handlers,
            legacy_resource_template_completion_handlers,
            final_prompt_completion_handlers,
            final_resource_template_completion_handlers,
            completion_handler,
            default_final_completion_enabled,
            &preexisting_prompts,
            &preexisting_templates,
            behavior,
        );

        // Log mount result
        if result.has_components() {
            debug!(
                target: targets::HANDLER,
                "mounted {} tools, {} resources, {} templates, {} prompts; prefix_present={}; prefix_key={}",
                result.tools,
                result.resources,
                result.resource_templates,
                result.prompts,
                prefix.is_some(),
                safe_log_label(prefix.unwrap_or_default())
            );
            self.advance_final_catalog_revision();
        }

        result
    }

    /// Mounts tools and prompts with an optional name prefix, and keeps
    /// resource and template keys exact.
    ///
    /// A nonempty `{prefix}/{uri}` key is not an absolute final URI. Callers
    /// that need a modern resource catalog after namespacing tools/prompts
    /// must preserve the child's resource URIs instead of prefixing them.
    pub fn mount_namespaced_with_behavior(
        &mut self,
        other: Router,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        let mut preflight = self.mount_preflight(&other, prefix, behavior, MountSelection::Tools);
        preflight.merge(self.mount_preflight(&other, prefix, behavior, MountSelection::Prompts));
        preflight.merge(self.mount_preflight(&other, None, behavior, MountSelection::Resources));
        if !preflight.is_success() {
            return preflight;
        }

        let mut result = preflight;
        let preexisting_prompts = self.completion_prompt_keys();
        let preexisting_templates = self
            .resource_templates
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let Router {
            tools,
            tool_order,
            resources,
            final_only_resources,
            final_resources,
            resource_order,
            prompts,
            final_only_prompts,
            final_prompts,
            prompt_order,
            resource_templates,
            resource_template_order,
            completion_handler,
            default_final_completion_enabled,
            legacy_prompt_completion_handlers,
            legacy_resource_template_completion_handlers,
            final_prompt_completion_handlers,
            final_resource_template_completion_handlers,
            ..
        } = other;

        result.merge(self.mount_tools_from(tools, tool_order, prefix, behavior));
        result.merge(self.mount_resources_from(
            resources,
            final_only_resources,
            final_resources,
            resource_order,
            None,
            behavior,
        ));
        result.merge(self.mount_resource_templates_from(
            resource_templates,
            resource_template_order,
            None,
            behavior,
        ));
        result.merge(self.mount_prompts_from(
            prompts,
            final_only_prompts,
            final_prompts,
            prompt_order,
            prefix,
            behavior,
        ));
        self.mount_completion_providers_from(
            prefix,
            None,
            legacy_prompt_completion_handlers,
            legacy_resource_template_completion_handlers,
            final_prompt_completion_handlers,
            final_resource_template_completion_handlers,
            completion_handler,
            default_final_completion_enabled,
            &preexisting_prompts,
            &preexisting_templates,
            behavior,
        );

        if result.has_components() {
            debug!(
                target: targets::HANDLER,
                "mounted namespaced {} tools, {} resources, {} templates, {} prompts; prefix_present={}; prefix_key={}",
                result.tools,
                result.resources,
                result.resource_templates,
                result.prompts,
                prefix.is_some(),
                safe_log_label(prefix.unwrap_or_default())
            );
            self.advance_final_catalog_revision();
        }

        result
    }

    /// Mounts only tools from a router.
    pub fn mount_tools(&mut self, other: Router, prefix: Option<&str>) -> MountResult {
        self.mount_tools_with_behavior(other, prefix, crate::DuplicateBehavior::Replace)
    }

    /// Mounts only tools using the specified duplicate behavior.
    pub fn mount_tools_with_behavior(
        &mut self,
        other: Router,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        let preflight = self.mount_preflight(&other, prefix, behavior, MountSelection::Tools);
        if !preflight.is_success() {
            return preflight;
        }
        let result = self.mount_tools_from(other.tools, other.tool_order, prefix, behavior);
        if result.has_components() {
            self.advance_final_catalog_revision();
        }
        result
    }

    /// Internal: mount tools from a HashMap.
    fn mount_tools_from(
        &mut self,
        mut tools: HashMap<String, AdmittedToolRegistration>,
        tool_order: Vec<String>,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        let mut result = MountResult::default();

        for name in tool_order {
            let Some(entry) = tools.remove(&name) else {
                continue;
            };
            let mounted_name = Self::apply_prefix(&name, prefix);
            trace!(
                target: targets::HANDLER,
                "mounting tool; source_key={}; mounted_key={}",
                safe_log_label(&name),
                safe_log_label(&mounted_name)
            );

            // Check for conflicts
            let existed = self.tools.contains_key(&mounted_name);
            if existed
                && !Self::should_mount_duplicate(behavior, "Tool", &mounted_name, &mut result)
            {
                continue;
            }

            // Rewrite the immutable definition snapshot and wrap only the
            // dispatch target. Admitted schemas and final metadata move as one
            // entry and are never recomputed during mounting.
            let mounted = entry.with_mounted_name(mounted_name.clone());
            let needs_order_push = !existed && !self.tool_order.iter().any(|n| n == &mounted_name);
            self.tools.insert(mounted_name.clone(), mounted);
            if needs_order_push {
                self.tool_order.push(mounted_name);
            }
            result.tools += 1;
        }

        if !tools.is_empty() {
            // Defensive: older Routers or unusual construction could leave items untracked by
            // tool_order. Mount them deterministically to avoid HashMap iteration order leaks.
            let mut remaining: Vec<(String, AdmittedToolRegistration)> =
                tools.into_iter().collect();
            remaining.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, entry) in remaining {
                let mounted_name = Self::apply_prefix(&name, prefix);

                let existed = self.tools.contains_key(&mounted_name);
                if existed
                    && !Self::should_mount_duplicate(behavior, "Tool", &mounted_name, &mut result)
                {
                    continue;
                }

                let mounted = entry.with_mounted_name(mounted_name.clone());
                self.tools.insert(mounted_name.clone(), mounted);
                if !existed && !self.tool_order.iter().any(|n| n == &mounted_name) {
                    self.tool_order.push(mounted_name);
                }
                result.tools += 1;
            }
        }

        result
    }

    /// Mounts only resources from a router.
    pub fn mount_resources(&mut self, other: Router, prefix: Option<&str>) -> MountResult {
        self.mount_resources_with_behavior(other, prefix, crate::DuplicateBehavior::Replace)
    }

    /// Mounts resources and resource templates using the specified duplicate
    /// behavior.
    pub fn mount_resources_with_behavior(
        &mut self,
        other: Router,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        let preflight = self.mount_preflight(&other, prefix, behavior, MountSelection::Resources);
        if !preflight.is_success() {
            return preflight;
        }

        let preexisting_templates = self
            .resource_templates
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let Router {
            resources,
            final_only_resources,
            final_resources,
            resource_order,
            resource_templates,
            resource_template_order,
            legacy_resource_template_completion_handlers,
            final_resource_template_completion_handlers,
            ..
        } = other;
        let mut result = preflight;
        result.merge(self.mount_resources_from(
            resources,
            final_only_resources,
            final_resources,
            resource_order,
            prefix,
            behavior,
        ));
        let template_result = self.mount_resource_templates_from(
            resource_templates,
            resource_template_order,
            prefix,
            behavior,
        );
        result.merge(template_result);
        self.mount_completion_providers_from(
            None,
            prefix,
            HashMap::new(),
            legacy_resource_template_completion_handlers,
            HashMap::new(),
            final_resource_template_completion_handlers,
            None,
            false,
            &HashSet::new(),
            &preexisting_templates,
            behavior,
        );
        if result.has_components() {
            self.advance_final_catalog_revision();
        }
        result
    }

    /// Internal: mount resources from a HashMap.
    fn mount_resources_from(
        &mut self,
        mut resources: HashMap<String, BoxedResourceHandler>,
        final_only_resources: HashSet<String>,
        mut final_resources: HashMap<String, AdmittedFinalResourceRegistration>,
        resource_order: Vec<String>,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        use crate::handler::MountedResourceHandler;

        let mut result = MountResult::default();

        for uri in resource_order {
            let Some(handler) = resources.remove(&uri) else {
                continue;
            };
            let mounted_uri = Self::apply_prefix(&uri, prefix);
            trace!(
                target: targets::HANDLER,
                "mounting resource; source_key={}; mounted_key={}",
                safe_log_label(&uri),
                safe_log_label(&mounted_uri)
            );

            // Check for conflicts
            let existed = self.resources.contains_key(&mounted_uri);
            if existed
                && !Self::should_mount_duplicate(behavior, "Resource", &mounted_uri, &mut result)
            {
                continue;
            }

            // Wrap with mounted URI and insert
            let mounted = MountedResourceHandler::new(handler, uri.clone(), mounted_uri.clone());
            let needs_order_push =
                !existed && !self.resource_order.iter().any(|u| u == &mounted_uri);
            self.resources
                .insert(mounted_uri.clone(), Box::new(mounted));
            if Self::prefix_preserves_keys(prefix) {
                if let Some(final_registration) = final_resources.remove(&uri) {
                    self.final_resources
                        .insert(mounted_uri.clone(), final_registration);
                } else {
                    self.final_resources.remove(&mounted_uri);
                }
            } else {
                // Nonempty-prefixed resource URIs are intentionally
                // legacy-only: the mounting namespace is not an absolute
                // final URI.
                self.final_resources.remove(&mounted_uri);
            }
            if Self::prefix_preserves_keys(prefix) && final_only_resources.contains(&uri) {
                self.final_only_resources.insert(mounted_uri.clone());
            } else {
                self.final_only_resources.remove(&mounted_uri);
            }
            if needs_order_push {
                self.resource_order.push(mounted_uri);
            }
            result.resources += 1;
        }

        if !resources.is_empty() {
            let mut remaining: Vec<(String, BoxedResourceHandler)> =
                resources.into_iter().collect();
            remaining.sort_by(|a, b| a.0.cmp(&b.0));
            for (uri, handler) in remaining {
                let mounted_uri = Self::apply_prefix(&uri, prefix);

                let existed = self.resources.contains_key(&mounted_uri);
                if existed
                    && !Self::should_mount_duplicate(
                        behavior,
                        "Resource",
                        &mounted_uri,
                        &mut result,
                    )
                {
                    continue;
                }

                let mounted =
                    MountedResourceHandler::new(handler, uri.clone(), mounted_uri.clone());
                self.resources
                    .insert(mounted_uri.clone(), Box::new(mounted));
                if Self::prefix_preserves_keys(prefix) {
                    if let Some(final_registration) = final_resources.remove(&uri) {
                        self.final_resources
                            .insert(mounted_uri.clone(), final_registration);
                    } else {
                        self.final_resources.remove(&mounted_uri);
                    }
                } else {
                    self.final_resources.remove(&mounted_uri);
                }
                if Self::prefix_preserves_keys(prefix) && final_only_resources.contains(&uri) {
                    self.final_only_resources.insert(mounted_uri.clone());
                } else {
                    self.final_only_resources.remove(&mounted_uri);
                }
                if !existed && !self.resource_order.iter().any(|u| u == &mounted_uri) {
                    self.resource_order.push(mounted_uri);
                }
                result.resources += 1;
            }
        }

        result
    }

    /// Internal: mount resource templates from a HashMap.
    fn mount_resource_templates_from(
        &mut self,
        mut templates: HashMap<String, ResourceTemplateEntry>,
        resource_template_order: Vec<String>,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        use crate::handler::MountedResourceHandler;

        let mut result = MountResult::default();

        for uri_template in resource_template_order {
            let Some(entry) = templates.remove(&uri_template) else {
                continue;
            };
            let mounted_uri_template = Self::apply_prefix(&uri_template, prefix);
            trace!(
                target: targets::HANDLER,
                "mounting resource template; source_key={}; mounted_key={}",
                safe_log_label(&uri_template),
                safe_log_label(&mounted_uri_template)
            );

            // Check for conflicts
            let existed = self.resource_templates.contains_key(&mounted_uri_template);
            if existed
                && !Self::should_mount_duplicate(
                    behavior,
                    "Resource template",
                    &mounted_uri_template,
                    &mut result,
                )
            {
                continue;
            }

            // Create new template with mounted URI
            let mut mounted_template = entry.template.clone();
            mounted_template.uri_template = mounted_uri_template.clone();

            // Wrap handler if present
            let mounted_handler = entry.handler.map(|h| {
                let wrapped: BoxedResourceHandler =
                    Box::new(MountedResourceHandler::with_template(
                        h,
                        uri_template.clone(),
                        mounted_uri_template.clone(),
                        mounted_template.clone(),
                    ));
                wrapped
            });

            // Create new entry with mounted template.
            let legacy_matcher = if entry.legacy_enabled {
                match admit_legacy_resource_template(&mounted_uri_template) {
                    Ok(matcher) => Some(matcher),
                    Err(error) => {
                        result.errors.push(format!(
                            "Mount rejected exact-2024 resource template; template_key={}; code={:?}",
                            safe_log_label(&mounted_uri_template),
                            error.code
                        ));
                        continue;
                    }
                }
            } else {
                None
            };
            let final_enabled =
                Self::prefix_preserves_keys(prefix) && entry.final_definition.is_some();
            let (matcher, specificity) = if final_enabled {
                match admit_resource_template(&mounted_uri_template) {
                    Ok((matcher, specificity)) => (Some(matcher), specificity),
                    Err(error) => {
                        result.errors.push(format!(
                            "Mount rejected resource template; template_key={}; code={:?}",
                            safe_log_label(&mounted_uri_template),
                            error.code
                        ));
                        continue;
                    }
                }
            } else {
                let specificity = legacy_matcher.as_ref().map_or(
                    entry.specificity,
                    LegacyResourceTemplateMatcher::specificity,
                );
                (None, specificity)
            };
            let mounted_entry = ResourceTemplateEntry {
                matcher,
                legacy_matcher,
                specificity,
                template: mounted_template,
                handler: mounted_handler,
                // A nonempty mount prefix produces a relative legacy
                // namespace (for example, `peer/mcp://...`). It cannot
                // preserve the exact final absolute-URI contract, so mirror
                // static-resource mounting and expose the mounted route to
                // legacy only.
                final_definition: if Self::prefix_preserves_keys(prefix) {
                    entry.final_definition.map(|mut definition| {
                        definition.uri_template = mounted_uri_template.clone();
                        definition
                    })
                } else {
                    None
                },
                uri_use_policy: entry.uri_use_policy,
                legacy_enabled: entry.legacy_enabled,
            };

            let needs_order_push = !existed
                && !self
                    .resource_template_order
                    .iter()
                    .any(|t| t == &mounted_uri_template);
            self.resource_templates
                .insert(mounted_uri_template.clone(), mounted_entry);
            if needs_order_push {
                self.resource_template_order.push(mounted_uri_template);
            }
            result.resource_templates += 1;
        }

        if !templates.is_empty() {
            let mut remaining: Vec<(String, ResourceTemplateEntry)> =
                templates.into_iter().collect();
            remaining.sort_by(|a, b| a.0.cmp(&b.0));
            for (uri_template, entry) in remaining {
                let mounted_uri_template = Self::apply_prefix(&uri_template, prefix);

                let existed = self.resource_templates.contains_key(&mounted_uri_template);
                if existed
                    && !Self::should_mount_duplicate(
                        behavior,
                        "Resource template",
                        &mounted_uri_template,
                        &mut result,
                    )
                {
                    continue;
                }

                let mut mounted_template = entry.template.clone();
                mounted_template.uri_template = mounted_uri_template.clone();

                let mounted_handler = entry.handler.map(|h| {
                    let wrapped: BoxedResourceHandler =
                        Box::new(MountedResourceHandler::with_template(
                            h,
                            uri_template,
                            mounted_uri_template.clone(),
                            mounted_template.clone(),
                        ));
                    wrapped
                });

                let legacy_matcher = if entry.legacy_enabled {
                    match admit_legacy_resource_template(&mounted_uri_template) {
                        Ok(matcher) => Some(matcher),
                        Err(error) => {
                            result.errors.push(format!(
                                "Mount rejected exact-2024 resource template; template_key={}; code={:?}",
                                safe_log_label(&mounted_uri_template),
                                error.code
                            ));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let final_enabled =
                    Self::prefix_preserves_keys(prefix) && entry.final_definition.is_some();
                let (matcher, specificity) = if final_enabled {
                    match admit_resource_template(&mounted_uri_template) {
                        Ok((matcher, specificity)) => (Some(matcher), specificity),
                        Err(error) => {
                            result.errors.push(format!(
                                "Mount rejected resource template; template_key={}; code={:?}",
                                safe_log_label(&mounted_uri_template),
                                error.code
                            ));
                            continue;
                        }
                    }
                } else {
                    let specificity = legacy_matcher.as_ref().map_or(
                        entry.specificity,
                        LegacyResourceTemplateMatcher::specificity,
                    );
                    (None, specificity)
                };
                let mounted_entry = ResourceTemplateEntry {
                    matcher,
                    legacy_matcher,
                    specificity,
                    template: mounted_template,
                    handler: mounted_handler,
                    // See the ordered-template path above: nonempty-prefixed
                    // template routes are legacy-only because their URI
                    // namespace is no longer absolute for exact final
                    // resource contents.
                    final_definition: if Self::prefix_preserves_keys(prefix) {
                        entry.final_definition.map(|mut definition| {
                            definition.uri_template = mounted_uri_template.clone();
                            definition
                        })
                    } else {
                        None
                    },
                    uri_use_policy: entry.uri_use_policy,
                    legacy_enabled: entry.legacy_enabled,
                };

                self.resource_templates
                    .insert(mounted_uri_template.clone(), mounted_entry);
                if !existed
                    && !self
                        .resource_template_order
                        .iter()
                        .any(|t| t == &mounted_uri_template)
                {
                    self.resource_template_order
                        .push(mounted_uri_template.clone());
                }
                result.resource_templates += 1;
            }
        }

        // Rebuild sorted keys if we added templates
        if result.resource_templates > 0 {
            self.rebuild_sorted_template_keys();
        }

        result
    }

    /// Mounts only prompts from a router.
    pub fn mount_prompts(&mut self, other: Router, prefix: Option<&str>) -> MountResult {
        self.mount_prompts_with_behavior(other, prefix, crate::DuplicateBehavior::Replace)
    }

    /// Mounts only prompts using the specified duplicate behavior.
    pub fn mount_prompts_with_behavior(
        &mut self,
        other: Router,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        let preflight = self.mount_preflight(&other, prefix, behavior, MountSelection::Prompts);
        if !preflight.is_success() {
            return preflight;
        }
        let preexisting_prompts = self.completion_prompt_keys();
        let Router {
            prompts,
            final_only_prompts,
            final_prompts,
            prompt_order,
            legacy_prompt_completion_handlers,
            final_prompt_completion_handlers,
            ..
        } = other;
        let mut result = preflight;
        result.merge(self.mount_prompts_from(
            prompts,
            final_only_prompts,
            final_prompts,
            prompt_order,
            prefix,
            behavior,
        ));
        self.mount_completion_providers_from(
            prefix,
            None,
            legacy_prompt_completion_handlers,
            HashMap::new(),
            final_prompt_completion_handlers,
            HashMap::new(),
            None,
            false,
            &preexisting_prompts,
            &HashSet::new(),
            behavior,
        );
        if result.has_components() {
            self.advance_final_catalog_revision();
        }
        result
    }

    /// Internal: mount prompts from a HashMap.
    fn mount_prompts_from(
        &mut self,
        mut prompts: HashMap<String, BoxedPromptHandler>,
        final_only_prompts: HashSet<String>,
        mut final_prompts: HashMap<String, AdmittedFinalPromptRegistration>,
        prompt_order: Vec<String>,
        prefix: Option<&str>,
        behavior: crate::DuplicateBehavior,
    ) -> MountResult {
        use crate::handler::MountedPromptHandler;

        let mut result = MountResult::default();

        for name in prompt_order {
            let Some(handler) = prompts.remove(&name) else {
                continue;
            };
            let mounted_name = Self::apply_prefix(&name, prefix);
            trace!(
                target: targets::HANDLER,
                "mounting prompt; source_key={}; mounted_key={}",
                safe_log_label(&name),
                safe_log_label(&mounted_name)
            );

            // Check for conflicts
            let existed = self.prompts.contains_key(&mounted_name);
            if existed
                && !Self::should_mount_duplicate(behavior, "Prompt", &mounted_name, &mut result)
            {
                continue;
            }

            // Wrap with mounted name and insert
            let mounted = MountedPromptHandler::new(handler, mounted_name.clone());
            let needs_order_push =
                !existed && !self.prompt_order.iter().any(|n| n == &mounted_name);
            self.prompts.insert(mounted_name.clone(), Box::new(mounted));
            match final_prompts.remove(&name) {
                Some(mut final_registration) => {
                    final_registration.definition.name.clone_from(&mounted_name);
                    self.final_prompts
                        .insert(mounted_name.clone(), final_registration);
                }
                None => {
                    self.final_prompts.remove(&mounted_name);
                }
            }
            if final_only_prompts.contains(&name) {
                self.final_only_prompts.insert(mounted_name.clone());
            } else {
                self.final_only_prompts.remove(&mounted_name);
            }
            if needs_order_push {
                self.prompt_order.push(mounted_name);
            }
            result.prompts += 1;
        }

        if !prompts.is_empty() {
            let mut remaining: Vec<(String, BoxedPromptHandler)> = prompts.into_iter().collect();
            remaining.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, handler) in remaining {
                let mounted_name = Self::apply_prefix(&name, prefix);

                let existed = self.prompts.contains_key(&mounted_name);
                if existed
                    && !Self::should_mount_duplicate(behavior, "Prompt", &mounted_name, &mut result)
                {
                    continue;
                }

                let mounted = MountedPromptHandler::new(handler, mounted_name.clone());
                self.prompts.insert(mounted_name.clone(), Box::new(mounted));
                match final_prompts.remove(&name) {
                    Some(mut final_registration) => {
                        final_registration.definition.name.clone_from(&mounted_name);
                        self.final_prompts
                            .insert(mounted_name.clone(), final_registration);
                    }
                    None => {
                        self.final_prompts.remove(&mounted_name);
                    }
                }
                if final_only_prompts.contains(&name) {
                    self.final_only_prompts.insert(mounted_name.clone());
                } else {
                    self.final_only_prompts.remove(&mounted_name);
                }
                if !existed && !self.prompt_order.iter().any(|n| n == &mounted_name) {
                    self.prompt_order.push(mounted_name);
                }
                result.prompts += 1;
            }
        }

        result
    }

    /// Consumes the router and returns its internal handlers.
    ///
    /// This is used internally for mounting operations.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn into_parts(
        self,
    ) -> (
        HashMap<String, BoxedToolHandler>,
        HashMap<String, BoxedResourceHandler>,
        HashMap<String, ResourceTemplateEntry>,
        HashMap<String, BoxedPromptHandler>,
    ) {
        let tools = self
            .tools
            .into_iter()
            .map(|(name, entry)| (name, entry.handler))
            .collect();
        (tools, self.resources, self.resource_templates, self.prompts)
    }
}

struct ResolvedResource<'a> {
    handler: &'a BoxedResourceHandler,
    params: UriParams,
    final_enabled: bool,
    legacy_enabled: bool,
    uri_use_policy: ResourceUriUsePolicy,
}

impl ResolvedResource<'_> {
    const fn is_enabled_in(&self, era: ProtocolEra) -> bool {
        match era {
            ProtocolEra::Legacy2024 => self.legacy_enabled,
            ProtocolEra::Modern2026 => self.final_enabled,
        }
    }
}

/// Entry for a resource template with its matcher and optional handler.
pub(crate) struct ResourceTemplateEntry {
    pub(crate) matcher: Option<ReversibleResourceTemplate>,
    legacy_matcher: Option<LegacyResourceTemplateMatcher>,
    specificity: (usize, usize, usize),
    pub(crate) template: ResourceTemplate,
    pub(crate) handler: Option<BoxedResourceHandler>,
    final_definition: Option<FinalResourceTemplate>,
    uri_use_policy: ResourceUriUsePolicy,
    legacy_enabled: bool,
}

/// The frozen, exact-2024 template matcher. Legacy registrations deliberately
/// do not inherit new RFC 6570 operators: their parameter surface is limited
/// to `{name}` and `{+name}`, with the historical non-empty, percent-decoded
/// capture rules retained for existing handlers.
#[derive(Debug, Clone)]
struct LegacyResourceTemplateMatcher {
    segments: Vec<LegacyResourceTemplateSegment>,
}

#[derive(Debug, Clone)]
enum LegacyResourceTemplateSegment {
    Literal(String),
    Parameter(String),
}

impl LegacyResourceTemplateMatcher {
    fn parse(pattern: &str) -> Result<Self, ()> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut chars = pattern.chars().peekable();
        let mut names = HashSet::new();

        while let Some(character) = chars.next() {
            match character {
                '{' if matches!(chars.peek(), Some('{')) => {
                    let _ = chars.next();
                    literal.push('{');
                }
                '{' => {
                    if !literal.is_empty() {
                        segments.push(LegacyResourceTemplateSegment::Literal(std::mem::take(
                            &mut literal,
                        )));
                    }
                    let mut expression = String::new();
                    let mut closed = false;
                    for next in chars.by_ref() {
                        if next == '}' {
                            closed = true;
                            break;
                        }
                        expression.push(next);
                    }
                    if !closed {
                        return Err(());
                    }

                    let name = expression.strip_prefix('+').unwrap_or(&expression);
                    if name.is_empty()
                        || name.starts_with('+')
                        || matches!(
                            expression.chars().next(),
                            Some('#' | '.' | '/' | ';' | '?' | '&')
                        )
                        || name
                            .chars()
                            .any(|character| matches!(character, '*' | ':' | ','))
                        || !names.insert(name.to_owned())
                    {
                        return Err(());
                    }
                    segments.push(LegacyResourceTemplateSegment::Parameter(name.to_owned()));
                }
                '}' if matches!(chars.peek(), Some('}')) => {
                    let _ = chars.next();
                    literal.push('}');
                }
                '}' => return Err(()),
                character => literal.push(character),
            }
        }

        if !literal.is_empty() {
            segments.push(LegacyResourceTemplateSegment::Literal(literal));
        }
        Ok(Self { segments })
    }

    fn specificity(&self) -> (usize, usize, usize) {
        let mut literal_bytes = 0usize;
        let mut literal_parts = 0usize;
        for segment in &self.segments {
            if let LegacyResourceTemplateSegment::Literal(literal) = segment {
                literal_bytes = literal_bytes.saturating_add(literal.len());
                literal_parts = literal_parts.saturating_add(1);
            }
        }
        (literal_bytes, literal_parts, self.segments.len())
    }

    fn matches(&self, uri: &str) -> Option<UriParams> {
        let mut params = UriParams::new();
        let mut remainder = uri;
        let mut segments = self.segments.iter().peekable();

        while let Some(segment) = segments.next() {
            match segment {
                LegacyResourceTemplateSegment::Literal(literal) => {
                    remainder = remainder.strip_prefix(literal)?;
                }
                LegacyResourceTemplateSegment::Parameter(name) => {
                    let next_literal = segments.peek().and_then(|next| match next {
                        LegacyResourceTemplateSegment::Literal(literal) => Some(literal.as_str()),
                        LegacyResourceTemplateSegment::Parameter(_) => None,
                    });
                    if next_literal.is_none() && segments.peek().is_some() {
                        return None;
                    }

                    let value = if let Some(literal) = next_literal {
                        let index = remainder.find(literal)?;
                        let value = &remainder[..index];
                        remainder = &remainder[index..];
                        value
                    } else {
                        if remainder.is_empty() {
                            return None;
                        }
                        let parameter_count = self
                            .segments
                            .iter()
                            .filter(|segment| {
                                matches!(segment, LegacyResourceTemplateSegment::Parameter(_))
                            })
                            .count();
                        let end = if parameter_count == 1 {
                            remainder.len()
                        } else {
                            remainder.find('/').unwrap_or(remainder.len())
                        };
                        let value = &remainder[..end];
                        remainder = &remainder[end..];
                        value
                    };
                    if value.is_empty() {
                        return None;
                    }
                    params.insert(name.clone(), legacy_percent_decode(value)?);
                }
            }
        }

        remainder.is_empty().then_some(params)
    }
}

fn admit_legacy_resource_template(source: &str) -> McpResult<LegacyResourceTemplateMatcher> {
    LegacyResourceTemplateMatcher::parse(source).map_err(|()| {
        McpError::invalid_params(
            "exact-2024 resource templates only admit unmodified {name} and {+name} parameters",
        )
    })
}

fn legacy_percent_decode(input: &str) -> Option<String> {
    if !input.as_bytes().contains(&b'%') {
        return Some(input.to_owned());
    }
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = legacy_hex_value(bytes[index + 1])?;
                let low = legacy_hex_value(bytes[index + 2])?;
                output.push((high << 4) | low);
                index += 3;
            }
            b'%' => return None,
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).ok()
}

const fn legacy_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parses a resource template with the canonical RFC 6570 implementation and
/// admits only templates the protocol can reverse-match exactly for routing.
fn admit_resource_template(
    source: &str,
) -> McpResult<(ReversibleResourceTemplate, (usize, usize, usize))> {
    let template = fastmcp_protocol::UriTemplate::parse(source)
        .map_err(|_| McpError::invalid_params("resource template is not valid RFC 6570"))?;
    let specificity = resource_template_specificity(&template);
    let matcher = template
        .compile_reversible()
        .map_err(|_| McpError::invalid_params("resource template cannot be matched reversibly"))?;
    Ok((matcher, specificity))
}

fn resource_template_specificity(
    template: &fastmcp_protocol::UriTemplate,
) -> (usize, usize, usize) {
    let mut literal_bytes = 0usize;
    let mut literal_parts = 0usize;
    for part in template.parts() {
        if let fastmcp_protocol::UriTemplatePart::Literal(literal) = part {
            literal_bytes = literal_bytes.saturating_add(literal.len());
            literal_parts = literal_parts.saturating_add(1);
        }
    }
    (literal_bytes, literal_parts, template.parts().len())
}

/// Returns whether two reversible template languages might intersect.
///
/// A literal prefix is emitted before any expression and therefore provides a
/// byte-exact proof of disjointness when the prefixes disagree. When the
/// prefixes are compatible, reject conservatively: an RFC 6570 capture can be
/// omitted or can absorb following literals, so accepting without a full
/// language-intersection proof would reintroduce dispatch-order authority.
fn reversible_templates_may_overlap(
    left: &ReversibleResourceTemplate,
    right: &ReversibleResourceTemplate,
) -> McpResult<bool> {
    let left_prefix = reversible_template_leading_literal_prefix(left)?;
    let right_prefix = reversible_template_leading_literal_prefix(right)?;
    Ok(left_prefix.starts_with(&right_prefix) || right_prefix.starts_with(&left_prefix))
}

fn reversible_template_leading_literal_prefix(
    matcher: &ReversibleResourceTemplate,
) -> McpResult<String> {
    let mut literal = String::new();
    for part in matcher.template().parts() {
        let UriTemplatePart::Literal(part) = part else {
            break;
        };
        literal.push_str(part);
    }
    fastmcp_protocol::UriTemplate::parse(&literal)
        .and_then(|template| template.expand(&fastmcp_protocol::TemplateValues::new()))
        .map_err(|_| McpError::internal_error("admitted resource template lost literal prefix"))
}

// ============================================================================
// Resource Reader Implementation
// ============================================================================

use fastmcp_core::{
    MAX_RESOURCE_READ_DEPTH, ResourceContentItem, ResourceReadResult, ResourceReader,
    ToolCallResult, ToolContentItem,
};
use std::pin::Pin;

fn nested_resource_content_from_final(contents: EmbeddedResourceContents) -> ResourceContentItem {
    match contents {
        EmbeddedResourceContents::Text {
            uri,
            text,
            mime_type,
            ..
        } => ResourceContentItem {
            uri: uri.as_str().to_owned(),
            mime_type,
            text: Some(text),
            blob: None,
        },
        EmbeddedResourceContents::Blob {
            uri,
            blob,
            mime_type,
            ..
        } => ResourceContentItem {
            uri: uri.as_str().to_owned(),
            mime_type,
            text: None,
            blob: Some(blob),
        },
    }
}

fn nested_tool_content_from_final(block: ContentBlock) -> ToolContentItem {
    match block {
        ContentBlock::Text { text, .. } => ToolContentItem::Text { text },
        ContentBlock::Image {
            data, mime_type, ..
        } => ToolContentItem::Image { data, mime_type },
        ContentBlock::Audio {
            data, mime_type, ..
        } => ToolContentItem::Audio { data, mime_type },
        ContentBlock::Resource { resource, .. } => match resource {
            EmbeddedResourceContents::Text {
                uri,
                text,
                mime_type,
                ..
            } => ToolContentItem::Resource {
                uri: uri.as_str().to_owned(),
                mime_type,
                text: Some(text),
                blob: None,
            },
            EmbeddedResourceContents::Blob {
                uri,
                blob,
                mime_type,
                ..
            } => ToolContentItem::Resource {
                uri: uri.as_str().to_owned(),
                mime_type,
                text: None,
                blob: Some(blob),
            },
        },
        ContentBlock::ResourceLink { uri, mime_type, .. } => ToolContentItem::Resource {
            uri: uri.as_str().to_owned(),
            mime_type,
            text: None,
            blob: None,
        },
    }
}

/// A wrapper that implements `ResourceReader` for a shared `Router`.
///
/// This allows handlers to read resources from within tool/resource/prompt
/// handlers, enabling cross-component access.
#[derive(Clone)]
enum RouterAccess {
    Shared(Arc<Router>),
    RequestScoped(Weak<Router>),
}

impl RouterAccess {
    fn upgrade(&self) -> McpResult<Arc<Router>> {
        match self {
            Self::Shared(router) => Ok(Arc::clone(router)),
            Self::RequestScoped(router) => router.upgrade().ok_or_else(|| {
                McpError::new(
                    McpErrorCode::RequestCancelled,
                    "Request router is no longer available",
                )
            }),
        }
    }
}

pub(crate) struct RouterResourceReader {
    /// Access to the router without extending a server request's lifetime.
    router: RouterAccess,
    /// Session state for handlers.
    session_state: SessionState,
}

impl RouterResourceReader {
    /// Creates a new resource reader with the given router and session state.
    #[must_use]
    pub(crate) fn new(router: Arc<Router>, session_state: SessionState) -> Self {
        Self {
            router: RouterAccess::Shared(router),
            session_state,
        }
    }

    pub(crate) fn request_scoped(router: Weak<Router>, session_state: SessionState) -> Self {
        Self {
            router: RouterAccess::RequestScoped(router),
            session_state,
        }
    }

    fn from_access(router: RouterAccess, session_state: SessionState) -> Self {
        Self {
            router,
            session_state,
        }
    }

    async fn read_resource_legacy(
        router: &Router,
        parent_ctx: &McpContext,
        router_access: RouterAccess,
        session_state: SessionState,
        uri: String,
        depth: u32,
    ) -> McpResult<ResourceReadResult> {
        let operation_started_at = parent_ctx.cx().now();
        if let Some(error) = budget_error(parent_ctx) {
            return Err(error);
        }
        if !session_state.is_resource_enabled(&uri) {
            return Err(McpError::new(
                McpErrorCode::ResourceNotFound,
                format!("Resource '{uri}' is disabled for this session"),
            ));
        }

        let resolved = router.resolve_resource(&uri).ok_or_else(|| {
            McpError::new(
                McpErrorCode::ResourceNotFound,
                format!("Resource not found: {uri}"),
            )
        })?;
        let handler_timeout = read_handler_timeout(parent_ctx.cx(), "resource_timeout", || {
            resolved.handler.timeout()
        })?;
        let effective_budget = compose_handler_budget(
            parent_ctx.cx().budget(),
            parent_ctx.budget(),
            handler_timeout,
            operation_started_at,
        );
        let child_ctx = with_nested_component_access(
            parent_ctx
                .clone()
                .with_operation_deadline(effective_budget.deadline),
            router_access.clone(),
            session_state.clone(),
            depth,
        );
        let outcome = run_handler_in_request(
            &child_ctx,
            parent_ctx.cx(),
            effective_budget,
            "resource",
            |request_cx| {
                resolved.handler.read_async_with_uri_in_request(
                    &child_ctx,
                    request_cx,
                    &uri,
                    &resolved.params,
                )
            },
        )
        .await?;
        let contents = match outcome {
            Outcome::Ok(contents) => contents,
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(parent_ctx.cx(), "resource", error));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(parent_ctx.cx(), "resource"));
            }
        };
        Ok(ResourceReadResult::new(
            contents
                .into_iter()
                .map(|content| ResourceContentItem {
                    uri: content.uri,
                    mime_type: content.mime_type,
                    text: content.text,
                    blob: content.blob,
                })
                .collect(),
        ))
    }

    async fn read_resource_final(
        router: &Router,
        parent_ctx: &McpContext,
        router_access: RouterAccess,
        session_state: SessionState,
        uri: String,
        depth: u32,
    ) -> McpResult<ResourceReadResult> {
        let operation_started_at = parent_ctx.cx().now();
        if let Some(error) = budget_error(parent_ctx) {
            return Err(error);
        }
        if !session_state.is_resource_enabled(&uri) || !parent_ctx.is_resource_enabled(&uri) {
            return Err(McpError::new(
                McpErrorCode::ResourceNotFound,
                format!("Resource '{uri}' is disabled for this session"),
            ));
        }
        let absolute = AbsoluteUri::parse(uri.clone()).map_err(|_| {
            McpError::with_data(
                McpErrorCode::InvalidParams,
                "Resource not found",
                serde_json::json!({ "uri": uri }),
            )
        })?;
        let resolved = router
            .resolve_resource_for_era(&uri, Some(ProtocolEra::Modern2026))
            .ok_or_else(|| {
                McpError::with_data(
                    McpErrorCode::InvalidParams,
                    "Resource not found",
                    serde_json::json!({ "uri": uri }),
                )
            })?;
        if !resolved.final_enabled {
            return Err(McpError::invalid_params(
                "resource is registered only for exact MCP 2024-11-05 dispatch",
            ));
        }
        if resolved.handler.declares_final_mrtr()
            && parent_ctx.retained_continuation_partition().is_none()
        {
            return Err(McpError::invalid_params(
                MRTR_REQUIRES_BOUND_MODERN_CONNECTION,
            ));
        }
        admit_final_resource_uri(
            resolved.uri_use_policy,
            &absolute,
            FinalResourceUriUse::ResourceReadTarget,
        )?;
        let handler_timeout = read_handler_timeout(parent_ctx.cx(), "resource_timeout", || {
            resolved.handler.timeout()
        })?;
        let effective_budget = compose_handler_budget(
            parent_ctx.cx().budget(),
            parent_ctx.budget(),
            handler_timeout,
            operation_started_at,
        );
        let child_ctx = with_nested_component_access(
            parent_ctx
                .clone()
                .with_operation_deadline(effective_budget.deadline),
            router_access,
            session_state,
            depth,
        );
        let outcome = run_handler_in_request(
            &child_ctx,
            parent_ctx.cx(),
            effective_budget,
            "resource",
            |request_cx| {
                resolved
                    .handler
                    .read_final_outcome_async_with_uri_in_request(
                        &child_ctx,
                        request_cx,
                        &uri,
                        &resolved.params,
                    )
            },
        )
        .await?;
        let result = match outcome {
            Outcome::Ok(result) => result,
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(parent_ctx.cx(), "resource", error));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(parent_ctx.cx(), "resource"));
            }
        };
        admit_final_resource_read_outcome(resolved.uri_use_policy, &result)?;
        let FinalMethodOutcome::Complete(complete) = result else {
            return Err(McpError::internal_error(
                "nested resource read cannot complete an input_required outcome",
            ));
        };
        Ok(ResourceReadResult::new(
            complete
                .payload
                .contents
                .into_iter()
                .map(nested_resource_content_from_final)
                .collect(),
        ))
    }
}

impl ResourceReader for RouterResourceReader {
    fn read_resource<'a>(
        &'a self,
        parent_ctx: &'a McpContext,
        uri: &'a str,
        depth: u32,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = fastmcp_core::McpResult<ResourceReadResult>>
                + Send
                + 'a,
        >,
    > {
        // Check recursion depth
        if depth > MAX_RESOURCE_READ_DEPTH {
            return Box::pin(async move {
                Err(McpError::new(
                    McpErrorCode::InternalError,
                    format!(
                        "Maximum resource read depth ({}) exceeded",
                        MAX_RESOURCE_READ_DEPTH
                    ),
                ))
            });
        }

        // Clone what we need for the async block
        let parent_ctx = parent_ctx.clone();
        let uri = uri.to_string();
        let router_access = self.router.clone();
        let session_state = self.session_state.clone();
        let final_surface = parent_ctx.is_final_request_surface();

        Box::pin(async move {
            debug!(
                target: targets::HANDLER,
                "cross-component resource read; resource_key={}; depth={}; request={}; final_surface={}",
                safe_log_label(&uri),
                depth,
                parent_ctx.request_id(),
                final_surface
            );
            let router = router_access.upgrade()?;
            if final_surface {
                Self::read_resource_final(
                    &router,
                    &parent_ctx,
                    router_access,
                    session_state,
                    uri,
                    depth,
                )
                .await
            } else {
                Self::read_resource_legacy(
                    &router,
                    &parent_ctx,
                    router_access,
                    session_state,
                    uri,
                    depth,
                )
                .await
            }
        })
    }
}

// ============================================================================
// Tool Caller Implementation
// ============================================================================

use fastmcp_core::{MAX_TOOL_CALL_DEPTH, ToolCaller};

/// A wrapper that implements `ToolCaller` for a shared `Router`.
///
/// This allows handlers to call other tools from within tool/resource/prompt
/// handlers, enabling cross-component access.
pub(crate) struct RouterToolCaller {
    /// Access to the router without extending a server request's lifetime.
    router: RouterAccess,
    /// Session state for handlers.
    session_state: SessionState,
}

impl RouterToolCaller {
    /// Creates a new tool caller with the given router and session state.
    #[must_use]
    pub(crate) fn new(router: Arc<Router>, session_state: SessionState) -> Self {
        Self {
            router: RouterAccess::Shared(router),
            session_state,
        }
    }

    pub(crate) fn request_scoped(router: Weak<Router>, session_state: SessionState) -> Self {
        Self {
            router: RouterAccess::RequestScoped(router),
            session_state,
        }
    }

    fn from_access(router: RouterAccess, session_state: SessionState) -> Self {
        Self {
            router,
            session_state,
        }
    }

    async fn call_tool_legacy(
        router: &Router,
        parent_ctx: &McpContext,
        router_access: RouterAccess,
        session_state: SessionState,
        name: String,
        args: serde_json::Value,
        depth: u32,
    ) -> McpResult<ToolCallResult> {
        let operation_started_at = parent_ctx.cx().now();
        if let Some(error) = budget_error(parent_ctx) {
            return Err(error);
        }
        if !session_state.is_tool_enabled(&name) {
            return Err(McpError::new(
                McpErrorCode::MethodNotFound,
                format!("Tool '{name}' is disabled for this session"),
            ));
        }
        let entry = router
            .tools
            .get(&name)
            .ok_or_else(|| McpError::method_not_found(&format!("tool: {name}")))?;
        if !entry.legacy_enabled {
            return Err(McpError::method_not_found(&format!("tool: {name}")));
        }
        let handler = &entry.handler;
        let validation_result = if router.strict_input_validation {
            validate_strict(&entry.definition.input_schema, &args)
        } else {
            validate(&entry.definition.input_schema, &args)
        };
        if let Err(validation_errors) = validation_result {
            let error_messages: Vec<String> = validation_errors
                .iter()
                .map(|error| format!("{}: {}", error.path, error.message))
                .collect();
            return Err(McpError::invalid_params(format!(
                "Input validation failed: {}",
                error_messages.join("; ")
            )));
        }
        let handler_timeout =
            read_handler_timeout(parent_ctx.cx(), "tool_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            parent_ctx.cx().budget(),
            parent_ctx.budget(),
            handler_timeout,
            operation_started_at,
        );
        let child_ctx = with_nested_component_access(
            parent_ctx
                .clone()
                .with_operation_deadline(effective_budget.deadline),
            router_access.clone(),
            session_state.clone(),
            depth,
        );
        let outcome = run_handler_in_request(
            &child_ctx,
            parent_ctx.cx(),
            effective_budget,
            "tool",
            |request_cx| handler.call_async_in_request(&child_ctx, request_cx, args),
        )
        .await?;
        match outcome {
            Outcome::Ok(content) => Ok(ToolCallResult::success(
                content
                    .into_iter()
                    .map(|content| match content {
                        Content::Text { text } => ToolContentItem::Text { text },
                        Content::Image { data, mime_type } => {
                            ToolContentItem::Image { data, mime_type }
                        }
                        Content::Audio { data, mime_type } => {
                            ToolContentItem::Audio { data, mime_type }
                        }
                        Content::Resource { resource } => ToolContentItem::Resource {
                            uri: resource.uri,
                            mime_type: resource.mime_type,
                            text: resource.text,
                            blob: resource.blob,
                        },
                    })
                    .collect(),
            )),
            Outcome::Err(error) => {
                let error = sanitize_handler_error(parent_ctx.cx(), "tool", error);
                if is_framework_terminal_tool_error(error.code) {
                    return Err(error);
                }
                Ok(ToolCallResult::error(error.message))
            }
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => Err(sanitized_handler_panic(parent_ctx.cx(), "tool")),
        }
    }

    async fn call_tool_final(
        router: &Router,
        parent_ctx: &McpContext,
        router_access: RouterAccess,
        session_state: SessionState,
        name: String,
        args: serde_json::Value,
        depth: u32,
    ) -> McpResult<ToolCallResult> {
        let operation_started_at = parent_ctx.cx().now();
        if let Some(error) = budget_error(parent_ctx) {
            return Err(error);
        }
        if !session_state.is_tool_enabled(&name) || !parent_ctx.is_tool_enabled(&name) {
            return Err(McpError::new(
                McpErrorCode::MethodNotFound,
                format!("Tool '{name}' is disabled for this session"),
            ));
        }
        let entry = router
            .tools
            .get(&name)
            .ok_or_else(|| McpError::invalid_params(format!("Unknown tool: {name}")))?;
        let final_registration = entry
            .final_registration
            .as_ref()
            .ok_or_else(|| McpError::invalid_params(format!("Unknown tool: {name}")))?;
        let handler = &entry.handler;
        if handler.declares_final_mrtr() && parent_ctx.retained_continuation_partition().is_none() {
            return Err(McpError::invalid_params(
                MRTR_REQUIRES_BOUND_MODERN_CONNECTION,
            ));
        }
        let input_schema = final_registration.schemas.input.as_ref();
        let input_validation_failed = match input_schema {
            Some(schema) => {
                let validation = if router.strict_input_validation {
                    validate_strict(schema.schema(), &args)
                } else {
                    schema.validate(&args)
                };
                validation.is_err()
            }
            None if router.strict_input_validation => {
                validate_strict(&final_registration.final_definition.input_schema, &args).is_err()
            }
            None => false,
        };
        if input_validation_failed {
            return Ok(ToolCallResult::error(
                "Tool arguments do not match the declared input schema.",
            ));
        }
        let handler_timeout =
            read_handler_timeout(parent_ctx.cx(), "tool_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            parent_ctx.cx().budget(),
            parent_ctx.budget(),
            handler_timeout,
            operation_started_at,
        );
        let child_ctx = with_nested_component_access(
            parent_ctx
                .clone()
                .with_operation_deadline(effective_budget.deadline),
            router_access,
            session_state,
            depth,
        );
        let outcome = run_handler_in_request(
            &child_ctx,
            parent_ctx.cx(),
            effective_budget,
            "tool",
            |request_cx| {
                handler.call_final_outcome_async_resuming_in_request(
                    &child_ctx, request_cx, args, None,
                )
            },
        )
        .await?;
        match outcome {
            Outcome::Ok(result) => match result {
                FinalToolOutcome::Complete(complete) => Ok(ToolCallResult {
                    content: complete
                        .payload
                        .content
                        .into_iter()
                        .map(nested_tool_content_from_final)
                        .collect(),
                    is_error: complete.payload.is_error,
                }),
                FinalToolOutcome::InputRequired(_) => Err(McpError::internal_error(
                    "nested tool call cannot complete an input_required outcome",
                )),
                #[cfg(feature = "tasks")]
                FinalToolOutcome::CreateTask { .. } => Err(McpError::internal_error(
                    "nested tool call cannot create a final task",
                )),
            },
            Outcome::Err(error) => {
                let error = sanitize_handler_error(parent_ctx.cx(), "tool", error);
                if is_framework_terminal_final_tool_error(&error) {
                    return Err(error);
                }
                Ok(ToolCallResult::error(error.message))
            }
            Outcome::Cancelled(_) => Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => Err(sanitized_handler_panic(parent_ctx.cx(), "tool")),
        }
    }
}

impl ToolCaller for RouterToolCaller {
    fn call_tool<'a>(
        &'a self,
        parent_ctx: &'a McpContext,
        name: &'a str,
        args: serde_json::Value,
        depth: u32,
    ) -> Pin<
        Box<dyn std::future::Future<Output = fastmcp_core::McpResult<ToolCallResult>> + Send + 'a>,
    > {
        // Check recursion depth
        if depth > MAX_TOOL_CALL_DEPTH {
            return Box::pin(async move {
                Err(McpError::new(
                    McpErrorCode::InternalError,
                    format!("Maximum tool call depth ({}) exceeded", MAX_TOOL_CALL_DEPTH),
                ))
            });
        }

        // Clone what we need for the async block
        let parent_ctx = parent_ctx.clone();
        let name = name.to_string();
        let router_access = self.router.clone();
        let session_state = self.session_state.clone();
        let final_surface = parent_ctx.is_final_request_surface();

        Box::pin(async move {
            debug!(
                target: targets::HANDLER,
                "cross-component tool call; tool_key={}; depth={}; request={}; final_surface={}",
                safe_log_label(&name),
                depth,
                parent_ctx.request_id(),
                final_surface
            );
            let router = router_access.upgrade()?;
            if final_surface {
                Self::call_tool_final(
                    &router,
                    &parent_ctx,
                    router_access,
                    session_state,
                    name,
                    args,
                    depth,
                )
                .await
            } else {
                Self::call_tool_legacy(
                    &router,
                    &parent_ctx,
                    router_access,
                    session_state,
                    name,
                    args,
                    depth,
                )
                .await
            }
        })
    }
}

fn with_nested_component_access(
    ctx: McpContext,
    router: RouterAccess,
    state: SessionState,
    depth: u32,
) -> McpContext {
    ctx.with_resource_read_depth(depth)
        .with_tool_call_depth(depth)
        .with_prompt_get_depth(depth)
        .with_tool_caller(Arc::new(RouterToolCaller::from_access(
            router.clone(),
            state.clone(),
        )))
        .with_resource_reader(Arc::new(RouterResourceReader::from_access(
            router.clone(),
            state.clone(),
        )))
        .with_prompt_caller(Arc::new(RouterPromptCaller::from_access(router, state)))
}

// ============================================================================
// Prompt Caller Implementation
// ============================================================================

pub(crate) struct RouterPromptCaller {
    router: RouterAccess,
    session_state: SessionState,
}

impl RouterPromptCaller {
    #[must_use]
    pub(crate) fn new(router: Arc<Router>, session_state: SessionState) -> Self {
        Self {
            router: RouterAccess::Shared(router),
            session_state,
        }
    }

    pub(crate) fn request_scoped(router: Weak<Router>, session_state: SessionState) -> Self {
        Self {
            router: RouterAccess::RequestScoped(router),
            session_state,
        }
    }

    fn from_access(router: RouterAccess, session_state: SessionState) -> Self {
        Self {
            router,
            session_state,
        }
    }
}

impl PromptCaller for RouterPromptCaller {
    fn get_prompt<'a>(
        &'a self,
        parent_ctx: &'a McpContext,
        name: &'a str,
        arguments: HashMap<String, String>,
        depth: u32,
    ) -> Pin<
        Box<dyn std::future::Future<Output = fastmcp_core::McpResult<PromptGetResult>> + Send + 'a>,
    > {
        if depth > fastmcp_core::MAX_PROMPT_GET_DEPTH {
            return Box::pin(async move {
                Err(McpError::new(
                    McpErrorCode::InternalError,
                    format!(
                        "Maximum prompt get depth ({}) exceeded",
                        fastmcp_core::MAX_PROMPT_GET_DEPTH
                    ),
                ))
            });
        }

        let parent_ctx = parent_ctx.clone();
        let name = name.to_string();
        let router_access = self.router.clone();
        let session_state = self.session_state.clone();
        let final_surface = parent_ctx.is_final_request_surface();

        Box::pin(async move {
            debug!(
                target: targets::HANDLER,
                "cross-component prompt get; prompt_key={}; depth={}; request={}; final_surface={}",
                safe_log_label(&name),
                depth,
                parent_ctx.request_id(),
                final_surface
            );
            let router = router_access.upgrade()?;
            if final_surface {
                Self::get_prompt_final(
                    &router,
                    &parent_ctx,
                    router_access,
                    session_state,
                    name,
                    arguments,
                    depth,
                )
                .await
            } else {
                Self::get_prompt_legacy(
                    &router,
                    &parent_ctx,
                    router_access,
                    session_state,
                    name,
                    arguments,
                    depth,
                )
                .await
            }
        })
    }
}

impl RouterPromptCaller {
    async fn get_prompt_legacy(
        router: &Router,
        parent_ctx: &McpContext,
        router_access: RouterAccess,
        session_state: SessionState,
        name: String,
        arguments: HashMap<String, String>,
        depth: u32,
    ) -> McpResult<PromptGetResult> {
        let operation_started_at = parent_ctx.cx().now();
        if let Some(error) = budget_error(parent_ctx) {
            return Err(error);
        }
        if !session_state.is_prompt_enabled(&name) {
            return Err(McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt '{name}' is disabled for this session"),
            ));
        }
        let handler = router.prompts.get(&name).ok_or_else(|| {
            McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt not found: {name}"),
            )
        })?;
        if router.final_only_prompts.contains(&name) {
            return Err(McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt not found: {name}"),
            ));
        }
        let description = crate::catch_extension_unwind(|| handler.definition().description)
            .map_err(|_payload| sanitized_handler_panic(parent_ctx.cx(), "prompt_definition"))?;
        let handler_timeout =
            read_handler_timeout(parent_ctx.cx(), "prompt_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            parent_ctx.cx().budget(),
            parent_ctx.budget(),
            handler_timeout,
            operation_started_at,
        );
        let child_ctx = with_nested_component_access(
            parent_ctx
                .clone()
                .with_operation_deadline(effective_budget.deadline),
            router_access,
            session_state,
            depth,
        );
        let outcome = run_handler_in_request(
            &child_ctx,
            parent_ctx.cx(),
            effective_budget,
            "prompt",
            |request_cx| handler.get_async_in_request(&child_ctx, request_cx, arguments),
        )
        .await?;
        let messages = match outcome {
            Outcome::Ok(messages) => messages,
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(parent_ctx.cx(), "prompt", error));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(parent_ctx.cx(), "prompt"));
            }
        };
        Ok(PromptGetResult {
            description,
            messages: messages
                .into_iter()
                .map(|message| PromptMessageItem {
                    role: match message.role {
                        Role::User => PromptMessageRole::User,
                        Role::Assistant => PromptMessageRole::Assistant,
                    },
                    text: match message.content {
                        Content::Text { text } => Some(text),
                        _ => None,
                    },
                })
                .collect(),
        })
    }

    async fn get_prompt_final(
        router: &Router,
        parent_ctx: &McpContext,
        router_access: RouterAccess,
        session_state: SessionState,
        name: String,
        arguments: HashMap<String, String>,
        depth: u32,
    ) -> McpResult<PromptGetResult> {
        let operation_started_at = parent_ctx.cx().now();
        if let Some(error) = budget_error(parent_ctx) {
            return Err(error);
        }
        if !session_state.is_prompt_enabled(&name) || !parent_ctx.is_prompt_enabled(&name) {
            return Err(McpError::new(
                McpErrorCode::PromptNotFound,
                format!("Prompt '{name}' is disabled for this session"),
            ));
        }
        let handler = router
            .prompts
            .get(&name)
            .ok_or_else(|| McpError::invalid_params(format!("Unknown prompt: {name}")))?;
        let final_registration = router.final_prompts.get(&name).ok_or_else(|| {
            McpError::invalid_params("prompt is registered only for exact MCP 2024-11-05 dispatch")
        })?;
        if handler.declares_final_mrtr() && parent_ctx.retained_continuation_partition().is_none() {
            return Err(McpError::invalid_params(
                MRTR_REQUIRES_BOUND_MODERN_CONNECTION,
            ));
        }
        admit_declared_final_prompt_arguments(
            final_registration
                .definition
                .arguments
                .as_deref()
                .unwrap_or_default(),
            arguments.keys(),
        )?;
        let handler_timeout =
            read_handler_timeout(parent_ctx.cx(), "prompt_timeout", || handler.timeout())?;
        let effective_budget = compose_handler_budget(
            parent_ctx.cx().budget(),
            parent_ctx.budget(),
            handler_timeout,
            operation_started_at,
        );
        let child_ctx = with_nested_component_access(
            parent_ctx
                .clone()
                .with_operation_deadline(effective_budget.deadline),
            router_access,
            session_state,
            depth,
        );
        let outcome = run_handler_in_request(
            &child_ctx,
            parent_ctx.cx(),
            effective_budget,
            "prompt",
            |request_cx| {
                handler.get_final_outcome_async_in_request(&child_ctx, request_cx, arguments)
            },
        )
        .await?;
        let result = match outcome {
            Outcome::Ok(result) => result,
            Outcome::Err(error) => {
                return Err(sanitize_handler_error(parent_ctx.cx(), "prompt", error));
            }
            Outcome::Cancelled(_) => return Err(McpError::request_cancelled()),
            Outcome::Panicked(_payload) => {
                return Err(sanitized_handler_panic(parent_ctx.cx(), "prompt"));
            }
        };
        admit_final_prompt_outcome(final_registration.uri_use_policy, &result)?;
        let FinalMethodOutcome::Complete(complete) = result else {
            return Err(McpError::internal_error(
                "nested prompt get cannot complete an input_required outcome",
            ));
        };
        Ok(PromptGetResult {
            description: complete.payload.description,
            messages: complete
                .payload
                .messages
                .into_iter()
                .map(|message| PromptMessageItem {
                    role: match message.role {
                        Role::User => PromptMessageRole::User,
                        Role::Assistant => PromptMessageRole::Assistant,
                    },
                    text: match message.content {
                        fastmcp_protocol::common_types::ContentBlock::Text { text, .. } => {
                            Some(text)
                        }
                        _ => None,
                    },
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod safe_log_label_tests {
    use super::{LOG_LABEL_HASH_INPUT_LIMIT, safe_log_label};

    #[test]
    fn safe_label_is_deterministic_non_verbatim_metadata() {
        let canary = "router-log-canary-secret";
        let first = format!("{}", safe_log_label(canary));
        let second = format!("{:?}", safe_log_label(canary));

        assert_eq!(first, second);
        assert!(first.contains(&format!("bytes={}", canary.len())));
        assert!(first.contains("sha256_prefix="));
        assert!(!first.contains(canary));
        assert_ne!(first, format!("{}", safe_log_label("different-label")));
    }

    #[test]
    fn safe_label_hashing_is_bounded_for_oversized_input() {
        let oversized = "x".repeat(LOG_LABEL_HASH_INPUT_LIMIT + 37);
        let rendered = format!("{}", safe_log_label(&oversized));

        assert!(rendered.contains(&format!("bytes={}", oversized.len())));
        assert!(rendered.contains(&format!("hashed_prefix_bytes={LOG_LABEL_HASH_INPUT_LIMIT}")));
        assert!(!rendered.contains(&oversized));
    }

    #[test]
    fn source_has_no_verbatim_argument_or_label_log_formats() {
        let source = include_str!("router.rs");
        let forbidden = [
            concat!("Tool ", "arguments:"),
            concat!("Prompt ", "arguments:"),
            concat!("Calling ", "tool: {}"),
            concat!("Reading ", "resource: {}"),
            concat!("Getting ", "prompt: {}"),
            concat!("Cross-component tool ", "call: {}"),
            concat!("Cross-component resource ", "read: {}"),
            concat!("Invalid URI template ", "'{}'"),
        ];

        for format in forbidden {
            assert!(!source.contains(format), "raw log format remains: {format}");
        }
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::{
        FinalCatalogKind, FinalCatalogQuery, decode_cursor_offset,
        decode_final_catalog_cursor_offset, encode_cursor_offset, encode_final_catalog_cursor,
    };

    #[test]
    fn roundtrip_zero() {
        let encoded = encode_cursor_offset(0);
        let decoded = decode_cursor_offset(Some(&encoded)).unwrap();
        assert_eq!(decoded, 0);
    }

    #[test]
    fn roundtrip_large_offset() {
        let encoded = encode_cursor_offset(12345);
        let decoded = decode_cursor_offset(Some(&encoded)).unwrap();
        assert_eq!(decoded, 12345);
    }

    #[test]
    fn none_cursor_returns_zero() {
        assert_eq!(decode_cursor_offset(None).unwrap(), 0);
    }

    #[test]
    fn invalid_base64_returns_error() {
        let err = decode_cursor_offset(Some("not-valid-base64!!!")).unwrap_err();
        assert!(err.message.contains("base64"));
    }

    #[test]
    fn valid_base64_but_not_json_returns_error() {
        let encoded =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"not json");
        let err = decode_cursor_offset(Some(&encoded)).unwrap_err();
        assert!(err.message.contains("JSON"));
    }

    #[test]
    fn valid_json_but_no_offset_returns_error() {
        let payload = serde_json::json!({"other": 1});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        let err = decode_cursor_offset(Some(&encoded)).unwrap_err();
        assert!(err.message.contains("offset"));
    }

    #[test]
    fn final_catalog_cursor_binds_the_catalog_revision_and_query() {
        let include_tags = vec!["Visible".to_owned(), "visible".to_owned()];
        let exclude_tags = vec!["excluded".to_owned()];
        let query = FinalCatalogQuery::from_tag_filters(Some(&include_tags), Some(&exclude_tags));
        let cursor = encode_final_catalog_cursor(FinalCatalogKind::Resources, 41, &query, 7, None);
        let equivalent_include_tags = vec!["visible".to_owned()];
        let equivalent_query = FinalCatalogQuery::from_tag_filters(
            Some(&equivalent_include_tags),
            Some(&exclude_tags),
        );
        assert_eq!(
            decode_final_catalog_cursor_offset(
                Some(&cursor),
                FinalCatalogKind::Resources,
                41,
                &equivalent_query,
                None,
                8,
            )
            .expect("a final cursor accepts a semantically equivalent canonical query"),
            7
        );
        let stale = decode_final_catalog_cursor_offset(
            Some(&cursor),
            FinalCatalogKind::Resources,
            42,
            &query,
            None,
            8,
        )
        .expect_err("changing only the catalog revision rejects a stale continuation");
        assert!(stale.message.contains("stale catalog revision"));

        let wrong_kind = decode_final_catalog_cursor_offset(
            Some(&cursor),
            FinalCatalogKind::Prompts,
            41,
            &query,
            None,
            8,
        )
        .expect_err("changing only the list method rejects a cross-catalog continuation");
        assert!(wrong_kind.message.contains("another list method"));

        let other_include_tags = vec!["other".to_owned()];
        let other_query =
            FinalCatalogQuery::from_tag_filters(Some(&other_include_tags), Some(&exclude_tags));
        let wrong_query = decode_final_catalog_cursor_offset(
            Some(&cursor),
            FinalCatalogKind::Resources,
            41,
            &other_query,
            None,
            8,
        )
        .expect_err("changing only the request filters rejects the continuation");
        assert!(wrong_query.message.contains("query filters"));

        let out_of_range =
            encode_final_catalog_cursor(FinalCatalogKind::Resources, 41, &query, 8, None);
        let range_error = decode_final_catalog_cursor_offset(
            Some(&out_of_range),
            FinalCatalogKind::Resources,
            41,
            &query,
            None,
            8,
        )
        .expect_err("an offset at the end of a catalog is never a router-minted continuation");
        assert!(
            range_error
                .message
                .contains("outside the requested catalog page")
        );

        let visible =
            encode_final_catalog_cursor(FinalCatalogKind::Tools, 41, &query, 0, Some("digest-a"));
        let visibility_error = decode_final_catalog_cursor_offset(
            Some(&visible),
            FinalCatalogKind::Tools,
            41,
            &query,
            Some("digest-b"),
            2,
        )
        .expect_err("changing only session visibility rejects the continuation");
        assert!(visibility_error.message.contains("session visibility"));
        assert_eq!(
            decode_final_catalog_cursor_offset(
                Some(&visible),
                FinalCatalogKind::Tools,
                41,
                &query,
                Some("digest-a"),
                2,
            )
            .expect("matching session visibility admits the continuation"),
            0
        );
    }
}

#[cfg(test)]
mod tag_filter_tests {
    use super::TagFilters;

    #[test]
    fn no_filters_matches_anything() {
        let f = TagFilters::default();
        assert!(f.matches(&[]));
        assert!(f.matches(&["a".to_string()]));
    }

    #[test]
    fn include_filter_requires_all_tags() {
        let include = vec!["a".to_string(), "b".to_string()];
        let f = TagFilters::new(Some(&include), None);
        assert!(f.matches(&["a".to_string(), "b".to_string(), "c".to_string()]));
        assert!(!f.matches(&["a".to_string()])); // missing "b"
    }

    #[test]
    fn exclude_filter_rejects_any_tag() {
        let exclude = vec!["x".to_string()];
        let f = TagFilters::new(None, Some(&exclude));
        assert!(f.matches(&["a".to_string(), "b".to_string()]));
        assert!(!f.matches(&["a".to_string(), "x".to_string()]));
    }

    #[test]
    fn include_and_exclude_combined() {
        let include = vec!["a".to_string()];
        let exclude = vec!["b".to_string()];
        let f = TagFilters::new(Some(&include), Some(&exclude));
        assert!(f.matches(&["a".to_string()]));
        assert!(!f.matches(&["a".to_string(), "b".to_string()])); // excluded
        assert!(!f.matches(&["c".to_string()])); // missing "a"
    }

    #[test]
    fn case_insensitive_matching() {
        let include = vec!["Alpha".to_string()];
        let f = TagFilters::new(Some(&include), None);
        assert!(f.matches(&["alpha".to_string()]));
        assert!(f.matches(&["ALPHA".to_string()]));
    }

    #[test]
    fn empty_include_array_passes_all() {
        let include: Vec<String> = vec![];
        let f = TagFilters::new(Some(&include), None);
        assert!(f.matches(&[]));
        assert!(f.matches(&["anything".to_string()]));
    }

    #[test]
    fn tag_filters_debug() {
        let f = TagFilters::default();
        let debug = format!("{:?}", f);
        assert!(debug.contains("TagFilters"));
    }
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use crate::bidirectional::MrtrInputResponse;
    use crate::handler::{
        CompletionHandler, DEFAULT_FINAL_RESOURCE_TTL_MS, FinalElicitationContextExt,
        FinalToolSchemaAuthority, PromptHandler, ResourceHandler, ToolHandler,
        UpstreamFinalToolSchemaRegistration,
    };
    use crate::http_admission::{HttpAdmissionLimits, HttpEndpointConfig, admit_modern_post};
    #[cfg(feature = "tasks")]
    use crate::tasks::{
        ApplicationTaskSupervisor, FinalTaskSupervisorFuture, FinalTaskSupervisorHandoff,
        FinalTaskWorkDescriptor,
    };
    #[cfg(feature = "tasks")]
    use crate::{FinalTaskRuntimeConfig, FinalTaskStore, InMemoryFinalTaskStore};
    use asupersync::channel::oneshot;
    use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
    use asupersync::types::CancelKind;
    use fastmcp_core::{ClientCapabilityInfo, McpContext, McpResult, SessionState};
    use fastmcp_protocol::common_types::{
        Annotations, ContentBlock, EmbeddedResourceContents, OpenMetadata, RawIcon,
    };
    use fastmcp_protocol::{
        CompleteResult, CompletionValues, Content, CoreResultDiscriminatorPolicy, DecodedResult,
        FinalCallToolResult, FinalCompletionParams, FinalGetPromptResult, FinalPromptMessage,
        LegacyCompletionParams, LegacyResourceContent, Prompt, PromptArgument, PromptMessage,
        Resource, ResourceContent, ResourceTemplate, ResultPeerEra, Tool, decode_peer_result,
    };
    use std::collections::BTreeMap;
    use std::fmt;
    #[cfg(feature = "tasks")]
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;

    fn request_context(
        cx: &Cx,
        request_id: u64,
        budget: Budget,
        state: &SessionState,
    ) -> McpContext {
        McpContext::with_state(cx.clone(), request_id, state.clone()).with_budget_ceiling(budget)
    }

    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(|task_cx| {
            if std::mem::replace(&mut yielded, true) {
                Poll::Ready(())
            } else {
                task_cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    #[test]
    fn legacy_result_adapters_emit_exact_2024_defaults() {
        let tool = CallToolResult {
            content: legacy_contents_from_handler(vec![
                Content::Text {
                    text: "ready".to_owned(),
                },
                Content::Image {
                    data: "aGVsbG8=".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
                Content::Resource {
                    resource: ResourceContent {
                        uri: "file:///tool.txt".to_owned(),
                        mime_type: Some("text/plain".to_owned()),
                        text: Some("tool resource".to_owned()),
                        blob: None,
                    },
                },
            ])
            .expect("handler content is representable by the exact legacy union"),
            is_error: false,
            meta: None,
            additional: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_value(tool).expect("legacy tool result serializes"),
            serde_json::json!({
                "content": [
                    {"type": "text", "text": "ready"},
                    {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
                    {
                        "type": "resource",
                        "resource": {
                            "uri": "file:///tool.txt",
                            "mimeType": "text/plain",
                            "text": "tool resource"
                        }
                    }
                ]
            })
        );

        let resource = ReadResourceResult {
            contents: legacy_resource_contents_from_handler(vec![ResourceContent {
                uri: "file:///report.bin".to_owned(),
                mime_type: Some("application/octet-stream".to_owned()),
                text: None,
                blob: Some("AAEC".to_owned()),
            }])
            .expect("handler resource is representable by the exact legacy union"),
            meta: None,
            additional: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_value(resource).expect("legacy resource result serializes"),
            serde_json::json!({
                "contents": [{
                    "uri": "file:///report.bin",
                    "mimeType": "application/octet-stream",
                    "blob": "AAEC"
                }]
            })
        );

        let prompt = GetPromptResult {
            description: Some("ask a question".to_owned()),
            messages: legacy_prompt_messages_from_handler(vec![PromptMessage {
                role: fastmcp_protocol::Role::User,
                content: Content::Text {
                    text: "summarize".to_owned(),
                },
            }])
            .expect("handler prompt message is representable by the exact legacy union"),
            meta: None,
            additional: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_value(prompt).expect("legacy prompt result serializes"),
            serde_json::json!({
                "description": "ask a question",
                "messages": [{
                    "role": "user",
                    "content": {"type": "text", "text": "summarize"}
                }]
            })
        );
    }

    #[test]
    fn legacy_resource_adapter_preserves_open_members_when_promoted() {
        let resource = LegacyResourceContent::Text {
            uri: "file:///open.txt".to_owned(),
            text: "preserve me".to_owned(),
            mime_type: Some("text/plain".to_owned()),
            additional: BTreeMap::from([
                (
                    "_meta".to_owned(),
                    serde_json::json!({"legacy": "uninterpreted"}),
                ),
                (
                    "com.example/legacy".to_owned(),
                    serde_json::json!({"retained": true}),
                ),
            ]),
        };

        let promoted = promote_legacy_resource_content(resource)
            .expect("schema-valid legacy resource is promotable");
        assert_eq!(
            serde_json::to_value(promoted).expect("promoted resource serializes"),
            serde_json::json!({
                "uri": "file:///open.txt",
                "mimeType": "text/plain",
                "text": "preserve me",
                "_meta": {"legacy": "uninterpreted"},
                "com.example/legacy": {"retained": true}
            })
        );
    }

    #[test]
    fn legacy_content_adapter_rejects_only_audio_without_mutating_baseline() {
        let baseline = vec![Content::Image {
            data: "aGVsbG8=".to_owned(),
            mime_type: "image/png".to_owned(),
        }];
        let baseline_wire = serde_json::to_value(
            legacy_contents_from_handler(baseline.clone())
                .expect("baseline content is representable by the legacy adapter"),
        )
        .expect("baseline legacy content serializes");
        let planted = vec![Content::Audio {
            data: "aGVsbG8=".to_owned(),
            mime_type: "image/png".to_owned(),
        }];

        assert!(
            legacy_contents_from_handler(planted).is_err(),
            "changing only the content discriminator to audio rejects the legacy adapter"
        );
        assert_eq!(
            serde_json::to_value(
                legacy_contents_from_handler(baseline)
                    .expect("baseline remains representable after rejection"),
            )
            .expect("baseline legacy content remains serializable"),
            baseline_wire,
            "rejected legacy conversion cannot mutate the accepted baseline"
        );
    }

    fn spawn_owned_modern_request(
        runtime: &RuntimeHandle,
        router: Arc<Router>,
        request_context_id: u64,
        wire_id: &'static str,
        label: &'static str,
        control_sender: Option<oneshot::Sender<Cx>>,
    ) -> oneshot::Receiver<McpResult<serde_json::Value>> {
        let (response_sender, response_receiver) = oneshot::channel();
        runtime
            .try_spawn_with_cx(move |request_cx| {
                if let Some(control_sender) = control_sender {
                    control_sender
                        .send_blocking(request_cx.clone())
                        .expect("the cancellation controller remains available");
                }
                // Modern tools/call computes an MRTR exchange binding, which
                // requires a session cache partition on the request context.
                let request_ctx =
                    McpContext::with_state(request_cx, request_context_id, SessionState::new());
                let request = JsonRpcRequest::new(
                    "tools/call",
                    Some(serde_json::json!({
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                        "name": "concurrent-modern-tool",
                        "arguments": {"request": label},
                    })),
                    wire_id,
                );
                async move {
                    let result = router.dispatch_stateless_owned(request_ctx, request).await;
                    response_sender
                        .send_blocking(result)
                        .expect("the modern dispatch observer remains available");
                }
            })
            .expect("the runtime admits the request owner");
        response_receiver
    }

    // ── Stub handlers ──────────────────────────────────────────────────

    struct NamedTool {
        name: String,
        tags: Vec<String>,
    }

    impl NamedTool {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                tags: vec![],
            }
        }
        fn with_tags(name: &str, tags: Vec<String>) -> Self {
            Self {
                name: name.to_string(),
                tags,
            }
        }
    }

    impl ToolHandler for NamedTool {
        fn definition(&self) -> Tool {
            Tool {
                name: self.name.clone(),
                description: Some(format!("Tool {}", self.name)),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: self.tags.clone(),
                annotations: None,
            }
        }
        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text(format!("called {}", self.name))])
        }
    }

    struct RouterProgressTool;

    impl ToolHandler for RouterProgressTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "router-progress-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["total"],
                    "properties": {"total": {"type": "number"}},
                    "additionalProperties": false,
                }),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
            let total = args
                .get("total")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| McpError::invalid_params("router progress total is required"))?;
            ctx.report_progress_with_total(12_000.0, total, Some("router-final"));
            Ok(vec![Content::text("progress emitted")])
        }
    }

    struct DuplicateInvariantTool {
        label: &'static str,
        schema_property: &'static str,
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
    }

    impl ToolHandler for DuplicateInvariantTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "duplicate-invariant-tool".to_owned(),
                description: Some(self.label.to_owned()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        (self.schema_property): {"type": "boolean"}
                    },
                    "additionalProperties": false
                }),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![self.label.to_owned()],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Content::text(self.label)])
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                FinalCallToolResult {
                    content: vec![ContentBlock::text(self.label)],
                    is_error: false,
                    structured_content: None,
                },
            )))
        }
    }

    struct EraBoundaryResource {
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
    }

    impl ResourceHandler for EraBoundaryResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "mcp://era-boundary".to_owned(),
                name: "era-boundary".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ResourceContent {
                uri: "mcp://era-boundary".to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: Some("legacy resource result".to_owned()),
                blob: None,
            }])
        }

        fn read_final(
            &self,
            _ctx: &McpContext,
        ) -> McpResult<CompleteResult<FinalReadResourceResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompleteResult::new(
                FinalReadResourceResult {
                    contents: vec![EmbeddedResourceContents::Text {
                        uri: AbsoluteUri::parse("mcp://era-boundary")
                            .expect("era-boundary resource URI is valid"),
                        text: "final resource result".to_owned(),
                        mime_type: Some("text/plain".to_owned()),
                        meta: None,
                        additional: BTreeMap::new(),
                    }],
                    ttl_ms: CacheTtl::milliseconds(DEFAULT_FINAL_RESOURCE_TTL_MS),
                    cache_scope: CacheScope::Private,
                },
                empty_final_result_meta()?,
            ))
        }
    }

    /// A normal-registration candidate whose final output-schema field is
    /// invalid. It is used to prove admission rejects without catalog change.
    struct InvalidFinalSchemaNamedTool {
        name: String,
        tags: Vec<String>,
    }

    impl InvalidFinalSchemaNamedTool {
        fn with_tags(name: &str, tags: Vec<String>) -> Self {
            Self {
                name: name.to_owned(),
                tags,
            }
        }
    }

    impl ToolHandler for InvalidFinalSchemaNamedTool {
        fn definition(&self) -> Tool {
            Tool {
                name: self.name.clone(),
                description: Some(format!("Invalid-schema tool {}", self.name)),
                input_schema: serde_json::json!({"type": "object"}),
                // The final field itself must be a JSON object. This scalar
                // schema document is deliberately invalid for local normal
                // registration.
                output_schema: Some(serde_json::json!(false)),
                icon: None,
                version: None,
                tags: self.tags.clone(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text(format!("called {}", self.name))])
        }
    }

    fn final_tools_list_request(
        cursor: Option<&str>,
        include_tags: Option<Vec<&str>>,
        exclude_tags: Option<Vec<&str>>,
        id: i64,
    ) -> JsonRpcRequest {
        let mut params = serde_json::Map::new();
        params.insert(
            "_meta".to_owned(),
            serde_json::json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            }),
        );
        if let Some(cursor) = cursor {
            params.insert("cursor".to_owned(), serde_json::json!(cursor));
        }
        if let Some(include_tags) = include_tags {
            params.insert("includeTags".to_owned(), serde_json::json!(include_tags));
        }
        if let Some(exclude_tags) = exclude_tags {
            params.insert("excludeTags".to_owned(), serde_json::json!(exclude_tags));
        }
        JsonRpcRequest::new("tools/list", Some(serde_json::Value::Object(params)), id)
    }

    fn mounted_tool_router() -> Router {
        let mut source = Router::new();
        source
            .add_tool(NamedTool::with_tags("first", vec!["visible".to_owned()]))
            .expect("tool registration succeeds");
        source
            .add_tool(NamedTool::with_tags("second", vec!["visible".to_owned()]))
            .expect("tool registration succeeds");
        source
            .add_tool(NamedTool::with_tags(
                "excluded",
                vec!["visible".to_owned(), "excluded".to_owned()],
            ))
            .expect("tool registration succeeds");
        source
            .add_tool(NamedTool::with_tags("other", vec!["other".to_owned()]))
            .expect("tool registration succeeds");

        let mut router = Router::new();
        let mounted = router.mount_tools(source, Some("peer"));
        assert_eq!(mounted.tools, 4);
        router
    }

    static MACRO_DUAL_ERA_TOOL_CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Serializes the tests that reset and assert the shared call counter;
    /// concurrent resets interleave and turn the absolute counts flaky.
    static MACRO_DUAL_ERA_TOOL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn final_tool_complete_result(
        payload: FinalCallToolResult,
    ) -> CompleteResult<FinalCallToolResult> {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
            "name": "macro_dual_era_tool",
        });
        let request = CoreRequest::decode(ProtocolEra::Modern2026, "tools/call", Some(&params))
            .expect("test final tools/call request");
        let mut wire = serde_json::to_value(payload).expect("final tool payload serializes");
        wire.as_object_mut()
            .expect("final tool payload is an object")
            .insert("resultType".to_owned(), serde_json::json!("complete"));
        let encoded = serde_json::to_string(&wire).expect("final tool wire serializes");
        let CoreResult::Final(FinalCoreResult::ToolsCall { result, .. }) = request
            .decode_result(&encoded)
            .expect("typed final tools/call result")
        else {
            panic!("typed final tools/call result is selected");
        };
        result
    }

    #[fastmcp_derive::tool]
    fn macro_dual_era_tool() -> CompleteResult<FinalCallToolResult> {
        MACRO_DUAL_ERA_TOOL_CALLS.fetch_add(1, Ordering::SeqCst);
        final_tool_complete_result(FinalCallToolResult {
            content: vec![ContentBlock::text("macro final tool result")],
            is_error: false,
            structured_content: Some(serde_json::json!({"weather":"clear"})),
        })
    }

    struct SchemaBoundaryTool {
        final_calls: Arc<AtomicUsize>,
        legacy_calls: Arc<AtomicUsize>,
        output_matches_schema: bool,
        output_is_error: bool,
        output_has_unevaluated_property: bool,
        invalid_final_input_schema: bool,
        missing_final_input_object_type: bool,
        invalid_final_output_schema: bool,
    }

    impl ToolHandler for SchemaBoundaryTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "schema-boundary-tool".to_owned(),
                description: None,
                input_schema: if self.invalid_final_input_schema {
                    serde_json::json!(42)
                } else if self.missing_final_input_object_type {
                    serde_json::json!({"properties": {"value": {"type": "string"}}})
                } else {
                    serde_json::json!({
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "required": ["value"],
                        "properties": {"value": {"type": "string"}},
                        "unevaluatedProperties": false,
                    })
                },
                output_schema: Some(if self.invalid_final_output_schema {
                    serde_json::json!(42)
                } else {
                    serde_json::json!({
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "properties": {
                            "accepted": {"type": "boolean"},
                            "error": {
                                "enum": ["input-validation", "handler"]
                            }
                        },
                        "anyOf": [
                            {"required": ["accepted"]},
                            {"required": ["error"]}
                        ],
                        "unevaluatedProperties": false,
                    })
                }),
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Content::text("legacy schema-boundary result")])
        }

        fn final_tool_error_structured_content(
            &self,
            kind: ToolErrorKind,
        ) -> Option<serde_json::Value> {
            Some(match kind {
                ToolErrorKind::InputValidation => {
                    serde_json::json!({"error": "input-validation"})
                }
                ToolErrorKind::Handler => serde_json::json!({"error": "handler"}),
            })
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            let structured_content = if !self.output_matches_schema {
                serde_json::json!({"accepted": "not-a-boolean"})
            } else if self.output_has_unevaluated_property {
                serde_json::json!({"accepted": true, "unexpected": true})
            } else {
                serde_json::json!({"accepted": true})
            };
            Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                FinalCallToolResult {
                    content: vec![ContentBlock::text("final schema-boundary result")],
                    is_error: self.output_is_error,
                    structured_content: Some(structured_content),
                },
            )))
        }
    }

    struct UpstreamScalarSchemaTool {
        registered_proxy: bool,
    }

    impl ToolHandler for UpstreamScalarSchemaTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "upstream-scalar-schema-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: Some(serde_json::json!(false)),
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn final_tool_schema_authority(&self) -> FinalToolSchemaAuthority {
            // This public, forgeable label must not bypass local validation.
            FinalToolSchemaAuthority::Upstream
        }

        fn upstream_final_tool_schema_registration(
            &self,
        ) -> Option<UpstreamFinalToolSchemaRegistration> {
            self.registered_proxy
                .then(UpstreamFinalToolSchemaRegistration::exact_proxy)
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy")])
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                FinalCallToolResult {
                    content: vec![ContentBlock::text("upstream")],
                    is_error: false,
                    structured_content: Some(serde_json::json!({"upstream": true})),
                },
            )))
        }
    }

    /// A dual-era tool whose admitted output schema and handlers deliberately
    /// differ across registrations. It proves a successful replacement commits
    /// both the handler and final schema together.
    struct AdmittedSchemaReplacementTool {
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
        legacy_label: &'static str,
        output_schema: serde_json::Value,
        structured_content: Option<serde_json::Value>,
    }

    impl ToolHandler for AdmittedSchemaReplacementTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "admitted-schema-replacement-tool".to_owned(),
                description: Some(self.legacy_label.to_owned()),
                input_schema: serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "unevaluatedProperties": false,
                }),
                output_schema: Some(self.output_schema.clone()),
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Content::text(self.legacy_label)])
        }

        fn final_tool_error_structured_content(
            &self,
            kind: ToolErrorKind,
        ) -> Option<serde_json::Value> {
            match self
                .output_schema
                .get("type")
                .and_then(serde_json::Value::as_str)
            {
                Some("string") => Some(serde_json::json!(match kind {
                    ToolErrorKind::InputValidation => "input-validation-error",
                    ToolErrorKind::Handler => "handler-error",
                })),
                Some("boolean") => Some(serde_json::json!(matches!(kind, ToolErrorKind::Handler))),
                Some("null") => Some(serde_json::Value::Null),
                Some("object") => Some(serde_json::json!({
                    "error": match kind {
                        ToolErrorKind::InputValidation => "input-validation",
                        ToolErrorKind::Handler => "handler",
                    }
                })),
                _ => None,
            }
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                FinalCallToolResult {
                    content: vec![ContentBlock::text(self.legacy_label)],
                    is_error: false,
                    structured_content: self.structured_content.clone(),
                },
            )))
        }
    }

    #[derive(Clone, Copy)]
    enum ErrorMapperMode {
        Complete,
        MissingHandler,
        InvalidHandler,
        OversizedHandler,
    }

    struct ErrorMappedTool {
        mode: ErrorMapperMode,
        calls: Arc<AtomicUsize>,
    }

    impl ToolHandler for ErrorMappedTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "error-mapped-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["value"],
                    "properties": {"value": {"type": "string"}},
                    "additionalProperties": false
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "required": ["error"],
                    "properties": {"error": {"type": "string"}},
                    "additionalProperties": false
                })),
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn final_tool_error_structured_content(
            &self,
            kind: ToolErrorKind,
        ) -> Option<serde_json::Value> {
            match (self.mode, kind) {
                (ErrorMapperMode::MissingHandler, ToolErrorKind::Handler) => None,
                (ErrorMapperMode::InvalidHandler, ToolErrorKind::Handler) => {
                    Some(serde_json::json!({"error": 7}))
                }
                (ErrorMapperMode::OversizedHandler, ToolErrorKind::Handler) => Some(
                    serde_json::json!({"error": "x".repeat(MAX_FINAL_TOOL_ERROR_STRUCTURED_CONTENT_BYTES)}),
                ),
                (_, ToolErrorKind::InputValidation) => {
                    Some(serde_json::json!({"error": "input-validation"}))
                }
                (_, ToolErrorKind::Handler) => Some(serde_json::json!({"error": "handler"})),
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(McpError::new(
                McpErrorCode::ToolExecutionError,
                "mapped handler failure",
            ))
        }
    }

    fn final_tools_call_request(
        name: &str,
        arguments: serde_json::Value,
        id: i64,
    ) -> JsonRpcRequest {
        JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": name,
                "arguments": arguments,
            })),
            id,
        )
    }

    fn admit_http_wire(
        method: &str,
        target: &str,
        body: &[u8],
    ) -> (JsonRpcRequest, Option<Arc<str>>) {
        let endpoint = HttpEndpointConfig::new(
            "/mcp",
            HttpAdmissionLimits::new(16, 8_192, 65_536).expect("nonzero HTTP limits"),
        )
        .expect("HTTP endpoint config");
        let headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
            ("MCP-Protocol-Version".to_owned(), "2026-07-28".to_owned()),
            ("Mcp-Method".to_owned(), method.to_owned()),
            ("Mcp-Name".to_owned(), target.to_owned()),
        ];
        admit_modern_post(&endpoint, "POST", "/mcp", &headers, body)
            .expect("wire request admits through HTTP")
            .into_request_and_raw_params()
    }

    #[test]
    fn final_router_progress_admits_one_smaller_total_without_replacing_an_outer_runtime() {
        let mut router = Router::new();
        router
            .add_tool(RouterProgressTool)
            .expect("router progress tool registers for both eras");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 177, Budget::INFINITE, &state);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let notification_sender: NotificationSender = Arc::new(move |notification| {
            sent_clone
                .lock()
                .expect("notification collection is not poisoned")
                .push(notification);
        });
        let baseline = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "router-final-progress",
            },
            "name": "router-progress-tool",
            "arguments": {"total": 12000.0},
        });
        let mut planted = baseline.clone();
        planted["arguments"]["total"] = serde_json::json!(11999.0);
        assert_eq!(
            baseline["_meta"], planted["_meta"],
            "the progress total is the sole planted request dimension"
        );
        assert_eq!(baseline["name"], planted["name"]);

        let baseline: FinalCallToolParams =
            serde_json::from_value(baseline).expect("baseline final request is valid");
        let outcome = block_on(router.handle_tools_call_final_in_request(
            &request_ctx,
            request_ctx.cx(),
            baseline,
            state.clone(),
            Some(&notification_sender),
            None,
            None,
        ))
        .expect("ordinary final router dispatch completes");
        assert!(matches!(outcome, FinalToolOutcome::Complete(_)));
        let notification = sent
            .lock()
            .expect("notification collection is not poisoned")[0]
            .clone();
        let wire = serde_json::to_string(
            notification
                .params
                .as_ref()
                .expect("ordinary final progress has parameters"),
        )
        .expect("ordinary final progress parameters serialize");
        assert!(wire.contains("\"progress\":12000"));
        assert!(wire.contains("\"total\":12000"));

        let planted: FinalCallToolParams =
            serde_json::from_value(planted).expect("one-variable planted request is valid");
        let outcome = block_on(router.handle_tools_call_final_in_request(
            &request_ctx,
            request_ctx.cx(),
            planted,
            state,
            Some(&notification_sender),
            None,
            None,
        ))
        .expect("the handler result remains valid when only total is smaller");
        assert!(matches!(outcome, FinalToolOutcome::Complete(_)));
        assert_eq!(
            sent.lock()
                .expect("notification collection is not poisoned")
                .len(),
            2,
            "a smaller total is not a final-progress violation"
        );
        let wire = serde_json::to_string(
            sent.lock()
                .expect("notification collection is not poisoned")[1]
                .params
                .as_ref()
                .expect("second final progress has parameters"),
        )
        .expect("second final progress parameters serialize");
        assert!(wire.contains("\"progress\":12000"));
        assert!(wire.contains("\"total\":11999"));
    }

    #[test]
    fn final_router_preserves_an_outer_final_progress_runtime() {
        let mut router = Router::new();
        router
            .add_tool(RouterProgressTool)
            .expect("router progress tool registers for both eras");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let outer_runtime = Arc::new(crate::handler::FinalProgressRuntime::new(
            ProgressMarker::from("outer-owned-marker"),
            move |notification| {
                sent_clone
                    .lock()
                    .expect("notification collection is not poisoned")
                    .push(notification);
            },
        ));
        let request_ctx =
            McpContext::with_progress(cx, 178, Arc::clone(&outer_runtime).into_reporter());
        let params: FinalCallToolParams = serde_json::from_value(serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "handler-supplied-marker",
            },
            "name": "router-progress-tool",
            "arguments": {"total": 11999.0},
        }))
        .expect("final tool parameters are valid");
        let notification_sender: NotificationSender =
            Arc::new(|_| panic!("router must not replace an installed final-progress runtime"));

        let outcome = block_on(router.handle_tools_call_final_in_request(
            &request_ctx,
            request_ctx.cx(),
            params,
            state,
            Some(&notification_sender),
            None,
            None,
        ))
        .expect("outer final progress runtime remains usable");
        assert!(matches!(outcome, FinalToolOutcome::Complete(_)));
        assert!(outer_runtime.flush_pending());

        let sent = sent
            .lock()
            .expect("notification collection is not poisoned");
        assert_eq!(sent.len(), 1);
        let wire = serde_json::to_string(
            sent[0]
                .params
                .as_ref()
                .expect("outer runtime notification has parameters"),
        )
        .expect("outer runtime parameters serialize");
        assert!(wire.contains("\"progressToken\":\"outer-owned-marker\""));
        assert!(wire.contains("\"progress\":12000"));
        assert!(wire.contains("\"total\":11999"));
    }

    #[cfg(feature = "tasks")]
    struct TaskCapableRouterTool {
        final_calls: Arc<AtomicUsize>,
    }

    #[cfg(feature = "tasks")]
    impl ToolHandler for TaskCapableRouterTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "task-capable-router-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "unevaluatedProperties": false,
                }),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn declares_final_tasks(&self) -> bool {
            true
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy task-capable router result")])
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::CreateTask {
                work_descriptor: FinalTaskWorkDescriptor::new(serde_json::json!({
                    "operation": "task-capable-router-tool"
                }))?,
                status_message: Some("router task created".to_owned()),
            })
        }
    }

    /// A single declared task-capable handler whose result branch is selected
    /// only by the `createTask` argument. It proves registration is not itself
    /// a Tasks operation.
    #[cfg(feature = "tasks")]
    struct ConditionalTaskCapableRouterTool {
        final_calls: Arc<AtomicUsize>,
    }

    #[cfg(feature = "tasks")]
    impl ToolHandler for ConditionalTaskCapableRouterTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "conditional-task-capable-router-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"createTask": {"type": "boolean"}},
                    "required": ["createTask"],
                    "unevaluatedProperties": false,
                }),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn declares_final_tasks(&self) -> bool {
            true
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text(
                "legacy conditional task-capable router result",
            )])
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            if args
                .get("createTask")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(FinalToolOutcome::CreateTask {
                    work_descriptor: FinalTaskWorkDescriptor::new(serde_json::json!({
                        "operation": "conditional-task-capable-router-tool"
                    }))?,
                    status_message: None,
                });
            }
            Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                FinalCallToolResult {
                    content: vec![ContentBlock::text("ordinary final result")],
                    is_error: false,
                    structured_content: None,
                },
            )))
        }
    }

    /// Simulates a handler that overrides the request-owned hook and bypasses
    /// the trait's ordinary declaration guard. The router must still prevent
    /// its undeclared task outcome from reaching task creation.
    #[cfg(feature = "tasks")]
    struct UndeclaredTaskOutcomeRouterTool {
        final_calls: Arc<AtomicUsize>,
    }

    #[cfg(feature = "tasks")]
    impl ToolHandler for UndeclaredTaskOutcomeRouterTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "undeclared-task-outcome-router-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "unevaluatedProperties": false,
                }),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy undeclared-task outcome")])
        }

        fn call_final_outcome_async_in_request<'a>(
            &'a self,
            _ctx: &'a McpContext,
            _request_cx: &'a Cx,
            _args: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            let work_descriptor = FinalTaskWorkDescriptor::new(serde_json::json!({
                "operation": "undeclared-task-outcome-router-tool"
            }));
            Box::pin(async move {
                match work_descriptor {
                    Ok(work_descriptor) => Outcome::Ok(FinalToolOutcome::CreateTask {
                        work_descriptor,
                        status_message: None,
                    }),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    #[cfg(feature = "tasks")]
    struct NoopFinalTaskSupervisor;

    #[cfg(feature = "tasks")]
    impl ApplicationTaskSupervisor for NoopFinalTaskSupervisor {
        fn resume<'a>(
            &'a self,
            _cx: &'a Cx,
            _handoff: FinalTaskSupervisorHandoff,
        ) -> FinalTaskSupervisorFuture<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    #[cfg(feature = "tasks")]
    fn task_runtime_for_router(store: Arc<InMemoryFinalTaskStore>) -> FinalTaskRuntime {
        let store: Arc<dyn FinalTaskStore> = store;
        FinalTaskRuntime::new(
            store,
            FinalTaskRuntimeConfig::new(60_000, Some(5_000))
                .expect("a finite final Task policy is valid"),
            Arc::new(|_notification| {}),
        )
    }

    #[cfg(feature = "tasks")]
    fn final_task_capable_tool_request(id: i64) -> JsonRpcRequest {
        JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": {
                            "io.modelcontextprotocol/tasks": {}
                        }
                    },
                },
                "name": "task-capable-router-tool",
                "arguments": {},
            })),
            id,
        )
    }

    struct FinalCatalogTool {
        metadata: OpenMetadata,
        icons: Vec<RawIcon>,
    }

    impl ToolHandler for FinalCatalogTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "final-catalog-tool".to_owned(),
                description: Some("final catalog description".to_owned()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: Some(serde_json::json!({"type": "object"})),
                icon: Some(fastmcp_protocol::Icon::new("https://legacy.test/icon.png")),
                version: Some("legacy-version".to_owned()),
                tags: vec!["legacy-tag".to_owned()],
                annotations: None,
            }
        }

        fn final_title(&self) -> Option<&str> {
            Some("Final Catalog Tool")
        }

        fn final_icons(&self) -> Option<&[RawIcon]> {
            Some(&self.icons)
        }

        fn final_metadata(&self) -> Option<&OpenMetadata> {
            Some(&self.metadata)
        }

        fn final_definition(&self) -> Option<FinalTool> {
            Some(FinalTool {
                name: "final-catalog-tool".to_owned(),
                title: Some("Exact Final Catalog Tool".to_owned()),
                description: Some("exact final catalog description".to_owned()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: Some(serde_json::json!({"type": "object"})),
                annotations: Some(FinalToolAnnotations {
                    title: Some("Exact annotation title".to_owned()),
                    destructive: Some(false),
                    idempotent: Some(true),
                    read_only: Some(true),
                    open_world_hint: Some(false),
                }),
                icons: Some(self.icons.clone()),
                meta: Some(self.metadata.clone()),
            })
        }

        fn final_tool_error_structured_content(
            &self,
            kind: ToolErrorKind,
        ) -> Option<serde_json::Value> {
            Some(serde_json::json!({
                "error": match kind {
                    ToolErrorKind::InputValidation => "input-validation",
                    ToolErrorKind::Handler => "handler",
                }
            }))
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy final catalog result")])
        }
    }

    struct FinalCatalogResource {
        metadata: OpenMetadata,
        icons: Vec<RawIcon>,
        annotations: Annotations,
    }

    impl ResourceHandler for FinalCatalogResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "file:///final-catalog-resource".to_owned(),
                name: "final-catalog-resource".to_owned(),
                description: Some("final resource description".to_owned()),
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn final_title(&self) -> Option<&str> {
            Some("Final Catalog Resource")
        }

        fn final_icons(&self) -> Option<&[RawIcon]> {
            Some(&self.icons)
        }

        fn final_annotations(&self) -> Option<&Annotations> {
            Some(&self.annotations)
        }

        fn final_metadata(&self) -> Option<&OpenMetadata> {
            Some(&self.metadata)
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(Vec::new())
        }
    }

    struct FinalCatalogResourceTemplate {
        metadata: OpenMetadata,
        icons: Vec<RawIcon>,
        annotations: Annotations,
    }

    impl ResourceHandler for FinalCatalogResourceTemplate {
        fn definition(&self) -> Resource {
            Resource {
                uri: "template://placeholder".to_owned(),
                name: "final-catalog-template".to_owned(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            Some(ResourceTemplate {
                uri_template: "template://{id}".to_owned(),
                name: "final-catalog-template".to_owned(),
                description: Some("final template description".to_owned()),
                mime_type: Some("application/json".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            })
        }

        fn final_template_title(&self) -> Option<&str> {
            Some("Final Catalog Template")
        }

        fn final_template_icons(&self) -> Option<&[RawIcon]> {
            Some(&self.icons)
        }

        fn final_template_annotations(&self) -> Option<&Annotations> {
            Some(&self.annotations)
        }

        fn final_template_metadata(&self) -> Option<&OpenMetadata> {
            Some(&self.metadata)
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(Vec::new())
        }
    }

    struct FinalCatalogPrompt {
        metadata: OpenMetadata,
        icons: Vec<RawIcon>,
    }

    impl PromptHandler for FinalCatalogPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "final-catalog-prompt".to_owned(),
                description: Some("final prompt description".to_owned()),
                arguments: vec![PromptArgument {
                    name: "optional-argument".to_owned(),
                    description: Some("must remain explicitly false".to_owned()),
                    required: false,
                }],
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn final_title(&self) -> Option<&str> {
            Some("Final Catalog Prompt")
        }

        fn final_icons(&self) -> Option<&[RawIcon]> {
            Some(&self.icons)
        }

        fn final_metadata(&self) -> Option<&OpenMetadata> {
            Some(&self.metadata)
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(Vec::new())
        }
    }

    struct DirectFinalPrompt {
        final_calls: Arc<AtomicUsize>,
    }

    impl PromptHandler for DirectFinalPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "direct-final-prompt".to_owned(),
                description: Some("legacy prompt definition".to_owned()),
                arguments: vec![],
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Err(McpError::internal_error(
                "legacy prompt projection must not service a final request",
            ))
        }

        fn get_final(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<CompleteResult<FinalGetPromptResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            let content_meta = OpenMetadata::try_from_entries([(
                "com.example/direct-prompt".to_owned(),
                serde_json::json!({"source": "final-handler"}),
            )])
            .expect("direct prompt content metadata is valid");
            Ok(CompleteResult::new(
                FinalGetPromptResult {
                    description: Some("direct final prompt description".to_owned()),
                    messages: vec![FinalPromptMessage {
                        role: fastmcp_protocol::Role::Assistant,
                        content: ContentBlock::Audio {
                            data: "aGVsbG8=".to_owned(),
                            mime_type: "audio/mpeg".to_owned(),
                            annotations: None,
                            meta: Some(content_meta),
                            additional: BTreeMap::from([(
                                "com.example/direct-field".to_owned(),
                                serde_json::json!(true),
                            )]),
                        },
                    }],
                },
                empty_final_result_meta()?,
            ))
        }
    }

    struct PromptArgumentBoundary {
        final_calls: Arc<AtomicUsize>,
        legacy_calls: Arc<AtomicUsize>,
    }

    impl PromptHandler for PromptArgumentBoundary {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "prompt-argument-boundary".to_owned(),
                description: None,
                arguments: vec![PromptArgument {
                    name: "topic".to_owned(),
                    description: Some("required modern prompt topic".to_owned()),
                    required: true,
                }],
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![PromptMessage {
                role: fastmcp_protocol::Role::Assistant,
                content: Content::text("legacy prompt-argument-boundary result"),
            }])
        }

        fn get_final(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<CompleteResult<FinalGetPromptResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompleteResult::new(
                FinalGetPromptResult {
                    description: None,
                    messages: vec![FinalPromptMessage {
                        role: fastmcp_protocol::Role::Assistant,
                        content: ContentBlock::text("final prompt-argument-boundary result"),
                    }],
                },
                empty_final_result_meta()?,
            ))
        }
    }

    /// Deliberately changes its legacy definition after registration. Modern
    /// prompt validation must use the admission snapshot instead.
    struct MutablePromptDefinition {
        expose_admitted_argument: Arc<AtomicBool>,
        final_calls: Arc<AtomicUsize>,
    }

    impl PromptHandler for MutablePromptDefinition {
        fn definition(&self) -> Prompt {
            let argument = if self.expose_admitted_argument.load(Ordering::SeqCst) {
                PromptArgument {
                    name: "topic".to_owned(),
                    description: Some("admitted required argument".to_owned()),
                    required: true,
                }
            } else {
                PromptArgument {
                    name: "mutated".to_owned(),
                    description: Some("must not affect final validation".to_owned()),
                    required: false,
                }
            };
            Prompt {
                name: "mutable-prompt-definition".to_owned(),
                description: None,
                arguments: vec![argument],
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(Vec::new())
        }

        fn get_final(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<CompleteResult<FinalGetPromptResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompleteResult::new(
                FinalGetPromptResult {
                    description: None,
                    messages: Vec::new(),
                },
                empty_final_result_meta()?,
            ))
        }
    }

    fn input_required_result(forged_request_state: &str) -> InputRequiredResult {
        let encoded = serde_json::json!({
            "resultType": "input_required",
            "inputRequests": {"roots": {"method": "roots/list"}},
            "requestState": forged_request_state,
        })
        .to_string();
        let (decoded, diagnostic) = decode_peer_result(
            &encoded,
            ResultPeerEra::Modern,
            &CoreResultDiscriminatorPolicy,
        )
        .expect("test input-required result decodes");
        assert!(diagnostic.is_none());
        let DecodedResult::InputRequired(result) = decoded else {
            panic!("test result is input_required");
        };
        result
    }

    fn sampling_input_required_result(forged_request_state: &str) -> InputRequiredResult {
        let encoded = serde_json::json!({
            "resultType": "input_required",
            "inputRequests": {
                "sample": {
                    "method": "sampling/createMessage",
                    "params": {
                        "messages": [{
                            "role": "assistant",
                            "content": {
                                "type": "tool_use",
                                "id": "weather-1",
                                "name": "weather",
                                "input": {"city": "Boston"},
                            },
                        }],
                        "maxTokens": 16,
                        "tools": [{"name": "weather", "inputSchema": {"type": "object"}}],
                        "toolChoice": {"mode": "required"},
                    },
                },
            },
            "requestState": forged_request_state,
        })
        .to_string();
        let (decoded, diagnostic) = decode_peer_result(
            &encoded,
            ResultPeerEra::Modern,
            &CoreResultDiscriminatorPolicy,
        )
        .expect("final sampling input-required result decodes");
        assert!(diagnostic.is_none());
        let DecodedResult::InputRequired(result) = decoded else {
            panic!("test result is sampling input_required");
        };
        result
    }

    fn state_only_input_required_result(forged_request_state: &str) -> InputRequiredResult {
        let encoded = serde_json::json!({
            "resultType": "input_required",
            "requestState": forged_request_state,
        })
        .to_string();
        let (decoded, diagnostic) = decode_peer_result(
            &encoded,
            ResultPeerEra::Modern,
            &CoreResultDiscriminatorPolicy,
        )
        .expect("test state-only input-required result decodes");
        assert!(diagnostic.is_none());
        let DecodedResult::InputRequired(result) = decoded else {
            panic!("test result is state-only input_required");
        };
        assert!(result.input_requests().is_none());
        result
    }

    fn router_roots_response_wire() -> serde_json::Value {
        serde_json::to_value(
            MrtrInputResponse::roots(fastmcp_protocol::ListRootsResult::empty())
                .expect("roots response serializes"),
        )
        .expect("roots response converts to a wire value")
    }

    struct InputRequiredTool {
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
    }

    struct StateOnlyInputRequiredTool {
        initial_calls: Arc<AtomicUsize>,
        resumed_calls: Arc<AtomicUsize>,
    }

    impl ToolHandler for StateOnlyInputRequiredTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "state-only-input-required-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text(
                "legacy state-only input-required result",
            )])
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.initial_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::InputRequired(
                state_only_input_required_result("handler-forged-state"),
            ))
        }

        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            arguments: serde_json::Value,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                // None marks the initial invocation under the unified
                // resuming hook; the retry must carry admitted inputs.
                let Some(resume_inputs) = resume_inputs else {
                    return match self.call_final_outcome(ctx, arguments) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    };
                };
                if !resume_inputs.responses().is_empty() {
                    return Outcome::Err(McpError::internal_error(
                        "state-only MRTR resume unexpectedly carried inputs",
                    ));
                }
                self.resumed_calls.fetch_add(1, Ordering::SeqCst);
                Outcome::Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                    FinalCallToolResult {
                        content: vec![ContentBlock::text("state-only resumed")],
                        is_error: false,
                        structured_content: None,
                    },
                )))
            })
        }
    }

    impl ToolHandler for InputRequiredTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "input-required-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Content::text("legacy tool result")])
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::InputRequired(input_required_result(
                "tool-retry-state",
            )))
        }

        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            arguments: serde_json::Value,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                // None marks the initial invocation under the unified
                // resuming hook; the retry must carry admitted inputs.
                let Some(resume_inputs) = resume_inputs else {
                    return match self.call_final_outcome(ctx, arguments) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    };
                };
                match resume_inputs.roots("roots") {
                    Ok(Some(_)) => match self.call_final_outcome(ctx, arguments) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    },
                    Ok(None) => Outcome::Err(McpError::internal_error("MRTR roots input was lost")),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    struct OneShotMrtrTool {
        calls: Arc<AtomicUsize>,
    }

    struct ContextElicitationTool {
        initial_calls: Arc<AtomicUsize>,
        resumed_calls: Arc<AtomicUsize>,
    }

    struct ContextSamplingTool {
        calls: Arc<AtomicUsize>,
    }

    impl ToolHandler for ContextElicitationTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "context-elicitation-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy result")])
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn call_final_outcome(
            &self,
            ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.initial_calls.fetch_add(1, Ordering::SeqCst);
            let elicitation = ctx.final_elicitation_form(
                "approval",
                "Approve this operation",
                serde_json::json!({
                    "type": "object",
                    "properties": {"approved": {"type": "boolean"}},
                    "required": ["approved"],
                }),
            )?;
            Ok(FinalToolOutcome::InputRequired(
                elicitation.into_input_required()?,
            ))
        }

        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            arguments: serde_json::Value,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                let Some(resume_inputs) = resume_inputs else {
                    return match self.call_final_outcome(ctx, arguments) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    };
                };
                let response = match resume_inputs.elicitation("approval") {
                    Ok(Some(response)) => response,
                    Ok(None) => {
                        return Outcome::Err(McpError::internal_error(
                            "MRTR elicitation response was lost",
                        ));
                    }
                    Err(error) => return Outcome::Err(error),
                };
                if response.action != fastmcp_protocol::ElicitAction::Accept
                    || response
                        .content
                        .as_ref()
                        .and_then(|content| content.get("approved"))
                        != Some(&fastmcp_protocol::ElicitContentValue::Bool(true))
                {
                    return Outcome::Err(McpError::invalid_params(
                        "MRTR elicitation response did not approve the operation",
                    ));
                }
                self.resumed_calls.fetch_add(1, Ordering::SeqCst);
                Outcome::Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                    FinalCallToolResult {
                        content: vec![ContentBlock::text("approved")],
                        is_error: false,
                        structured_content: None,
                    },
                )))
            })
        }
    }

    impl ToolHandler for ContextSamplingTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "context-sampling-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("legacy result")])
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::InputRequired(
                sampling_input_required_result("handler-forged-state"),
            ))
        }
    }

    impl ToolHandler for OneShotMrtrTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "one-shot-mrtr-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text("one-shot legacy result")])
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::InputRequired(input_required_result(
                "one-shot-handler-state",
            )))
        }

        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            arguments: serde_json::Value,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                let Some(resume_inputs) = resume_inputs else {
                    return match self.call_final_outcome(ctx, arguments) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    };
                };
                match resume_inputs.roots("roots") {
                    Ok(Some(_)) => {
                        self.calls.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(
                            resume_inputs.handler_request_state(),
                            Some("one-shot-handler-state"),
                            "the router returns the original handler state only after bound admission"
                        );
                        Outcome::Ok(FinalToolOutcome::Complete(final_tool_complete_result(
                            FinalCallToolResult {
                                content: vec![ContentBlock::text("one-shot resumed")],
                                is_error: false,
                                structured_content: None,
                            },
                        )))
                    }
                    Ok(None) => Outcome::Err(McpError::internal_error("MRTR roots input was lost")),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    #[cfg(feature = "tasks")]
    struct TaskCapableInputRequiredTool {
        final_calls: Arc<AtomicUsize>,
    }

    #[cfg(feature = "tasks")]
    impl ToolHandler for TaskCapableInputRequiredTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "task-capable-input-required-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn declares_final_tasks(&self) -> bool {
            true
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::text(
                "legacy task-capable input-required result",
            )])
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalToolOutcome::InputRequired(input_required_result(
                "task-capable-tool-retry-state",
            )))
        }
    }

    struct InputRequiredResource {
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
    }

    impl ResourceHandler for InputRequiredResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "file:///input-required-resource".to_owned(),
                name: "input-required-resource".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ResourceContent {
                uri: "file:///input-required-resource".to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: Some("legacy resource result".to_owned()),
                blob: None,
            }])
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn read_final_outcome(
            &self,
            _ctx: &McpContext,
        ) -> McpResult<FinalMethodOutcome<FinalReadResourceResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalMethodOutcome::InputRequired(input_required_result(
                "resource-retry-state",
            )))
        }

        fn read_final_outcome_async_with_uri_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            _uri: &'a str,
            _params: &'a UriParams,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
            Box::pin(async move {
                let Some(resume_inputs) = resume_inputs else {
                    return Outcome::Err(McpError::internal_error("MRTR resume inputs were lost"));
                };
                match resume_inputs.roots("roots") {
                    Ok(Some(_)) => match self.read_final_outcome(ctx) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    },
                    Ok(None) => Outcome::Err(McpError::internal_error("MRTR roots input was lost")),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    struct InputRequiredPrompt {
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
    }

    impl PromptHandler for InputRequiredPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "input-required-prompt".to_owned(),
                description: None,
                arguments: Vec::new(),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn get_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<FinalMethodOutcome<FinalGetPromptResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalMethodOutcome::InputRequired(input_required_result(
                "prompt-retry-state",
            )))
        }

        fn get_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            arguments: std::collections::HashMap<String, String>,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>> {
            Box::pin(async move {
                let Some(resume_inputs) = resume_inputs else {
                    return Outcome::Err(McpError::internal_error("MRTR resume inputs were lost"));
                };
                match resume_inputs.roots("roots") {
                    Ok(Some(_)) => match self.get_final_outcome(ctx, arguments) {
                        Ok(result) => Outcome::Ok(result),
                        Err(error) => Outcome::Err(error),
                    },
                    Ok(None) => Outcome::Err(McpError::internal_error("MRTR roots input was lost")),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    fn direct_final_prompt_request(id: i64) -> JsonRpcRequest {
        JsonRpcRequest::new(
            "prompts/get",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "direct-final-prompt",
            })),
            id,
        )
    }

    fn final_prompt_get_request(
        name: &str,
        arguments: serde_json::Value,
        id: i64,
    ) -> JsonRpcRequest {
        JsonRpcRequest::new(
            "prompts/get",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": name,
                "arguments": arguments,
            })),
            id,
        )
    }

    struct DirectFinalResource {
        legacy_calls: Arc<AtomicUsize>,
        final_calls: Arc<AtomicUsize>,
    }

    impl ResourceHandler for DirectFinalResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "file:///direct-final-resource".to_owned(),
                name: "direct-final-resource".to_owned(),
                description: Some("legacy resource definition".to_owned()),
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ResourceContent {
                uri: "file:///direct-final-resource".to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: Some("legacy resource result".to_owned()),
                blob: None,
            }])
        }

        fn read_final(
            &self,
            _ctx: &McpContext,
        ) -> McpResult<CompleteResult<FinalReadResourceResult>> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            let content_meta = OpenMetadata::try_from_entries([(
                "com.example/direct-resource".to_owned(),
                serde_json::json!({"source": "final-handler"}),
            )])
            .expect("direct resource content metadata is valid");
            Ok(CompleteResult::new(
                FinalReadResourceResult {
                    contents: vec![EmbeddedResourceContents::Text {
                        uri: AbsoluteUri::parse("file:///direct-final-resource")
                            .expect("routed direct resource URI is valid"),
                        text: "direct final resource result".to_owned(),
                        mime_type: Some("text/markdown".to_owned()),
                        meta: Some(content_meta),
                        additional: BTreeMap::from([(
                            "com.example/direct-field".to_owned(),
                            serde_json::json!(true),
                        )]),
                    }],
                    ttl_ms: CacheTtl::milliseconds(321),
                    cache_scope: CacheScope::Public,
                },
                empty_final_result_meta()?,
            ))
        }

        fn final_resource_read_cache_hint_provenance(
            &self,
        ) -> FinalResourceReadCacheHintProvenance {
            FinalResourceReadCacheHintProvenance::Explicit
        }
    }

    struct HttpsCatalogResource {
        client_direct_https: bool,
    }

    impl ResourceHandler for HttpsCatalogResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "https://client.example.test/catalog.txt".to_owned(),
                name: "client-direct-catalog".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn final_client_direct_https(&self) -> bool {
            self.client_direct_https
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(Vec::new())
        }
    }

    struct ClientDirectHttpsPrompt;

    impl PromptHandler for ClientDirectHttpsPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "client-direct-https-prompt".to_owned(),
                description: None,
                arguments: Vec::new(),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn final_client_direct_https(&self) -> bool {
            true
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(Vec::new())
        }

        fn get_final(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<CompleteResult<FinalGetPromptResult>> {
            Ok(CompleteResult::new(
                FinalGetPromptResult {
                    description: None,
                    messages: vec![FinalPromptMessage {
                        role: fastmcp_protocol::Role::Assistant,
                        content: ContentBlock::ResourceLink {
                            icons: None,
                            name: "client-direct-link".to_owned(),
                            title: None,
                            uri: AbsoluteUri::parse("https://client.example.test/prompt-link")
                                .expect("test HTTPS URI is valid"),
                            description: None,
                            mime_type: Some("text/plain".to_owned()),
                            annotations: None,
                            size: None,
                            meta: None,
                            additional: BTreeMap::new(),
                        },
                    }],
                },
                empty_final_result_meta()?,
            ))
        }
    }

    struct MrtrHttpsEmbeddedResource {
        initial_calls: Arc<AtomicUsize>,
        resumed_calls: Arc<AtomicUsize>,
    }

    impl ResourceHandler for MrtrHttpsEmbeddedResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "mcp://uri-policy/mrtr".to_owned(),
                name: "uri-policy-mrtr".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn final_client_direct_https(&self) -> bool {
            true
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(Vec::new())
        }

        fn read_final_outcome(
            &self,
            _ctx: &McpContext,
        ) -> McpResult<FinalMethodOutcome<FinalReadResourceResult>> {
            self.initial_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FinalMethodOutcome::InputRequired(input_required_result(
                "uri-policy-mrtr-state",
            )))
        }

        fn read_final_outcome_async_with_uri_resuming_in_request<'a>(
            &'a self,
            _ctx: &'a McpContext,
            _request_cx: &'a Cx,
            _uri: &'a str,
            _params: &'a UriParams,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
            Box::pin(async move {
                let Some(resume_inputs) = resume_inputs else {
                    return Outcome::Err(McpError::internal_error("MRTR resume inputs were lost"));
                };
                if !matches!(resume_inputs.roots("roots"), Ok(Some(_))) {
                    return Outcome::Err(McpError::internal_error(
                        "MRTR roots input was not preserved",
                    ));
                }
                self.resumed_calls.fetch_add(1, Ordering::SeqCst);
                Outcome::Ok(FinalMethodOutcome::Complete(CompleteResult::new(
                    FinalReadResourceResult {
                        contents: vec![EmbeddedResourceContents::Text {
                            uri: AbsoluteUri::parse("https://client.example.test/mrtr-content")
                                .expect("test HTTPS URI is valid"),
                            text: "must not be emitted as embedded content".to_owned(),
                            mime_type: Some("text/plain".to_owned()),
                            meta: None,
                            additional: BTreeMap::new(),
                        }],
                        ttl_ms: CacheTtl::milliseconds(1),
                        cache_scope: CacheScope::Private,
                    },
                    empty_final_result_meta().expect("empty final metadata is valid"),
                )))
            })
        }
    }

    struct SentinelHintResource {
        provenance: FinalResourceReadCacheHintProvenance,
    }

    impl ResourceHandler for SentinelHintResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "file:///sentinel-hint-resource".to_owned(),
                name: "sentinel-hint-resource".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(Vec::new())
        }

        fn read_final(
            &self,
            _ctx: &McpContext,
        ) -> McpResult<CompleteResult<FinalReadResourceResult>> {
            Ok(CompleteResult::new(
                FinalReadResourceResult {
                    contents: Vec::new(),
                    // This is intentionally the former sentinel value. Only
                    // the explicit provenance may preserve it.
                    ttl_ms: CacheTtl::milliseconds(DEFAULT_FINAL_RESOURCE_TTL_MS),
                    cache_scope: CacheScope::Private,
                },
                empty_final_result_meta()?,
            ))
        }

        fn final_resource_read_cache_hint_provenance(
            &self,
        ) -> FinalResourceReadCacheHintProvenance {
            self.provenance
        }
    }

    fn direct_final_resource_request(id: i64) -> JsonRpcRequest {
        JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "uri": "file:///direct-final-resource",
            })),
            id,
        )
    }

    struct EchoCompletion;

    impl CompletionHandler for EchoCompletion {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            Ok(CompletionValues {
                values: vec![format!("{}ging", params.argument.value)],
                total: Some(1),
                has_more: Some(false),
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            params: FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: vec![format!("{}ging", params.argument.value)],
                total: Some(fastmcp_protocol::JsonInteger::from(1_i64)),
                has_more: Some(false),
            })
        }
    }

    #[derive(Clone)]
    struct LegacyCompletionProbe {
        calls: Arc<AtomicUsize>,
        value_count: Arc<AtomicUsize>,
    }

    impl CompletionHandler for LegacyCompletionProbe {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            _params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionValues {
                values: (0..self.value_count.load(Ordering::SeqCst))
                    .rev()
                    .map(|index| format!("legacy-completion-{index}"))
                    .collect(),
                // The exact legacy schema has no minimum on total.
                total: Some(-1),
                has_more: None,
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            _params: FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            panic!("legacy completion must not invoke a final provider")
        }
    }

    struct CountingCompletion {
        final_calls: Arc<AtomicUsize>,
    }

    impl CompletionHandler for CountingCompletion {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            _params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            Ok(CompletionValues {
                values: vec!["legacy".to_owned()],
                total: Some(1),
                has_more: Some(false),
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            params: FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: vec![format!("{}ging", params.argument.value)],
                total: Some(fastmcp_protocol::JsonInteger::from(1_i64)),
                has_more: Some(false),
            })
        }
    }

    struct ProviderCompletion {
        value: &'static str,
        final_calls: Arc<AtomicUsize>,
    }

    impl CompletionHandler for ProviderCompletion {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            _params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            Ok(CompletionValues {
                values: vec![self.value.to_owned()],
                total: Some(1),
                has_more: Some(false),
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            _params: FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: vec![self.value.to_owned()],
                total: Some(fastmcp_protocol::JsonInteger::from(1_i64)),
                has_more: Some(false),
            })
        }
    }

    struct ReversibleLevelFourTemplateResource {
        read_calls: Arc<AtomicUsize>,
    }

    impl ResourceHandler for ReversibleLevelFourTemplateResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "mcp://resource/template".to_owned(),
                name: "level-four-template".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            Some(marked_template(
                "mcp://resource{/collection*}/manifest{?revision*}",
                "level-four-template",
            ))
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            unreachable!("templated reads receive their URI parameters")
        }

        fn read_with_uri(
            &self,
            _ctx: &McpContext,
            uri: &str,
            params: &UriParams,
        ) -> McpResult<Vec<ResourceContent>> {
            self.read_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ResourceContent {
                uri: uri.to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: Some(format!(
                    "{}:{}",
                    params.get("collection").expect("collection is captured"),
                    params.get("revision").expect("revision is captured")
                )),
                blob: None,
            }])
        }
    }

    struct LegacyTemplateResource {
        read_calls: Arc<AtomicUsize>,
    }

    impl ResourceHandler for LegacyTemplateResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "mcp://resource/legacy-template".to_owned(),
                name: "legacy-template".to_owned(),
                description: None,
                mime_type: Some("text/plain".to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            Some(marked_template(
                "mcp://resource/{collection}/manifest?revision={revision}",
                "legacy-template",
            ))
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            unreachable!("templated reads receive their URI parameters")
        }

        fn read_with_uri(
            &self,
            _ctx: &McpContext,
            uri: &str,
            params: &UriParams,
        ) -> McpResult<Vec<ResourceContent>> {
            self.read_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ResourceContent {
                uri: uri.to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: Some(format!(
                    "{}:{}",
                    params.get("collection").expect("collection is captured"),
                    params.get("revision").expect("revision is captured")
                )),
                blob: None,
            }])
        }
    }

    struct CompletionValueBoundary {
        final_calls: Arc<AtomicUsize>,
    }

    impl CompletionHandler for CompletionValueBoundary {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            _params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            Ok(CompletionValues {
                values: Vec::new(),
                total: None,
                has_more: None,
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            params: FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            let value_count = if params.argument.value == "one-over" {
                fastmcp_protocol::MAX_COMPLETION_VALUES + 1
            } else {
                fastmcp_protocol::MAX_COMPLETION_VALUES
            };
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: (0..value_count)
                    .map(|index| format!("completion-{index}"))
                    .collect(),
                total: (params.argument.value == "negative-total")
                    .then(|| fastmcp_protocol::JsonInteger::from(-1_i64)),
                has_more: None,
            })
        }
    }

    struct ConcurrentModernTool {
        started: Arc<AtomicUsize>,
        completed: Arc<Mutex<Vec<String>>>,
    }

    impl ConcurrentModernTool {
        fn new(started: Arc<AtomicUsize>, completed: Arc<Mutex<Vec<String>>>) -> Self {
            Self { started, completed }
        }
    }

    impl ToolHandler for ConcurrentModernTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "concurrent-modern-tool".to_string(),
                description: Some("deterministic modern dispatch probe".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["request"],
                    "properties": {"request": {"type": "string"}},
                }),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Err(McpError::internal_error(
                "concurrent modern dispatch requires the async request hook",
            ))
        }

        // The modern stateless in-request path consults the disjoint final
        // outcome hook, not `call_async_in_request`; override the hook the
        // router actually dispatches through.
        fn call_final_outcome_async_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            request_cx: &'a Cx,
            arguments: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                let label = arguments
                    .get("request")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("missing")
                    .to_string();
                self.started.fetch_add(1, Ordering::SeqCst);

                while self.started.load(Ordering::SeqCst) < 2 {
                    if ctx.checkpoint().is_err() || request_cx.is_cancel_requested() {
                        return Outcome::Cancelled(asupersync::CancelReason::user(
                            "request cancelled before concurrent admission",
                        ));
                    }
                    yield_once().await;
                }

                if label == "cancelled" {
                    loop {
                        if ctx.checkpoint().is_err() || request_cx.is_cancel_requested() {
                            return Outcome::Cancelled(asupersync::CancelReason::user(
                                "request cancellation observed by child Cx",
                            ));
                        }
                        yield_once().await;
                    }
                }

                if ctx.checkpoint().is_err() || request_cx.is_cancel_requested() {
                    return Outcome::Cancelled(asupersync::CancelReason::user(
                        "request cancelled before completion",
                    ));
                }
                self.completed
                    .lock()
                    .expect("completion probe lock is not poisoned")
                    .push(label.clone());
                match crate::handler::promote_legacy_tool_content(vec![Content::text(label)]) {
                    Ok(result) => Outcome::Ok(FinalToolOutcome::Complete(result)),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    struct ErrorTool {
        name: &'static str,
        code: McpErrorCode,
    }

    impl ToolHandler for ErrorTool {
        fn definition(&self) -> Tool {
            Tool {
                name: self.name.to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Err(McpError::new(self.code, "nested tool error"))
        }
    }

    #[cfg(feature = "tasks")]
    struct DetailedFinalErrorTool {
        name: &'static str,
        error: McpError,
    }

    #[cfg(feature = "tasks")]
    impl ToolHandler for DetailedFinalErrorTool {
        fn definition(&self) -> Tool {
            Tool {
                name: self.name.to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Err(self.error.clone())
        }

        fn call_final_outcome(
            &self,
            _ctx: &McpContext,
            _args: serde_json::Value,
        ) -> McpResult<FinalToolOutcome> {
            Err(self.error.clone())
        }
    }

    struct AlternatingTool {
        calls: Arc<AtomicU64>,
    }

    impl ToolHandler for AlternatingTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "alternating_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Err(McpError::internal_error("async alternating tool only"))
        }

        fn call_async<'a>(
            &'a self,
            ctx: &'a McpContext,
            _arguments: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                match ctx.read_resource("loop://resource").await {
                    Ok(_) => Outcome::Ok(vec![Content::text("unexpected completion")]),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    struct NamedResource {
        uri: String,
        tags: Vec<String>,
    }

    impl NamedResource {
        fn new(uri: &str) -> Self {
            Self {
                uri: uri.to_string(),
                tags: vec![],
            }
        }
        fn with_tags(uri: &str, tags: Vec<String>) -> Self {
            Self {
                uri: uri.to_string(),
                tags,
            }
        }
    }

    impl ResourceHandler for NamedResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: self.uri.clone(),
                name: self.uri.clone(),
                description: None,
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: self.tags.clone(),
            }
        }
        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: self.uri.clone(),
                mime_type: Some("text/plain".to_string()),
                text: Some("content".to_string()),
                blob: None,
            }])
        }
    }

    struct AlternatingResource {
        calls: Arc<AtomicU64>,
    }

    impl ResourceHandler for AlternatingResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "loop://resource".to_string(),
                name: "alternating-resource".to_string(),
                description: None,
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Err(McpError::internal_error("async alternating resource only"))
        }

        fn read_async<'a>(
            &'a self,
            ctx: &'a McpContext,
        ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                match ctx
                    .call_tool("alternating_tool", serde_json::json!({}))
                    .await
                {
                    Ok(_) => Outcome::Ok(vec![ResourceContent {
                        uri: "loop://resource".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("unexpected completion".to_string()),
                        blob: None,
                    }]),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    struct CostLedgerTool {
        remaining_after_parent_debit: Arc<AtomicU64>,
        remaining_after_nested_read: Arc<AtomicU64>,
    }

    impl ToolHandler for CostLedgerTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "cost_ledger_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Err(McpError::internal_error("async cost-ledger tool only"))
        }

        fn call_async<'a>(
            &'a self,
            ctx: &'a McpContext,
            _arguments: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
            Box::pin(async move {
                if ctx.consume_cost(1).is_err() {
                    return Outcome::Err(McpError::request_cancelled());
                }
                self.remaining_after_parent_debit.store(
                    ctx.budget()
                        .cost_quota
                        .expect("test request has finite cost quota"),
                    Ordering::Relaxed,
                );

                if let Err(error) = ctx.read_resource("cost://nested").await {
                    return Outcome::Err(error);
                }
                self.remaining_after_nested_read.store(
                    ctx.budget()
                        .cost_quota
                        .expect("test request has finite cost quota"),
                    Ordering::Relaxed,
                );
                Outcome::Ok(vec![Content::text("shared ledger")])
            })
        }
    }

    struct CostLedgerResource {
        remaining_after_nested_debit: Arc<AtomicU64>,
    }

    impl ResourceHandler for CostLedgerResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "cost://nested".to_string(),
                name: "cost-ledger-resource".to_string(),
                description: None,
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            ctx.consume_cost(1)
                .map_err(|_| McpError::request_cancelled())?;
            self.remaining_after_nested_debit.store(
                ctx.budget()
                    .cost_quota
                    .expect("test request has finite cost quota"),
                Ordering::Relaxed,
            );
            Ok(vec![ResourceContent {
                uri: "cost://nested".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("nested debit".to_string()),
                blob: None,
            }])
        }
    }

    struct NamedPrompt {
        name: String,
        tags: Vec<String>,
    }

    impl NamedPrompt {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                tags: vec![],
            }
        }
        fn with_tags(name: &str, tags: Vec<String>) -> Self {
            Self {
                name: name.to_string(),
                tags,
            }
        }
    }

    impl PromptHandler for NamedPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: self.name.clone(),
                description: Some(format!("Prompt {}", self.name)),
                arguments: vec![],
                icon: None,
                version: None,
                tags: self.tags.clone(),
            }
        }
        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(vec![])
        }
    }

    fn marked_template(uri_template: &str, marker: &str) -> ResourceTemplate {
        ResourceTemplate {
            uri_template: uri_template.to_string(),
            name: marker.to_string(),
            description: Some(marker.to_string()),
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![marker.to_string()],
        }
    }

    fn marked_router(marker: &str) -> Router {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::with_tags(
                "duplicate_tool",
                vec![marker.to_string()],
            ))
            .expect("tool registration succeeds");
        router.add_resource(NamedResource::with_tags(
            "duplicate://resource",
            vec![marker.to_string()],
        ));
        router
            .add_resource_template_with_behavior(
                marked_template("mcp://duplicate-item/{item}", marker),
                crate::DuplicateBehavior::Replace,
            )
            .expect("marked resource template is disjoint from the marked exact resource");
        router.add_prompt(NamedPrompt::with_tags(
            "duplicate_prompt",
            vec![marker.to_string()],
        ));
        router
    }

    fn assert_router_marker(router: &Router, marker: &str) {
        assert_eq!(
            router
                .get_tool("duplicate_tool")
                .expect("tool exists")
                .definition()
                .tags,
            vec![marker.to_string()]
        );
        assert_eq!(
            router
                .get_resource("duplicate://resource")
                .expect("resource exists")
                .definition()
                .tags,
            vec![marker.to_string()]
        );
        assert_eq!(
            router
                .get_resource_template("mcp://duplicate-item/{item}")
                .expect("resource template exists")
                .tags,
            vec![marker.to_string()]
        );
        assert_eq!(
            router
                .get_prompt("duplicate_prompt")
                .expect("prompt exists")
                .definition()
                .tags,
            vec![marker.to_string()]
        );
    }

    struct BudgetProbeTool {
        timeout: Option<Duration>,
        delay: Duration,
        observed_deadline: Arc<Mutex<Option<Time>>>,
        timeout_read: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ToolHandler for BudgetProbeTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "budget_probe".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn timeout(&self) -> Option<Duration> {
            self.timeout_read.store(true, Ordering::Relaxed);
            self.timeout
        }

        fn call(&self, ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            eprintln!(
                "WM-BUDGET call entered deadline={:?}",
                ctx.budget().deadline
            );
            *self
                .observed_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = ctx.budget().deadline;
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            Ok(vec![Content::text("completed")])
        }
    }

    struct SlowDefinitionTool {
        definition_reads: Arc<AtomicU64>,
        called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ToolHandler for SlowDefinitionTool {
        fn definition(&self) -> Tool {
            if self.definition_reads.fetch_add(1, Ordering::Relaxed) > 0 {
                eprintln!(
                    "WM-PROBE requery backtrace:\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            Tool {
                name: "slow_definition".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn timeout(&self) -> Option<Duration> {
            Some(Duration::from_secs(1))
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            self.called.store(true, Ordering::Relaxed);
            Ok(vec![Content::text("must not run")])
        }
    }

    const PANIC_CANARY: &str = "Bearer peer-secret\n\u{001b}[31mred\u{001b}[0m\u{001b}]8;;https://invalid\u{0007}link\u{202e}";

    struct PanickingDisplay(String);

    impl fmt::Display for PanickingDisplay {
        fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            panic!("display-must-not-run: {}", self.0);
        }
    }

    struct UnwindingPanicTool {
        payload: String,
        non_string: bool,
    }

    impl ToolHandler for UnwindingPanicTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "panic_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            unreachable!("the async override is the handler boundary under test")
        }

        fn call_async<'a>(
            &'a self,
            _ctx: &'a McpContext,
            _arguments: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
            Box::pin(async move {
                if self.non_string {
                    std::panic::panic_any(PanickingDisplay(self.payload.clone()));
                }
                panic!("{}", self.payload);
            })
        }
    }

    struct OutcomePanicTool(String);

    impl ToolHandler for OutcomePanicTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "outcome_panic_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            unreachable!("the async override is the handler boundary under test")
        }

        fn call_async<'a>(
            &'a self,
            _ctx: &'a McpContext,
            _arguments: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
            Box::pin(async move {
                Outcome::Panicked(asupersync::types::PanicPayload::new(self.0.clone()))
            })
        }
    }

    struct OpaqueInternalTool;

    impl ToolHandler for OpaqueInternalTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "opaque_internal_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Err(McpError::with_data(
                McpErrorCode::InternalError,
                PANIC_CANARY,
                serde_json::json!({"secret": PANIC_CANARY}),
            ))
        }
    }

    struct OpaqueInternalResource;

    impl ResourceHandler for OpaqueInternalResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "opaque://internal".to_string(),
                name: "opaque-internal-resource".to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Err(McpError::with_data(
                McpErrorCode::InternalError,
                PANIC_CANARY,
                serde_json::json!({"secret": PANIC_CANARY}),
            ))
        }
    }

    struct OpaqueInternalPrompt;

    impl PromptHandler for OpaqueInternalPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "opaque_internal_prompt".to_string(),
                description: None,
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Err(McpError::with_data(
                McpErrorCode::InternalError,
                PANIC_CANARY,
                serde_json::json!({"secret": PANIC_CANARY}),
            ))
        }
    }

    struct PanicResource;

    impl ResourceHandler for PanicResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "panic://resource".to_string(),
                name: "panic-resource".to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            panic!("{PANIC_CANARY}")
        }
    }

    struct PanicPrompt;

    impl PromptHandler for PanicPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "panic_prompt".to_string(),
                description: None,
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            panic!("{PANIC_CANARY}")
        }
    }

    struct DefinitionPanicTool(std::sync::atomic::AtomicBool);

    impl ToolHandler for DefinitionPanicTool {
        fn definition(&self) -> Tool {
            if self.0.swap(true, Ordering::Relaxed) {
                panic!("{PANIC_CANARY}");
            }
            Tool {
                name: "definition_panic_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![])
        }
    }

    struct DefinitionPanicResource(std::sync::atomic::AtomicBool);

    impl ResourceHandler for DefinitionPanicResource {
        fn definition(&self) -> Resource {
            if self.0.swap(true, Ordering::Relaxed) {
                panic!("{PANIC_CANARY}");
            }
            Resource {
                uri: "panic://definition".to_string(),
                name: "definition-panic-resource".to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![])
        }
    }

    struct TemplatePanicResource(std::sync::atomic::AtomicBool);

    impl ResourceHandler for TemplatePanicResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "panic-template://placeholder".to_string(),
                name: "template-panic-resource".to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            if self.0.swap(true, Ordering::Relaxed) {
                panic!("{PANIC_CANARY}");
            }
            Some(ResourceTemplate {
                uri_template: "panic-template://{id}".to_string(),
                name: "template-panic-resource".to_string(),
                description: None,
                mime_type: None,
                icon: None,
                version: None,
                tags: vec![],
            })
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            unreachable!("templated reads use read_with_uri")
        }

        fn read_with_uri(
            &self,
            _ctx: &McpContext,
            uri: &str,
            _params: &UriParams,
        ) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: uri.to_string(),
                mime_type: None,
                text: Some("template-content".to_string()),
                blob: None,
            }])
        }
    }

    struct DefinitionPanicPrompt(std::sync::atomic::AtomicBool);

    impl PromptHandler for DefinitionPanicPrompt {
        fn definition(&self) -> Prompt {
            if self.0.swap(true, Ordering::Relaxed) {
                panic!("{PANIC_CANARY}");
            }
            Prompt {
                name: "definition_panic_prompt".to_string(),
                description: None,
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _args: std::collections::HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(vec![])
        }
    }

    // ── Router::new ────────────────────────────────────────────────────

    #[test]
    fn new_router_is_empty() {
        let r = Router::new();
        assert_eq!(r.tools_count(), 0);
        assert_eq!(r.resources_count(), 0);
        assert_eq!(r.resource_templates_count(), 0);
        assert_eq!(r.prompts_count(), 0);
        assert!(r.tools().is_empty());
        assert_eq!(r.resources().len(), 0);
        assert!(r.resource_templates().is_empty());
        assert!(r.prompts().is_empty());
    }

    #[test]
    fn default_router_is_empty() {
        let r = Router::default();
        assert_eq!(r.tools_count(), 0);
    }

    // ── add_tool / get_tool ────────────────────────────────────────────

    #[test]
    fn add_and_get_tool() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("my_tool"))
            .expect("tool registration succeeds");
        assert_eq!(r.tools_count(), 1);
        assert!(r.get_tool("my_tool").is_some());
        assert!(r.get_tool("other").is_none());
    }

    #[test]
    fn caller_owned_legacy_dispatch_is_frozen_at_registration() {
        struct AsyncNamedTool;

        impl ToolHandler for AsyncNamedTool {
            fn definition(&self) -> Tool {
                NamedTool::new("async_tool").definition()
            }

            fn execution_mode(&self) -> crate::ToolExecutionMode {
                crate::ToolExecutionMode::Async
            }

            fn call(
                &self,
                _context: &McpContext,
                _arguments: serde_json::Value,
            ) -> McpResult<Vec<Content>> {
                Ok(Vec::new())
            }
        }

        let mut router = Router::new();
        router
            .add_legacy_tool(AsyncNamedTool)
            .expect("async legacy registration succeeds");
        router
            .add_legacy_tool(NamedTool::new("ordinary_tool"))
            .expect("ordinary legacy registration succeeds");

        let proxy_request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({"name": "async_tool", "arguments": {}})),
            1_i64,
        );
        assert!(router.legacy_request_uses_transport_owned_async_dispatch(&proxy_request));

        let ordinary_request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({"name": "ordinary_tool", "arguments": {}})),
            2_i64,
        );
        assert!(!router.legacy_request_uses_transport_owned_async_dispatch(&ordinary_request));
    }

    #[test]
    fn add_tool_replace_on_duplicate() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("t"))
            .expect("initial registration succeeds");
        r.add_tool(NamedTool::new("t"))
            .expect("valid replacement succeeds");
        assert_eq!(r.tools_count(), 1);
        // Order preserved (only one entry)
        assert_eq!(r.tools().len(), 1);
    }

    #[test]
    fn tools_returns_definitions_in_order() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        let names: Vec<_> = r.tools().iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["b", "a"]); // insertion order
    }

    #[test]
    fn explicit_legacy_tool_is_callable_only_on_exact_2024_routes() {
        let mut router = Router::new();
        router
            .add_legacy_tool(InvalidFinalSchemaNamedTool::with_tags(
                "legacy-only-tool",
                vec!["legacy".to_owned()],
            ))
            .expect("explicit legacy registration does not claim final admission");
        assert_eq!(
            router
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            vec!["legacy-only-tool"]
        );
        assert!(
            !router
                .server_discovery_behavior_registry()
                .contains(ServerBehavior::ToolsList),
            "a legacy-only catalog cannot produce a modern discovery claim"
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 78, Budget::INFINITE, &state);
        let legacy = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "legacy-only-tool".to_owned(),
                arguments: Some(serde_json::json!({})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("exact 2024 dispatch reaches the explicit legacy handler");
        assert_eq!(
            serde_json::to_value(legacy).expect("legacy result serializes")["content"][0]["text"],
            "called legacy-only-tool"
        );

        let modern_catalog = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 79_i64),
        ))
        .expect("modern tools/list remains a valid empty catalog");
        assert_eq!(modern_catalog["tools"], serde_json::json!([]));
        let modern_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request("legacy-only-tool", serde_json::json!({}), 80_i64),
        ))
        .expect_err("modern dispatch cannot resolve an explicit legacy-only tool");
        assert_eq!(modern_error.code, McpErrorCode::InvalidParams);
        assert_eq!(router.tools_count(), 1);
    }

    #[test]
    fn explicit_legacy_resource_and_prompt_are_inert_on_final_routes() {
        let mut router = Router::new();
        router.add_legacy_resource(NamedResource::new("file:///legacy-only"));
        router.add_legacy_resource_template(marked_template("file:///{name}", "legacy-only"));
        router.add_legacy_prompt(NamedPrompt::new("legacy-only-prompt"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 181, Budget::INFINITE, &state);

        let legacy_resource = block_on(router.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "file:///legacy-only".to_owned(),
                meta: None,
            },
            state.clone(),
            None,
            None,
        ))
        .expect("the exact legacy resource route retains its explicit registration");
        assert_eq!(legacy_resource.contents.len(), 1);
        let legacy_prompt = block_on(router.handle_prompts_get(
            &request_ctx,
            GetPromptParams {
                name: "legacy-only-prompt".to_owned(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the exact legacy prompt route retains its explicit registration");
        assert_eq!(legacy_prompt.messages.len(), 0);

        let modern_resources = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/list",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                })),
                182_i64,
            ),
        ))
        .expect("final discovery remains valid with only legacy resources");
        assert_eq!(modern_resources["resources"], serde_json::json!([]));
        let modern_templates = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/templates/list",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                })),
                183_i64,
            ),
        ))
        .expect("final template discovery remains valid with only legacy templates");
        assert_eq!(modern_templates["resourceTemplates"], serde_json::json!([]));

        let modern_prompt = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "prompts/get",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "name": "legacy-only-prompt",
                })),
                184_i64,
            ),
        ))
        .expect_err("changing only the dispatch era refuses the legacy-only prompt");
        assert_eq!(modern_prompt.code, McpErrorCode::InvalidParams);
        assert!(
            !router
                .server_discovery_behavior_registry()
                .contains(ServerBehavior::ResourcesList)
        );
        assert!(
            !router
                .server_discovery_behavior_registry()
                .contains(ServerBehavior::PromptsList)
        );
    }

    // ── add_tool_with_behavior ─────────────────────────────────────────

    #[test]
    fn duplicate_error_warn_and_ignore_preserve_handler_schema_and_order() {
        for behavior in [
            crate::DuplicateBehavior::Error,
            crate::DuplicateBehavior::Warn,
            crate::DuplicateBehavior::Ignore,
        ] {
            let original_legacy_calls = Arc::new(AtomicUsize::new(0));
            let original_final_calls = Arc::new(AtomicUsize::new(0));
            let candidate_legacy_calls = Arc::new(AtomicUsize::new(0));
            let candidate_final_calls = Arc::new(AtomicUsize::new(0));
            let mut router = Router::new();
            router
                .add_tool(NamedTool::new("before"))
                .expect("baseline tool admission succeeds");
            router
                .add_tool(DuplicateInvariantTool {
                    label: "original",
                    schema_property: "original_property",
                    legacy_calls: Arc::clone(&original_legacy_calls),
                    final_calls: Arc::clone(&original_final_calls),
                })
                .expect("original admission succeeds");
            router
                .add_tool(NamedTool::new("after"))
                .expect("trailing tool admission succeeds");

            let cx = Cx::for_testing();
            let state = SessionState::new();
            let request_ctx = request_context(&cx, 81, Budget::INFINITE, &state);
            let legacy_before =
                serde_json::to_value(router.tools()).expect("legacy catalog serializes");
            let modern_before = block_on(router.dispatch_stateless(
                &request_ctx,
                &final_tools_list_request(None, None, None, 81_i64),
            ))
            .expect("modern catalog is available");

            let registration = router.add_tool_with_behavior(
                DuplicateInvariantTool {
                    label: "candidate",
                    schema_property: "candidate_property",
                    legacy_calls: Arc::clone(&candidate_legacy_calls),
                    final_calls: Arc::clone(&candidate_final_calls),
                },
                behavior,
            );
            if behavior == crate::DuplicateBehavior::Error {
                let error = registration.expect_err("Error rejects the duplicate");
                assert!(error.message.contains("already exists"));
            } else {
                registration.expect("Warn and Ignore retain the original");
            }

            assert_eq!(
                serde_json::to_value(router.tools()).expect("legacy catalog serializes"),
                legacy_before
            );
            assert_eq!(
                block_on(router.dispatch_stateless(
                    &request_ctx,
                    &final_tools_list_request(None, None, None, 82_i64),
                ))
                .expect("modern catalog remains available"),
                modern_before
            );
            assert_eq!(
                router
                    .tools()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>(),
                vec!["before", "duplicate-invariant-tool", "after"]
            );

            let legacy = block_on(router.handle_tools_call(
                &request_ctx,
                CallToolParams {
                    name: "duplicate-invariant-tool".to_owned(),
                    arguments: Some(serde_json::json!({})),
                    meta: None,
                },
                state.clone(),
                None,
                None,
            ))
            .expect("legacy dispatch retains the original");
            assert_eq!(
                serde_json::to_value(legacy).expect("legacy result serializes")["content"][0]["text"],
                "original"
            );
            let modern = block_on(router.dispatch_stateless(
                &request_ctx,
                &final_tools_call_request(
                    "duplicate-invariant-tool",
                    serde_json::json!({}),
                    83_i64,
                ),
            ))
            .expect("modern dispatch retains the original");
            assert_eq!(modern["content"][0]["text"], "original");
            assert_eq!(original_legacy_calls.load(Ordering::SeqCst), 1);
            assert_eq!(original_final_calls.load(Ordering::SeqCst), 1);
            assert_eq!(candidate_legacy_calls.load(Ordering::SeqCst), 0);
            assert_eq!(candidate_final_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn duplicate_replace_atomically_updates_handler_schema_and_retains_order() {
        let replacement_legacy_calls = Arc::new(AtomicUsize::new(0));
        let replacement_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("before"))
            .expect("baseline admission succeeds");
        router
            .add_tool(DuplicateInvariantTool {
                label: "original",
                schema_property: "original_property",
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::new(AtomicUsize::new(0)),
            })
            .expect("original admission succeeds");
        router
            .add_tool(NamedTool::new("after"))
            .expect("trailing admission succeeds");
        router
            .add_tool_with_behavior(
                DuplicateInvariantTool {
                    label: "replacement",
                    schema_property: "replacement_property",
                    legacy_calls: Arc::clone(&replacement_legacy_calls),
                    final_calls: Arc::clone(&replacement_final_calls),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("Replace admits before committing the candidate");

        assert_eq!(
            router
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            vec!["before", "duplicate-invariant-tool", "after"]
        );
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 84, Budget::INFINITE, &state);
        let modern_catalog = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 84_i64),
        ))
        .expect("modern replacement catalog is available");
        assert_eq!(
            modern_catalog["tools"][1]["inputSchema"]["properties"],
            serde_json::json!({"replacement_property": {"type": "boolean"}})
        );
        assert_eq!(
            router.tools()[1].description.as_deref(),
            Some("replacement")
        );

        let legacy = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "duplicate-invariant-tool".to_owned(),
                arguments: Some(serde_json::json!({})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("legacy dispatch uses replacement");
        assert_eq!(
            serde_json::to_value(legacy).expect("legacy result serializes")["content"][0]["text"],
            "replacement"
        );
        let modern = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "duplicate-invariant-tool",
                serde_json::json!({}),
                85_i64,
            ),
        ))
        .expect("modern dispatch uses replacement");
        assert_eq!(modern["content"][0]["text"], "replacement");
        assert_eq!(replacement_legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn add_tool_behavior_new_tool_ok() {
        let mut r = Router::new();
        r.add_tool_with_behavior(NamedTool::new("t"), crate::DuplicateBehavior::Error)
            .unwrap();
        assert_eq!(r.tools_count(), 1);
    }

    // ── add_resource / get_resource ────────────────────────────────────

    #[test]
    fn add_and_get_resource() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a.txt"));
        assert_eq!(r.resources_count(), 1);
        assert!(r.get_resource("file:///a.txt").is_some());
        assert!(r.get_resource("file:///b.txt").is_none());
    }

    #[test]
    fn resources_returns_definitions_in_order() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///b"));
        r.add_resource(NamedResource::new("file:///a"));
        let uris: Vec<_> = r.resources().iter().map(|res| res.uri.clone()).collect();
        assert_eq!(uris, vec!["file:///b", "file:///a"]);
    }

    // ── add_resource_with_behavior ─────────────────────────────────────

    #[test]
    fn add_resource_behavior_error_on_duplicate() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        let err = r
            .add_resource_with_behavior(
                NamedResource::new("file:///a"),
                crate::DuplicateBehavior::Error,
            )
            .unwrap_err();
        assert!(err.message.contains("already exists"));
    }

    #[test]
    fn add_resource_behavior_ignore() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource_with_behavior(
            NamedResource::new("file:///a"),
            crate::DuplicateBehavior::Ignore,
        )
        .unwrap();
        assert_eq!(r.resources_count(), 1);
    }

    // ── add_prompt / get_prompt ────────────────────────────────────────

    #[test]
    fn add_and_get_prompt() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("greet"));
        assert_eq!(r.prompts_count(), 1);
        assert!(r.get_prompt("greet").is_some());
        assert!(r.get_prompt("other").is_none());
    }

    #[test]
    fn prompts_returns_definitions_in_order() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("z"));
        r.add_prompt(NamedPrompt::new("a"));
        let names: Vec<_> = r.prompts().iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["z", "a"]);
    }

    // ── add_prompt_with_behavior ───────────────────────────────────────

    #[test]
    fn add_prompt_behavior_error_on_duplicate() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        let err = r
            .add_prompt_with_behavior(NamedPrompt::new("p"), crate::DuplicateBehavior::Error)
            .unwrap_err();
        assert!(err.message.contains("already exists"));
    }

    #[test]
    fn add_prompt_behavior_warn_keeps_original() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        r.add_prompt_with_behavior(NamedPrompt::new("p"), crate::DuplicateBehavior::Warn)
            .unwrap();
        assert_eq!(r.prompts_count(), 1);
    }

    #[test]
    fn duplicate_registration_errors_do_not_echo_peer_identifiers() {
        let canary = "raw-peer-identifier-canary";

        let mut tools = Router::new();
        tools
            .add_tool(NamedTool::new(canary))
            .expect("tool registration succeeds");
        let tool_error = tools
            .add_tool_with_behavior(NamedTool::new(canary), crate::DuplicateBehavior::Error)
            .unwrap_err();

        // Resource identities must be scheme-valid to survive final-catalog
        // projection; the canary stays the peer-controlled URI component.
        let canary_uri = format!("resource://{canary}");
        let mut resources = Router::new();
        resources.add_resource(NamedResource::new(&canary_uri));
        let resource_error = resources
            .add_resource_with_behavior(
                NamedResource::new(&canary_uri),
                crate::DuplicateBehavior::Error,
            )
            .unwrap_err();

        let mut templates = Router::new();
        templates.add_resource_template(marked_template(&canary_uri, "original"));
        let template_error = templates
            .add_resource_template_with_behavior(
                marked_template(&canary_uri, "incoming"),
                crate::DuplicateBehavior::Error,
            )
            .unwrap_err();

        let mut prompts = Router::new();
        prompts.add_prompt(NamedPrompt::new(canary));
        let prompt_error = prompts
            .add_prompt_with_behavior(NamedPrompt::new(canary), crate::DuplicateBehavior::Error)
            .unwrap_err();

        for error in [tool_error, resource_error, template_error, prompt_error] {
            assert!(error.message.contains("already exists"));
            assert!(!error.message.contains(canary));
            assert!(
                is_duplicate_registration_error(&error),
                "duplicate registration errors must be recognizable without the peer key: {error:?}"
            );
        }
        assert!(
            !is_duplicate_registration_error(&McpError::internal_error(
                "tool metadata hook panicked during admission"
            )),
            "a non-duplicate admission failure must not be treated as a colliding catalog skip"
        );
        assert!(
            !is_duplicate_registration_error(&McpError::invalid_request(
                "MCP Apps tool metadata requires ServerBuilder::mcp_apps_tool"
            )),
            "an InvalidRequest that is not a duplicate must still fail prefixed as_proxy_typed"
        );
    }

    // ── add_resource_template ──────────────────────────────────────────

    #[test]
    fn add_resource_template_and_list() {
        let mut r = Router::new();
        let tmpl = ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        r.add_resource_template(tmpl);
        assert_eq!(r.resource_templates_count(), 1);
        assert!(r.get_resource_template("db://{table}").is_some());
        assert!(r.get_resource_template("db://{other}").is_none());
    }

    #[test]
    fn add_resource_template_replaces_existing() {
        let mut r = Router::new();
        let tmpl1 = ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db1".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let tmpl2 = ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db2".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        r.add_resource_template(tmpl1);
        r.add_resource_template(tmpl2);
        assert_eq!(r.resource_templates_count(), 1);
        let tmpl = r.get_resource_template("db://{table}").unwrap();
        assert_eq!(tmpl.name, "db2");
    }

    #[test]
    fn add_resource_template_with_behavior_preserves_or_replaces_identity() {
        for behavior in [
            crate::DuplicateBehavior::Warn,
            crate::DuplicateBehavior::Ignore,
            crate::DuplicateBehavior::Error,
        ] {
            let mut router = Router::new();
            router.add_resource_template(marked_template("peer://{secret}", "original"));
            let result = router.add_resource_template_with_behavior(
                marked_template("peer://{secret}", "incoming"),
                behavior,
            );

            if behavior == crate::DuplicateBehavior::Error {
                let error = result.expect_err("Error policy rejects the duplicate");
                assert!(error.message.contains("already exists"));
                assert!(!error.message.contains("peer://{secret}"));
            } else {
                result.expect("Warn and Ignore keep the original");
            }
            assert_eq!(router.resource_templates_count(), 1);
            assert_eq!(
                router
                    .get_resource_template("peer://{secret}")
                    .expect("original template remains")
                    .name,
                "original"
            );
        }

        let mut router = Router::new();
        router.add_resource_template(marked_template("peer://{secret}", "original"));
        router
            .add_resource_template_with_behavior(
                marked_template("peer://{secret}", "incoming"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("Replace accepts the duplicate");
        assert_eq!(router.resource_templates_count(), 1);
        assert_eq!(
            router
                .get_resource_template("peer://{secret}")
                .expect("replacement template exists")
                .name,
            "incoming"
        );
    }

    // ── resource_exists / resolve_resource ──────────────────────────────

    #[test]
    fn resource_exists_for_static_resource() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a.txt"));
        assert!(r.resource_exists("file:///a.txt"));
        assert!(!r.resource_exists("file:///b.txt"));
    }

    // ── strict_input_validation ────────────────────────────────────────

    #[test]
    fn strict_input_validation_default_off() {
        let r = Router::new();
        assert!(!r.strict_input_validation());
    }

    #[test]
    fn set_strict_input_validation() {
        let mut r = Router::new();
        r.set_strict_input_validation(true);
        assert!(r.strict_input_validation());
        r.set_strict_input_validation(false);
        assert!(!r.strict_input_validation());
    }

    // ── set_list_page_size ─────────────────────────────────────────────

    #[test]
    fn set_list_page_size_zero_treated_as_none() {
        let mut r = Router::new();
        r.set_list_page_size(Some(0));
        // Zero page size is filtered to None
        assert!(r.list_page_size.is_none());
    }

    #[test]
    fn set_list_page_size_positive() {
        let mut r = Router::new();
        r.set_list_page_size(Some(10));
        assert_eq!(r.list_page_size, Some(10));
    }

    #[test]
    fn set_list_page_size_none() {
        let mut r = Router::new();
        r.set_list_page_size(Some(10));
        r.set_list_page_size(None);
        assert!(r.list_page_size.is_none());
    }

    // ── tools_filtered ─────────────────────────────────────────────────

    #[test]
    fn tools_filtered_no_filters_returns_all() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let tools = r.tools_filtered(None, None);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn tools_filtered_by_session_state_disables() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_tools", &disabled);
        let tools = r.tools_filtered(Some(&state), None);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "b");
    }

    #[test]
    fn tools_filtered_by_tags() {
        let mut r = Router::new();
        r.add_tool(NamedTool::with_tags("a", vec!["db".to_string()]))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::with_tags("b", vec!["web".to_string()]))
            .expect("tool registration succeeds");
        let include = vec!["db".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let tools = r.tools_filtered(None, Some(&filters));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "a");
    }

    // ── resources_filtered ─────────────────────────────────────────────

    #[test]
    fn resources_filtered_by_session_state() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource(NamedResource::new("file:///b"));
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["file:///a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_resources", &disabled);
        let res = r.resources_filtered(Some(&state), None);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].uri, "file:///b");
    }

    // ── prompts_filtered ───────────────────────────────────────────────

    #[test]
    fn prompts_filtered_by_session_state() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("a"));
        r.add_prompt(NamedPrompt::new("b"));
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_prompts", &disabled);
        let prompts = r.prompts_filtered(Some(&state), None);
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "b");
    }

    #[test]
    fn prompts_filtered_by_tags() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::with_tags("a", vec!["internal".to_string()]));
        r.add_prompt(NamedPrompt::with_tags("b", vec!["public".to_string()]));
        let exclude = vec!["internal".to_string()];
        let filters = TagFilters::new(None, Some(&exclude));
        let prompts = r.prompts_filtered(None, Some(&filters));
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "b");
    }

    // ── resource_templates_filtered ────────────────────────────────────

    #[test]
    fn resource_templates_filtered_by_session_state() {
        let mut r = Router::new();
        r.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["admin".to_string()],
        });
        r.add_resource_template(ResourceTemplate {
            uri_template: "cache://{key}".to_string(),
            name: "cache".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["db://{table}".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_resources", &disabled);
        let tmpls = r.resource_templates_filtered(Some(&state), None);
        assert_eq!(tmpls.len(), 1);
        assert_eq!(tmpls[0].name, "cache");
    }

    // ── apply_prefix / validate_prefix ─────────────────────────────────

    #[test]
    fn apply_prefix_with_prefix() {
        assert_eq!(Router::apply_prefix("tool", Some("ns")), "ns/tool");
    }

    #[test]
    fn apply_prefix_no_prefix() {
        assert_eq!(Router::apply_prefix("tool", None), "tool");
    }

    #[test]
    fn apply_prefix_empty_prefix() {
        assert_eq!(Router::apply_prefix("tool", Some("")), "tool");
    }

    #[test]
    fn validate_prefix_valid() {
        assert!(Router::validate_prefix("my-prefix_1").is_ok());
    }

    #[test]
    fn validate_prefix_empty_is_ok() {
        assert!(Router::validate_prefix("").is_ok());
    }

    #[test]
    fn validate_prefix_rejects_slashes() {
        let err = Router::validate_prefix("a/b").unwrap_err();
        assert!(err.contains("slashes"));
    }

    #[test]
    fn validate_prefix_rejects_special_chars() {
        let err = Router::validate_prefix("a@b").unwrap_err();
        assert!(err.contains("invalid character"));
    }

    // ── MountResult ────────────────────────────────────────────────────

    #[test]
    fn mount_result_default_has_no_components() {
        let r = MountResult::default();
        assert!(!r.has_components());
        assert!(r.is_success());
    }

    #[test]
    fn mount_result_with_tools_has_components() {
        let mut r = MountResult::default();
        r.tools = 1;
        assert!(r.has_components());
    }

    #[test]
    fn mount_result_debug() {
        let r = MountResult::default();
        let debug = format!("{:?}", r);
        assert!(debug.contains("MountResult"));
    }

    // ── mount ──────────────────────────────────────────────────────────

    #[test]
    fn mount_tools_with_prefix() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("query"))
            .expect("tool registration succeeds");
        let result = main.mount(sub, Some("db"));
        assert_eq!(result.tools, 1);
        assert!(main.get_tool("db/query").is_some());
        assert!(main.get_tool("query").is_none());
    }

    #[test]
    fn mount_without_prefix() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("query"))
            .expect("tool registration succeeds");
        let result = main.mount(sub, None);
        assert_eq!(result.tools, 1);
        assert!(main.get_tool("query").is_some());
    }

    struct RecordingNameCompletion {
        names: Arc<Mutex<Vec<String>>>,
    }

    impl CompletionHandler for RecordingNameCompletion {
        fn complete_legacy(
            &self,
            _ctx: &McpContext,
            params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            let name = match params.reference {
                fastmcp_protocol::LegacyCompletionReference::Prompt { name } => name,
                fastmcp_protocol::LegacyCompletionReference::Resource { uri } => uri,
            };
            self.names
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(name);
            Ok(CompletionValues {
                values: vec![format!("{}ging", params.argument.value)],
                total: Some(1),
                has_more: Some(false),
            })
        }

        fn complete_final(
            &self,
            _ctx: &McpContext,
            params: FinalCompletionParams,
        ) -> McpResult<fastmcp_protocol::FinalCompletionValues> {
            let name = match params.reference {
                fastmcp_protocol::FinalCompletionReference::Prompt { name }
                | fastmcp_protocol::FinalCompletionReference::PromptWithTitle { name, .. } => name,
                fastmcp_protocol::FinalCompletionReference::Resource { uri } => uri,
            };
            self.names
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(name);
            Ok(fastmcp_protocol::FinalCompletionValues {
                values: vec![format!("{}ging", params.argument.value)],
                total: Some(fastmcp_protocol::JsonInteger::from(1_i64)),
                has_more: Some(false),
            })
        }
    }

    #[test]
    fn mount_transfers_prompt_completion_providers_and_rewrites_prefixed_names() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut child = Router::new();
        child.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        child.add_legacy_prompt_completion_handler(
            "prompt-argument-boundary",
            RecordingNameCompletion {
                names: Arc::clone(&seen),
            },
        );
        child.add_prompt_completion_handler(
            "prompt-argument-boundary",
            RecordingNameCompletion {
                names: Arc::clone(&seen),
            },
        );

        let mut main = Router::new();
        let result = main.mount(child, Some("ns"));
        assert!(result.is_success());
        assert_eq!(result.prompts, 1);
        assert!(main.has_completion_handler());

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 401, Budget::INFINITE, &state);
        let legacy = block_on(main.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "ns/prompt-argument-boundary"},
                    "argument": {"name": "topic", "value": "sta"},
                })),
                401_i64,
            ),
        ))
        .expect("prefixed legacy completion must reach the mounted provider");
        assert_eq!(
            legacy["completion"]["values"],
            serde_json::json!(["staging"])
        );

        let unprefixed = block_on(main.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                    "argument": {"name": "topic", "value": "sta"},
                })),
                402_i64,
            ),
        ));
        assert!(
            unprefixed.is_err(),
            "the child's original completion name must not remain after a prefixed mount"
        );

        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            seen.as_slice(),
            ["prompt-argument-boundary"],
            "the mounted provider must observe the child's original prompt name"
        );
    }

    #[test]
    fn mount_without_prefix_keeps_completion_provider_keys() {
        let mut child = Router::new();
        child.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        child.add_legacy_prompt_completion_handler("prompt-argument-boundary", EchoCompletion);
        let mut main = Router::new();
        assert!(main.mount(child, None).is_success());
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 403, Budget::INFINITE, &state);
        let legacy = block_on(main.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                    "argument": {"name": "topic", "value": "sta"},
                })),
                403_i64,
            ),
        ))
        .expect("unprefixed mount keeps the original completion key");
        assert_eq!(
            legacy["completion"]["values"],
            serde_json::json!(["staging"])
        );
    }

    #[test]
    fn mount_transfers_resource_template_completion_providers_and_rewrites_prefixed_uris() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut child = Router::new();
        child
            .add_resource_template_with_behavior(
                marked_template("mcp://books/{id}", "books"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("child resource template is admitted");
        child.add_legacy_resource_template_completion_handler(
            "mcp://books/{id}",
            RecordingNameCompletion {
                names: Arc::clone(&seen),
            },
        );
        child.add_resource_template_completion_handler(
            "mcp://books/{id}",
            RecordingNameCompletion {
                names: Arc::clone(&seen),
            },
        );

        let mut main = Router::new();
        let result = main.mount(child, Some("ns"));
        assert!(result.is_success());
        assert_eq!(result.resource_templates, 1);
        assert!(main.has_completion_handler());

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 411, Budget::INFINITE, &state);
        let legacy = block_on(main.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/resource", "uri": "ns/mcp://books/{id}"},
                    "argument": {"name": "id", "value": "sta"},
                })),
                411_i64,
            ),
        ))
        .expect("prefixed legacy template completion must reach the mounted provider");
        assert_eq!(
            legacy["completion"]["values"],
            serde_json::json!(["staging"])
        );

        let unprefixed = block_on(main.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/resource", "uri": "mcp://books/{id}"},
                    "argument": {"name": "id", "value": "sta"},
                })),
                412_i64,
            ),
        ));
        assert!(
            unprefixed.is_err(),
            "the child's original template completion URI must not remain after a prefixed mount"
        );

        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            seen.as_slice(),
            ["mcp://books/{id}"],
            "the mounted provider must observe the child's original template URI"
        );
    }

    #[test]
    fn prefixed_mount_strips_prefix_for_server_wide_completion_fallback() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut child = Router::new();
        child.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        child.add_completion_handler(RecordingNameCompletion {
            names: Arc::clone(&seen),
        });

        let mut main = Router::new();
        assert!(main.mount(child, Some("ns")).is_success());

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 414, Budget::INFINITE, &state);
        let legacy = block_on(main.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "ns/prompt-argument-boundary"},
                    "argument": {"name": "topic", "value": "sta"},
                })),
                414_i64,
            ),
        ))
        .expect("prefixed fallback completion must reach the child's server-wide handler");
        assert_eq!(
            legacy["completion"]["values"],
            serde_json::json!(["staging"])
        );

        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            seen.as_slice(),
            ["prompt-argument-boundary"],
            "the child's server-wide handler must observe the original prompt name"
        );
    }

    #[test]
    fn mount_prompts_does_not_replace_destination_fallback_completion_handler() {
        let mut dest = Router::new();
        dest.add_prompt(NamedPrompt::new("kept"));
        dest.add_completion_handler(CountingCompletion {
            final_calls: Arc::new(AtomicUsize::new(0)),
        });
        let mut child = Router::new();
        child.add_prompt(NamedPrompt::new("incoming"));
        child.add_completion_handler(EchoCompletion);

        assert!(
            dest.mount_prompts_with_behavior(child, None, crate::DuplicateBehavior::Replace)
                .is_success()
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 413, Budget::INFINITE, &state);
        let kept = block_on(dest.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "kept"},
                    "argument": {"name": "topic", "value": "sta"},
                })),
                413_i64,
            ),
        ))
        .expect("prompts-only mount must keep the destination fallback completion handler");
        assert_eq!(kept["completion"]["values"], serde_json::json!(["legacy"]));
    }

    #[test]
    fn mount_resources_with_prefix() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_resource(NamedResource::new("file:///a"));
        let result = main.mount(sub, Some("ns"));
        assert_eq!(result.resources, 1);
        assert!(main.get_resource("ns/file:///a").is_some());

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let read = block_on(main.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "ns/file:///a".to_string(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("mounted resource is readable through its public URI");
        let [
            LegacyResourceContent::Text {
                uri,
                text,
                additional,
                ..
            },
        ] = read.contents.as_slice()
        else {
            panic!("mounted resource must retain the exact legacy text shape");
        };
        assert_eq!(uri, "ns/file:///a");
        assert_eq!(text, "content");
        assert!(additional.is_empty());
    }

    #[test]
    fn mount_namespaced_prefixes_tools_and_keeps_resource_uris() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("query"))
            .expect("namespaced query tool registers");
        sub.add_resource(NamedResource::new("file:///a"));
        let result =
            main.mount_namespaced_with_behavior(sub, Some("ns"), crate::DuplicateBehavior::Replace);
        assert!(result.is_success());
        assert_eq!(result.tools, 1);
        assert_eq!(result.resources, 1);
        assert!(main.get_tool("ns/query").is_some());
        assert!(main.get_tool("query").is_none());
        assert!(main.get_resource("file:///a").is_some());
        assert!(main.get_resource("ns/file:///a").is_none());
        assert!(main.final_resources.contains_key("file:///a"));
    }

    #[test]
    fn mounting_resource_does_not_requery_one_shot_definition() {
        let mut source = Router::new();
        source.add_resource(DefinitionPanicResource(std::sync::atomic::AtomicBool::new(
            false,
        )));
        let mut destination = Router::new();

        let result = destination.mount_resources(source, Some("ns"));

        assert!(result.is_success());
        assert_eq!(result.resources, 1);
        assert!(destination.get_resource("ns/panic://definition").is_some());
    }

    #[test]
    fn mounting_resource_template_does_not_requery_one_shot_template() {
        let mut source = Router::new();
        source.add_resource(TemplatePanicResource(std::sync::atomic::AtomicBool::new(
            false,
        )));
        let mut destination = Router::new();

        let result = destination.mount_resources(source, Some("ns"));

        assert!(result.is_success());
        assert_eq!(result.resource_templates, 1);
        assert!(
            destination
                .get_resource_template("ns/panic-template://{id}")
                .is_some()
        );
    }

    #[test]
    fn prefixed_resource_template_mount_is_legacy_only_after_final_source_dispatch() {
        struct FinalTemplateResource;

        impl ResourceHandler for FinalTemplateResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "mcp://mounted/template".to_owned(),
                    name: "mounted-template".to_owned(),
                    description: None,
                    mime_type: Some("text/plain".to_owned()),
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }
            }

            fn template(&self) -> Option<ResourceTemplate> {
                Some(marked_template("mcp://mounted/{id}", "mounted-template"))
            }

            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                unreachable!("templated reads receive their URI parameters")
            }

            fn read_with_uri(
                &self,
                _ctx: &McpContext,
                uri: &str,
                _params: &UriParams,
            ) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![ResourceContent {
                    uri: uri.to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    text: Some("source-template".to_owned()),
                    blob: None,
                }])
            }
        }

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 191, Budget::INFINITE, &state);
        let final_metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let mut source = Router::new();
        source.add_resource(FinalTemplateResource);

        let source_final_read = block_on(source.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": final_metadata.clone(),
                    "uri": "mcp://mounted/item",
                })),
                191_i64,
            ),
        ))
        .expect("the unprefixed final template dispatches");
        assert_eq!(source_final_read["contents"][0]["text"], "source-template");

        let mut destination = Router::new();
        let mounted = destination.mount_resources(source, Some("peer"));
        assert!(mounted.is_success());
        assert_eq!(mounted.resource_templates, 1);

        let legacy_templates = destination
            .handle_resource_templates_list(
                &request_ctx,
                ListResourceTemplatesParams::default(),
                None,
            )
            .expect("the prefixed template remains in legacy discovery");
        assert_eq!(
            legacy_templates.resource_templates[0].uri_template,
            "peer/mcp://mounted/{id}"
        );
        let legacy_read = block_on(destination.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "peer/mcp://mounted/item".to_owned(),
                meta: None,
            },
            state.clone(),
            None,
            None,
        ))
        .expect("the prefixed template remains readable on the legacy surface");
        assert_eq!(
            serde_json::to_value(legacy_read).expect("legacy result serializes")["contents"][0]["text"],
            "source-template"
        );

        let final_templates = block_on(destination.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/templates/list",
                Some(serde_json::json!({"_meta": final_metadata.clone()})),
                192_i64,
            ),
        ))
        .expect("prefixed legacy-only templates do not break final discovery");
        assert_eq!(final_templates["resourceTemplates"], serde_json::json!([]));

        let final_error = block_on(destination.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": final_metadata,
                    "uri": "peer/mcp://mounted/item",
                })),
                193_i64,
            ),
        ))
        .expect_err("the relative mounted namespace is not exposed to final dispatch");
        assert_eq!(final_error.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn nested_resource_mounts_translate_both_namespace_layers() {
        let mut leaf = Router::new();
        leaf.add_resource(NamedResource::new("file:///a"));

        let mut middle = Router::new();
        assert!(middle.mount(leaf, Some("ns")).is_success());
        let mut outer = Router::new();
        assert!(outer.mount(middle, Some("ns")).is_success());

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let read = block_on(outer.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "ns/ns/file:///a".to_string(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("nested mounted resource is readable");
        let [
            LegacyResourceContent::Text {
                uri,
                text,
                additional,
                ..
            },
        ] = read.contents.as_slice()
        else {
            panic!("nested mounted resource must retain the exact legacy text shape");
        };
        assert_eq!(uri, "ns/ns/file:///a");
        assert_eq!(text, "content");
        assert!(additional.is_empty());
    }

    #[test]
    fn nested_resource_template_mounts_translate_every_namespace_layer() {
        struct TemplatedResource;

        impl ResourceHandler for TemplatedResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "db://placeholder".to_string(),
                    name: "database".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }

            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "db://{table}".to_string(),
                    name: "database".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }

            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                unreachable!("templated reads use read_with_uri")
            }

            fn read_with_uri(
                &self,
                _ctx: &McpContext,
                uri: &str,
                params: &UriParams,
            ) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![ResourceContent {
                    uri: uri.to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: params.get("table").cloned(),
                    blob: None,
                }])
            }
        }

        let mut source = Router::new();
        source.add_resource(TemplatedResource);
        let mut middle = Router::new();
        assert!(middle.mount_resources(source, Some("peer")).is_success());
        let mut mounted = Router::new();
        assert!(mounted.mount_resources(middle, Some("peer")).is_success());

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let read = block_on(mounted.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "peer/peer/db://users".to_string(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("mounted template is readable through its public URI");
        let [
            LegacyResourceContent::Text {
                uri,
                text,
                additional,
                ..
            },
        ] = read.contents.as_slice()
        else {
            panic!("mounted template must retain the exact legacy text shape");
        };
        assert_eq!(uri, "peer/peer/db://users");
        assert_eq!(text, "users");
        assert!(additional.is_empty());
    }

    #[test]
    fn handle_resources_read_resolves_true_async_mounted_template_and_translates_uri() {
        struct AsyncTemplatedResource {
            observed: Arc<Mutex<Option<(String, String)>>>,
        }

        impl ResourceHandler for AsyncTemplatedResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "async-db://placeholder".to_string(),
                    name: "async-database".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }

            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "async-db://{table}".to_string(),
                    name: "async-database".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }

            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                unreachable!("router must dispatch templated resources through the async override")
            }

            fn read_with_uri(
                &self,
                _ctx: &McpContext,
                _uri: &str,
                _params: &UriParams,
            ) -> McpResult<Vec<ResourceContent>> {
                unreachable!("router must not fall back to the synchronous templated read")
            }

            fn read_async_with_uri<'a>(
                &'a self,
                _ctx: &'a McpContext,
                uri: &'a str,
                params: &'a UriParams,
            ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
                Box::pin(async move {
                    let mut first_poll = true;
                    std::future::poll_fn(move |waker| {
                        if std::mem::take(&mut first_poll) {
                            waker.waker().wake_by_ref();
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(())
                        }
                    })
                    .await;
                    let Some(table) = params.get("table").cloned() else {
                        return Outcome::Err(McpError::invalid_params(
                            "mounted template did not resolve its table parameter",
                        ));
                    };
                    *self.observed.lock().expect("observation mutex poisoned") =
                        Some((uri.to_string(), table.clone()));
                    Outcome::Ok(vec![ResourceContent {
                        uri: uri.to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some(format!("async table {table}")),
                        blob: None,
                    }])
                })
            }
        }

        let observed = Arc::new(Mutex::new(None));
        let mut source = Router::new();
        source.add_resource(AsyncTemplatedResource {
            observed: Arc::clone(&observed),
        });
        let mut mounted = Router::new();
        let mount_result = mounted.mount_resources(source, Some("peer"));
        assert!(mount_result.is_success());
        assert_eq!(mount_result.resource_templates, 1);

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let read = block_on(mounted.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "peer/async-db://users".to_string(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("true-async mounted template is readable through its public URI");

        assert_eq!(read.contents.len(), 1);
        let [
            LegacyResourceContent::Text {
                uri,
                text,
                additional,
                ..
            },
        ] = read.contents.as_slice()
        else {
            panic!("async mounted template must retain the exact legacy text shape");
        };
        assert_eq!(uri, "peer/async-db://users");
        assert_eq!(text, "async table users");
        assert!(additional.is_empty());
        assert_eq!(
            *observed.lock().expect("observation mutex poisoned"),
            Some(("async-db://users".to_string(), "users".to_string()))
        );
    }

    #[test]
    fn mount_prompts_with_prefix() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_prompt(NamedPrompt::new("greet"));
        let result = main.mount(sub, Some("ns"));
        assert_eq!(result.prompts, 1);
        assert!(main.get_prompt("ns/greet").is_some());
    }

    #[test]
    fn mount_warns_on_conflict() {
        let mut main = Router::new();
        main.add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let result = main.mount(sub, None);
        assert_eq!(result.tools, 1);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("already exists"));
    }

    #[test]
    fn mount_rejects_invalid_prefix_without_mutating() {
        let mut main = Router::new();
        main.add_tool(NamedTool::new("original"))
            .expect("tool registration succeeds");
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("incoming"))
            .expect("tool registration succeeds");
        let result = main.mount(sub, Some("bad/prefix"));
        assert!(!result.is_success());
        assert_eq!(result.tools, 0);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("slashes"));
        assert!(!result.errors[0].contains("bad/prefix"));
        assert_eq!(main.tools_count(), 1);
        assert!(main.get_tool("original").is_some());
        assert!(main.get_tool("bad/prefix/incoming").is_none());
    }

    #[test]
    fn mount_with_behavior_honors_policy_for_every_component_kind() {
        for behavior in [
            crate::DuplicateBehavior::Warn,
            crate::DuplicateBehavior::Ignore,
            crate::DuplicateBehavior::Replace,
            crate::DuplicateBehavior::Error,
        ] {
            let mut main = marked_router("original");
            let mut sub = marked_router("incoming");
            if behavior == crate::DuplicateBehavior::Error {
                sub.add_tool(NamedTool::new("unique_tool"))
                    .expect("tool registration succeeds");
            }

            let result = main.mount_with_behavior(sub, None, behavior);
            let replaced = behavior == crate::DuplicateBehavior::Replace;
            let rejected = behavior == crate::DuplicateBehavior::Error;
            let expected_mounted = usize::from(replaced);

            assert_eq!(result.is_success(), !rejected);
            assert_eq!(result.tools, expected_mounted);
            assert_eq!(result.resources, expected_mounted);
            assert_eq!(result.resource_templates, expected_mounted);
            assert_eq!(result.prompts, expected_mounted);
            assert_eq!(result.errors.len(), if rejected { 4 } else { 0 });
            assert_eq!(
                result.warnings.len(),
                if matches!(
                    behavior,
                    crate::DuplicateBehavior::Warn | crate::DuplicateBehavior::Replace
                ) {
                    4
                } else {
                    0
                }
            );
            assert_router_marker(&main, if replaced { "incoming" } else { "original" });
            assert!(main.get_tool("unique_tool").is_none());

            for message in result.warnings.iter().chain(&result.errors) {
                assert!(!message.contains("duplicate_tool"));
                assert!(!message.contains("duplicate://resource"));
                assert!(!message.contains("mcp://duplicate-item/{item}"));
                assert!(!message.contains("duplicate_prompt"));
            }
        }
    }

    #[test]
    fn behavior_aware_partial_mounts_preflight_error_atomically() {
        let mut tools = Router::new();
        tools
            .add_tool(NamedTool::with_tags("same", vec!["original".to_string()]))
            .expect("tool registration succeeds");
        let mut tool_source = Router::new();
        tool_source
            .add_tool(NamedTool::with_tags("same", vec!["incoming".to_string()]))
            .expect("tool registration succeeds");
        tool_source
            .add_tool(NamedTool::new("unique"))
            .expect("tool registration succeeds");
        let tool_result =
            tools.mount_tools_with_behavior(tool_source, None, crate::DuplicateBehavior::Error);
        assert!(!tool_result.is_success());
        assert_eq!(tool_result.tools, 0);
        assert_eq!(tools.tools_count(), 1);
        assert_eq!(
            tools.get_tool("same").unwrap().definition().tags,
            vec!["original".to_string()]
        );
        assert!(tools.get_tool("unique").is_none());

        let mut resources = Router::new();
        resources.add_resource(NamedResource::with_tags(
            "same://resource",
            vec!["original".to_string()],
        ));
        let mut resource_source = Router::new();
        resource_source.add_resource(NamedResource::with_tags(
            "same://resource",
            vec!["incoming".to_string()],
        ));
        resource_source.add_resource_template(marked_template("unique://{item}", "incoming"));
        let resource_result = resources.mount_resources_with_behavior(
            resource_source,
            None,
            crate::DuplicateBehavior::Error,
        );
        assert!(!resource_result.is_success());
        assert_eq!(resource_result.resources, 0);
        assert_eq!(resource_result.resource_templates, 0);
        assert_eq!(resources.resources_count(), 1);
        assert_eq!(resources.resource_templates_count(), 0);
        assert_eq!(
            resources
                .get_resource("same://resource")
                .unwrap()
                .definition()
                .tags,
            vec!["original".to_string()]
        );

        let mut prompts = Router::new();
        prompts.add_prompt(NamedPrompt::with_tags("same", vec!["original".to_string()]));
        let mut prompt_source = Router::new();
        prompt_source.add_prompt(NamedPrompt::with_tags("same", vec!["incoming".to_string()]));
        prompt_source.add_prompt(NamedPrompt::new("unique"));
        let prompt_result = prompts.mount_prompts_with_behavior(
            prompt_source,
            None,
            crate::DuplicateBehavior::Error,
        );
        assert!(!prompt_result.is_success());
        assert_eq!(prompt_result.prompts, 0);
        assert_eq!(prompts.prompts_count(), 1);
        assert_eq!(
            prompts.get_prompt("same").unwrap().definition().tags,
            vec!["original".to_string()]
        );
        assert!(prompts.get_prompt("unique").is_none());
    }

    #[test]
    fn full_error_mount_is_atomic_across_component_kinds() {
        let mut main = Router::new();
        main.add_tool(NamedTool::with_tags(
            "conflict",
            vec!["original".to_string()],
        ))
        .expect("tool registration succeeds");

        let mut sub = Router::new();
        sub.add_tool(NamedTool::with_tags(
            "conflict",
            vec!["incoming".to_string()],
        ))
        .expect("tool registration succeeds");
        sub.add_resource(NamedResource::new("unique://resource"));
        sub.add_resource_template(marked_template("unique://{item}", "incoming"));
        sub.add_prompt(NamedPrompt::new("unique_prompt"));

        let result = main.mount_with_behavior(sub, None, crate::DuplicateBehavior::Error);
        assert!(!result.is_success());
        assert_eq!(result.errors.len(), 1);
        assert!(!result.has_components());
        assert_eq!(main.tools_count(), 1);
        assert_eq!(main.resources_count(), 0);
        assert_eq!(main.resource_templates_count(), 0);
        assert_eq!(main.prompts_count(), 0);
        assert_eq!(
            main.get_tool("conflict").unwrap().definition().tags,
            vec!["original".to_string()]
        );
    }

    #[test]
    fn invalid_prefix_rejects_every_partial_mount_without_mutation() {
        let mut tools = Router::new();
        let mut tool_source = Router::new();
        tool_source
            .add_tool(NamedTool::new("tool"))
            .expect("tool registration succeeds");
        let tool_result = tools.mount_tools_with_behavior(
            tool_source,
            Some("peer/secret"),
            crate::DuplicateBehavior::Replace,
        );
        assert!(!tool_result.is_success());
        assert_eq!(tools.tools_count(), 0);

        let mut resources = Router::new();
        let mut resource_source = Router::new();
        resource_source.add_resource(NamedResource::new("resource://value"));
        resource_source.add_resource_template(marked_template("template://{value}", "incoming"));
        let resource_result = resources.mount_resources_with_behavior(
            resource_source,
            Some("peer/secret"),
            crate::DuplicateBehavior::Replace,
        );
        assert!(!resource_result.is_success());
        assert_eq!(resources.resources_count(), 0);
        assert_eq!(resources.resource_templates_count(), 0);

        let mut prompts = Router::new();
        let mut prompt_source = Router::new();
        prompt_source.add_prompt(NamedPrompt::new("prompt"));
        let prompt_result = prompts.mount_prompts_with_behavior(
            prompt_source,
            Some("peer/secret"),
            crate::DuplicateBehavior::Replace,
        );
        assert!(!prompt_result.is_success());
        assert_eq!(prompts.prompts_count(), 0);

        for result in [tool_result, resource_result, prompt_result] {
            assert_eq!(result.errors.len(), 1);
            assert!(!result.errors[0].contains("peer/secret"));
            assert!(!result.has_components());
        }
    }

    // ── mount_tools / mount_resources / mount_prompts ──────────────────

    #[test]
    fn mount_tools_only() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("t1"))
            .expect("tool registration succeeds");
        sub.add_prompt(NamedPrompt::new("p1"));
        let result = main.mount_tools(sub, Some("ns"));
        assert_eq!(result.tools, 1);
        assert!(main.get_tool("ns/t1").is_some());
        assert_eq!(main.prompts_count(), 0); // prompts not mounted
    }

    #[test]
    fn mount_prompts_only() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("t1"))
            .expect("tool registration succeeds");
        sub.add_prompt(NamedPrompt::new("p1"));
        let result = main.mount_prompts(sub, Some("ns"));
        assert_eq!(result.prompts, 1);
        assert!(main.get_prompt("ns/p1").is_some());
        assert_eq!(main.tools_count(), 0); // tools not mounted
    }

    // ── handle_tools_list pagination ───────────────────────────────────

    #[test]
    fn handle_tools_list_no_pagination() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let params = ListToolsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.tools.len(), 2);
        assert!(result.next_cursor.is_none());
    }

    #[test]
    fn handle_tools_list_with_pagination() {
        let mut r = Router::new();
        r.set_list_page_size(Some(1));
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);

        // First page
        let params = ListToolsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "a");
        assert!(result.next_cursor.is_some());

        // Second page
        let params = ListToolsParams {
            cursor: result.next_cursor,
            include_tags: None,
            exclude_tags: None,
        };
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "b");
        assert!(result.next_cursor.is_none());
    }

    #[test]
    fn handle_tools_list_with_tag_filter() {
        let mut r = Router::new();
        r.add_tool(NamedTool::with_tags("a", vec!["db".to_string()]))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::with_tags("b", vec!["web".to_string()]))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let params = ListToolsParams {
            cursor: None,
            include_tags: Some(vec!["db".to_string()]),
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "a");
    }

    // ── handle_resources_list pagination ───────────────────────────────

    #[test]
    fn handle_resources_list_no_pagination() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        let cx = Cx::for_testing();
        let params = ListResourcesParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_resources_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert!(result.next_cursor.is_none());
    }

    #[test]
    fn handle_resources_list_with_pagination() {
        let mut r = Router::new();
        r.set_list_page_size(Some(1));
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource(NamedResource::new("file:///b"));
        let cx = Cx::for_testing();
        let params = ListResourcesParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_resources_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert!(result.next_cursor.is_some());
    }

    // ── handle_prompts_list pagination ─────────────────────────────────

    #[test]
    fn handle_prompts_list_no_pagination() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("greet"));
        let cx = Cx::for_testing();
        let params = ListPromptsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_prompts_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert!(result.next_cursor.is_none());
    }

    // ── handle_resource_templates_list ──────────────────────────────────

    #[test]
    fn handle_resource_templates_list_no_pagination() {
        let mut r = Router::new();
        r.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let cx = Cx::for_testing();
        let params = ListResourceTemplatesParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r
            .handle_resource_templates_list(&request_ctx, params, None)
            .unwrap();
        assert_eq!(result.resource_templates.len(), 1);
        assert!(result.next_cursor.is_none());
    }

    // ── handle_initialize ──────────────────────────────────────────────

    #[test]
    fn handle_initialize_returns_protocol_version() {
        let r = Router::new();
        let cx = Cx::for_testing();
        let mut session = Session::new(
            fastmcp_protocol::ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            fastmcp_protocol::ServerCapabilities::default(),
        );
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: fastmcp_protocol::ClientCapabilities::default(),
            client_info: fastmcp_protocol::ClientInfo {
                name: "test-client".to_string(),
                version: "1.0".to_string(),
            },
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r
            .handle_initialize(
                &request_ctx,
                &mut session,
                params,
                Some("test instructions"),
            )
            .unwrap();
        assert_eq!(result.protocol_version, PROTOCOL_VERSION);
        assert_eq!(result.server_info.name, "test");
        assert_eq!(result.instructions.as_deref(), Some("test instructions"));
    }

    #[test]
    fn handle_initialize_no_instructions() {
        let r = Router::new();
        let cx = Cx::for_testing();
        let mut session = Session::new(
            fastmcp_protocol::ServerInfo {
                name: "srv".to_string(),
                version: "0.1".to_string(),
            },
            fastmcp_protocol::ServerCapabilities::default(),
        );
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: fastmcp_protocol::ClientCapabilities::default(),
            client_info: fastmcp_protocol::ClientInfo {
                name: "c".to_string(),
                version: "0.1".to_string(),
            },
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r
            .handle_initialize(&request_ctx, &mut session, params, None)
            .unwrap();
        assert!(result.instructions.is_none());
    }

    // ── add_resource_with_behavior (Warn / Replace) ─────────────────────

    #[test]
    fn add_resource_behavior_warn_keeps_original() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource_with_behavior(
            NamedResource::new("file:///a"),
            crate::DuplicateBehavior::Warn,
        )
        .unwrap();
        assert_eq!(r.resources_count(), 1);
    }

    #[test]
    fn add_resource_behavior_replace() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource_with_behavior(
            NamedResource::new("file:///a"),
            crate::DuplicateBehavior::Replace,
        )
        .unwrap();
        assert_eq!(r.resources_count(), 1);
    }

    #[test]
    fn add_resource_behavior_new_resource_ok() {
        let mut r = Router::new();
        r.add_resource_with_behavior(
            NamedResource::new("file:///a"),
            crate::DuplicateBehavior::Error,
        )
        .unwrap();
        assert_eq!(r.resources_count(), 1);
    }

    // ── add_prompt_with_behavior (Replace / Ignore / new) ───────────────

    #[test]
    fn add_prompt_behavior_replace() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        r.add_prompt_with_behavior(NamedPrompt::new("p"), crate::DuplicateBehavior::Replace)
            .unwrap();
        assert_eq!(r.prompts_count(), 1);
    }

    #[test]
    fn add_prompt_behavior_ignore() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        r.add_prompt_with_behavior(NamedPrompt::new("p"), crate::DuplicateBehavior::Ignore)
            .unwrap();
        assert_eq!(r.prompts_count(), 1);
    }

    #[test]
    fn add_prompt_behavior_new_prompt_ok() {
        let mut r = Router::new();
        r.add_prompt_with_behavior(NamedPrompt::new("p"), crate::DuplicateBehavior::Error)
            .unwrap();
        assert_eq!(r.prompts_count(), 1);
    }

    // ── add_resource / add_prompt duplicate replace ─────────────────────

    #[test]
    fn add_resource_replaces_on_duplicate() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource(NamedResource::new("file:///a"));
        assert_eq!(r.resources_count(), 1);
        assert_eq!(r.resources().len(), 1);
    }

    #[test]
    fn add_prompt_replaces_on_duplicate() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        r.add_prompt(NamedPrompt::new("p"));
        assert_eq!(r.prompts_count(), 1);
        assert_eq!(r.prompts().len(), 1);
    }

    // ── resource_exists for template match ──────────────────────────────

    #[test]
    fn resource_exists_for_template_match() {
        struct DbResource;
        impl ResourceHandler for DbResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "db://placeholder".to_string(),
                    name: "db".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }
            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "db://{table}".to_string(),
                    name: "db".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<fastmcp_protocol::ResourceContent>> {
                Ok(vec![])
            }
        }
        let mut r = Router::new();
        r.add_resource(DbResource);
        assert!(r.resource_exists("db://users"));
        assert!(!r.resource_exists("file://other"));
    }

    // ── resources_filtered by tags ──────────────────────────────────────

    #[test]
    fn resources_filtered_by_tags() {
        let mut r = Router::new();
        r.add_resource(NamedResource::with_tags(
            "file:///a",
            vec!["internal".to_string()],
        ));
        r.add_resource(NamedResource::with_tags(
            "file:///b",
            vec!["public".to_string()],
        ));
        let include = vec!["public".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let res = r.resources_filtered(None, Some(&filters));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].uri, "file:///b");
    }

    // ── resource_templates_filtered by tags ─────────────────────────────

    #[test]
    fn resource_templates_filtered_by_tags() {
        let mut r = Router::new();
        r.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["admin".to_string()],
        });
        r.add_resource_template(ResourceTemplate {
            uri_template: "cache://{key}".to_string(),
            name: "cache".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["public".to_string()],
        });
        let exclude = vec!["admin".to_string()];
        let filters = TagFilters::new(None, Some(&exclude));
        let tmpls = r.resource_templates_filtered(None, Some(&filters));
        assert_eq!(tmpls.len(), 1);
        assert_eq!(tmpls[0].name, "cache");
    }

    // ── handle_tools_list with session state ────────────────────────────

    #[test]
    fn handle_tools_list_with_session_state_filter() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_tools", &disabled);
        let params = ListToolsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let result = r
            .handle_tools_list(&request_ctx, params, Some(&state))
            .unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "b");
    }

    #[test]
    fn handle_tools_list_cursor_rejects_changed_session_visibility() {
        let mut router = Router::new();
        router.set_list_page_size(Some(1));
        router
            .add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        router
            .add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let first = router
            .handle_tools_list(
                &request_ctx,
                ListToolsParams {
                    cursor: None,
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&state),
            )
            .expect("the first session-filtered page is admitted");
        assert_eq!(first.tools[0].name, "a");
        let cursor = first
            .next_cursor
            .expect("the first page has a continuation");

        let continued = router
            .handle_tools_list(
                &request_ctx,
                ListToolsParams {
                    cursor: Some(cursor.clone()),
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&state),
            )
            .expect("unchanged session visibility continues the exact-2024 catalog");
        assert_eq!(continued.tools[0].name, "b");
        assert!(continued.next_cursor.is_none());

        let disabled: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_tools", &disabled);
        let stale = router
            .handle_tools_list(
                &request_ctx,
                ListToolsParams {
                    cursor: Some(cursor),
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&state),
            )
            .expect_err("changing only session visibility rejects the continuation");
        assert_eq!(stale.code, McpErrorCode::InvalidParams);
        assert!(stale.message.contains("session visibility"));
    }

    // ── handle_resources_list with tag filter ────────────────────────────

    #[test]
    fn handle_resources_list_with_tag_filter() {
        let mut r = Router::new();
        r.add_resource(NamedResource::with_tags(
            "file:///a",
            vec!["db".to_string()],
        ));
        r.add_resource(NamedResource::with_tags(
            "file:///b",
            vec!["web".to_string()],
        ));
        let cx = Cx::for_testing();
        let params = ListResourcesParams {
            cursor: None,
            include_tags: Some(vec!["web".to_string()]),
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_resources_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].uri, "file:///b");
    }

    // ── handle_prompts_list with pagination ──────────────────────────────

    #[test]
    fn handle_prompts_list_with_pagination() {
        let mut r = Router::new();
        r.set_list_page_size(Some(1));
        r.add_prompt(NamedPrompt::new("a"));
        r.add_prompt(NamedPrompt::new("b"));
        let cx = Cx::for_testing();
        let params = ListPromptsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_prompts_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert_eq!(result.prompts[0].name, "a");
        assert!(result.next_cursor.is_some());

        let params = ListPromptsParams {
            cursor: result.next_cursor,
            include_tags: None,
            exclude_tags: None,
        };
        let result = r.handle_prompts_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert_eq!(result.prompts[0].name, "b");
        assert!(result.next_cursor.is_none());
    }

    // ── handle_prompts_list with tag filter ──────────────────────────────

    #[test]
    fn handle_prompts_list_with_tag_filter() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::with_tags("a", vec!["internal".to_string()]));
        r.add_prompt(NamedPrompt::with_tags("b", vec!["public".to_string()]));
        let cx = Cx::for_testing();
        let params = ListPromptsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: Some(vec!["internal".to_string()]),
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_prompts_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert_eq!(result.prompts[0].name, "b");
    }

    // ── handle_resource_templates_list with pagination ───────────────────

    #[test]
    fn handle_resource_templates_list_with_pagination() {
        let mut r = Router::new();
        r.set_list_page_size(Some(1));
        r.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        r.add_resource_template(ResourceTemplate {
            uri_template: "cache://{key}".to_string(),
            name: "cache".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);
        let params = ListResourceTemplatesParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let result = r
            .handle_resource_templates_list(&request_ctx, params, None)
            .unwrap();
        assert_eq!(result.resource_templates.len(), 1);
        assert!(result.next_cursor.is_some());

        let params = ListResourceTemplatesParams {
            cursor: result.next_cursor,
            include_tags: None,
            exclude_tags: None,
        };
        let result = r
            .handle_resource_templates_list(&request_ctx, params, None)
            .unwrap();
        assert_eq!(result.resource_templates.len(), 1);
        assert!(result.next_cursor.is_none());
    }

    // ── handle_resource_templates_list with tag filter ───────────────────

    #[test]
    fn handle_resource_templates_list_with_tag_filter() {
        let mut r = Router::new();
        r.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["admin".to_string()],
        });
        r.add_resource_template(ResourceTemplate {
            uri_template: "cache://{key}".to_string(),
            name: "cache".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["public".to_string()],
        });
        let cx = Cx::for_testing();
        let params = ListResourceTemplatesParams {
            cursor: None,
            include_tags: Some(vec!["public".to_string()]),
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r
            .handle_resource_templates_list(&request_ctx, params, None)
            .unwrap();
        assert_eq!(result.resource_templates.len(), 1);
        assert_eq!(result.resource_templates[0].name, "cache");
    }

    // ── mount_resources (selective method) ───────────────────────────────

    #[test]
    fn mount_resources_only() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_resource(NamedResource::new("file:///a"));
        sub.add_tool(NamedTool::new("t1"))
            .expect("tool registration succeeds");
        sub.add_resource_template(ResourceTemplate {
            uri_template: "db://{t}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let result = main.mount_resources(sub, Some("ns"));
        assert_eq!(result.resources, 1);
        assert_eq!(result.resource_templates, 1);
        assert!(main.get_resource("ns/file:///a").is_some());
        assert_eq!(main.tools_count(), 0); // tools not mounted
    }

    // ── MountResult has_components with all fields ──────────────────────

    #[test]
    fn mount_result_with_resources_has_components() {
        let mut r = MountResult::default();
        r.resources = 1;
        assert!(r.has_components());
    }

    #[test]
    fn mount_result_with_templates_has_components() {
        let mut r = MountResult::default();
        r.resource_templates = 1;
        assert!(r.has_components());
    }

    #[test]
    fn mount_result_with_prompts_has_components() {
        let mut r = MountResult::default();
        r.prompts = 1;
        assert!(r.has_components());
    }

    #[test]
    fn mount_result_is_success_with_warnings() {
        let mut r = MountResult::default();
        r.warnings.push("something".to_string());
        assert!(r.is_success());
    }

    #[test]
    fn mount_result_reports_errors_as_failure() {
        let mut result = MountResult::default();
        result.errors.push("mount rejected".to_string());
        assert!(!result.is_success());
        assert!(!result.has_components());
    }

    // ── mount with all component types ──────────────────────────────────

    #[test]
    fn mount_all_component_types() {
        let mut main = Router::new();
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("t1"))
            .expect("tool registration succeeds");
        sub.add_resource(NamedResource::new("file:///r1"));
        sub.add_prompt(NamedPrompt::new("p1"));
        sub.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let result = main.mount(sub, Some("ns"));
        assert_eq!(result.tools, 1);
        assert_eq!(result.resources, 1);
        assert_eq!(result.prompts, 1);
        assert_eq!(result.resource_templates, 1);
        assert!(result.has_components());
        assert!(main.get_tool("ns/t1").is_some());
        assert!(main.get_resource("ns/file:///r1").is_some());
        assert!(main.get_prompt("ns/p1").is_some());
    }

    // ── mount resource conflict warnings ────────────────────────────────

    #[test]
    fn mount_warns_on_resource_conflict() {
        let mut main = Router::new();
        main.add_resource(NamedResource::new("file:///a"));
        let mut sub = Router::new();
        sub.add_resource(NamedResource::new("file:///a"));
        let result = main.mount(sub, None);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("Resource"));
    }

    #[test]
    fn mount_warns_on_prompt_conflict() {
        let mut main = Router::new();
        main.add_prompt(NamedPrompt::new("p"));
        let mut sub = Router::new();
        sub.add_prompt(NamedPrompt::new("p"));
        let result = main.mount(sub, None);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("Prompt"));
    }

    // ── TagFilters::clone ───────────────────────────────────────────────

    #[test]
    fn tag_filters_clone() {
        let include = vec!["a".to_string()];
        let f = TagFilters::new(Some(&include), None);
        let cloned = f.clone();
        assert!(cloned.matches(&["a".to_string()]));
        assert!(!cloned.matches(&["b".to_string()]));
    }

    // ── handle_tools_list with pagination AND tags ───────────────────────

    #[test]
    fn handle_tools_list_pagination_with_tags() {
        let mut r = Router::new();
        r.set_list_page_size(Some(1));
        r.add_tool(NamedTool::with_tags("a", vec!["db".to_string()]))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::with_tags("b", vec!["db".to_string()]))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::with_tags("c", vec!["web".to_string()]))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);

        // Only "db" tagged tools, page 1
        let params = ListToolsParams {
            cursor: None,
            include_tags: Some(vec!["db".to_string()]),
            exclude_tags: None,
        };
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "a");
        assert!(result.next_cursor.is_some());

        // Page 2
        let params = ListToolsParams {
            cursor: result.next_cursor,
            include_tags: Some(vec!["db".to_string()]),
            exclude_tags: None,
        };
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "b");
        assert!(result.next_cursor.is_none());
    }

    #[test]
    fn final_mounted_tools_preserve_admitted_order_before_cursor_pagination() {
        let mut router = mounted_tool_router();
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 164, Budget::INFINITE, &state);

        let legacy = router
            .handle_tools_list(
                &request_ctx,
                ListToolsParams {
                    cursor: None,
                    include_tags: Some(vec!["visible".to_owned()]),
                    exclude_tags: Some(vec!["excluded".to_owned()]),
                },
                None,
            )
            .expect("legacy filtering retains every registered tool");
        assert_eq!(
            legacy
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["peer/first", "peer/second"],
            "the legacy catalog retains mounted tools in source order"
        );

        router.set_list_page_size(Some(1));
        let first_page = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(
                None,
                Some(vec!["visible"]),
                Some(vec!["excluded"]),
                164_i64,
            ),
        ))
        .expect("the first final page contains the first admitted entry");
        assert_eq!(first_page["tools"][0]["name"], "peer/first");
        assert_eq!(first_page["tools"].as_array().map(Vec::len), Some(1));
        let cursor = first_page["nextCursor"]
            .as_str()
            .expect("the first admitted page has a continuation cursor");
        let include_tags = vec!["visible".to_owned()];
        let exclude_tags = vec!["excluded".to_owned()];
        let query = FinalCatalogQuery::from_tag_filters(Some(&include_tags), Some(&exclude_tags));
        assert_eq!(
            decode_final_catalog_cursor_offset(
                Some(cursor),
                FinalCatalogKind::Tools,
                router.final_catalog_revision,
                &query,
                None,
                2,
            )
            .expect("cursor is router-generated for this exact final catalog revision"),
            1,
            "the cursor advances across admitted entries"
        );

        let query_mismatch = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(
                Some(cursor),
                Some(vec!["other"]),
                Some(vec!["excluded"]),
                165_i64,
            ),
        ))
        .expect_err("changing only the final list filter rejects the continuation");
        assert_eq!(query_mismatch.code, McpErrorCode::InvalidParams);
        assert!(query_mismatch.message.contains("query filters"));

        let second_page = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(
                Some(cursor),
                Some(vec!["visible"]),
                Some(vec!["excluded"]),
                166_i64,
            ),
        ))
        .expect("the continuation page keeps admitted source order");
        assert_eq!(second_page["tools"][0]["name"], "peer/second");
        assert_eq!(second_page["tools"].as_array().map(Vec::len), Some(1));
        assert!(
            second_page.get("nextCursor").is_none(),
            "the second admitted entry terminates the filtered final sequence"
        );
    }

    #[test]
    fn final_default_list_validates_every_supplied_catalog_cursor() {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("default-cursor-first"))
            .expect("first final tool registers");
        router
            .add_tool(NamedTool::new("default-cursor-second"))
            .expect("second final tool registers");
        assert!(
            router.list_page_size.is_none(),
            "the default final list path has pagination disabled"
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 167, Budget::INFINITE, &state);
        let full = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 167_i64),
        ))
        .expect("a cursor-free default final list returns the full catalog");
        assert_eq!(full["tools"].as_array().map(Vec::len), Some(2));
        assert!(full.get("nextCursor").is_none());

        let query = FinalCatalogQuery::from_tag_filters(None, None);
        let valid_cursor = encode_final_catalog_cursor(
            FinalCatalogKind::Tools,
            router.final_catalog_revision,
            &query,
            0,
            None,
        );
        let validated_full = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(Some(&valid_cursor), None, None, 168_i64),
        ))
        .expect("a valid cursor remains admitted while default listing returns the full catalog");
        assert_eq!(validated_full["tools"].as_array().map(Vec::len), Some(2));
        assert!(validated_full.get("nextCursor").is_none());

        let stale_revision = router
            .final_catalog_revision
            .checked_sub(1)
            .expect("registered tools advance the final catalog revision");
        let stale_cursor =
            encode_final_catalog_cursor(FinalCatalogKind::Tools, stale_revision, &query, 0, None);
        let stale = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(Some(&stale_cursor), None, None, 169_i64),
        ))
        .expect_err("changing only the revision rejects a default-list cursor");
        assert_eq!(stale.code, McpErrorCode::InvalidParams);
        assert!(stale.message.contains("stale catalog revision"));

        let wrong_kind_cursor = encode_final_catalog_cursor(
            FinalCatalogKind::Prompts,
            router.final_catalog_revision,
            &query,
            0,
            None,
        );
        let wrong_kind = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(Some(&wrong_kind_cursor), None, None, 170_i64),
        ))
        .expect_err("changing only the kind rejects a default-list cursor");
        assert_eq!(wrong_kind.code, McpErrorCode::InvalidParams);
        assert!(wrong_kind.message.contains("another list method"));

        let other_tags = vec!["other".to_owned()];
        let other_query = FinalCatalogQuery::from_tag_filters(Some(&other_tags), None);
        let wrong_query_cursor = encode_final_catalog_cursor(
            FinalCatalogKind::Tools,
            router.final_catalog_revision,
            &other_query,
            0,
            None,
        );
        let wrong_query = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(Some(&wrong_query_cursor), None, None, 171_i64),
        ))
        .expect_err("changing only the filters rejects a default-list cursor");
        assert_eq!(wrong_query.code, McpErrorCode::InvalidParams);
        assert!(wrong_query.message.contains("query filters"));

        let out_of_range_cursor = encode_final_catalog_cursor(
            FinalCatalogKind::Tools,
            router.final_catalog_revision,
            &query,
            2,
            None,
        );
        let out_of_range = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(Some(&out_of_range_cursor), None, None, 172_i64),
        ))
        .expect_err("changing only the offset rejects a default-list cursor");
        assert_eq!(out_of_range.code, McpErrorCode::InvalidParams);
        assert!(
            out_of_range
                .message
                .contains("outside the requested catalog page")
        );
    }

    #[test]
    fn final_resource_and_prompt_cursors_reject_stale_catalog_revisions() {
        fn final_list_request(method: &str, cursor: Option<&str>, id: i64) -> JsonRpcRequest {
            let mut params = serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            });
            if let Some(cursor) = cursor {
                params["cursor"] = serde_json::json!(cursor);
            }
            JsonRpcRequest::new(method, Some(params), id)
        }

        let mut router = Router::new();
        router.set_list_page_size(Some(1));
        router.add_resource(NamedResource::new("file:///cursor-resource-a"));
        router.add_resource(NamedResource::new("file:///cursor-resource-b"));
        router.add_prompt(NamedPrompt::new("cursor-prompt-a"));
        router.add_prompt(NamedPrompt::new("cursor-prompt-b"));

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 166, Budget::INFINITE, &state);

        let resource_first = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_list_request("resources/list", None, 166_i64),
        ))
        .expect("the first final resource page is emitted");
        let resource_cursor = resource_first["nextCursor"]
            .as_str()
            .expect("the first resource page has a continuation")
            .to_owned();
        let resource_second = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_list_request("resources/list", Some(&resource_cursor), 167_i64),
        ))
        .expect("an unchanged final resource catalog accepts its cursor");
        assert_eq!(
            resource_second["resources"][0]["uri"],
            "file:///cursor-resource-b"
        );

        router.add_resource(NamedResource::new("file:///cursor-resource-c"));
        let resource_stale = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_list_request("resources/list", Some(&resource_cursor), 168_i64),
        ))
        .expect_err("adding only one catalog resource invalidates the old continuation");
        assert_eq!(resource_stale.code, McpErrorCode::InvalidParams);
        assert!(resource_stale.message.contains("stale catalog revision"));

        let prompt_first = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_list_request("prompts/list", None, 169_i64),
        ))
        .expect("the first final prompt page is emitted");
        let prompt_cursor = prompt_first["nextCursor"]
            .as_str()
            .expect("the first prompt page has a continuation")
            .to_owned();
        let prompt_second = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_list_request("prompts/list", Some(&prompt_cursor), 170_i64),
        ))
        .expect("an unchanged final prompt catalog accepts its cursor");
        assert_eq!(prompt_second["prompts"][0]["name"], "cursor-prompt-b");

        router.add_prompt(NamedPrompt::new("cursor-prompt-c"));
        let prompt_stale = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_list_request("prompts/list", Some(&prompt_cursor), 171_i64),
        ))
        .expect_err("adding only one catalog prompt invalidates the old continuation");
        assert_eq!(prompt_stale.code, McpErrorCode::InvalidParams);
        assert!(prompt_stale.message.contains("stale catalog revision"));
    }

    #[test]
    fn final_mounted_tools_apply_tag_filters_in_admitted_order() {
        let router = mounted_tool_router();
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 166, Budget::INFINITE, &state);

        let visible = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, Some(vec!["visible"]), None, 166_i64),
        ))
        .expect("the final include filter projects only admitted visible entries");
        assert_eq!(
            visible["tools"]
                .as_array()
                .expect("final tools are an array")
                .iter()
                .map(|tool| tool["name"].as_str().expect("tool name is a string"))
                .collect::<Vec<_>>(),
            vec!["peer/first", "peer/second", "peer/excluded"],
            "tag filtering preserves admitted insertion order"
        );

        let other = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, Some(vec!["other"]), None, 167_i64),
        ))
        .expect("the include filter is evaluated after final admission");
        assert_eq!(other["tools"][0]["name"], "peer/other");
        assert_eq!(other["tools"].as_array().map(Vec::len), Some(1));
    }

    // ── handle_resources_list with session state filter ──────────────────

    #[test]
    fn handle_resources_list_with_session_state_filter() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        r.add_resource(NamedResource::new("file:///b"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["file:///a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_resources", &disabled);
        let params = ListResourcesParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let result = r
            .handle_resources_list(&request_ctx, params, Some(&state))
            .unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].uri, "file:///b");
    }

    // ── handle_prompts_list with session state filter ────────────────────

    #[test]
    fn handle_prompts_list_with_session_state_filter() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("a"));
        r.add_prompt(NamedPrompt::new("b"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_prompts", &disabled);
        let params = ListPromptsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let result = r
            .handle_prompts_list(&request_ctx, params, Some(&state))
            .unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert_eq!(result.prompts[0].name, "b");
    }

    // ── resource_templates_filtered by session + tags combined ───────────

    #[test]
    fn resource_templates_filtered_session_and_tags_combined() {
        let mut r = Router::new();
        r.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["admin".to_string()],
        });
        r.add_resource_template(ResourceTemplate {
            uri_template: "cache://{key}".to_string(),
            name: "cache".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["admin".to_string()],
        });
        r.add_resource_template(ResourceTemplate {
            uri_template: "log://{entry}".to_string(),
            name: "log".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec!["public".to_string()],
        });
        // Disable db template via session state
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["db://{table}".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_resources", &disabled);
        // Also filter by admin tag
        let include = vec!["admin".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let tmpls = r.resource_templates_filtered(Some(&state), Some(&filters));
        // db is disabled, log doesn't have admin tag => only cache
        assert_eq!(tmpls.len(), 1);
        assert_eq!(tmpls[0].name, "cache");
    }

    // ── mount_tools warns on template conflict ──────────────────────────

    #[test]
    fn mount_resource_template_warns_on_conflict() {
        let mut main = Router::new();
        main.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let mut sub = Router::new();
        sub.add_resource_template(ResourceTemplate {
            uri_template: "db://{table}".to_string(),
            name: "db2".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        });
        let result = main.mount(sub, None);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("Resource template"));
    }

    // ── handle_tools_call: tool disabled via session ─────────────────────

    #[test]
    fn handle_tools_call_disabled_tool_returns_error() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("my_tool"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["my_tool".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_tools", &disabled);
        let params = CallToolParams {
            name: "my_tool".to_string(),
            arguments: None,
            meta: None,
        };
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_tools_call(&request_ctx, params, state, None, None))
            .unwrap_err();
        assert!(err.message.contains("disabled"));
    }

    // ── handle_tools_call: success path ──────────────────────────────────

    #[test]
    fn handle_tools_call_success() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("echo"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = CallToolParams {
            name: "echo".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let result = block_on(r.handle_tools_call(&request_ctx, params, state, None, None))
            .unwrap();
        assert!(!result.is_error);
        assert_ne!(result.content.len(), 0);
    }

    // ── handle_tools_call: not found ─────────────────────────────────────

    #[test]
    fn handle_tools_call_not_found() {
        let r = Router::new();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = CallToolParams {
            name: "missing".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_tools_call(&request_ctx, params, state, None, None))
            .unwrap_err();
        // The refusal deliberately does not echo the peer-controlled tool
        // name; only the sanitized method-not-found classification surfaces.
        assert_eq!(err.code, McpErrorCode::MethodNotFound);
        assert!(!err.message.contains("missing"));
    }

    // ── handle_tools_call: zero poll balance without poll admission ──────

    #[test]
    fn handle_tools_call_zero_poll_balance_allows_handler_without_checkpoint() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let budget = Budget::unlimited().with_poll_quota(0);
        let params = CallToolParams {
            name: "t".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let result = block_on(r.handle_tools_call(&request_ctx, params, state, None, None))
            .expect("a zero balance is not a retroactive failure");
        assert!(!result.is_error);
    }

    #[test]
    fn handle_tools_call_defers_pending_cancellation_inside_context_mask() {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let params = || CallToolParams {
            name: "t".to_string(),
            arguments: None,
            meta: None,
        };

        let masked_result = request_ctx
            .masked(|| {
                block_on(router.handle_tools_call(
                    &request_ctx,
                    params(),
                    state.clone(),
                    None,
                    None,
                ))
            })
            .expect("mask should be admitted");
        assert!(masked_result.is_ok());

        let unmasked_error =
            block_on(router.handle_tools_call(&request_ctx, params(), state, None, None))
                .expect_err("pending cancellation should surface after mask exit");
        assert_eq!(unmasked_error.code, McpErrorCode::RequestCancelled);
    }

    // ── handle_resources_read: resource disabled via session ──────────────

    #[test]
    fn handle_resources_read_disabled_resource_returns_error() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///secret"));
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["file:///secret".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_resources", &disabled);
        let params = ReadResourceParams {
            uri: "file:///secret".to_string(),
            meta: None,
        };
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_resources_read(&request_ctx, &params, state, None, None))
            .unwrap_err();
        assert!(err.message.contains("disabled"));
    }

    // ── handle_resources_read: success path ──────────────────────────────

    #[test]
    fn handle_resources_read_success() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = ReadResourceParams {
            uri: "file:///a".to_string(),
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let result = block_on(r.handle_resources_read(&request_ctx, &params, state, None, None))
            .unwrap();
        assert_eq!(result.contents.len(), 1);
        let [
            LegacyResourceContent::Text {
                uri,
                text,
                additional,
                ..
            },
        ] = result.contents.as_slice()
        else {
            panic!("resource read must retain the exact legacy text shape");
        };
        assert_eq!(uri, "file:///a");
        assert_eq!(text, "content");
        assert!(additional.is_empty());
    }

    // ── handle_resources_read: not found ─────────────────────────────────

    #[test]
    fn handle_resources_read_not_found() {
        let r = Router::new();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = ReadResourceParams {
            uri: "file:///nonexistent".to_string(),
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_resources_read(&request_ctx, &params, state, None, None))
            .unwrap_err();
        assert!(err.message.contains("nonexistent") || err.message.contains("not found"));
    }

    // ── handle_resources_read: zero poll balance without admission ───────

    #[test]
    fn handle_resources_read_zero_poll_balance_allows_handler_without_checkpoint() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a"));
        let cx = Cx::for_testing();
        let budget = Budget::unlimited().with_poll_quota(0);
        let params = ReadResourceParams {
            uri: "file:///a".to_string(),
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let result = block_on(r.handle_resources_read(&request_ctx, &params, state, None, None))
            .expect("a zero balance is not a retroactive failure");
        assert_eq!(result.contents.len(), 1);
    }

    // ── handle_prompts_get: prompt disabled via session ───────────────────

    #[test]
    fn handle_prompts_get_disabled_prompt_returns_error() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("secret_prompt"));
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let state = SessionState::new();
        let disabled: std::collections::HashSet<String> =
            ["secret_prompt".to_string()].into_iter().collect();
        state.set("fastmcp.disabled_prompts", &disabled);
        let params = GetPromptParams {
            name: "secret_prompt".to_string(),
            arguments: None,
            meta: None,
        };
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_prompts_get(&request_ctx, params, state, None, None))
            .unwrap_err();
        assert!(err.message.contains("disabled"));
    }

    // ── handle_prompts_get: success path ─────────────────────────────────

    #[test]
    fn nested_get_prompt_returns_handler_text_and_refuses_missing() {
        let mut router = Router::new();
        router.add_prompt(NamedPrompt::new("greet"));
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_prompt_caller(
            Arc::new(RouterPromptCaller::new(Arc::clone(&router), state)),
        );

        let missing = block_on(request_ctx.get_prompt("missing", HashMap::new()))
            .expect_err("an unknown nested prompt must stay refused");
        assert_eq!(missing.code, McpErrorCode::PromptNotFound);

        let result = block_on(request_ctx.get_prompt("greet", HashMap::new()))
            .expect("a registered nested prompt must complete");
        assert_eq!(result.description.as_deref(), Some("Prompt greet"));
        assert_eq!(result.messages.len(), 0);
    }

    #[test]
    fn nested_get_prompt_from_legacy_surface_skips_required_argument_admission() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::clone(&final_calls),
            legacy_calls: Arc::clone(&legacy_calls),
        });
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_prompt_caller(
            Arc::new(RouterPromptCaller::new(Arc::clone(&router), state)),
        );

        let result = block_on(request_ctx.get_prompt("prompt-argument-boundary", HashMap::new()))
            .expect("legacy nested get_prompt retains unvalidated argument behavior");
        assert_eq!(
            result.messages[0].text.as_deref(),
            Some("legacy prompt-argument-boundary result")
        );
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nested_get_prompt_from_final_surface_admits_arguments_before_handler() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::clone(&final_calls),
            legacy_calls: Arc::clone(&legacy_calls),
        });
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone())
            .with_final_request_surface(true)
            .with_prompt_caller(Arc::new(RouterPromptCaller::new(
                Arc::clone(&router),
                state,
            )));

        let missing = block_on(request_ctx.get_prompt("prompt-argument-boundary", HashMap::new()))
            .expect_err("removing only the required nested argument is rejected");
        assert_eq!(missing.code, McpErrorCode::InvalidParams);
        assert_eq!(missing.message, "Missing required prompt argument");
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let mut unknown = HashMap::new();
        unknown.insert("topic".to_owned(), "release".to_owned());
        unknown.insert("extra".to_owned(), "nope".to_owned());
        let unknown_error = block_on(request_ctx.get_prompt("prompt-argument-boundary", unknown))
            .expect_err("an undeclared nested argument is rejected before the handler");
        assert_eq!(unknown_error.code, McpErrorCode::InvalidParams);
        assert_eq!(unknown_error.message, "Unknown prompt argument");
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let mut required = HashMap::new();
        required.insert("topic".to_owned(), "release".to_owned());
        let accepted = block_on(request_ctx.get_prompt("prompt-argument-boundary", required))
            .expect("the complete required nested argument is accepted");
        assert_eq!(
            accepted.messages[0].text.as_deref(),
            Some("final prompt-argument-boundary result")
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
    }

    fn handle_prompts_get_success() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("greet"));
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = GetPromptParams {
            name: "greet".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let result = block_on(r.handle_prompts_get(&request_ctx, params, state, None, None))
            .unwrap();
        assert!(result.description.is_some());
    }

    // ── handle_prompts_get: not found ────────────────────────────────────

    #[test]
    fn handle_prompts_get_not_found() {
        let r = Router::new();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = GetPromptParams {
            name: "missing".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_prompts_get(&request_ctx, params, state, None, None))
            .unwrap_err();
        assert!(err.message.contains("missing") || err.message.contains("not found"));
    }

    // ── handle_prompts_get: zero poll balance without admission ──────────

    #[test]
    fn handle_prompts_get_zero_poll_balance_allows_handler_without_checkpoint() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        let cx = Cx::for_testing();
        let budget = Budget::unlimited().with_poll_quota(0);
        let params = GetPromptParams {
            name: "p".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let result = block_on(r.handle_prompts_get(&request_ctx, params, state, None, None))
            .expect("a zero balance is not a retroactive failure");
        assert_eq!(result.messages.len(), 0);
    }

    #[test]
    fn handler_budget_composition_uses_exact_earliest_deadline() {
        let now = Time::from_secs(100);
        let at = |seconds| Budget::new().with_deadline(Time::from_secs(seconds));
        let cases = [
            (Budget::INFINITE, Budget::INFINITE, None, None),
            (at(110), Budget::INFINITE, None, Some(Time::from_secs(110))),
            (Budget::INFINITE, at(115), None, Some(Time::from_secs(115))),
            (
                at(110),
                at(115),
                Some(Duration::from_secs(30)),
                Some(Time::from_secs(110)),
            ),
            (
                at(140),
                at(130),
                Some(Duration::from_secs(5)),
                Some(Time::from_secs(105)),
            ),
            (
                at(110),
                at(115),
                Some(Duration::ZERO),
                Some(Time::from_secs(110)),
            ),
            (
                at(90),
                at(115),
                Some(Duration::from_secs(5)),
                Some(Time::from_secs(90)),
            ),
            (
                Budget::INFINITE,
                Budget::new().with_deadline(Time::from_nanos(u64::MAX - 1)),
                Some(Duration::MAX),
                Some(Time::from_nanos(u64::MAX - 1)),
            ),
        ];

        for (ambient, request, handler, expected) in cases {
            assert_eq!(
                compose_handler_budget(ambient, request, handler, now).deadline,
                expected
            );
        }
    }

    #[test]
    fn alternating_tool_resource_recursion_uses_one_effective_depth() {
        let calls = Arc::new(AtomicU64::new(0));
        let mut router = Router::new();
        router
            .add_tool(AlternatingTool {
                calls: Arc::clone(&calls),
            })
            .expect("tool registration succeeds");
        router.add_resource(AlternatingResource {
            calls: Arc::clone(&calls),
        });
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone())
            .with_tool_caller(Arc::new(RouterToolCaller::new(
                Arc::clone(&router),
                state.clone(),
            )))
            .with_resource_reader(Arc::new(RouterResourceReader::new(
                Arc::clone(&router),
                state.clone(),
            )));

        let error = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "alternating_tool".to_string(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect_err("alternating recursion must stop at the shared depth limit");

        assert_eq!(error.code, McpErrorCode::InternalError);
        // Internal errors crossing the handler boundary are sanitized, so the
        // depth diagnosis never reaches the peer; the call count proves the
        // shared limit stopped the alternating recursion.
        assert_eq!(error.message, SANITIZED_HANDLER_PANIC_MESSAGE);
        assert_eq!(calls.load(Ordering::Relaxed), 11);
    }

    #[test]
    fn nested_tool_resource_call_shares_parent_cost_ledger() {
        let remaining_after_parent_debit = Arc::new(AtomicU64::new(u64::MAX));
        let remaining_after_nested_debit = Arc::new(AtomicU64::new(u64::MAX));
        let remaining_after_nested_read = Arc::new(AtomicU64::new(u64::MAX));
        let mut router = Router::new();
        router
            .add_tool(CostLedgerTool {
                remaining_after_parent_debit: Arc::clone(&remaining_after_parent_debit),
                remaining_after_nested_read: Arc::clone(&remaining_after_nested_read),
            })
            .expect("tool registration succeeds");
        router.add_resource(CostLedgerResource {
            remaining_after_nested_debit: Arc::clone(&remaining_after_nested_debit),
        });

        let router = Arc::new(router);
        let state = SessionState::new();
        let cx = Cx::for_testing_with_budget(Budget::new().with_cost_quota(3));
        let request_ctx = McpContext::with_state(cx, 77, state.clone())
            .with_tool_caller(Arc::new(RouterToolCaller::new(
                Arc::clone(&router),
                state.clone(),
            )))
            .with_resource_reader(Arc::new(RouterResourceReader::new(router, state)));

        let result = block_on(request_ctx.call_tool("cost_ledger_tool", serde_json::json!({})))
            .expect("parent and nested debits fit the shared cost quota");

        assert!(!result.is_error);
        assert_eq!(remaining_after_parent_debit.load(Ordering::Relaxed), 2);
        assert_eq!(remaining_after_nested_debit.load(Ordering::Relaxed), 1);
        assert_eq!(remaining_after_nested_read.load(Ordering::Relaxed), 1);
        assert_eq!(request_ctx.budget().cost_quota, Some(1));
    }

    #[test]
    fn nested_tool_calls_preserve_framework_terminal_errors() {
        let mut router = Router::new();
        router
            .add_tool(ErrorTool {
                name: "nested_cancelled",
                code: McpErrorCode::RequestCancelled,
            })
            .expect("tool registration succeeds");
        router
            .add_tool(ErrorTool {
                name: "nested_internal",
                code: McpErrorCode::InternalError,
            })
            .expect("tool registration succeeds");
        router
            .add_tool(ErrorTool {
                name: "nested_tool_failure",
                code: McpErrorCode::ToolExecutionError,
            })
            .expect("tool registration succeeds");
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 88, state.clone())
            .with_tool_caller(Arc::new(RouterToolCaller::new(Arc::clone(&router), state)));

        for (name, expected) in [
            ("nested_cancelled", McpErrorCode::RequestCancelled),
            ("nested_internal", McpErrorCode::InternalError),
        ] {
            let error = block_on(request_ctx.call_tool(name, serde_json::json!({})))
                .expect_err("framework terminal errors must remain outer failures");
            assert_eq!(error.code, expected);
        }

        let tool_failure =
            block_on(request_ctx.call_tool("nested_tool_failure", serde_json::json!({})))
                .expect("ordinary tool failures remain protocol-level tool results");
        assert!(tool_failure.is_error);
    }

    #[test]
    fn nested_call_tool_from_legacy_surface_uses_legacy_hook() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(DuplicateInvariantTool {
                label: "nested-legacy",
                schema_property: "allowed",
                legacy_calls: Arc::clone(&legacy_calls),
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone())
            .with_tool_caller(Arc::new(RouterToolCaller::new(Arc::clone(&router), state)));

        let rejected = block_on(request_ctx.call_tool(
            "duplicate-invariant-tool",
            serde_json::json!({"allowed": true, "extra": true}),
        ))
        .expect_err("legacy nested schema rejection remains an outer invalid-params error");
        assert_eq!(rejected.code, McpErrorCode::InvalidParams);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);

        let result = block_on(request_ctx.call_tool(
            "duplicate-invariant-tool",
            serde_json::json!({"allowed": true}),
        ))
        .expect("legacy nested call_tool reaches the legacy hook");
        assert!(!result.is_error);
        assert_eq!(result.first_text(), Some("nested-legacy"));
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nested_call_tool_from_final_surface_validates_before_handler() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(DuplicateInvariantTool {
                label: "nested-final",
                schema_property: "allowed",
                legacy_calls: Arc::clone(&legacy_calls),
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone())
            .with_final_request_surface(true)
            .with_tool_caller(Arc::new(RouterToolCaller::new(Arc::clone(&router), state)));

        let rejected = block_on(request_ctx.call_tool(
            "duplicate-invariant-tool",
            serde_json::json!({"allowed": true, "extra": true}),
        ))
        .expect("schema rejection remains a tool-level error result");
        assert!(rejected.is_error);
        assert_eq!(
            rejected.first_text(),
            Some("Tool arguments do not match the declared input schema.")
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let accepted = block_on(request_ctx.call_tool(
            "duplicate-invariant-tool",
            serde_json::json!({"allowed": true}),
        ))
        .expect("the complete nested final arguments are accepted");
        assert!(!accepted.is_error);
        assert_eq!(accepted.first_text(), Some("nested-final"));
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nested_read_resource_from_final_surface_uses_final_hook() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_resource(EraBoundaryResource {
            legacy_calls: Arc::clone(&legacy_calls),
            final_calls: Arc::clone(&final_calls),
        });
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone())
            .with_final_request_surface(true)
            .with_resource_reader(Arc::new(RouterResourceReader::new(
                Arc::clone(&router),
                state,
            )));

        let accepted = block_on(request_ctx.read_resource("mcp://era-boundary"))
            .expect("nested final resource read reaches the final hook");
        assert_eq!(
            accepted.contents[0].text.as_deref(),
            Some("final resource result")
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let missing = block_on(request_ctx.read_resource("mcp://missing"))
            .expect_err("an unknown nested final resource is invalid params");
        assert_eq!(missing.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nested_read_resource_from_legacy_surface_uses_legacy_hook() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_resource(EraBoundaryResource {
            legacy_calls: Arc::clone(&legacy_calls),
            final_calls: Arc::clone(&final_calls),
        });
        let router = Arc::new(router);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_resource_reader(
            Arc::new(RouterResourceReader::new(Arc::clone(&router), state)),
        );

        let accepted = block_on(request_ctx.read_resource("mcp://era-boundary"))
            .expect("legacy nested resource read reaches the legacy hook");
        assert_eq!(
            accepted.contents[0].text.as_deref(),
            Some("legacy resource result")
        );
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn manual_handler_timeout_is_read_exposed_and_enforced() {
        let observed_deadline = Arc::new(Mutex::new(None));
        let timeout_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut router = Router::new();
        // The timeout must outlive dispatch admission (a too-tight deadline
        // rejects before the handler starts, proving nothing about read
        // exposure) while still expiring before the handler's delay finishes.
        router
            .add_tool(BudgetProbeTool {
                timeout: Some(Duration::from_millis(10)),
                delay: Duration::from_millis(100),
                observed_deadline: Arc::clone(&observed_deadline),
                timeout_read: Arc::clone(&timeout_read),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let error = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "budget_probe".to_string(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect_err("the handler deadline must reject a late completion");

        assert!(timeout_read.load(Ordering::Relaxed));
        assert!(
            observed_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        );
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(error.message, "Request timeout exceeded");
        assert!(!cx.is_cancel_requested());
    }

    /// A handler that sees the request deadline first through `checkpoint()`
    /// still ended on that deadline, so the framework reports the timeout.
    #[test]
    fn deadline_first_observed_by_the_handler_reports_request_timeout() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let ctx = request_context(&cx, 1, Budget::new().with_deadline(Time::ZERO), &state)
            .with_request_cancellation(fastmcp_core::McpRequestCancellation::new());

        assert!(
            ctx.checkpoint().is_err(),
            "the handler observes the deadline"
        );
        let error = budget_error(&ctx).expect("an expired request is not live");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(error.message, "Request timeout exceeded");
    }

    /// Near-identical negative: an explicit cancellation first observed by
    /// the handler stays a plain cancellation.
    #[test]
    fn explicit_cancellation_first_observed_by_the_handler_reports_request_cancelled() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let cancellation = fastmcp_core::McpRequestCancellation::new();
        let ctx = request_context(&cx, 1, Budget::INFINITE, &state)
            .with_request_cancellation(cancellation.clone());

        assert!(cancellation.cancel());
        assert!(
            ctx.checkpoint().is_err(),
            "the handler observes the cancellation"
        );
        let error = budget_error(&ctx).expect("a cancelled request is not live");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(error.message, McpError::request_cancelled().message);
    }

    fn run_ready_handler(ctx: &McpContext, request_cx: &Cx) -> McpResult<McpOutcome<()>> {
        fastmcp_core::block_on(run_handler_in_request(
            ctx,
            request_cx,
            Budget::INFINITE,
            "tool",
            |_| Box::pin(async { Outcome::Ok(()) }),
        ))
    }

    /// A request Cx cancelled once the request's deadline has passed is the
    /// deadline ending the request, so the handler runner reports the timeout.
    #[test]
    fn handler_runner_reports_a_cancelled_request_cx_past_its_deadline_as_timeout() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let ctx = request_context(&cx, 1, Budget::new().with_deadline(Time::ZERO), &state)
            .with_request_cancellation(fastmcp_core::McpRequestCancellation::new());
        let request_cx = Cx::for_testing();
        request_cx.set_cancel_requested(true);

        let error = run_ready_handler(&ctx, &request_cx)
            .expect_err("a cancelled request Cx is refused before the handler runs");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(error.message, "Request timeout exceeded");
    }

    /// Near-identical negative: the same cancelled request Cx under an explicit
    /// cancellation and no deadline stays a plain cancellation.
    #[test]
    fn handler_runner_reports_an_explicitly_cancelled_request_cx_as_cancellation() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let cancellation = fastmcp_core::McpRequestCancellation::new();
        let ctx = request_context(&cx, 1, Budget::INFINITE, &state)
            .with_request_cancellation(cancellation.clone());
        assert!(cancellation.cancel());
        let request_cx = Cx::for_testing();
        request_cx.set_cancel_requested(true);

        let error = run_ready_handler(&ctx, &request_cx)
            .expect_err("a cancelled request Cx is refused before the handler runs");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(error.message, McpError::request_cancelled().message);
    }

    #[test]
    fn admitted_definition_is_not_requeried_inside_handler_budget() {
        let definition_reads = Arc::new(AtomicU64::new(0));
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut router = Router::new();
        router
            .add_tool(SlowDefinitionTool {
                definition_reads: Arc::clone(&definition_reads),
                called: Arc::clone(&called),
            })
            .expect("the definition snapshot is admitted once");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);

        block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "slow_definition".to_string(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("dispatch uses the admitted snapshot and reaches the handler");

        assert!(called.load(Ordering::Relaxed));
        assert_eq!(definition_reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn zero_handler_timeout_cannot_relax_request_deadline() {
        let observed_deadline = Arc::new(Mutex::new(None));
        let timeout_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let request_deadline = wall_now().saturating_add_nanos(5_000_000_000);
        let mut router = Router::new();
        router
            .add_tool(BudgetProbeTool {
                timeout: Some(Duration::ZERO),
                delay: Duration::ZERO,
                observed_deadline: Arc::clone(&observed_deadline),
                timeout_read: Arc::clone(&timeout_read),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(
            &cx,
            1,
            Budget::new().with_deadline(request_deadline),
            &state,
        );
        block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "budget_probe".to_string(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("zero adds no ceiling but preserves the request deadline");

        assert!(timeout_read.load(Ordering::Relaxed));
        assert_eq!(
            *observed_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(request_deadline)
        );
    }

    #[test]
    fn ambient_deadline_remains_visible_when_server_and_handler_are_looser() {
        let observed_deadline = Arc::new(Mutex::new(None));
        let timeout_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ambient_deadline = wall_now().saturating_add_nanos(2_000_000_000);
        let request_deadline = ambient_deadline.saturating_add_nanos(2_000_000_000);
        let mut router = Router::new();
        router
            .add_tool(BudgetProbeTool {
                timeout: Some(Duration::from_secs(10)),
                delay: Duration::ZERO,
                observed_deadline: Arc::clone(&observed_deadline),
                timeout_read,
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing_with_budget(Budget::new().with_deadline(ambient_deadline));
        let state = SessionState::new();
        let request_ctx = request_context(
            &cx,
            1,
            Budget::new().with_deadline(request_deadline),
            &state,
        );
        block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "budget_probe".to_string(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("looser inner limits must preserve the ambient deadline");

        assert_eq!(
            *observed_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(ambient_deadline)
        );
    }

    fn assert_sanitized_panic_tool(handler: impl ToolHandler + 'static, name: &str) {
        let mut router = Router::new();
        router
            .add_tool(handler)
            .expect("panic-test tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, Budget::INFINITE, &state);
        let error = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: name.to_string(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect_err("panic must terminate as a sanitized protocol error");
        let wire = serde_json::to_string(&error).expect("error serializes");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(error.message, SANITIZED_HANDLER_PANIC_MESSAGE);
        assert_eq!(error.data, None);
        assert!(!wire.contains(PANIC_CANARY));
        assert!(!wire.contains("Bearer"));
        assert!(!wire.contains("secret"));
        assert!(!wire.contains("peer-secret"));
        assert!(!wire.contains('\u{001b}'));
        assert!(wire.len() < 256);
        assert!(!cx.is_cancel_requested());
    }

    #[test]
    fn unwinding_string_and_non_string_panics_have_one_fixed_peer_error() {
        assert_sanitized_panic_tool(
            UnwindingPanicTool {
                payload: "Bearer actual-secret".to_string(),
                non_string: false,
            },
            "panic_tool",
        );
        assert_sanitized_panic_tool(
            UnwindingPanicTool {
                payload: PANIC_CANARY.to_string(),
                non_string: true,
            },
            "panic_tool",
        );
    }

    #[test]
    fn four_valued_panic_payload_is_never_rendered_for_peer() {
        assert_sanitized_panic_tool(
            OutcomePanicTool(format!("{PANIC_CANARY}{}", "y".repeat(64 * 1024))),
            "outcome_panic_tool",
        );
    }

    #[test]
    fn opaque_internal_handler_errors_use_the_fixed_peer_contract() {
        let cx = Cx::for_testing();

        let mut tool_router = Router::new();
        tool_router
            .add_tool(OpaqueInternalTool)
            .expect("tool registration succeeds");
        let tool_state = SessionState::new();
        let tool_ctx = request_context(&cx, 1, Budget::INFINITE, &tool_state);
        let tool_error = block_on(tool_router.handle_tools_call(
            &tool_ctx,
            CallToolParams {
                name: "opaque_internal_tool".to_string(),
                arguments: None,
                meta: None,
            },
            tool_state,
            None,
            None,
        ))
        .expect_err("opaque internal tool errors must remain protocol failures");

        let mut resource_router = Router::new();
        resource_router.add_resource(OpaqueInternalResource);
        let resource_state = SessionState::new();
        let resource_ctx = request_context(&cx, 2, Budget::INFINITE, &resource_state);
        let resource_error = block_on(resource_router.handle_resources_read(
            &resource_ctx,
            &ReadResourceParams {
                uri: "opaque://internal".to_string(),
                meta: None,
            },
            resource_state,
            None,
            None,
        ))
        .expect_err("opaque internal resource errors must be sanitized");

        let mut prompt_router = Router::new();
        prompt_router.add_prompt(OpaqueInternalPrompt);
        let prompt_state = SessionState::new();
        let prompt_ctx = request_context(&cx, 3, Budget::INFINITE, &prompt_state);
        let prompt_error = block_on(prompt_router.handle_prompts_get(
            &prompt_ctx,
            GetPromptParams {
                name: "opaque_internal_prompt".to_string(),
                arguments: None,
                meta: None,
            },
            prompt_state,
            None,
            None,
        ))
        .expect_err("opaque internal prompt errors must be sanitized");

        for error in [tool_error, resource_error, prompt_error] {
            let wire = serde_json::to_string(&error).expect("error serializes");
            assert_eq!(error.code, McpErrorCode::InternalError);
            assert_eq!(error.message, SANITIZED_HANDLER_PANIC_MESSAGE);
            assert_eq!(error.data, None);
            assert!(!wire.contains(PANIC_CANARY));
            assert!(wire.len() < 256);
        }
    }

    #[test]
    fn resource_and_prompt_panics_use_same_sanitized_contract() {
        let mut resource_router = Router::new();
        resource_router.add_resource(PanicResource);
        let resource_cx = Cx::for_testing();
        let resource_state = SessionState::new();
        let resource_ctx = request_context(&resource_cx, 1, Budget::INFINITE, &resource_state);
        let resource_error = block_on(resource_router.handle_resources_read(
            &resource_ctx,
            &ReadResourceParams {
                uri: "panic://resource".to_string(),
                meta: None,
            },
            resource_state,
            None,
            None,
        ))
        .expect_err("resource panic must be sanitized");
        assert_eq!(resource_error.message, SANITIZED_HANDLER_PANIC_MESSAGE);

        let mut prompt_router = Router::new();
        prompt_router.add_prompt(PanicPrompt);
        let prompt_cx = Cx::for_testing();
        let prompt_state = SessionState::new();
        let prompt_ctx = request_context(&prompt_cx, 1, Budget::INFINITE, &prompt_state);
        let prompt_error = block_on(prompt_router.handle_prompts_get(
            &prompt_ctx,
            GetPromptParams {
                name: "panic_prompt".to_string(),
                arguments: None,
                meta: None,
            },
            prompt_state,
            None,
            None,
        ))
        .expect_err("prompt panic must be sanitized");
        assert_eq!(prompt_error.message, SANITIZED_HANDLER_PANIC_MESSAGE);

        for error in [resource_error, prompt_error] {
            let wire = serde_json::to_string(&error).expect("error serializes");
            assert!(!wire.contains(PANIC_CANARY));
            assert!(wire.len() < 256);
        }
    }

    #[test]
    fn admitted_tool_definition_is_snapshotted_while_other_list_panics_are_sanitized() {
        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);

        let mut tool_router = Router::new();
        tool_router
            .add_tool(DefinitionPanicTool(std::sync::atomic::AtomicBool::new(
                false,
            )))
            .expect("the definition hook is read exactly once during admission");
        let tools = tool_router
            .handle_tools_list(&request_ctx, ListToolsParams::default(), None)
            .expect("listing clones the immutable admitted definition snapshot");
        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "definition_panic_tool");

        let mut resource_router = Router::new();
        resource_router.add_resource(DefinitionPanicResource(std::sync::atomic::AtomicBool::new(
            false,
        )));
        let resource_error = resource_router
            .handle_resources_list(&request_ctx, ListResourcesParams::default(), None)
            .expect_err("resource definition panic must be sanitized");

        let mut prompt_router = Router::new();
        prompt_router.add_prompt(DefinitionPanicPrompt(std::sync::atomic::AtomicBool::new(
            false,
        )));
        let prompt_error = prompt_router
            .handle_prompts_list(&request_ctx, ListPromptsParams::default(), None)
            .expect_err("prompt definition panic must be sanitized");

        for error in [resource_error, prompt_error] {
            let wire = serde_json::to_string(&error).expect("error serializes");
            assert_eq!(error.code, McpErrorCode::InternalError);
            assert_eq!(error.message, SANITIZED_HANDLER_PANIC_MESSAGE);
            assert_eq!(error.data, None);
            assert!(!wire.contains(PANIC_CANARY));
            assert!(wire.len() < 256);
        }
    }

    // ── add_resource_with_behavior: template resource Error ───────────────

    #[test]
    fn add_resource_with_behavior_template_error_on_duplicate() {
        struct TmplResource;
        impl ResourceHandler for TmplResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "db://placeholder".to_string(),
                    name: "db".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }
            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "db://{table}".to_string(),
                    name: "db".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![])
            }
        }
        let mut r = Router::new();
        r.add_resource(TmplResource);
        let err = r
            .add_resource_with_behavior(TmplResource, crate::DuplicateBehavior::Error)
            .unwrap_err();
        assert!(err.message.contains("already exists"));
    }

    // ── add_resource_with_behavior: template resource Ignore ─────────────

    #[test]
    fn add_resource_with_behavior_template_ignore_on_duplicate() {
        struct TmplResource2;
        impl ResourceHandler for TmplResource2 {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "cache://placeholder".to_string(),
                    name: "cache".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }
            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "cache://{key}".to_string(),
                    name: "cache".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![])
            }
        }
        let mut r = Router::new();
        r.add_resource(TmplResource2);
        r.add_resource_with_behavior(TmplResource2, crate::DuplicateBehavior::Ignore)
            .unwrap();
        assert_eq!(r.resource_templates_count(), 1);
    }

    // ── add_resource_with_behavior: template resource Warn ───────────────

    #[test]
    fn add_resource_with_behavior_template_warn_on_duplicate() {
        struct TmplResource3;
        impl ResourceHandler for TmplResource3 {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "log://placeholder".to_string(),
                    name: "log".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }
            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "log://{entry}".to_string(),
                    name: "log".to_string(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![])
            }
        }
        let mut r = Router::new();
        r.add_resource(TmplResource3);
        r.add_resource_with_behavior(TmplResource3, crate::DuplicateBehavior::Warn)
            .unwrap();
        assert_eq!(r.resource_templates_count(), 1);
    }

    // ── mount_tools warns on conflict ────────────────────────────────

    #[test]
    fn mount_tools_warns_on_tool_conflict() {
        let mut main = Router::new();
        main.add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let mut sub = Router::new();
        sub.add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let result = main.mount_tools(sub, None);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("Tool"));
    }

    // ── mount_prompts warns on conflict ──────────────────────────────────

    #[test]
    fn mount_prompts_warns_on_prompt_conflict() {
        let mut main = Router::new();
        main.add_prompt(NamedPrompt::new("p"));
        let mut sub = Router::new();
        sub.add_prompt(NamedPrompt::new("p"));
        let result = main.mount_prompts(sub, None);
        assert_ne!(result.warnings.len(), 0);
        assert!(result.warnings[0].contains("Prompt"));
    }

    // ── invalid cursor returns error ─────────────────────────────────────

    #[test]
    fn invalid_cursor_returns_error() {
        let mut r = Router::new();
        r.set_list_page_size(Some(1));
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let params = ListToolsParams {
            cursor: Some("not-valid-base64!!!".to_string()),
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let err = r.handle_tools_list(&request_ctx, params, None).unwrap_err();
        assert!(err.message.contains("cursor") || err.message.contains("Invalid"));
    }

    // ── set_list_page_size zero is treated as None ───────────────────────

    #[test]
    fn set_list_page_size_zero_disables_pagination() {
        let mut r = Router::new();
        r.set_list_page_size(Some(0));
        r.add_tool(NamedTool::new("a"))
            .expect("tool registration succeeds");
        r.add_tool(NamedTool::new("b"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let params = ListToolsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = r.handle_tools_list(&request_ctx, params, None).unwrap();
        // With page_size = 0, all items returned (no pagination)
        assert_eq!(result.tools.len(), 2);
        assert!(result.next_cursor.is_none());
    }

    // ── strict_input_validation getter ───────────────────────────────────

    #[test]
    fn strict_input_validation_toggle() {
        let mut r = Router::new();
        assert!(!r.strict_input_validation());
        r.set_strict_input_validation(true);
        assert!(r.strict_input_validation());
        r.set_strict_input_validation(false);
        assert!(!r.strict_input_validation());
    }

    // ── cx-cancelled early return paths ──────────────────────────────────

    #[test]
    fn handle_tools_call_cancelled_cx_returns_error() {
        let mut r = Router::new();
        r.add_tool(NamedTool::new("t"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let budget = Budget::INFINITE;
        let params = CallToolParams {
            name: "t".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_tools_call(&request_ctx, params, state, None, None))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::RequestCancelled);
    }

    #[test]
    fn handle_resources_read_cancelled_cx_returns_error() {
        let mut r = Router::new();
        r.add_resource(NamedResource::new("file:///a.txt"));
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let budget = Budget::INFINITE;
        let params = ReadResourceParams {
            uri: "file:///a.txt".to_string(),
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_resources_read(&request_ctx, &params, state, None, None))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::RequestCancelled);
    }

    #[test]
    fn handle_prompts_get_cancelled_cx_returns_error() {
        let mut r = Router::new();
        r.add_prompt(NamedPrompt::new("p"));
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let budget = Budget::INFINITE;
        let params = GetPromptParams {
            name: "p".to_string(),
            arguments: None,
            meta: None,
        };
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1, budget, &state);
        let err = block_on(r.handle_prompts_get(&request_ctx, params, state, None, None))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::RequestCancelled);
    }

    #[test]
    fn completion_handler_dispatches_exact_legacy_and_final_contracts() {
        let mut router = Router::new();
        assert!(!router.has_completion_handler());
        router.add_completion_handler(EchoCompletion);
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        assert!(router.has_completion_handler());
        assert!(
            router
                .server_discovery_behavior_registry()
                .contains(ServerBehavior::CompletionComplete),
            "discovery advertises completion only after the handler is installed"
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 87, Budget::INFINITE, &state);
        let legacy_request = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                "argument": {"name": "topic", "value": "sta"},
            })),
            87_i64,
        );
        let legacy = block_on(router.dispatch_legacy_completion(&request_ctx, &legacy_request))
            .expect("the exact legacy request reaches the registered completion handler");
        assert!(
            legacy.get("resultType").is_none(),
            "the exact legacy completion result remains discriminator-free"
        );
        assert_eq!(
            legacy["completion"]["values"],
            serde_json::json!(["staging"])
        );

        let modern = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                    "argument": {"name": "topic", "value": "sta"},
                })),
                88_i64,
            ),
        ))
        .expect("the final request reaches the same registered completion handler");
        assert_eq!(
            modern.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(modern.get("completion"), legacy.get("completion"));
    }

    #[test]
    fn legacy_completion_visibility_blocks_fallback_and_targeted_providers() {
        const PROMPT: &str = "legacy-completion-prompt";
        const TEMPLATE: &str = "resource://legacy-completion/{id}";
        for targeted in [false, true] {
            let fallback_calls = Arc::new(AtomicUsize::new(0));
            let target_calls = Arc::new(AtomicUsize::new(0));
            let value_count = Arc::new(AtomicUsize::new(2));
            let mut router = Router::new();
            router.add_legacy_prompt(NamedPrompt::new(PROMPT));
            router.add_legacy_resource_template(marked_template(TEMPLATE, "legacy-completion"));
            router.add_legacy_completion_handler(LegacyCompletionProbe {
                calls: Arc::clone(&fallback_calls),
                value_count: Arc::clone(&value_count),
            });
            if targeted {
                let provider = LegacyCompletionProbe {
                    calls: Arc::clone(&target_calls),
                    value_count: Arc::clone(&value_count),
                };
                router.add_legacy_prompt_completion_handler(PROMPT, provider.clone());
                router.add_legacy_resource_template_completion_handler(TEMPLATE, provider);
            }
            let catalogs_before = (
                serde_json::to_value(router.prompts()).unwrap(),
                serde_json::to_value(router.resource_templates()).unwrap(),
            );
            let cx = Cx::for_testing();
            let state = SessionState::new();
            let context = request_context(&cx, 187, Budget::INFINITE, &state);
            let sibling_state = SessionState::new();
            let sibling = request_context(&cx, 188, Budget::INFINITE, &sibling_state);

            for (is_prompt, target) in [
                (true, PROMPT),
                (false, TEMPLATE),
                (true, "provider-owned-prompt"),
                (false, "resource://provider-owned/{id}"),
            ] {
                let reference = if is_prompt {
                    serde_json::json!({"type": "ref/prompt", "name": target})
                } else {
                    serde_json::json!({"type": "ref/resource", "uri": target})
                };
                let params: LegacyCompletionParams = serde_json::from_value(serde_json::json!({
                    "ref": reference,
                    "argument": {"name": "provider-owned-argument", "value": "sta"},
                }))
                .unwrap();
                let params_before = serde_json::to_vec(&params).unwrap();
                let accepted = block_on(router.handle_completion_legacy(&context, params.clone()))
                    .expect("visible legacy targets retain provider-owned argument admission");
                let accepted = serde_json::to_value(accepted).unwrap();
                assert_eq!(
                    accepted["completion"]["values"],
                    serde_json::json!(["legacy-completion-1", "legacy-completion-0"])
                );
                assert_eq!(accepted["completion"]["total"], -1);
                assert!(accepted["completion"].get("hasMore").is_none());
                assert!(accepted.get("resultType").is_none());

                assert!(if is_prompt {
                    context.disable_prompt(target)
                } else {
                    context.disable_resource(target)
                });
                let visibility_before = (context.disabled_prompts(), context.disabled_resources());
                let calls_before = (
                    fallback_calls.load(Ordering::SeqCst),
                    target_calls.load(Ordering::SeqCst),
                );
                let denied = block_on(router.handle_completion_legacy(&context, params.clone()))
                    .expect_err("session-hidden completion cannot consult any provider");
                assert_eq!(denied.code, McpErrorCode::InvalidParams);
                assert_eq!(
                    denied.message,
                    if is_prompt {
                        "completion prompt reference is not available"
                    } else {
                        "completion resource reference is not available"
                    }
                );
                assert_eq!(
                    (
                        fallback_calls.load(Ordering::SeqCst),
                        target_calls.load(Ordering::SeqCst),
                    ),
                    calls_before,
                    "refusal must not invoke either provider"
                );
                assert_eq!(
                    (context.disabled_prompts(), context.disabled_resources()),
                    visibility_before
                );
                assert_eq!(serde_json::to_vec(&params).unwrap(), params_before);
                assert_eq!(
                    serde_json::to_value(
                        block_on(router.handle_completion_legacy(&sibling, params.clone()))
                            .unwrap()
                    )
                    .unwrap(),
                    accepted,
                    "one session's visibility cannot affect another session"
                );
                assert!(if is_prompt {
                    context.enable_prompt(target)
                } else {
                    context.enable_resource(target)
                });
                assert_eq!(
                    serde_json::to_value(
                        block_on(router.handle_completion_legacy(&context, params)).unwrap()
                    )
                    .unwrap(),
                    accepted,
                    "a re-enabled reference must remain usable after refusal"
                );
            }
            assert_eq!(
                (
                    serde_json::to_value(router.prompts()).unwrap(),
                    serde_json::to_value(router.resource_templates()).unwrap(),
                ),
                catalogs_before
            );
            assert_eq!(
                target_calls.load(Ordering::SeqCst),
                if targeted { 6 } else { 0 }
            );
            assert_eq!(
                fallback_calls.load(Ordering::SeqCst),
                if targeted { 6 } else { 12 }
            );
        }
    }

    #[test]
    fn legacy_completion_cannot_expose_final_only_targets() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_legacy_completion_handler(LegacyCompletionProbe {
            calls: Arc::clone(&calls),
            value_count: Arc::new(AtomicUsize::new(2)),
        });
        router.add_prompt(NamedPrompt::new("final-only-completion"));
        router.add_resource(LegacyTemplateResource {
            read_calls: Arc::new(AtomicUsize::new(0)),
        });
        router.add_resource(NamedResource::new("resource://final-only-completion"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let context = request_context(&cx, 189, Budget::INFINITE, &state);
        let params: [LegacyCompletionParams; 3] = [
            serde_json::json!({"type": "ref/prompt", "name": "final-only-completion"}),
            serde_json::json!({
                "type": "ref/resource",
                "uri": "mcp://resource/{collection}/manifest?revision={revision}",
            }),
            serde_json::json!({"type": "ref/resource", "uri": "resource://final-only-completion"}),
        ]
        .map(|reference| {
            serde_json::from_value(serde_json::json!({
                "ref": reference,
                "argument": {"name": "collection", "value": "bo"},
            }))
            .unwrap()
        });
        for request in &params {
            let result = block_on(router.handle_completion_legacy(&context, request.clone()))
                .expect("legacy-visible registrations can reach the completion provider");
            assert_eq!(result.completion.values.len(), 2);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Change only each registration's era visibility. Keep the requests,
        // provider, session, and catalog identities exactly the same.
        router
            .add_final_prompt_with_behavior(
                NamedPrompt::new("final-only-completion"),
                crate::DuplicateBehavior::Replace,
            )
            .unwrap();
        router
            .add_final_resource_with_behavior(
                LegacyTemplateResource {
                    read_calls: Arc::new(AtomicUsize::new(0)),
                },
                crate::DuplicateBehavior::Replace,
            )
            .unwrap();
        router
            .add_final_resource_with_behavior(
                NamedResource::new("resource://final-only-completion"),
                crate::DuplicateBehavior::Replace,
            )
            .unwrap();
        for request in params {
            let denied = block_on(router.handle_completion_legacy(&context, request))
                .expect_err("final-only catalog entries cannot leak through legacy completion");
            assert_eq!(denied.code, McpErrorCode::InvalidParams);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn legacy_completion_enforces_value_limit_before_returning_handler_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let value_count = Arc::new(AtomicUsize::new(99));
        let mut router = Router::new();
        router.add_legacy_completion_handler(LegacyCompletionProbe {
            calls: Arc::clone(&calls),
            value_count: Arc::clone(&value_count),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let context = request_context(&cx, 190, Budget::INFINITE, &state);
        let params: LegacyCompletionParams = serde_json::from_value(serde_json::json!({
            "ref": {"type": "ref/prompt", "name": "provider-owned-prompt"},
            "argument": {"name": "topic", "value": "sta"},
        }))
        .unwrap();
        for count in [99, 100, 101, 100] {
            value_count.store(count, Ordering::SeqCst);
            let result = block_on(router.handle_completion_legacy(&context, params.clone()));
            if count > fastmcp_protocol::MAX_COMPLETION_VALUES {
                let error = result.expect_err("a public handler result cannot contain 101 values");
                assert_eq!(error.code, McpErrorCode::InternalError);
                assert_eq!(
                    error.message,
                    "completion handler returned more than 100 values"
                );
            } else {
                let result = result.expect("a bounded legacy result remains accepted after refusal");
                assert_eq!(result.completion.values.len(), count);
                assert_eq!(
                    result.completion.values[0],
                    format!("legacy-completion-{}", count - 1)
                );
                assert_eq!(result.completion.total, Some(-1));
                assert!(result.completion.has_more.is_none());
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn final_completion_routes_to_the_registered_prompt_or_resource_provider() {
        let prompt_calls = Arc::new(AtomicUsize::new(0));
        let resource_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_completion_handler(ProviderCompletion {
            value: "fallback-provider",
            final_calls: Arc::clone(&fallback_calls),
        });
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        router.add_resource_template(marked_template("resource://first/{id}", "first"));
        router.add_resource_template(marked_template("resource://second/{id}", "second"));
        router.add_prompt_completion_handler(
            "prompt-argument-boundary",
            ProviderCompletion {
                value: "prompt-provider",
                final_calls: Arc::clone(&prompt_calls),
            },
        );
        router.add_resource_template_completion_handler(
            "resource://first/{id}",
            ProviderCompletion {
                value: "resource-provider",
                final_calls: Arc::clone(&resource_calls),
            },
        );

        assert!(
            router
                .server_discovery_behavior_registry()
                .contains(ServerBehavior::CompletionComplete),
            "a registered final completion provider enables discovery"
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 187, Budget::INFINITE, &state);
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let prompt_request = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                "argument": {"name": "topic", "value": "pro"},
            })),
            187_i64,
        );
        let resource_request = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": metadata,
                "ref": {"type": "ref/resource", "uri": "resource://first/{id}"},
                "argument": {"name": "id", "value": "pro"},
            })),
            188_i64,
        );

        let prompt = block_on(router.dispatch_stateless(&request_ctx, &prompt_request))
            .expect("the registered prompt provider handles its exact target");
        let resource = block_on(router.dispatch_stateless(&request_ctx, &resource_request))
            .expect("the registered resource provider handles its exact target");
        assert_eq!(
            prompt["completion"]["values"],
            serde_json::json!(["prompt-provider"])
        );
        assert_eq!(
            resource["completion"]["values"],
            serde_json::json!(["resource-provider"])
        );
        assert_eq!(prompt_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resource_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            0,
            "a target-specific provider takes precedence over the installed fallback"
        );

        let mut unregistered_provider = resource_request.clone();
        unregistered_provider
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("ref"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion reference is an object")
            .insert(
                "uri".to_owned(),
                serde_json::json!("resource://second/{id}"),
            );
        assert_eq!(resource_request.method, unregistered_provider.method);
        assert_eq!(resource_request.id, unregistered_provider.id);
        assert_eq!(
            resource_request
                .params
                .as_ref()
                .and_then(|params| params.get("argument")),
            unregistered_provider
                .params
                .as_ref()
                .and_then(|params| params.get("argument")),
            "the referenced resource template is the sole planted dimension"
        );
        let fallback = block_on(router.dispatch_stateless(&request_ctx, &unregistered_provider))
            .expect("an admitted target without a provider-specific handler reaches the fallback");
        assert_eq!(
            fallback["completion"]["values"],
            serde_json::json!(["fallback-provider"])
        );
        assert_eq!(prompt_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resource_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn modern_connection_visibility_blocks_final_completion_before_provider_invocation() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_completion_handler(CountingCompletion {
            final_calls: Arc::clone(&final_calls),
        });
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        const TEMPLATE_URI: &str = "resource://completion-visibility/{id}";
        router.add_resource_template(marked_template(TEMPLATE_URI, "completion-visibility"));

        let prompt_catalog_before = serde_json::to_vec(&router.prompts())
            .expect("prompt catalog serializes before completion dispatch");
        let template_catalog_before = serde_json::to_vec(&router.resource_templates())
            .expect("resource-template catalog serializes before completion dispatch");
        let prompt_request = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                "argument": {"name": "topic", "value": "sta"},
            })),
            1_871_i64,
        );
        let resource_request = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "ref": {"type": "ref/resource", "uri": TEMPLATE_URI},
                "argument": {"name": "id", "value": "sta"},
            })),
            1_872_i64,
        );
        let prompt_request_bytes =
            serde_json::to_vec(&prompt_request).expect("prompt completion request serializes");
        let resource_request_bytes = serde_json::to_vec(&resource_request)
            .expect("resource-template completion request serializes");

        let cx = Cx::for_testing();
        let allowed_connection = ModernConnection::new();
        let allowed_inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            1_871,
            InboundRequestTransport::Stdio,
            &allowed_connection,
        );
        let allowed_context = allowed_inbound.request_context();
        for request in [&prompt_request, &resource_request] {
            let result = block_on(router.dispatch_stateless(&allowed_context, request))
                .expect("a visible completion target reaches the fallback provider");
            assert_eq!(
                result["completion"]["values"],
                serde_json::json!(["staging"])
            );
        }
        assert_eq!(final_calls.load(Ordering::SeqCst), 2);

        let denied_connection = ModernConnection::new();
        let denied_inbound = InboundRequestContext::with_modern_connection(
            cx,
            1_871,
            InboundRequestTransport::Stdio,
            &denied_connection,
        );
        let denied_context = denied_inbound.request_context();
        assert!(denied_context.disable_prompt("prompt-argument-boundary"));
        assert!(denied_context.disable_resource(TEMPLATE_URI));

        assert_eq!(
            serde_json::to_vec(&prompt_request)
                .expect("prompt completion request remains serializable"),
            prompt_request_bytes,
            "connection visibility is the sole planted prompt-completion dimension"
        );
        let hidden_prompt = block_on(router.dispatch_stateless(&denied_context, &prompt_request))
            .expect_err("a hidden prompt cannot be used as a completion reference");
        assert_eq!(hidden_prompt.code, McpErrorCode::InvalidParams);
        assert_eq!(
            hidden_prompt.message,
            "completion prompt reference is not registered"
        );

        assert_eq!(
            serde_json::to_vec(&resource_request)
                .expect("resource-template completion request remains serializable"),
            resource_request_bytes,
            "connection visibility is the sole planted resource-template-completion dimension"
        );
        let hidden_resource =
            block_on(router.dispatch_stateless(&denied_context, &resource_request))
                .expect_err("a hidden resource template cannot reach the fallback provider");
        assert_eq!(hidden_resource.code, McpErrorCode::InvalidParams);
        assert_eq!(
            hidden_resource.message,
            "completion resource reference is not registered"
        );
        assert_eq!(
            final_calls.load(Ordering::SeqCst),
            2,
            "refused completion references must not invoke the fallback provider"
        );
        assert_eq!(
            serde_json::to_vec(&router.prompts())
                .expect("prompt catalog serializes after completion refusal"),
            prompt_catalog_before,
            "completion refusal cannot mutate the prompt catalog"
        );
        assert_eq!(
            serde_json::to_vec(&router.resource_templates())
                .expect("resource-template catalog serializes after completion refusal"),
            template_catalog_before,
            "completion refusal cannot mutate the resource-template catalog"
        );
    }

    #[test]
    fn explicit_legacy_completion_is_not_discovered_or_dispatched_as_final() {
        let mut router = Router::new();
        router.add_legacy_completion_handler(EchoCompletion);
        router.add_prompt(NamedPrompt::new("deploy"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 184, Budget::INFINITE, &state);
        let request = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "deploy"},
                "argument": {"name": "environment", "value": "sta"},
            })),
            184_i64,
        );
        assert!(
            block_on(router.dispatch_legacy_completion(&request_ctx, &request))
                .is_ok()
        );

        let mut final_request = request.clone();
        final_request
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion parameters are an object")
            .insert(
                "_meta".to_owned(),
                serde_json::json!({
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                }),
            );
        let error = block_on(router.dispatch_stateless(&request_ctx, &final_request))
            .expect_err("only the selected protocol era changes completion availability");
        assert_eq!(error.code, McpErrorCode::MethodNotFound);
        assert!(
            !router
                .server_discovery_behavior_registry()
                .contains(ServerBehavior::CompletionComplete)
        );
    }

    #[test]
    fn final_completion_resource_template_reference_and_argument_are_validated_before_handler() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_completion_handler(CountingCompletion {
            final_calls: Arc::clone(&final_calls),
        });
        router.add_resource(NamedResource::new("resource://static"));
        router.add_resource_template(marked_template("resource:///items/{id}", "registered"));
        router
            .add_legacy_resource_template(marked_template("resource://{legacy_id}", "legacy-only"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 188, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "ref": {"type": "ref/resource", "uri": "resource:///items/{id}"},
                "argument": {"name": "id", "value": "sta"},
            })),
            188_i64,
        );
        let templates_before = serde_json::to_vec(&router.resource_templates())
            .expect("resource-template catalog serializes");
        let template_accepted = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("a registered final resource-template reference is accepted");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let mut static_reference = baseline.clone();
        static_reference
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("ref"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion reference is an object")
            .insert("uri".to_owned(), serde_json::json!("resource://static"));
        let static_error = block_on(router.dispatch_stateless(&request_ctx, &static_reference))
            .expect_err("a static resource is not a final completion-template target");
        assert_eq!(static_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let mut legacy_template = baseline.clone();
        legacy_template
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("ref"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion reference is an object")
            .insert(
                "uri".to_owned(),
                serde_json::json!("resource://{legacy_id}"),
            );
        assert_eq!(baseline.method, legacy_template.method);
        assert_eq!(baseline.id, legacy_template.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("argument")),
            legacy_template
                .params
                .as_ref()
                .and_then(|params| params.get("argument")),
            "the target URI is the sole planted visibility dimension"
        );
        let legacy_template_error =
            block_on(router.dispatch_stateless(&request_ctx, &legacy_template))
                .expect_err("an exact-2024-only template is not final-visible");
        assert_eq!(legacy_template_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let mut unknown_argument = baseline.clone();
        unknown_argument
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("argument"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion argument is an object")
            .insert("name".to_owned(), serde_json::json!("unknown"));
        assert_eq!(baseline.method, unknown_argument.method);
        assert_eq!(baseline.id, unknown_argument.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            unknown_argument
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            "the argument name is the sole planted validation dimension"
        );
        let unknown_argument_error =
            block_on(router.dispatch_stateless(&request_ctx, &unknown_argument))
                .expect_err("an undeclared template argument is rejected before the handler");
        assert_eq!(unknown_argument_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_vec(&router.resource_templates())
                .expect("resource-template catalog serializes after refusal"),
            templates_before,
            "completion reference refusal cannot mutate the registered catalog"
        );
        assert_eq!(
            block_on(router.dispatch_stateless(&request_ctx, &baseline))
                .expect("the registered reference remains accepted after refusal"),
            template_accepted,
            "rejected target or argument changes cannot alter the accepted completion"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn final_completion_prompt_reference_and_argument_are_validated_before_handler() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_completion_handler(CountingCompletion {
            final_calls: Arc::clone(&final_calls),
        });
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        router.add_legacy_prompt(NamedPrompt::new("legacy-completion-prompt"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 189, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                "argument": {"name": "topic", "value": "sta"},
            })),
            189_i64,
        );
        let accepted = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("a final-visible prompt and its declared argument are accepted");
        assert_eq!(accepted["resultType"], "complete");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let mut legacy_prompt = baseline.clone();
        legacy_prompt
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("ref"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion reference is an object")
            .insert(
                "name".to_owned(),
                serde_json::json!("legacy-completion-prompt"),
            );
        assert_eq!(baseline.method, legacy_prompt.method);
        assert_eq!(baseline.id, legacy_prompt.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("argument")),
            legacy_prompt
                .params
                .as_ref()
                .and_then(|params| params.get("argument")),
            "the prompt name is the sole planted visibility dimension"
        );
        let legacy_prompt_error = block_on(router.dispatch_stateless(&request_ctx, &legacy_prompt))
            .expect_err("an exact-2024-only prompt is not final-visible");
        assert_eq!(legacy_prompt_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let mut unknown_argument = baseline.clone();
        unknown_argument
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("argument"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion argument is an object")
            .insert("name".to_owned(), serde_json::json!("unknown"));
        assert_eq!(baseline.method, unknown_argument.method);
        assert_eq!(baseline.id, unknown_argument.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            unknown_argument
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            "the argument name is the sole planted validation dimension"
        );
        let unknown_argument_error =
            block_on(router.dispatch_stateless(&request_ctx, &unknown_argument))
                .expect_err("an undeclared prompt argument is rejected before the handler");
        assert_eq!(unknown_argument_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let legacy = block_on(router.dispatch_legacy_completion(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "ref": {"type": "ref/prompt", "name": "legacy-completion-prompt"},
                    "argument": {"name": "unknown", "value": "sta"},
                })),
                190_i64,
            ),
        ))
        .expect("exact-2024 completion retains its unvalidated target argument behavior");
        assert!(legacy.get("resultType").is_none());
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_completion_rejects_invalid_local_handler_results() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_completion_handler(CompletionValueBoundary {
            final_calls: Arc::clone(&final_calls),
        });
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::new(AtomicUsize::new(0)),
            legacy_calls: Arc::new(AtomicUsize::new(0)),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 191, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "ref": {"type": "ref/prompt", "name": "prompt-argument-boundary"},
                "argument": {"name": "topic", "value": "at-bound"},
            })),
            191_i64,
        );
        let accepted = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("a local handler result at the 100-value limit is accepted");
        assert_eq!(
            accepted["completion"]["values"].as_array().map(Vec::len),
            Some(fastmcp_protocol::MAX_COMPLETION_VALUES)
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let mut one_over = baseline.clone();
        one_over
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("argument"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion argument is an object")
            .insert("value".to_owned(), serde_json::json!("one-over"));
        assert_eq!(baseline.method, one_over.method);
        assert_eq!(baseline.id, one_over.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            one_over
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            "the handler result boundary is the sole planted dimension"
        );
        let error = block_on(router.dispatch_stateless(&request_ctx, &one_over))
            .expect_err("a local handler cannot return a 101st completion value");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            error.message,
            "completion handler returned more than 100 values"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 2);

        let mut negative_total = baseline;
        negative_total
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("argument"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion argument is an object")
            .insert("value".to_owned(), serde_json::json!("negative-total"));
        let error = block_on(router.dispatch_stateless(&request_ctx, &negative_total))
            .expect_err("a local handler cannot return a negative final completion total");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            error.message,
            "final completion total must be a nonnegative JSON integer"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn resource_template_registration_is_final_visible_via_protocol_rfc6570_matcher() {
        struct AmbiguousTemplateResource;

        impl ResourceHandler for AmbiguousTemplateResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "mcp://resource/ambiguous".to_owned(),
                    name: "ambiguous-template".to_owned(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }
            }

            fn template(&self) -> Option<ResourceTemplate> {
                Some(marked_template(
                    "mcp://resource/{first}{second}",
                    "ambiguous",
                ))
            }

            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                Ok(Vec::new())
            }
        }

        let mut router = Router::new();
        let read_calls = Arc::new(AtomicUsize::new(0));
        let completion_calls = Arc::new(AtomicUsize::new(0));
        router.add_completion_handler(CountingCompletion {
            final_calls: Arc::clone(&completion_calls),
        });
        router
            .add_resource_with_behavior(
                ReversibleLevelFourTemplateResource {
                    read_calls: Arc::clone(&read_calls),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("a reversible level-four template is admitted");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 189, Budget::INFINITE, &state);
        let legacy_error = block_on(router.handle_resources_read(
            &request_ctx,
            &fastmcp_protocol::ReadResourceParams {
                uri: "mcp://resource/books/manifest?revision=stable".to_owned(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect_err("scalar explode remains unavailable to exact-2024 routing");
        assert_eq!(legacy_error.code, McpErrorCode::ResourceNotFound);
        assert_eq!(read_calls.load(Ordering::SeqCst), 0);

        let final_metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let templates = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/templates/list",
                Some(serde_json::json!({"_meta": final_metadata.clone()})),
                189_i64,
            ),
        ))
        .expect("the admitted template is final-visible");
        assert_eq!(templates["resultType"], "complete");
        assert_eq!(
            templates["resourceTemplates"][0]["uriTemplate"],
            "mcp://resource{/collection*}/manifest{?revision*}"
        );

        let final_read = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": final_metadata.clone(),
                    "uri": "mcp://resource/books%2Ffiction/manifest?revision=stable",
                })),
                189_i64,
            ),
        ))
        .expect("the final route uses the same reversible matcher");
        assert_eq!(final_read["resultType"], "complete");
        assert_eq!(
            final_read["contents"][0]["text"], "books/fiction:stable",
            "the final route decodes the scalar capture exactly once"
        );
        assert_eq!(read_calls.load(Ordering::SeqCst), 1);

        let completion = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "_meta": final_metadata,
                    "ref": {
                        "type": "ref/resource",
                        "uri": "mcp://resource{/collection*}/manifest{?revision*}",
                    },
                    "argument": {"name": "revision", "value": "sta"},
                })),
                189_i64,
            ),
        ))
        .expect("the final completion target exposes protocol-derived variables");
        assert_eq!(completion["resultType"], "complete");
        assert_eq!(
            completion["completion"]["values"],
            serde_json::json!(["staging"])
        );
        assert_eq!(completion_calls.load(Ordering::SeqCst), 1);

        let template_count = router.resource_templates_count();
        let error = router
            .add_resource_with_behavior(
                AmbiguousTemplateResource,
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("changing only the adjacent scalar boundary remains ambiguous");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            router.resource_templates_count(),
            template_count,
            "rejected template admission cannot mutate the registered catalog"
        );
    }

    #[test]
    fn final_resource_registration_rejects_template_language_collisions_atomically() {
        let mut template_router = Router::new();
        template_router
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{first}", "first"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the first reversible template is admitted");
        let templates_before = serde_json::to_vec(&template_router.resource_templates())
            .expect("the admitted template catalog serializes");

        let error = template_router
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{second}", "second"),
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("renaming only the capture must not create a tie-broken route");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&template_router.resource_templates())
                .expect("a rejected collision leaves the template catalog serializable"),
            templates_before,
            "template-template collision rejection leaves the catalog unchanged"
        );

        let mut exact_router = Router::new();
        exact_router
            .add_resource_with_behavior(
                NamedResource::new("mcp://resource/books"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the exact final resource is admitted");
        let exact_count = exact_router.resources_count();
        let error = exact_router
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{id}", "template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("a template cannot shadow an exact final resource");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(exact_router.resources_count(), exact_count);
        assert_eq!(exact_router.resource_templates_count(), 0);

        let mut reverse_router = Router::new();
        reverse_router
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{id}", "template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the reversible template is admitted before the planted exact collision");
        let templates_before = serde_json::to_vec(&reverse_router.resource_templates())
            .expect("the admitted template catalog serializes");
        let error = reverse_router
            .add_resource_with_behavior(
                NamedResource::new("mcp://resource/books"),
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("an exact final resource cannot be hidden behind a template");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(reverse_router.resources_count(), 0);
        assert_eq!(
            serde_json::to_vec(&reverse_router.resource_templates())
                .expect("the template catalog remains serializable after refusal"),
            templates_before,
            "exact-resource collision rejection leaves the existing template unchanged"
        );

        let mut disjoint_router = Router::new();
        disjoint_router
            .add_resource_template_with_behavior(
                marked_template("mcp://alpha/{id}", "alpha"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("a disjoint template is admitted");
        disjoint_router
            .add_resource_template_with_behavior(
                marked_template("mcp://beta/{id}", "beta"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("a template with a conflicting literal prefix remains independent");
        assert_eq!(disjoint_router.resource_templates_count(), 2);
    }

    #[test]
    fn final_resource_mount_rejects_cross_router_language_collisions_atomically() {
        let exact_uri = "mcp://resource/books";

        let mut exact_destination = Router::new();
        exact_destination
            .add_resource_with_behavior(
                NamedResource::new(exact_uri),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the exact final resource is admitted before mounting");
        let exact_before = serde_json::to_vec(&exact_destination.resources())
            .expect("the exact destination catalog serializes");
        let mut template_source = Router::new();
        template_source
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{id}", "mounted-template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the independently admitted source template is valid");

        let result = exact_destination.mount_resources(template_source, None);
        assert!(!result.is_success());
        assert_eq!(
            serde_json::to_vec(&exact_destination.resources())
                .expect("a rejected mount preserves the destination catalog"),
            exact_before,
            "template-after-exact mount rejection leaves every destination resource unchanged"
        );
        assert_eq!(exact_destination.resource_templates_count(), 0);

        let mut template_destination = Router::new();
        template_destination
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{id}", "destination-template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the final template is admitted before mounting");
        let templates_before = serde_json::to_vec(&template_destination.resource_templates())
            .expect("the template destination catalog serializes");
        let mut exact_source = Router::new();
        exact_source
            .add_resource_with_behavior(
                NamedResource::new(exact_uri),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the independently admitted source resource is valid");

        let result = template_destination.mount_resources(exact_source, None);
        assert!(!result.is_success());
        assert_eq!(template_destination.resources_count(), 0);
        assert_eq!(
            serde_json::to_vec(&template_destination.resource_templates())
                .expect("a rejected mount preserves the destination template catalog"),
            templates_before,
            "exact-after-template mount rejection leaves every destination template unchanged"
        );

        let mut first_template_destination = Router::new();
        first_template_destination
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{first}", "first-template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the first final template is admitted");
        let templates_before = serde_json::to_vec(&first_template_destination.resource_templates())
            .expect("the first template destination catalog serializes");
        let mut second_template_source = Router::new();
        second_template_source
            .add_resource_template_with_behavior(
                marked_template("mcp://resource/{second}", "second-template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the independently admitted second template is valid");

        let result = first_template_destination.mount_resources(second_template_source, None);
        assert!(!result.is_success());
        assert_eq!(
            serde_json::to_vec(&first_template_destination.resource_templates())
                .expect("a rejected mount preserves the original template catalog"),
            templates_before,
            "template-language overlap cannot make mount order a dispatch authority"
        );
    }

    #[test]
    fn empty_prefix_mount_matches_unprefixed_final_route_projection() {
        const EXACT_URI: &str = "mcp://resource/books";

        for prefix in [None, Some("")] {
            let mut destination = Router::new();
            destination
                .add_resource_with_behavior(
                    NamedResource::new(EXACT_URI),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the destination exact final resource is admitted");
            let resources_before = serde_json::to_vec(&destination.resources())
                .expect("the destination catalog serializes before the mount");
            let mut source = Router::new();
            source
                .add_resource_template_with_behavior(
                    marked_template("mcp://resource/{id}", "one-variable-template"),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the source one-variable final template is admitted");

            let result = destination.mount_resources(source, prefix);

            assert!(
                !result.is_success(),
                "an unchanged-key mount must reject the final route collision for {prefix:?}"
            );
            assert!(
                result
                    .errors
                    .iter()
                    .any(|error| error.contains("collides with an exact final resource")),
                "the rejection must retain the exact/template collision reason for {prefix:?}"
            );
            assert_eq!(
                serde_json::to_vec(&destination.resources())
                    .expect("the rejected mount leaves the destination catalog serializable"),
                resources_before,
                "the one-variable collision leaves the destination unchanged for {prefix:?}"
            );
            assert_eq!(destination.resource_templates_count(), 0);
            assert!(destination.final_resources.contains_key(EXACT_URI));

            let mut template_destination = Router::new();
            template_destination
                .add_resource_template_with_behavior(
                    marked_template("mcp://resource/{id}", "one-variable-destination-template"),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the destination one-variable final template is admitted");
            let templates_before = serde_json::to_vec(&template_destination.resource_templates())
                .expect("the destination template catalog serializes before the mount");
            let mut exact_source = Router::new();
            exact_source
                .add_resource_with_behavior(
                    NamedResource::new(EXACT_URI),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the source exact final resource is admitted");

            let result = template_destination.mount_resources(exact_source, prefix);

            assert!(
                !result.is_success(),
                "the reverse unchanged-key collision must reject for {prefix:?}"
            );
            assert!(
                result
                    .errors
                    .iter()
                    .any(|error| error.contains("collides with an exact final resource")),
                "the reverse rejection must retain the exact/template collision reason for {prefix:?}"
            );
            assert_eq!(
                serde_json::to_vec(&template_destination.resource_templates())
                    .expect("the rejected reverse mount leaves the template catalog serializable"),
                templates_before,
                "the reverse one-variable collision leaves the destination unchanged for {prefix:?}"
            );
            assert_eq!(template_destination.resources_count(), 0);
            assert!(
                template_destination.resource_templates["mcp://resource/{id}"]
                    .final_definition
                    .is_some()
            );
        }

        for prefix in [None, Some("")] {
            let mut destination = Router::new();
            destination
                .add_resource_template_with_behavior(
                    marked_template("mcp://alpha/{id}", "destination-disjoint"),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the destination disjoint final template is admitted");
            let mut source = Router::new();
            source
                .add_resource_template_with_behavior(
                    marked_template("mcp://beta/{id}", "source-disjoint"),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the source disjoint final template is admitted");

            let result = destination.mount_resources(source, prefix);

            assert!(
                result.is_success(),
                "a disjoint unchanged-key mount remains admissible for {prefix:?}"
            );
            assert_eq!(destination.resource_templates_count(), 2);
            assert!(
                destination.resource_templates["mcp://beta/{id}"]
                    .final_definition
                    .is_some(),
                "an empty prefix cannot downgrade a disjoint final template for {prefix:?}"
            );
        }

        for prefix in [None, Some("")] {
            let mut destination = Router::new();
            destination
                .add_final_resource_with_behavior(
                    NamedResource::new(EXACT_URI),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the destination final-only exact resource is admitted");
            let mut source = Router::new();
            source
                .add_legacy_resource_template_with_behavior(
                    marked_template("mcp://resource/{id}", "legacy-template"),
                    crate::DuplicateBehavior::Replace,
                )
                .expect("the source legacy-only template is admitted");

            let result = destination.mount_resources(source, prefix);

            assert!(
                result.is_success(),
                "a legacy-only template remains isolated for {prefix:?}"
            );
            assert!(destination.final_resources.contains_key(EXACT_URI));
            assert!(
                destination.resource_templates["mcp://resource/{id}"]
                    .final_definition
                    .is_none(),
                "legacy-only template mounting cannot enter final routing for {prefix:?}"
            );
        }
    }

    #[test]
    fn legacy_only_template_mount_remains_final_isolated() {
        let exact_uri = "mcp://resource/books";
        let mut destination = Router::new();
        destination
            .add_final_resource_with_behavior(
                NamedResource::new(exact_uri),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the final-only exact resource is admitted");
        let mut legacy_source = Router::new();
        legacy_source
            .add_legacy_resource_template_with_behavior(
                marked_template("mcp://resource/{id}", "legacy-template"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the frozen exact-2024 template is admitted in its own catalog");

        let result = destination.mount_resources(legacy_source, None);
        assert!(result.is_success());
        assert_eq!(destination.resources_count(), 1);
        assert_eq!(destination.resource_templates_count(), 1);
        assert!(destination.final_resources.contains_key(exact_uri));
        assert!(
            destination.resource_templates["mcp://resource/{id}"]
                .final_definition
                .is_none(),
            "a legacy-only template remains absent from final dispatch after mounting"
        );
    }

    #[test]
    fn legacy_resource_template_is_inert_on_final_list_read_and_completion() {
        let mut router = Router::new();
        let read_calls = Arc::new(AtomicUsize::new(0));
        let completion_calls = Arc::new(AtomicUsize::new(0));
        router.add_completion_handler(CountingCompletion {
            final_calls: Arc::clone(&completion_calls),
        });
        router
            .add_legacy_resource_with_behavior(
                LegacyTemplateResource {
                    read_calls: Arc::clone(&read_calls),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("the exact-2024 template is admitted for its own route");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 190, Budget::INFINITE, &state);
        let uri = "mcp://resource/books/manifest?revision=stable";
        let legacy_read = block_on(router.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: uri.to_owned(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the exact legacy route retains the registered template");
        let legacy_wire =
            serde_json::to_value(legacy_read).expect("legacy resource result serializes");
        assert_eq!(legacy_wire["contents"][0]["text"], "books:stable");
        assert!(legacy_wire.get("resultType").is_none());
        assert_eq!(read_calls.load(Ordering::SeqCst), 1);

        let final_metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let templates = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/templates/list",
                Some(serde_json::json!({"_meta": final_metadata.clone()})),
                190_i64,
            ),
        ))
        .expect("final template discovery remains valid with only exact-2024 templates");
        assert_eq!(templates["resultType"], "complete");
        assert_eq!(templates["resourceTemplates"], serde_json::json!([]));

        let final_read = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": final_metadata.clone(),
                    "uri": uri,
                })),
                190_i64,
            ),
        ))
        .expect_err("changing only the dispatch era cannot invoke a legacy-only template");
        assert_eq!(final_read.code, McpErrorCode::InvalidParams);
        assert_eq!(read_calls.load(Ordering::SeqCst), 1);

        let completion = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                COMPLETION_COMPLETE,
                Some(serde_json::json!({
                    "_meta": final_metadata,
                    "ref": {
                        "type": "ref/resource",
                        "uri": "mcp://resource/{collection}/manifest?revision={revision}",
                    },
                    "argument": {"name": "revision", "value": "sta"},
                })),
                190_i64,
            ),
        ))
        .expect_err("a legacy-only template is not a final completion target");
        assert_eq!(completion.code, McpErrorCode::InvalidParams);
        assert_eq!(completion_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn exact_2024_resource_template_admission_rejects_final_only_star_and_multi_variable_forms() {
        let mut legacy_router = Router::new();
        legacy_router
            .add_legacy_resource_with_behavior(
                LegacyTemplateResource {
                    read_calls: Arc::new(AtomicUsize::new(0)),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("the frozen {name} legacy grammar remains admitted");
        let catalog_before = serde_json::to_vec(&legacy_router.resource_templates())
            .expect("the admitted legacy catalog serializes");

        let star_error = legacy_router
            .add_legacy_resource_with_behavior(
                ReversibleLevelFourTemplateResource {
                    read_calls: Arc::new(AtomicUsize::new(0)),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("changing only a legacy variable to scalar explode is final-only");
        assert_eq!(star_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&legacy_router.resource_templates())
                .expect("rejected star admission leaves the catalog serializable"),
            catalog_before,
            "legacy rejection cannot mutate the existing catalog"
        );

        let multi_variable_error = legacy_router
            .add_legacy_resource_template_with_behavior(
                marked_template("mcp://resource{?collection*,revision*}", "final-only-multi"),
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("changing only to a named multi-variable expression is final-only");
        assert_eq!(multi_variable_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&legacy_router.resource_templates())
                .expect("rejected multi-variable admission leaves the catalog serializable"),
            catalog_before,
            "the near-identical final syntax cannot alter an exact-2024 catalog"
        );

        let mut final_router = Router::new();
        final_router
            .add_final_resource_with_behavior(
                ReversibleLevelFourTemplateResource {
                    read_calls: Arc::new(AtomicUsize::new(0)),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("the scalar-explode form remains admitted on the final route");
        final_router
            .add_resource_template_with_behavior(
                marked_template("mcp://catalog{?collection*,revision*}", "final-multi"),
                crate::DuplicateBehavior::Replace,
            )
            .expect("the named multi-variable form remains admitted on the final route");
        assert_eq!(final_router.resource_templates_count(), 2);
    }

    #[test]
    fn frozen_exact_2024_matcher_preserves_plus_capture_and_percent_decoding() {
        let matcher = admit_legacy_resource_template("legacy://resource/{+path}")
            .expect("the frozen legacy grammar retains {+name}");
        let params = matcher
            .matches("legacy://resource/books%2Ffiction")
            .expect("legacy plus capture matches a percent-encoded path");
        assert_eq!(
            params.get("path").map(String::as_str),
            Some("books/fiction")
        );

        assert!(
            admit_legacy_resource_template("legacy://resource/{+path*}").is_err(),
            "changing only the scalar modifier keeps the new syntax final-only"
        );
    }

    #[test]
    fn resource_template_admission_rejects_bare_literal_percent_without_catalog_mutation() {
        let accepted = marked_template("mcp://percent/reports%2Fdaily", "percent-template");
        let rejected = marked_template("mcp://percent/reports%Qdaily", "percent-template");
        let mut router = Router::new();

        router
            .add_resource_template_with_behavior(accepted, crate::DuplicateBehavior::Replace)
            .expect("a complete literal percent triplet is admitted");
        let catalog_before = serde_json::to_vec(&router.resource_templates())
            .expect("accepted resource-template catalog serializes");

        let error = router
            .add_resource_template_with_behavior(rejected, crate::DuplicateBehavior::Replace)
            .expect_err("changing only the percent triplet to a bare percent is refused");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&router.resource_templates())
                .expect("rejected admission leaves the catalog serializable"),
            catalog_before,
            "rejected literal syntax cannot rewrite the advertised template"
        );
    }

    #[test]
    fn resource_resolution_skips_cross_era_static_shadows_for_matching_templates() {
        struct ShadowTemplate {
            label: &'static str,
        }

        impl ResourceHandler for ShadowTemplate {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "mcp://shadow/template".to_owned(),
                    name: self.label.to_owned(),
                    description: None,
                    mime_type: Some("text/plain".to_owned()),
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                }
            }

            fn template(&self) -> Option<ResourceTemplate> {
                Some(marked_template("mcp://shadow/{id}", self.label))
            }

            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                unreachable!("templated reads receive their URI parameters")
            }

            fn read_with_uri(
                &self,
                _ctx: &McpContext,
                uri: &str,
                _params: &UriParams,
            ) -> McpResult<Vec<ResourceContent>> {
                Ok(vec![ResourceContent {
                    uri: uri.to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    text: Some(self.label.to_owned()),
                    blob: None,
                }])
            }
        }

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 190, Budget::INFINITE, &state);
        let uri = "mcp://shadow/item";

        let mut legacy_router = Router::new();
        legacy_router
            .add_legacy_resource_with_behavior(
                ShadowTemplate {
                    label: "legacy-template",
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("legacy template registers");
        legacy_router
            .add_final_resource_with_behavior(
                NamedResource::new(uri),
                crate::DuplicateBehavior::Replace,
            )
            .expect("final-only static resource registers");
        let legacy_read = block_on(legacy_router.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: uri.to_owned(),
                meta: None,
            },
            state.clone(),
            None,
            None,
        ))
        .expect("a final-only static URI cannot hide a listed legacy template");
        assert_eq!(
            serde_json::to_value(legacy_read).expect("legacy result serializes")["contents"][0]["text"],
            "legacy-template"
        );

        let mut final_router = Router::new();
        final_router
            .add_final_resource_with_behavior(
                ShadowTemplate {
                    label: "final-template",
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("final template registers");
        final_router
            .add_legacy_resource_with_behavior(
                NamedResource::new(uri),
                crate::DuplicateBehavior::Replace,
            )
            .expect("legacy-only static resource registers");
        let final_read = block_on(final_router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "uri": uri,
                })),
                190_i64,
            ),
        ))
        .expect("a legacy-only static URI cannot hide a listed final template");
        assert_eq!(final_read["contents"][0]["text"], "final-template");
    }

    #[test]
    fn completion_handler_rejects_one_field_final_metadata_in_legacy_request() {
        let mut router = Router::new();
        router.add_completion_handler(EchoCompletion);
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 89, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            COMPLETION_COMPLETE,
            Some(serde_json::json!({
                "ref": {"type": "ref/prompt", "name": "deploy"},
                "argument": {"name": "environment", "value": "sta"},
            })),
            89_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("completion parameters are an object")
            .insert(
                "_meta".to_string(),
                serde_json::json!({
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                }),
            );

        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("ref")),
            planted.params.as_ref().and_then(|params| params.get("ref")),
            "the final metadata object is the sole planted dimension"
        );
        let catalog_before = router.has_completion_handler();
        let planted_before = serde_json::to_vec(&planted).expect("planted request serializes");

        let baseline_result = block_on(router.dispatch_legacy_completion(&request_ctx, &baseline))
            .expect("the baseline legacy completion request is accepted");
        let error = block_on(router.dispatch_legacy_completion(&request_ctx, &planted))
            .expect_err("only final metadata is refused in the exact legacy request");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&planted).expect("rejected request serializes"),
            planted_before,
            "cross-era rejection cannot mutate caller-owned completion parameters"
        );
        assert_eq!(
            router.has_completion_handler(),
            catalog_before,
            "cross-era rejection cannot alter the installed completion handler"
        );
        assert_eq!(
            block_on(router.dispatch_legacy_completion(&request_ctx, &baseline))
                .expect("the baseline remains accepted after the planted rejection"),
            baseline_result,
            "the one-field rejection cannot alter the accepted legacy completion result"
        );
    }

    #[test]
    fn macro_tool_dispatches_exact_legacy_and_final_complete_results() {
        let _counter_guard = MACRO_DUAL_ERA_TOOL_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut router = Router::new();
        MACRO_DUAL_ERA_TOOL_CALLS.store(0, Ordering::SeqCst);
        router
            .add_tool(MacroDualEraTool)
            .expect("macro tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 91, Budget::INFINITE, &state);

        let legacy = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "macro_dual_era_tool".to_string(),
                arguments: Some(serde_json::json!({})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the legacy adapter still invokes the registered handler");
        let legacy_wire = serde_json::to_value(&legacy).expect("legacy result serializes");
        assert!(
            legacy_wire.get("resultType").is_none(),
            "the exact legacy result shape remains unchanged"
        );
        assert_eq!(legacy_wire["content"][0]["text"], "macro final tool result");

        let modern = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "name": "macro_dual_era_tool",
                    "arguments": {},
                })),
                91_i64,
            ),
        ))
        .expect("the modern router invokes the same installed handler");

        assert_eq!(
            modern.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(modern.get("content"), legacy_wire.get("content"));
        assert_eq!(modern.get("isError"), legacy_wire.get("isError"));
        assert!(modern.get("serverInfo").is_none());
        assert_eq!(
            modern["structuredContent"],
            serde_json::json!({"weather": "clear"})
        );
        assert_eq!(MACRO_DUAL_ERA_TOOL_CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn final_tools_call_input_schema_failure_is_bounded_tool_error_without_handler_call() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&final_calls),
                legacy_calls: Arc::clone(&legacy_calls),
                output_matches_schema: true,
                output_is_error: false,
                output_has_unevaluated_property: false,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 150, Budget::INFINITE, &state);

        let rejected = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": 7}),
                150_i64,
            ),
        ))
        .expect("registered final tool input failures are tool results");
        assert_eq!(rejected["resultType"], "complete");
        assert_eq!(rejected["isError"], true);
        assert_eq!(
            rejected["content"][0]["text"],
            "Tool arguments do not match the declared input schema."
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let accepted = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                151_i64,
            ),
        ))
        .expect("changing only the input value to match the schema is accepted");
        assert_eq!(accepted["resultType"], "complete");
        assert!(accepted.get("isError").is_none());
        assert_eq!(
            accepted["structuredContent"],
            serde_json::json!({"accepted": true})
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn final_unknown_tool_is_invalid_params_while_legacy_unknown_tool_is_unchanged() {
        let router = Router::new();
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 152, Budget::INFINITE, &state);

        let modern_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request("unknown-tool", serde_json::json!({}), 152_i64),
        ))
        .expect_err("an unknown final tool is an invalid-params protocol error");
        assert_eq!(modern_error.code, McpErrorCode::InvalidParams);

        let legacy_error = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "unknown-tool".to_owned(),
                arguments: Some(serde_json::json!({})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect_err("the exact legacy unknown-tool result remains method-not-found");
        assert_eq!(legacy_error.code, McpErrorCode::MethodNotFound);
    }

    #[test]
    fn final_tool_output_schema_is_checked_before_success_and_legacy_is_unchanged() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&final_calls),
                legacy_calls: Arc::clone(&legacy_calls),
                output_matches_schema: true,
                output_is_error: false,
                output_has_unevaluated_property: false,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 153, Budget::INFINITE, &state);

        let accepted = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                153_i64,
            ),
        ))
        .expect("a complete result matching the declared output schema is emitted");
        assert_eq!(
            accepted["structuredContent"],
            serde_json::json!({"accepted": true})
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let legacy = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "schema-boundary-tool".to_owned(),
                arguments: Some(serde_json::json!({"value": "accepted"})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the legacy tool path does not apply final output-schema validation");
        assert!(!legacy.is_error);
        let legacy_wire = serde_json::to_value(&legacy).expect("legacy result serializes");
        assert_eq!(
            legacy_wire["content"][0]["text"],
            "legacy schema-boundary result"
        );
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);

        let rejected_final_calls = Arc::new(AtomicUsize::new(0));
        let rejected_legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut rejected_router = Router::new();
        rejected_router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&rejected_final_calls),
                legacy_calls: Arc::clone(&rejected_legacy_calls),
                output_matches_schema: false,
                output_is_error: false,
                output_has_unevaluated_property: false,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");
        let rejected = block_on(rejected_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                154_i64,
            ),
        ))
        .expect_err("a complete result failing the declared output schema is not emitted");
        assert_eq!(rejected.code, McpErrorCode::InternalError);
        assert_eq!(
            rejected.message,
            "tool output does not match the declared output schema"
        );
        assert_eq!(rejected_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(rejected_legacy_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn final_tool_output_schema_applies_to_complete_error_payloads() {
        let accepted_calls = Arc::new(AtomicUsize::new(0));
        let mut accepted_router = Router::new();
        accepted_router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&accepted_calls),
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                output_matches_schema: true,
                output_is_error: true,
                output_has_unevaluated_property: false,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 154, Budget::INFINITE, &state);

        let accepted = block_on(accepted_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                154_i64,
            ),
        ))
        .expect("a schema-conforming complete error payload is emitted");
        assert_eq!(accepted["resultType"], "complete");
        assert_eq!(accepted["isError"], true);
        assert_eq!(
            accepted["structuredContent"],
            serde_json::json!({"accepted": true})
        );
        assert_eq!(accepted_calls.load(Ordering::SeqCst), 1);

        let rejected_calls = Arc::new(AtomicUsize::new(0));
        let mut rejected_router = Router::new();
        rejected_router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&rejected_calls),
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                output_matches_schema: false,
                output_is_error: true,
                output_has_unevaluated_property: false,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");

        let rejected = block_on(rejected_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                155_i64,
            ),
        ))
        .expect_err("a nonconforming complete error payload is not emitted");
        assert_eq!(rejected.code, McpErrorCode::InternalError);
        assert_eq!(
            rejected.message,
            "tool output does not match the declared output schema"
        );
        assert_eq!(rejected_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_tool_admitted_schemas_enforce_unevaluated_properties_on_shipped_path() {
        let input_final_calls = Arc::new(AtomicUsize::new(0));
        let input_legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut input_router = Router::new();
        input_router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&input_final_calls),
                legacy_calls: Arc::clone(&input_legacy_calls),
                output_matches_schema: true,
                output_is_error: false,
                output_has_unevaluated_property: false,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 155, Budget::INFINITE, &state);

        let rejected_input = block_on(input_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted", "unexpected": true}),
                155_i64,
            ),
        ))
        .expect("unevaluated final input properties return the bounded tool error result");
        assert_eq!(rejected_input["resultType"], "complete");
        assert_eq!(rejected_input["isError"], true);
        assert_eq!(input_final_calls.load(Ordering::SeqCst), 0);
        assert_eq!(input_legacy_calls.load(Ordering::SeqCst), 0);

        let accepted_input = block_on(input_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                156_i64,
            ),
        ))
        .expect("removing only the unevaluated input property reaches the handler");
        assert_eq!(accepted_input["resultType"], "complete");
        assert_eq!(input_final_calls.load(Ordering::SeqCst), 1);

        let output_final_calls = Arc::new(AtomicUsize::new(0));
        let output_legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut output_router = Router::new();
        output_router
            .add_tool(SchemaBoundaryTool {
                final_calls: Arc::clone(&output_final_calls),
                legacy_calls: Arc::clone(&output_legacy_calls),
                output_matches_schema: true,
                output_is_error: false,
                output_has_unevaluated_property: true,
                invalid_final_input_schema: false,
                missing_final_input_object_type: false,
                invalid_final_output_schema: false,
            })
            .expect("schema-boundary tool registration succeeds");

        let rejected_output = block_on(output_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "schema-boundary-tool",
                serde_json::json!({"value": "accepted"}),
                157_i64,
            ),
        ))
        .expect_err("an unevaluated final output property is not emitted as success");
        assert_eq!(rejected_output.code, McpErrorCode::InternalError);
        assert_eq!(
            rejected_output.message,
            "tool output does not match the declared output schema"
        );
        assert_eq!(output_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(output_legacy_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn valid_final_tool_replace_updates_both_catalogs_without_reordering() {
        let original_legacy_calls = Arc::new(AtomicUsize::new(0));
        let original_final_calls = Arc::new(AtomicUsize::new(0));
        let replacement_legacy_calls = Arc::new(AtomicUsize::new(0));
        let replacement_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("before"))
            .expect("tool registration succeeds");
        router
            .add_tool(AdmittedSchemaReplacementTool {
                legacy_calls: Arc::clone(&original_legacy_calls),
                final_calls: Arc::clone(&original_final_calls),
                legacy_label: "original",
                output_schema: serde_json::json!({"type": "string"}),
                structured_content: Some(serde_json::json!("original")),
            })
            .expect("original tool registration succeeds");
        router
            .add_tool(NamedTool::new("after"))
            .expect("tool registration succeeds");
        let order_before = router
            .tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();

        router
            .add_tool_with_behavior(
                AdmittedSchemaReplacementTool {
                    legacy_calls: Arc::clone(&replacement_legacy_calls),
                    final_calls: Arc::clone(&replacement_final_calls),
                    legacy_label: "replacement",
                    output_schema: serde_json::json!({"type": "boolean"}),
                    structured_content: Some(serde_json::json!(true)),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("a fully admitted replacement commits both catalog views");
        assert_eq!(
            router
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            order_before,
            "replacement retains the original registration position"
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 158, Budget::INFINITE, &state);
        let legacy = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "admitted-schema-replacement-tool".to_owned(),
                arguments: Some(serde_json::json!({})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the replacement is installed for legacy dispatch");
        let legacy_wire = serde_json::to_value(&legacy).expect("legacy result serializes");
        assert_eq!(legacy_wire["content"][0]["text"], "replacement");
        assert_eq!(replacement_legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(original_legacy_calls.load(Ordering::SeqCst), 0);

        let modern = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "admitted-schema-replacement-tool",
                serde_json::json!({}),
                159_i64,
            ),
        ))
        .expect("the replacement is installed for modern dispatch");
        assert_eq!(modern["structuredContent"], serde_json::json!(true));
        assert_eq!(replacement_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(original_final_calls.load(Ordering::SeqCst), 0);

        let modern_catalog = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 160_i64),
        ))
        .expect("the admitted replacement remains visible to the modern catalog");
        assert_eq!(
            modern_catalog["tools"]
                .as_array()
                .expect("modern tools remain an array")
                .iter()
                .map(|tool| tool["name"].as_str().expect("tool name is a string"))
                .collect::<Vec<_>>(),
            vec!["before", "admitted-schema-replacement-tool", "after"]
        );
        assert_eq!(
            modern_catalog["tools"][1]["outputSchema"]["type"],
            "boolean"
        );
    }

    #[test]
    fn invalid_final_schema_new_tool_leaves_both_catalogs_unchanged() {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("existing"))
            .expect("baseline tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 161, Budget::INFINITE, &state);
        let legacy_before =
            serde_json::to_value(router.tools()).expect("legacy catalog serializes");
        let modern_before = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 161_i64),
        ))
        .expect("baseline modern catalog is available");

        let error = router
            .add_tool_with_behavior(
                InvalidFinalSchemaNamedTool::with_tags("new-invalid", Vec::new()),
                crate::DuplicateBehavior::Error,
            )
            .expect_err("a new normal tool with a scalar outputSchema is rejected");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            serde_json::to_value(router.tools()).expect("legacy catalog serializes"),
            legacy_before,
            "failed admission cannot add a legacy-only entry"
        );
        assert_eq!(
            block_on(router.dispatch_stateless(
                &request_ctx,
                &final_tools_list_request(None, None, None, 162_i64),
            ))
            .expect("modern catalog remains available"),
            modern_before,
            "failed admission cannot alter the modern catalog"
        );
        assert!(router.get_tool("new-invalid").is_none());
    }

    #[test]
    fn only_tokenized_upstream_schema_registration_bypasses_local_validation() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1601, Budget::INFINITE, &state);
        let mut registered_proxy_router = Router::new();
        registered_proxy_router
            .add_tool(UpstreamScalarSchemaTool {
                registered_proxy: true,
            })
            .expect("an upstream-owned scalar schema is retained without local admission");

        let response = block_on(registered_proxy_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "upstream-scalar-schema-tool",
                serde_json::json!({}),
                1601_i64,
            ),
        ))
        .expect("upstream-owned structured content is not locally revalidated");
        assert_eq!(
            response["structuredContent"],
            serde_json::json!({"upstream": true})
        );

        let mut forged_router = Router::new();
        let error = forged_router
            .add_tool(UpstreamScalarSchemaTool {
                registered_proxy: false,
            })
            .expect_err("a forgeable authority label cannot bypass local schema admission");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert!(
            forged_router
                .get_tool("upstream-scalar-schema-tool")
                .is_none()
        );
    }

    #[test]
    fn tokenized_upstream_schema_still_honors_strict_input_validation() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1602, Budget::INFINITE, &state);
        let mut router = Router::new();
        router
            .add_tool(UpstreamScalarSchemaTool {
                registered_proxy: true,
            })
            .expect("an upstream-owned object schema is retained without local admission");
        router.set_strict_input_validation(true);

        let refused = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "upstream-scalar-schema-tool",
                serde_json::json!({"extra": 1}),
                1602_i64,
            ),
        ))
        .expect("gateway strict mode returns a complete tools/call result");
        assert_eq!(refused["resultType"], "complete");
        assert_eq!(refused["isError"], true);
        assert_eq!(
            refused["content"][0]["text"],
            "Tool arguments do not match the declared input schema."
        );

        let admitted = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "upstream-scalar-schema-tool",
                serde_json::json!({}),
                1603_i64,
            ),
        ))
        .expect("declared empty object arguments still reach the proxy handler");
        assert_eq!(
            admitted["structuredContent"],
            serde_json::json!({"upstream": true})
        );

        router.set_strict_input_validation(false);
        let extra = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "upstream-scalar-schema-tool",
                serde_json::json!({"extra": 1}),
                1604_i64,
            ),
        ))
        .expect("changing only the strict flag must admit the extra property");
        assert_eq!(
            extra["structuredContent"],
            serde_json::json!({"upstream": true})
        );
    }

    #[test]
    fn normal_registration_rejects_missing_input_schema_type_without_catalog_mutation() {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("existing"))
            .expect("baseline tool registration succeeds");
        let legacy_before =
            serde_json::to_value(router.tools()).expect("legacy catalog serializes");

        let error = router
            .add_tool_with_behavior(
                SchemaBoundaryTool {
                    final_calls: Arc::new(AtomicUsize::new(0)),
                    legacy_calls: Arc::new(AtomicUsize::new(0)),
                    output_matches_schema: true,
                    output_is_error: false,
                    output_has_unevaluated_property: false,
                    invalid_final_input_schema: false,
                    missing_final_input_object_type: true,
                    invalid_final_output_schema: false,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect_err("a normal inputSchema must declare type object");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            serde_json::to_value(router.tools()).expect("legacy catalog serializes"),
            legacy_before
        );
        assert!(router.get_tool("schema-boundary-tool").is_none());
    }

    #[test]
    fn final_tool_scalar_and_null_structured_content_are_present_and_validated() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 163, Budget::INFINITE, &state);

        let mut scalar_router = Router::new();
        scalar_router
            .add_tool(AdmittedSchemaReplacementTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::new(AtomicUsize::new(0)),
                legacy_label: "scalar",
                output_schema: serde_json::json!({"type": "string"}),
                structured_content: Some(serde_json::json!("")),
            })
            .expect("scalar-output tool registration succeeds");
        let scalar = block_on(scalar_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "admitted-schema-replacement-tool",
                serde_json::json!({}),
                163_i64,
            ),
        ))
        .expect("an object-valued schema document may describe a scalar result");
        assert_eq!(
            scalar.get("structuredContent"),
            Some(&serde_json::json!("")),
            "an empty string is present structured content, not an omitted value"
        );

        let mut null_router = Router::new();
        null_router
            .add_tool(AdmittedSchemaReplacementTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::new(AtomicUsize::new(0)),
                legacy_label: "null",
                output_schema: serde_json::json!({"type": "null"}),
                structured_content: Some(serde_json::Value::Null),
            })
            .expect("null-output tool registration succeeds");
        let null = block_on(null_router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "admitted-schema-replacement-tool",
                serde_json::json!({}),
                164_i64,
            ),
        ))
        .expect("present JSON null is validated against a null output schema");
        assert_eq!(
            null.get("structuredContent"),
            Some(&serde_json::Value::Null),
            "explicit JSON null remains present on the server-emission path"
        );
    }

    #[test]
    fn final_tool_declared_output_schema_rejects_absent_structured_content() {
        let mut router = Router::new();
        router
            .add_tool(AdmittedSchemaReplacementTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::new(AtomicUsize::new(0)),
                legacy_label: "missing",
                output_schema: serde_json::json!({"type": "string"}),
                structured_content: None,
            })
            .expect("mapped tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 165, Budget::INFINITE, &state);

        let error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "admitted-schema-replacement-tool",
                serde_json::json!({}),
                165_i64,
            ),
        ))
        .expect_err("a declared output schema requires structured content on complete output");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            error.message,
            "tool output is missing structuredContent required by the declared output schema"
        );
    }

    #[test]
    fn final_tool_error_mapper_covers_input_validation_and_handler_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(ErrorMappedTool {
                mode: ErrorMapperMode::Complete,
                calls: Arc::clone(&calls),
            })
            .expect("both bounded mapper branches satisfy outputSchema");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 169, Budget::INFINITE, &state);

        let input_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "error-mapped-tool",
                serde_json::json!({"value": 7}),
                169_i64,
            ),
        ))
        .expect("input rejection is a schema-valid complete tool error");
        assert_eq!(input_error["isError"], true);
        assert_eq!(
            input_error["structuredContent"],
            serde_json::json!({"error": "input-validation"})
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let handler_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "error-mapped-tool",
                serde_json::json!({"value": "accepted"}),
                170_i64,
            ),
        ))
        .expect("handler rejection is a schema-valid complete tool error");
        assert_eq!(handler_error["isError"], true);
        assert_eq!(
            handler_error["structuredContent"],
            serde_json::json!({"error": "handler"})
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn incomplete_invalid_or_oversized_tool_error_mapper_is_rejected_atomically() {
        for (mode, expected_message) in [
            (
                ErrorMapperMode::MissingHandler,
                "tool declares outputSchema without a complete tool-error structured-content mapper",
            ),
            (
                ErrorMapperMode::InvalidHandler,
                "tool error structured-content mapper does not satisfy outputSchema",
            ),
            (
                ErrorMapperMode::OversizedHandler,
                "tool error structured-content mapper exceeded the registration limit",
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut router = Router::new();
            router
                .add_tool(NamedTool::new("existing"))
                .expect("baseline tool admission succeeds");
            let cx = Cx::for_testing();
            let state = SessionState::new();
            let request_ctx = request_context(&cx, 171, Budget::INFINITE, &state);
            let legacy_before =
                serde_json::to_value(router.tools()).expect("legacy catalog serializes");
            let modern_before = block_on(router.dispatch_stateless(
                &request_ctx,
                &final_tools_list_request(None, None, None, 171_i64),
            ))
            .expect("modern catalog is available");

            let error = router
                .add_tool(ErrorMappedTool {
                    mode,
                    calls: Arc::clone(&calls),
                })
                .expect_err("public add_tool exposes mapper admission failure");
            assert_eq!(error.code, McpErrorCode::InternalError);
            assert_eq!(error.message, expected_message);
            assert_eq!(
                serde_json::to_value(router.tools()).expect("legacy catalog serializes"),
                legacy_before
            );
            assert_eq!(
                block_on(router.dispatch_stateless(
                    &request_ctx,
                    &final_tools_list_request(None, None, None, 172_i64),
                ))
                .expect("modern catalog remains available"),
                modern_before
            );
            assert!(router.get_tool("error-mapped-tool").is_none());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn normal_registration_rejects_null_output_schema_without_catalog_mutation() {
        let mut router = Router::new();
        let error = router
            .add_tool_with_behavior(
                AdmittedSchemaReplacementTool {
                    legacy_calls: Arc::new(AtomicUsize::new(0)),
                    final_calls: Arc::new(AtomicUsize::new(0)),
                    legacy_label: "null-schema",
                    output_schema: serde_json::Value::Null,
                    structured_content: None,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect_err("the outputSchema wire field itself must be an object");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert!(router.tools().is_empty());
        assert!(
            router
                .get_tool("admitted-schema-replacement-tool")
                .is_none()
        );
    }

    #[test]
    fn invalid_final_schema_replace_leaves_handlers_and_catalogs_unchanged() {
        let original_legacy_calls = Arc::new(AtomicUsize::new(0));
        let original_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(AdmittedSchemaReplacementTool {
                legacy_calls: Arc::clone(&original_legacy_calls),
                final_calls: Arc::clone(&original_final_calls),
                legacy_label: "original",
                output_schema: serde_json::json!({"type": "string"}),
                structured_content: Some(serde_json::json!("original")),
            })
            .expect("original tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 166, Budget::INFINITE, &state);
        let legacy_before =
            serde_json::to_value(router.tools()).expect("legacy catalog serializes");
        let modern_before = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 166_i64),
        ))
        .expect("baseline modern catalog is available");

        let error = router
            .add_tool_with_behavior(
                InvalidFinalSchemaNamedTool::with_tags(
                    "admitted-schema-replacement-tool",
                    Vec::new(),
                ),
                crate::DuplicateBehavior::Replace,
            )
            .expect_err("changing only the candidate outputSchema rejects replacement");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            serde_json::to_value(router.tools()).expect("legacy catalog serializes"),
            legacy_before,
            "a rejected replacement cannot replace the legacy handler"
        );
        assert_eq!(
            block_on(router.dispatch_stateless(
                &request_ctx,
                &final_tools_list_request(None, None, None, 167_i64),
            ))
            .expect("modern catalog remains available"),
            modern_before,
            "a rejected replacement cannot remove the admitted modern entry"
        );

        let legacy = block_on(router.handle_tools_call(
            &request_ctx,
            CallToolParams {
                name: "admitted-schema-replacement-tool".to_owned(),
                arguments: Some(serde_json::json!({})),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the original legacy handler remains installed");
        let legacy_wire = serde_json::to_value(&legacy).expect("legacy result serializes");
        assert_eq!(legacy_wire["content"][0]["text"], "original");
        assert_eq!(original_legacy_calls.load(Ordering::SeqCst), 1);

        let modern = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "admitted-schema-replacement-tool",
                serde_json::json!({}),
                168_i64,
            ),
        ))
        .expect("the original modern handler remains installed");
        assert_eq!(modern["structuredContent"], serde_json::json!("original"));
        assert_eq!(original_final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_tool_and_prompt_argument_nulls_are_rejected_without_erasing_absence() {
        let tool_final_calls = Arc::new(AtomicUsize::new(0));
        let prompt_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::clone(&tool_final_calls),
            })
            .expect("tool registration succeeds");
        router.add_prompt(DirectFinalPrompt {
            final_calls: Arc::clone(&prompt_final_calls),
        });
        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            160,
            InboundRequestTransport::Memory,
            &connection,
        );
        let request_ctx = inbound.request_context();

        let absent_tool_arguments = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "input-required-tool",
            })),
            160_i64,
        );
        let mut null_tool_arguments = absent_tool_arguments.clone();
        null_tool_arguments
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("final tool parameters are an object")
            .insert("arguments".to_owned(), serde_json::Value::Null);

        let CoreRequest::Final(FinalCoreRequest::ToolsCall(absent_tool_params)) =
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                "tools/call",
                absent_tool_arguments.params.as_ref(),
            )
            .expect("absent final tool arguments decode")
        else {
            panic!("the final tool parameter shape is selected");
        };
        assert!(absent_tool_params.arguments.is_absent());
        // Final arguments deserialization now rejects an explicit null outright,
        // so a null `arguments` member fails at decode rather than reaching the
        // handler as an explicit-null presence marker.
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                "tools/call",
                null_tool_arguments.params.as_ref(),
            )
            .is_err(),
            "explicit-null final tool arguments are rejected at decode"
        );

        let absent_tool_result =
            block_on(router.dispatch_stateless(&request_ctx, &absent_tool_arguments))
                .expect("absent final tool arguments default to an empty object");
        assert_eq!(absent_tool_result["resultType"], "input_required");
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);
        let null_tool_error =
            block_on(router.dispatch_stateless(&request_ctx, &null_tool_arguments))
                .expect_err("explicit-null final tool arguments are rejected");
        assert_eq!(null_tool_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let absent_prompt_arguments = direct_final_prompt_request(161_i64);
        let mut null_prompt_arguments = absent_prompt_arguments.clone();
        null_prompt_arguments
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("final prompt parameters are an object")
            .insert("arguments".to_owned(), serde_json::Value::Null);

        let CoreRequest::Final(FinalCoreRequest::PromptsGet(absent_prompt_params)) =
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                "prompts/get",
                absent_prompt_arguments.params.as_ref(),
            )
            .expect("absent final prompt arguments decode")
        else {
            panic!("the final prompt parameter shape is selected");
        };
        assert!(absent_prompt_params.arguments.is_absent());
        // As with tools/call, a null `arguments` member is rejected at decode.
        assert!(
            CoreRequest::decode(
                ProtocolEra::Modern2026,
                "prompts/get",
                null_prompt_arguments.params.as_ref(),
            )
            .is_err(),
            "explicit-null final prompt arguments are rejected at decode"
        );

        let absent_prompt_result =
            block_on(router.dispatch_stateless(&request_ctx, &absent_prompt_arguments))
                .expect("absent final prompt arguments default to an empty map");
        assert_eq!(absent_prompt_result["resultType"], "complete");
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 1);
        let null_prompt_error =
            block_on(router.dispatch_stateless(&request_ctx, &null_prompt_arguments))
                .expect_err("explicit-null final prompt arguments are rejected");
        assert_eq!(null_prompt_error.code, McpErrorCode::InvalidParams);
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn only_the_exact_missing_capability_error_is_terminal_for_final_tools() {
        let canonical = require_final_tasks_capability(&OpenMetadata::default())
            .expect_err("missing Tasks capability has a canonical refusal");
        assert!(is_framework_terminal_final_tool_error(&canonical));

        let mut altered_message = canonical.clone();
        altered_message.message.push('.');
        let mut altered_data = canonical.clone();
        altered_data
            .data
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("canonical refusal data is an object")
            .insert("extra".to_owned(), serde_json::Value::Bool(true));
        let mut non_object_required_capabilities = canonical.clone();
        non_object_required_capabilities.data = Some(serde_json::json!({
            "requiredCapabilities": [],
        }));
        assert!(!is_framework_terminal_final_tool_error(&altered_message));
        assert!(!is_framework_terminal_final_tool_error(&altered_data));
        assert!(!is_framework_terminal_final_tool_error(
            &non_object_required_capabilities
        ));

        let mut router = Router::new();
        for (name, error) in [
            ("canonical-capability-error", canonical.clone()),
            ("message-lookalike", altered_message),
            ("data-lookalike", altered_data),
            (
                "non-object-required-capabilities",
                non_object_required_capabilities,
            ),
        ] {
            router
                .add_tool(DetailedFinalErrorTool { name, error })
                .expect("error tool registration succeeds");
        }
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 159, Budget::INFINITE, &state);

        let observed = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "canonical-capability-error",
                serde_json::json!({}),
                159_i64,
            ),
        ))
        .expect_err("the exact framework-owned refusal remains a JSON-RPC error");
        assert_eq!(observed.code, canonical.code);
        assert_eq!(observed.message, canonical.message);
        assert_eq!(observed.data, canonical.data);

        for (name, request_id) in [
            ("message-lookalike", 160_i64),
            ("data-lookalike", 161_i64),
            ("non-object-required-capabilities", 162_i64),
        ] {
            let result = block_on(router.dispatch_stateless(
                &request_ctx,
                &final_tools_call_request(name, serde_json::json!({}), request_id),
            ))
            .expect("a handler-controlled lookalike remains a tool-level error result");
            assert_eq!(result["isError"], true);
        }
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_task_capable_tool_creates_work_bound_task_after_capability_and_service_admission() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryFinalTaskStore::default());
        let runtime = task_runtime_for_router(Arc::clone(&store));
        let service_runner = runtime
            .install_task_service(1, Arc::new(NoopFinalTaskSupervisor))
            .expect("a bounded application-owned task service is installed");
        let service_cx = Cx::for_testing();
        let mut running_service = Box::pin(service_runner.run(&service_cx));
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Future::poll(running_service.as_mut(), &mut task_cx),
            Poll::Pending
        ));
        let mut router = Router::new();
        router.set_final_task_runtime(Some(runtime));
        router
            .add_tool(TaskCapableRouterTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 160, Budget::INFINITE, &state);

        let result = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_task_capable_tool_request(160_i64),
        ))
        .expect("the admitted task-capable outcome creates a work-bound task");
        assert_eq!(result["resultType"], "task");
        assert_eq!(result["status"], "working");
        assert_eq!(result["statusMessage"], "router task created");
        assert!(
            result["taskId"].as_str().is_some(),
            "the created task has a final task identifier"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.task_count(), 1);
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_task_declaration_gates_only_the_create_task_outcome() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryFinalTaskStore::default());
        let mut router = Router::new();
        router
            .add_tool(ConditionalTaskCapableRouterTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("conditional task-capable tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 164, Budget::INFINITE, &state);

        let complete = final_tools_call_request(
            "conditional-task-capable-router-tool",
            serde_json::json!({ "createTask": false }),
            164_i64,
        );
        let task = final_tools_call_request(
            "conditional-task-capable-router-tool",
            serde_json::json!({ "createTask": true }),
            165_i64,
        );
        assert_eq!(complete.method, task.method);
        assert_eq!(
            complete
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            task.params.as_ref().and_then(|params| params.get("_meta")),
            "Tasks negotiation is unchanged between the paired requests"
        );

        let result = block_on(router.dispatch_stateless(&request_ctx, &complete))
            .expect("a declared task-capable handler may complete without Tasks negotiation");
        assert_eq!(result["resultType"], "complete");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.task_count(), 0);

        let error = block_on(router.dispatch_stateless(&request_ctx, &task))
            .expect_err("only the CreateTask outcome requires Tasks negotiation");
        assert!(matches!(error.code, McpErrorCode::Custom(_)));
        assert_eq!(final_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            store.task_count(),
            0,
            "a rejected CreateTask outcome must not mutate the task store"
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_task_outcome_without_runtime_rejects_after_handler_before_store_mutation() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryFinalTaskStore::default());
        let mut router = Router::new();
        router
            .add_tool(TaskCapableRouterTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 161, Budget::INFINITE, &state);

        let error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_task_capable_tool_request(161_i64),
        ))
        .expect_err("a CreateTask outcome cannot persist without a final Tasks runtime");
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(
            error.message,
            "task-capable tool requires an installed final Tasks runtime"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.task_count(),
            0,
            "outcome-aware runtime admission rejects before task-store mutation"
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_task_outcome_with_unready_service_rejects_after_handler_before_store_mutation() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryFinalTaskStore::default());
        let runtime = task_runtime_for_router(Arc::clone(&store));
        let _unready_service = runtime
            .install_task_service(1, Arc::new(NoopFinalTaskSupervisor))
            .expect("an installed but unpolled task service remains unready");
        let mut router = Router::new();
        router.set_final_task_runtime(Some(runtime));
        router
            .add_tool(TaskCapableRouterTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 162, Budget::INFINITE, &state);

        let error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_task_capable_tool_request(162_i64),
        ))
        .expect_err("an installed but unready task service is refused before task creation");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            error.message,
            "Final task creation requires an installed ready task service"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.task_count(),
            0,
            "outcome-aware readiness admission cannot persist a task"
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_task_outcome_requires_peer_capability_before_store_mutation() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryFinalTaskStore::default());
        let runtime = task_runtime_for_router(Arc::clone(&store));
        let service_runner = runtime
            .install_task_service(1, Arc::new(NoopFinalTaskSupervisor))
            .expect("a bounded application-owned task service is installed");
        let service_cx = Cx::for_testing();
        let mut running_service = Box::pin(service_runner.run(&service_cx));
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Future::poll(running_service.as_mut(), &mut task_cx),
            Poll::Pending
        ));
        let mut router = Router::new();
        router.set_final_task_runtime(Some(runtime));
        router
            .add_tool(TaskCapableRouterTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 162, Budget::INFINITE, &state);

        let error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request(
                "task-capable-router-tool",
                serde_json::json!({}),
                162_i64,
            ),
        ))
        .expect_err("a missing peer Tasks capability is refused before task-store mutation");
        assert!(matches!(error.code, McpErrorCode::Custom(_)));
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.task_count(),
            0,
            "a rejected CreateTask outcome must not reach the task store"
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn final_router_defensively_rejects_an_undeclared_task_outcome() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(UndeclaredTaskOutcomeRouterTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 163, Budget::INFINITE, &state);
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": {
                            "io.modelcontextprotocol/tasks": {}
                        }
                    },
                },
                "name": "undeclared-task-outcome-router-tool",
                "arguments": {},
            })),
            163_i64,
        );

        let error = block_on(router.dispatch_stateless(&request_ctx, &request))
            .expect_err("the router refuses a task outcome without a preflight declaration");
        assert_eq!(error.code, McpErrorCode::InvalidRequest);
        assert_eq!(
            error.message,
            "tool returned CreateTask without declaring final Tasks capability"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn macro_tool_final_metadata_negative_is_non_mutating() {
        let _counter_guard = MACRO_DUAL_ERA_TOOL_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut router = Router::new();
        MACRO_DUAL_ERA_TOOL_CALLS.store(0, Ordering::SeqCst);
        router
            .add_tool(MacroDualEraTool)
            .expect("macro tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 92, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "macro_dual_era_tool",
                "arguments": {},
            })),
            92_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("tools/call parameters are an object")
            .remove("_meta");

        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            planted
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            "the final metadata object is the sole planted dimension"
        );
        let catalog_before = serde_json::to_vec(&router.tools()).expect("catalog serializes");
        let planted_before = serde_json::to_vec(&planted).expect("request serializes");

        let baseline_result = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("the baseline invokes the registered handler");
        assert_eq!(
            baseline_result.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(MACRO_DUAL_ERA_TOOL_CALLS.load(Ordering::SeqCst), 1);

        let error = block_on(router.dispatch_stateless(&request_ctx, &planted))
            .expect_err("only final metadata is refused");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&planted).expect("rejected request serializes"),
            planted_before,
            "typed refusal cannot mutate caller-owned input"
        );
        assert_eq!(
            serde_json::to_vec(&router.tools()).expect("catalog serializes"),
            catalog_before,
            "typed refusal cannot mutate the installed handler catalog"
        );
        assert_eq!(
            MACRO_DUAL_ERA_TOOL_CALLS.load(Ordering::SeqCst),
            1,
            "metadata refusal cannot invoke the macro-generated tool"
        );
        assert_eq!(
            block_on(router.dispatch_stateless(&request_ctx, &baseline))
                .expect("the unchanged baseline remains accepted after the rejection"),
            baseline_result,
            "the one-field rejection cannot alter the accepted final result"
        );
        assert_eq!(MACRO_DUAL_ERA_TOOL_CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn public_final_dispatch_encodes_input_required_handler_outcomes() {
        let tool_legacy_calls = Arc::new(AtomicUsize::new(0));
        let tool_final_calls = Arc::new(AtomicUsize::new(0));
        let resource_legacy_calls = Arc::new(AtomicUsize::new(0));
        let resource_final_calls = Arc::new(AtomicUsize::new(0));
        let prompt_legacy_calls = Arc::new(AtomicUsize::new(0));
        let prompt_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::clone(&tool_legacy_calls),
                final_calls: Arc::clone(&tool_final_calls),
            })
            .expect("tool registration succeeds");
        router.add_resource(InputRequiredResource {
            legacy_calls: Arc::clone(&resource_legacy_calls),
            final_calls: Arc::clone(&resource_final_calls),
        });
        router.add_prompt(InputRequiredPrompt {
            legacy_calls: Arc::clone(&prompt_legacy_calls),
            final_calls: Arc::clone(&prompt_final_calls),
        });

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            141,
            InboundRequestTransport::Memory,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let tool_request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-tool",
                "arguments": {},
            })),
            141_i64,
        );
        let tool_typed = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "tools/call",
            tool_request.params.as_ref(),
        )
        .expect("final tools/call request decodes");
        let tool_response = block_on(router.dispatch_stateless(&request_ctx, &tool_request))
            .expect("public final tools/call dispatch encodes input_required");
        assert_eq!(tool_response["resultType"], "input_required");
        assert_ne!(tool_response["requestState"], "tool-retry-state");
        assert_eq!(
            tool_response["inputRequests"]["roots"]["method"],
            "roots/list"
        );
        let tool_wire = serde_json::to_string(&tool_response).expect("tool response serializes");
        assert!(matches!(
            tool_typed.decode_result(&tool_wire),
            Ok(CoreResult::Final(FinalCoreResult::ToolsCallInputRequired { result, .. }))
                if result.request_state().is_some()
        ));

        let resource_request = JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "uri": "file:///input-required-resource",
            })),
            142_i64,
        );
        let resource_typed = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "resources/read",
            resource_request.params.as_ref(),
        )
        .expect("final resources/read request decodes");
        let resource_response = block_on(router.dispatch_stateless(&request_ctx, &resource_request))
            .expect("public final resources/read dispatch encodes input_required");
        assert_eq!(resource_response["resultType"], "input_required");
        assert_ne!(resource_response["requestState"], "resource-retry-state");
        assert_eq!(
            resource_response["inputRequests"]["roots"]["method"],
            "roots/list"
        );
        let resource_wire =
            serde_json::to_string(&resource_response).expect("resource response serializes");
        assert!(matches!(
            resource_typed.decode_result(&resource_wire),
            Ok(CoreResult::Final(FinalCoreResult::ResourcesReadInputRequired { result, .. }))
                if result.request_state().is_some()
        ));

        let prompt_request = JsonRpcRequest::new(
            "prompts/get",
            Some(serde_json::json!({
                "_meta": metadata,
                "name": "input-required-prompt",
            })),
            143_i64,
        );
        let prompt_typed = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "prompts/get",
            prompt_request.params.as_ref(),
        )
        .expect("final prompts/get request decodes");
        let prompt_response = block_on(router.dispatch_stateless(&request_ctx, &prompt_request))
            .expect("public final prompts/get dispatch encodes input_required");
        assert_eq!(prompt_response["resultType"], "input_required");
        assert_ne!(prompt_response["requestState"], "prompt-retry-state");
        assert_eq!(
            prompt_response["inputRequests"]["roots"]["method"],
            "roots/list"
        );
        let prompt_wire =
            serde_json::to_string(&prompt_response).expect("prompt response serializes");
        assert!(matches!(
            prompt_typed.decode_result(&prompt_wire),
            Ok(CoreResult::Final(FinalCoreResult::PromptsGetInputRequired { result, .. }))
                if result.request_state().is_some()
        ));

        assert_eq!(tool_legacy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resource_legacy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(prompt_legacy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resource_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_context_elicitation_round_trips_through_tools_call_mrtr() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let resumed_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(ContextElicitationTool {
                initial_calls: Arc::clone(&initial_calls),
                resumed_calls: Arc::clone(&resumed_calls),
            })
            .expect("context elicitation tool registers");

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let mut initial_request =
            final_tools_call_request("context-elicitation-tool", serde_json::json!({}), 940_i64);
        initial_request
            .params
            .as_mut()
            .expect("tool params are present")["_meta"]["io.modelcontextprotocol/clientCapabilities"] = serde_json::json!({
            "elicitation": {"form": {}},
        });
        let initial_context = InboundRequestContext::with_modern_connection(
            cx.clone(),
            940,
            InboundRequestTransport::Memory,
            &connection,
        )
        .request_context()
        .with_client_capabilities(ClientCapabilityInfo::new().with_elicitation(true, false));

        let initial = block_on(router.dispatch_stateless(&initial_context, &initial_request))
            .expect("an elicitation-capable final client receives MRTR input_required");
        assert_eq!(initial["resultType"], "input_required");
        assert_eq!(
            initial["inputRequests"]["approval"]["method"],
            "elicitation/create"
        );
        assert_eq!(
            initial["inputRequests"]["approval"]["params"]["mode"],
            "form"
        );
        assert!(
            initial["inputRequests"]["approval"]
                .get("jsonrpc")
                .is_none()
                && initial["inputRequests"]["approval"].get("id").is_none(),
            "final elicitation must be embedded input, never a reverse JSON-RPC request"
        );
        let request_state = initial["requestState"]
            .as_str()
            .expect("router mints an opaque MRTR request state")
            .to_owned();

        let mut retry_request =
            final_tools_call_request("context-elicitation-tool", serde_json::json!({}), 941_i64);
        let retry_params = retry_request
            .params
            .as_mut()
            .expect("tool params are present");
        retry_params["_meta"]["io.modelcontextprotocol/clientCapabilities"] = serde_json::json!({
            "elicitation": {"form": {}},
        });
        retry_params["inputResponses"] = serde_json::json!({
            "approval": {"action": "accept", "content": {"approved": true}},
        });
        retry_params["requestState"] = serde_json::Value::String(request_state);
        let retry_context = InboundRequestContext::with_modern_connection(
            cx,
            941,
            InboundRequestTransport::Memory,
            &connection,
        )
        .request_context()
        .with_client_capabilities(ClientCapabilityInfo::new().with_elicitation(true, false));

        let completed = block_on(router.dispatch_stateless(&retry_context, &retry_request))
            .expect("the accepted final elicitation resumes the original tools/call");
        assert_eq!(completed["resultType"], "complete");
        assert_eq!(completed["content"][0]["text"], "approved");
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_context_elicitation_rejects_invalid_answers_before_consuming_handler_state() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let resumed_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(ContextElicitationTool {
                initial_calls: Arc::clone(&initial_calls),
                resumed_calls: Arc::clone(&resumed_calls),
            })
            .expect("context elicitation tool registers");
        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let context_for = |id| {
            InboundRequestContext::with_modern_connection(
                cx.clone(),
                id,
                InboundRequestTransport::Memory,
                &connection,
            )
            .request_context()
            .with_client_capabilities(ClientCapabilityInfo::new().with_elicitation(true, false))
        };
        let mut initial_request =
            final_tools_call_request("context-elicitation-tool", serde_json::json!({}), 950_i64);
        initial_request.params.as_mut().expect("initial params")["_meta"]
            ["io.modelcontextprotocol/clientCapabilities"] = serde_json::json!({
            "elicitation": {"form": {}},
        });
        let initial = block_on(router.dispatch_stateless(&context_for(950), &initial_request))
            .expect("initial form request issues continuation state");
        let state = initial["requestState"]
            .as_str()
            .expect("opaque state")
            .to_owned();
        assert_eq!(
            initial["inputRequests"]["approval"]["params"]["requestedSchema"]["required"],
            serde_json::json!(["approved"])
        );
        for (id, answer) in (951_u64..).zip([
            serde_json::json!({"action": "accept"}),
            serde_json::json!({"action": "accept", "content": {}}),
            serde_json::json!({"action": "accept", "content": {"approved": "private-invalid-answer"}}),
            serde_json::json!({"action": "accept", "content": null}),
        ]) {
            let mut retry = final_tools_call_request(
                "context-elicitation-tool",
                serde_json::json!({}),
                i64::try_from(id).expect("test request ID fits"),
            );
            let params = retry.params.as_mut().expect("retry params");
            params["_meta"] =
                initial_request.params.as_ref().expect("initial params")["_meta"].clone();
            params["requestState"] = serde_json::json!(state);
            params["inputResponses"] = serde_json::json!({"approval": answer});
            let raw = serde_json::to_string(params).expect("retry params encode");
            let error = block_on(router.dispatch_stateless_with_raw_params(
                &context_for(id),
                &retry,
                Some(&raw),
            ))
            .expect_err("invalid answers fail before a handler continuation can consume state");
            assert_eq!(error.code, McpErrorCode::InvalidParams);
            assert!(!error.message.contains("private-invalid-answer"));
            assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
            assert_eq!(resumed_calls.load(Ordering::SeqCst), 0);
            assert_eq!(router.mrtr_exchanges.active_len(), 1);
        }
        let mut valid = final_tools_call_request(
            "context-elicitation-tool",
            serde_json::json!({}),
            955_i64,
        );
        let params = valid.params.as_mut().expect("valid retry params");
        params["_meta"] =
            initial_request.params.as_ref().expect("initial params")["_meta"].clone();
        params["requestState"] = serde_json::json!(state);
        params["inputResponses"] = serde_json::json!({
            "approval": {"action": "accept", "content": {"approved": true}},
        });
        let raw = serde_json::to_string(params).expect("valid retry params encode");
        let completed = block_on(router.dispatch_stateless_with_raw_params(
            &context_for(955),
            &valid,
            Some(&raw),
        ))
        .expect("the original continuation state remains usable for a corrected answer");
        assert_eq!(completed["resultType"], "complete");
        assert_eq!(completed["content"][0]["text"], "approved");
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(router.mrtr_exchanges.active_len(), 0);
    }

    #[test]
    fn final_context_elicitation_without_capability_rejects_before_mrtr_state_mutation() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let resumed_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(ContextElicitationTool {
                initial_calls: Arc::clone(&initial_calls),
                resumed_calls: Arc::clone(&resumed_calls),
            })
            .expect("context elicitation tool registers");

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let request =
            final_tools_call_request("context-elicitation-tool", serde_json::json!({}), 942_i64);
        let context = InboundRequestContext::with_modern_connection(
            cx,
            942,
            InboundRequestTransport::Memory,
            &connection,
        )
        .request_context();

        let refusal = block_on(router.dispatch_stateless(&context, &request))
            .expect("changing only client elicitation capability returns a final tool result");
        assert_eq!(refusal["resultType"], "complete");
        assert_eq!(refusal["isError"], true);
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            router.mrtr_exchanges.active_len(),
            0,
            "capability refusal cannot mint MRTR continuation state"
        );
    }

    #[test]
    fn final_sampling_capability_preserves_tool_choice_and_rejection_leaves_registry_unchanged() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(ContextSamplingTool {
                calls: Arc::clone(&calls),
            })
            .expect("context sampling tool registers");

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let request =
            final_tools_call_request("context-sampling-tool", serde_json::json!({}), 943_i64);
        let admitted_context = InboundRequestContext::with_modern_connection(
            cx.clone(),
            943,
            InboundRequestTransport::Memory,
            &connection,
        )
        .request_context()
        .with_client_capabilities(ClientCapabilityInfo::new().with_sampling());
        let admitted = block_on(router.dispatch_stateless(&admitted_context, &request))
            .expect("sampling capability admits final MRTR sampling");
        assert_eq!(admitted["resultType"], "input_required");
        assert_eq!(
            admitted["inputRequests"]["sample"]["params"]["toolChoice"],
            serde_json::json!({"mode": "required"})
        );
        assert_eq!(
            admitted["inputRequests"]["sample"]["params"]["messages"][0]["content"]["type"],
            "tool_use"
        );
        assert_eq!(router.mrtr_exchanges.active_len(), 1);

        let removed_capability_context = InboundRequestContext::with_modern_connection(
            cx,
            944,
            InboundRequestTransport::Memory,
            &connection,
        )
        .request_context();
        let rejection = block_on(router.dispatch_stateless(&removed_capability_context, &request))
            .expect_err("removing only sampling capability rejects the descriptor");
        assert_eq!(rejection.code, McpErrorCode::InvalidRequest);
        assert_eq!(
            router.mrtr_exchanges.active_len(),
            1,
            "the one-field capability removal cannot mutate the admitted registry"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn http_admission_raw_sidecar_reaches_tool_resource_and_prompt_mrtr_retries() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let resource_calls = Arc::new(AtomicUsize::new(0));
        let prompt_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::clone(&tool_calls),
            })
            .expect("tool registers");
        router.add_resource(InputRequiredResource {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&resource_calls),
        });
        router.add_prompt(InputRequiredPrompt {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&prompt_calls),
        });

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            1438,
            InboundRequestTransport::Http,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let cancellation = inbound
            .mrtr_continuation_cancellation()
            .expect("HTTP-bound modern context owns continuations");
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let roots = serde_json::to_string(&router_roots_response_wire())
            .expect("roots response serializes");

        let tool_initial_body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1438, "method": "tools/call",
            "params": {"_meta": metadata.clone(), "name": "input-required-tool", "arguments": {}},
        }))
        .expect("tool body serializes");
        let (tool_initial, tool_initial_raw) =
            admit_http_wire("tools/call", "input-required-tool", &tool_initial_body);
        let tool_issued = block_on(
            router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
                &request_ctx,
                &tool_initial,
                tool_initial_raw.as_deref(),
                &cancellation,
            ),
        )
        .expect("HTTP-admitted tool issues state");
        let tool_retry_body = format!(
            r#"{{"jsonrpc":"2.0","id":1439,"method":"tools/call","params":{{"_meta":{},"name":"input-required-tool","arguments":{{}},"inputResponses":{{"inert":{},"roots":{}}},"requestState":{}}}}}"#,
            serde_json::to_string(&metadata).expect("metadata serializes"),
            roots,
            roots,
            serde_json::to_string(&tool_issued["requestState"]).expect("tool state serializes"),
        );
        let (tool_retry, tool_retry_raw) = admit_http_wire(
            "tools/call",
            "input-required-tool",
            tool_retry_body.as_bytes(),
        );
        block_on(router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
            &request_ctx,
            &tool_retry,
            tool_retry_raw.as_deref(),
            &cancellation,
        ))
        .expect("HTTP-admitted ordered tool retry reaches the handler");

        let resource_initial_body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1440, "method": "resources/read",
            "params": {"_meta": metadata.clone(), "uri": "file:///input-required-resource"},
        }))
        .expect("resource body serializes");
        let (resource_initial, resource_initial_raw) = admit_http_wire(
            "resources/read",
            "file:///input-required-resource",
            &resource_initial_body,
        );
        let resource_issued = block_on(
            router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
                &request_ctx,
                &resource_initial,
                resource_initial_raw.as_deref(),
                &cancellation,
            ),
        )
        .expect("HTTP-admitted resource issues state");
        let resource_retry_body = format!(
            r#"{{"jsonrpc":"2.0","id":1441,"method":"resources/read","params":{{"_meta":{},"uri":"file:///input-required-resource","inputResponses":{{"inert":{},"roots":{}}},"requestState":{}}}}}"#,
            serde_json::to_string(&metadata).expect("metadata serializes"),
            roots,
            roots,
            serde_json::to_string(&resource_issued["requestState"])
                .expect("resource state serializes"),
        );
        let (resource_retry, resource_retry_raw) = admit_http_wire(
            "resources/read",
            "file:///input-required-resource",
            resource_retry_body.as_bytes(),
        );
        let mut sanitized_resource_retry = resource_retry.clone();
        sanitized_resource_retry
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("HTTP-admitted resource parameters are an object")
            .insert(
                "uri".to_owned(),
                serde_json::json!("file:///sanitized-resource"),
            );
        let resource_sidecar_error = block_on(
            router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
                &request_ctx,
                &sanitized_resource_retry,
                resource_retry_raw.as_deref(),
                &cancellation,
            ),
        )
        .expect_err("a resource raw sidecar cannot survive a one-field sanitized typed mismatch");
        assert_eq!(resource_sidecar_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            resource_calls.load(Ordering::SeqCst),
            1,
            "the mismatched resource sidecar is rejected before handler invocation"
        );
        assert_eq!(
            router.mrtr_exchanges.active_len(),
            2,
            "the rejected resource sidecar leaves its continuation available"
        );
        block_on(router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
            &request_ctx,
            &resource_retry,
            resource_retry_raw.as_deref(),
            &cancellation,
        ))
        .expect("HTTP-admitted ordered resource retry reaches the handler");

        let prompt_initial_body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1442, "method": "prompts/get",
            "params": {"_meta": metadata.clone(), "name": "input-required-prompt"},
        }))
        .expect("prompt body serializes");
        let (prompt_initial, prompt_initial_raw) =
            admit_http_wire("prompts/get", "input-required-prompt", &prompt_initial_body);
        let prompt_issued = block_on(
            router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
                &request_ctx,
                &prompt_initial,
                prompt_initial_raw.as_deref(),
                &cancellation,
            ),
        )
        .expect("HTTP-admitted prompt issues state");
        let prompt_retry_body = format!(
            r#"{{"jsonrpc":"2.0","id":1443,"method":"prompts/get","params":{{"_meta":{},"name":"input-required-prompt","inputResponses":{{"inert":{},"roots":{}}},"requestState":{}}}}}"#,
            serde_json::to_string(&metadata).expect("metadata serializes"),
            roots,
            roots,
            serde_json::to_string(&prompt_issued["requestState"]).expect("prompt state serializes"),
        );
        let (prompt_retry, prompt_retry_raw) = admit_http_wire(
            "prompts/get",
            "input-required-prompt",
            prompt_retry_body.as_bytes(),
        );
        let mut sanitized_prompt_retry = prompt_retry.clone();
        sanitized_prompt_retry
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("HTTP-admitted prompt parameters are an object")
            .insert("name".to_owned(), serde_json::json!("sanitized-prompt"));
        let prompt_sidecar_error = block_on(
            router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
                &request_ctx,
                &sanitized_prompt_retry,
                prompt_retry_raw.as_deref(),
                &cancellation,
            ),
        )
        .expect_err("a prompt raw sidecar cannot survive a one-field sanitized typed mismatch");
        assert_eq!(prompt_sidecar_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            prompt_calls.load(Ordering::SeqCst),
            1,
            "the mismatched prompt sidecar is rejected before handler invocation"
        );
        assert_eq!(
            router.mrtr_exchanges.active_len(),
            3,
            "the rejected prompt sidecar leaves its continuation available"
        );
        block_on(router.dispatch_stateless_with_continuation_cancellation_and_raw_params(
            &request_ctx,
            &prompt_retry,
            prompt_retry_raw.as_deref(),
            &cancellation,
        ))
        .expect("HTTP-admitted ordered prompt retry reaches the handler");

        assert_eq!(tool_calls.load(Ordering::SeqCst), 2);
        assert_eq!(resource_calls.load(Ordering::SeqCst), 2);
        assert_eq!(prompt_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stateless_final_dispatch_keeps_complete_only_handlers_available_for_all_families() {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("complete-only-tool"))
            .expect("complete-only tool registers");
        router.add_resource(NamedResource::new("file:///complete-only-resource"));
        router.add_prompt(NamedPrompt::new("complete-only-prompt"));

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 1430, Budget::INFINITE, &state);
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let tool = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request("complete-only-tool", serde_json::json!({}), 1430_i64),
        ))
        .expect("a complete-only tool remains available without a session partition");
        assert_eq!(tool["resultType"], "complete");

        let resource = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": metadata.clone(),
                    "uri": "file:///complete-only-resource",
                })),
                1431_i64,
            ),
        ))
        .expect("a complete-only resource remains available without a session partition");
        assert_eq!(resource["resultType"], "complete");

        let prompt = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "prompts/get",
                Some(serde_json::json!({
                    "_meta": metadata,
                    "name": "complete-only-prompt",
                })),
                1432_i64,
            ),
        ))
        .expect("a complete-only prompt remains available without a session partition");
        assert_eq!(prompt["resultType"], "complete");
    }

    #[test]
    fn stateless_mrtr_handlers_are_rejected_before_all_family_invocations() {
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let resource_calls = Arc::new(AtomicUsize::new(0));
        let prompt_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::clone(&tool_calls),
            })
            .expect("MRTR tool registers");
        router.add_resource(InputRequiredResource {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&resource_calls),
        });
        router.add_prompt(InputRequiredPrompt {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&prompt_calls),
        });

        let cx = Cx::for_testing();
        let state = SessionState::ephemeral();
        let request_ctx = request_context(&cx, 1433, Budget::INFINITE, &state);
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let tool_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_call_request("input-required-tool", serde_json::json!({}), 1433_i64),
        ))
        .expect_err("a stateless MRTR tool is rejected before its handler runs");
        assert_eq!(tool_error.code, McpErrorCode::InvalidParams);

        let resource_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": metadata.clone(),
                    "uri": "file:///input-required-resource",
                })),
                1434_i64,
            ),
        ))
        .expect_err("a stateless MRTR resource is rejected before its handler runs");
        assert_eq!(resource_error.code, McpErrorCode::InvalidParams);

        let prompt_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "prompts/get",
                Some(serde_json::json!({
                    "_meta": metadata,
                    "name": "input-required-prompt",
                })),
                1435_i64,
            ),
        ))
        .expect_err("a stateless MRTR prompt is rejected before its handler runs");
        assert_eq!(prompt_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resource_calls.load(Ordering::SeqCst), 0);
        assert_eq!(prompt_calls.load(Ordering::SeqCst), 0);
        assert_eq!(router.mrtr_exchanges.active_len(), 0);
    }

    #[test]
    fn bound_owned_mrtr_retry_rejects_foreign_and_stale_state_without_mutation_then_resumes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut unshared_router = Router::new();
        unshared_router
            .add_tool(OneShotMrtrTool {
                calls: Arc::clone(&calls),
            })
            .expect("one-shot MRTR tool registers");
        let router = Arc::new(unshared_router);
        let cx = Cx::for_testing();
        let origin_connection = ModernConnection::new();
        let origin_inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            1436,
            InboundRequestTransport::Memory,
            &origin_connection,
        );
        let origin_ctx = origin_inbound.request_context();
        let origin_cancellation = origin_inbound
            .mrtr_continuation_cancellation()
            .expect("a bound origin supplies continuation ownership");
        let initial =
            final_tools_call_request("one-shot-mrtr-tool", serde_json::json!({}), 1436_i64);
        let issued = block_on(
            Arc::clone(&router).dispatch_stateless_owned_with_continuation_cancellation(
                origin_ctx,
                initial,
                origin_cancellation.clone(),
            ),
        )
        .expect("the bound owned path issues one continuation");
        let request_state = issued["requestState"]
            .as_str()
            .expect("issued continuation has opaque state")
            .to_owned();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(router.mrtr_exchanges.active_len(), 1);

        let retry = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "one-shot-mrtr-tool",
                "arguments": {},
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": request_state,
            })),
            1437_i64,
        );

        let mismatched_sidecar = serde_json::to_string(
            retry
                .params
                .as_ref()
                .expect("retry has materialized parameters"),
        )
        .expect("retry parameters serialize")
        .replacen("one-shot-mrtr-tool", "other-tool", 1);
        let sidecar_error = block_on(
            Arc::clone(&router)
                .dispatch_stateless_owned_with_continuation_cancellation_and_raw_params(
                    origin_inbound.request_context(),
                    retry.clone(),
                    Some(Arc::<str>::from(mismatched_sidecar)),
                    origin_cancellation.clone(),
                ),
        )
        .expect_err("a mismatched raw sidecar cannot reach continuation admission");
        assert_eq!(sidecar_error.code, McpErrorCode::InvalidParams);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(router.mrtr_exchanges.active_len(), 1);

        let foreign_connection = ModernConnection::new();
        let foreign_inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            1437,
            InboundRequestTransport::Memory,
            &foreign_connection,
        );
        let foreign_error = block_on(
            Arc::clone(&router).dispatch_stateless_owned_with_continuation_cancellation(
                foreign_inbound.request_context(),
                retry.clone(),
                foreign_inbound
                    .mrtr_continuation_cancellation()
                    .expect("a foreign connection still supplies its own cancellation"),
            ),
        )
        .expect_err("a foreign connection cannot consume origin continuation state");
        assert_eq!(foreign_error.code, McpErrorCode::InvalidParams);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(router.mrtr_exchanges.active_len(), 1);

        let mut stale_retry = retry.clone();
        stale_retry
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("retry parameters are an object")
            .insert(
                "requestState".to_owned(),
                serde_json::json!("stale-request-state"),
            );
        let stale_error = block_on(
            Arc::clone(&router).dispatch_stateless_owned_with_continuation_cancellation(
                origin_inbound.request_context(),
                stale_retry,
                origin_cancellation.clone(),
            ),
        )
        .expect_err("a stale state cannot consume the live continuation");
        assert_eq!(stale_error.code, McpErrorCode::InvalidParams);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(router.mrtr_exchanges.active_len(), 1);

        let completed = block_on(
            Arc::clone(&router).dispatch_stateless_owned_with_continuation_cancellation(
                origin_inbound.request_context(),
                retry,
                origin_cancellation,
            ),
        )
        .expect("the unchanged origin retry resumes exactly one continuation");
        assert_eq!(completed["resultType"], "complete");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(router.mrtr_exchanges.active_len(), 0);
    }

    #[test]
    fn modern_mrtr_state_only_retry_round_trips_only_when_input_responses_are_absent() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let resumed_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(StateOnlyInputRequiredTool {
                initial_calls: Arc::clone(&initial_calls),
                resumed_calls: Arc::clone(&resumed_calls),
            })
            .expect("state-only tool registration succeeds");

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            144,
            InboundRequestTransport::Memory,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let initial = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "state-only-input-required-tool",
                "arguments": {},
            })),
            144_i64,
        );
        let input_required = block_on(router.dispatch_stateless(&request_ctx, &initial))
            .expect("state-only handler outcome is framework-bound");
        assert_eq!(input_required["resultType"], "input_required");
        assert!(
            input_required.get("inputRequests").is_none(),
            "the framework preserves state-only input_required without an empty request map"
        );
        let request_state = input_required["requestState"]
            .as_str()
            .expect("framework output includes opaque state")
            .to_owned();

        let retry = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata,
                "name": "state-only-input-required-tool",
                "arguments": {},
                "requestState": request_state,
            })),
            145_i64,
        );
        assert!(
            retry
                .params
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .is_some_and(|params| !params.contains_key("inputResponses")),
            "the admitted retry keeps inputResponses absent"
        );
        let mut explicit_empty = retry.clone();
        explicit_empty
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("retry parameters are an object")
            .insert("inputResponses".to_owned(), serde_json::json!({}));
        let explicit_empty_raw_params = serde_json::to_string(
            explicit_empty
                .params
                .as_ref()
                .expect("explicit-empty retry has parameters"),
        )
        .expect("explicit-empty retry parameters serialize");
        let empty_error = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &explicit_empty,
            Some(&explicit_empty_raw_params),
        ))
        .expect_err("an explicit empty inputResponses map cannot impersonate an absent member");
        assert_eq!(empty_error.code, McpErrorCode::InvalidParams);
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_calls.load(Ordering::SeqCst), 0);

        let retry_raw_params = serde_json::to_string(
            retry
                .params
                .as_ref()
                .expect("absent-member retry has parameters"),
        )
        .expect("absent-member retry parameters serialize");
        let completed = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &retry,
            Some(&retry_raw_params),
        ))
        .expect("the unchanged absent-member retry resumes exactly once");
        assert_eq!(completed["resultType"], "complete");
        assert_eq!(completed["content"][0]["text"], "state-only resumed");
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn modern_mrtr_retries_resume_each_method_once_and_refuse_replay_or_kind_mismatch() {
        let tool_final_calls = Arc::new(AtomicUsize::new(0));
        let resource_final_calls = Arc::new(AtomicUsize::new(0));
        let prompt_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::clone(&tool_final_calls),
            })
            .expect("tool registration succeeds");
        router.add_resource(InputRequiredResource {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&resource_final_calls),
        });
        router.add_prompt(InputRequiredPrompt {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&prompt_final_calls),
        });

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            145,
            InboundRequestTransport::Memory,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let tool_initial = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-tool",
                "arguments": {},
            })),
            145_i64,
        );
        let tool_initial_result = block_on(router.dispatch_stateless(&request_ctx, &tool_initial))
            .expect("normal final dispatch mints the tool request state");
        let tool_state = tool_initial_result["requestState"]
            .as_str()
            .expect("framework result carries opaque tool state")
            .to_owned();
        let tool_retry = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-tool",
                "arguments": {},
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": tool_state,
            })),
            145_i64,
        );
        let raw_params_for = |request: &JsonRpcRequest| {
            serde_json::to_string(request.params.as_ref().expect("retry has parameters"))
                .expect("retry parameters serialize")
        };
        let mut kind_mismatch = tool_retry.clone();
        kind_mismatch
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("tool retry contains an inputResponses object")
            .insert(
                "roots".to_owned(),
                serde_json::to_value(
                    MrtrInputResponse::sampling(
                        serde_json::from_value(serde_json::json!({
                            "content": {"type": "text", "text": "not roots"},
                            "role": "assistant",
                            "model": "test-model",
                        }))
                        .expect("final sampling response must decode"),
                    )
                    .expect("sampling response serializes"),
                )
                .expect("sampling response converts to a wire value"),
            );
        let kind_mismatch_raw_params = raw_params_for(&kind_mismatch);
        let kind_error = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &kind_mismatch,
            Some(&kind_mismatch_raw_params),
        ))
        .expect_err("changing only the response kind is refused before handler invocation");
        assert_eq!(kind_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            tool_final_calls.load(Ordering::SeqCst),
            1,
            "a wrong-kind retry must leave the matching state unconsumed"
        );

        let missing_responses = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-tool",
                "arguments": {},
                "inputResponses": {},
                "requestState": tool_state,
            })),
            145_i64,
        );
        let missing_raw_params = raw_params_for(&missing_responses);
        let missing_error = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &missing_responses,
            Some(&missing_raw_params),
        ))
        .expect_err("a missing roots response cannot consume a tool continuation");
        assert_eq!(missing_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let roots_wire = serde_json::to_string(&router_roots_response_wire())
            .expect("roots response serializes for duplicate-key raw ingress");
        let duplicate_raw_params = format!(
            r#"{{"_meta":{},"name":"input-required-tool","arguments":{{}},"inputResponses":{{"roots":{roots_wire},"roots":{roots_wire}}},"requestState":{}}}"#,
            serde_json::to_string(&metadata).expect("metadata serializes"),
            serde_json::to_string(&tool_state).expect("opaque state serializes"),
        );
        let duplicate_retry = JsonRpcRequest::new(
            "tools/call",
            Some(
                serde_json::from_str(&duplicate_raw_params)
                    .expect("duplicate raw parameters materialize for source equality"),
            ),
            145_i64,
        );
        let duplicate_error = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &duplicate_retry,
            Some(&duplicate_raw_params),
        ))
        .expect_err("duplicate raw response keys cannot collapse before MRTR admission");
        assert_eq!(duplicate_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let mut unknown_only = tool_retry.clone();
        *unknown_only
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .expect("tool retry contains inputResponses") = serde_json::json!({"inert": null});
        let unknown_only_raw_params = raw_params_for(&unknown_only);
        let unknown_error = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &unknown_only,
            Some(&unknown_only_raw_params),
        ))
        .expect_err("unknown-only inputResponses cannot consume a tool continuation");
        assert_eq!(unknown_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let mut oversized_map = tool_retry.clone();
        let responses = oversized_map
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("tool retry contains an inputResponses object");
        for index in 0..crate::bidirectional::DEFAULT_MAX_MRTR_INPUT_REQUESTS_PER_ROUND {
            responses.insert(format!("inert-{index}"), serde_json::Value::Null);
        }
        let oversized_error = block_on(router.dispatch_stateless(&request_ctx, &oversized_map))
            .expect_err("an oversized raw response map is refused before retry decoding");
        assert_eq!(oversized_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let mut tool_oversized_bytes = tool_retry.clone();
        tool_oversized_bytes
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("tool retry contains an inputResponses object")
            .insert(
                "roots".to_owned(),
                serde_json::Value::String("x".repeat(MAX_MRTR_RAW_INPUT_RESPONSES_BYTES + 1)),
            );
        let tool_bytes_error =
            block_on(router.dispatch_stateless(&request_ctx, &tool_oversized_bytes))
                .expect_err(
                    "only an oversized tool response value is refused before retry decoding",
                );
        assert_eq!(tool_bytes_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let mut target_mismatch = tool_retry.clone();
        target_mismatch
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("tool retry parameters are an object")
            .insert("name".to_owned(), serde_json::json!("other-tool"));
        let target_error = block_on(router.dispatch_stateless(&request_ctx, &target_mismatch))
            .expect_err("changing only the target cannot consume a tool state");
        assert_eq!(target_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let other_connection = ModernConnection::new();
        let other_inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            145,
            InboundRequestTransport::Memory,
            &other_connection,
        );
        let other_session_ctx = other_inbound.request_context();
        let session_error = block_on(router.dispatch_stateless(&other_session_ctx, &tool_retry))
            .expect_err("changing only the session cannot consume a tool state");
        assert_eq!(session_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let principal_ctx = inbound.request_context();
        assert!(principal_ctx.set_auth(fastmcp_core::AuthContext::with_subject("other-user")));
        let principal_error = block_on(router.dispatch_stateless(&principal_ctx, &tool_retry))
            .expect_err("changing only the principal cannot consume a tool state");
        assert_eq!(principal_error.code, McpErrorCode::InvalidParams);
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);

        let tool_retry_raw_params = raw_params_for(&tool_retry);
        let tool_response = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &tool_retry,
            Some(&tool_retry_raw_params),
        ))
        .expect("a framework-minted tool state resumes through the final handler");
        assert_eq!(tool_response["resultType"], "input_required");
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 2);

        let resource_initial = JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "uri": "file:///input-required-resource",
            })),
            146_i64,
        );
        let resource_initial_result =
            block_on(router.dispatch_stateless(&request_ctx, &resource_initial))
                .expect("normal final dispatch mints the resource request state");
        let resource_state = resource_initial_result["requestState"]
            .as_str()
            .expect("framework result carries opaque resource state")
            .to_owned();
        let resource_retry = JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "uri": "file:///input-required-resource",
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": resource_state,
            })),
            146_i64,
        );
        let mut resource_unknown_only = resource_retry.clone();
        *resource_unknown_only
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .expect("resource retry contains inputResponses") = serde_json::json!({"inert": null});
        let resource_unknown_error =
            block_on(router.dispatch_stateless(&request_ctx, &resource_unknown_only))
                .expect_err("unknown-only inputResponses cannot consume a resource continuation");
        assert_eq!(resource_unknown_error.code, McpErrorCode::InvalidParams);
        assert_eq!(resource_final_calls.load(Ordering::SeqCst), 1);

        let mut resource_nested_value = serde_json::Value::Null;
        for _ in 0..=MAX_MRTR_RAW_JSON_DEPTH {
            resource_nested_value = serde_json::Value::Array(vec![resource_nested_value]);
        }
        let mut resource_oversized_depth = resource_retry.clone();
        resource_oversized_depth
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("resource retry contains an inputResponses object")
            .insert("roots".to_owned(), resource_nested_value);
        let resource_depth_error =
            block_on(router.dispatch_stateless(&request_ctx, &resource_oversized_depth))
                .expect_err("only excessive response nesting is refused before retry decoding");
        assert_eq!(resource_depth_error.code, McpErrorCode::InvalidParams);
        assert_eq!(resource_final_calls.load(Ordering::SeqCst), 1);

        let resource_retry_raw_params = raw_params_for(&resource_retry);
        let resource_response = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &resource_retry,
            Some(&resource_retry_raw_params),
        ))
        .expect("a framework-minted resource state resumes through the final handler");
        assert_eq!(resource_response["resultType"], "input_required");
        assert_eq!(resource_final_calls.load(Ordering::SeqCst), 2);

        let prompt_initial = JsonRpcRequest::new(
            "prompts/get",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-prompt",
            })),
            147_i64,
        );
        let prompt_initial_result =
            block_on(router.dispatch_stateless(&request_ctx, &prompt_initial))
                .expect("normal final dispatch mints the prompt request state");
        let prompt_state = prompt_initial_result["requestState"]
            .as_str()
            .expect("framework result carries opaque prompt state")
            .to_owned();
        let prompt_retry = JsonRpcRequest::new(
            "prompts/get",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-prompt",
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": prompt_state,
            })),
            147_i64,
        );
        let mut prompt_unknown_only = prompt_retry.clone();
        *prompt_unknown_only
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .expect("prompt retry contains inputResponses") = serde_json::json!({"inert": null});
        let prompt_unknown_error =
            block_on(router.dispatch_stateless(&request_ctx, &prompt_unknown_only))
                .expect_err("unknown-only inputResponses cannot consume a prompt continuation");
        assert_eq!(prompt_unknown_error.code, McpErrorCode::InvalidParams);
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 1);

        let mut prompt_oversized_values = prompt_retry.clone();
        prompt_oversized_values
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("inputResponses"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("prompt retry contains an inputResponses object")
            .insert(
                "roots".to_owned(),
                serde_json::Value::Array(vec![serde_json::Value::Null; MAX_MRTR_RAW_JSON_VALUES]),
            );
        let prompt_values_error =
            block_on(router.dispatch_stateless(&request_ctx, &prompt_oversized_values))
                .expect_err(
                    "only an oversized prompt response value set is refused before retry decoding",
                );
        assert_eq!(prompt_values_error.code, McpErrorCode::InvalidParams);
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 1);

        let prompt_retry_raw_params = raw_params_for(&prompt_retry);
        let prompt_response = block_on(router.dispatch_stateless_with_raw_params(
            &request_ctx,
            &prompt_retry,
            Some(&prompt_retry_raw_params),
        ))
        .expect("a framework-minted prompt state resumes through the final handler");
        assert_eq!(prompt_response["resultType"], "input_required");
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 2);

        // Each retry receives the prior round's typed roots response through
        // the handler resume hook, then emits a distinct continuation for the
        // next JSON-RPC round. This exercises the public final dispatch path
        // across tools, resources, and prompts rather than only the registry.
        let mut tool_second_retry = tool_retry.clone();
        tool_second_retry
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("tool retry parameters are an object")
            .insert(
                "requestState".to_owned(),
                tool_response["requestState"].clone(),
            );
        let tool_second_response =
            block_on(router.dispatch_stateless(&request_ctx, &tool_second_retry))
                .expect("a second public tools/call round reaches the resumed handler");
        assert_eq!(tool_second_response["resultType"], "input_required");

        let mut resource_second_retry = resource_retry.clone();
        resource_second_retry
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("resource retry parameters are an object")
            .insert(
                "requestState".to_owned(),
                resource_response["requestState"].clone(),
            );
        let resource_second_response =
            block_on(router.dispatch_stateless(&request_ctx, &resource_second_retry))
                .expect("a second public resources/read round reaches the resumed handler");
        assert_eq!(resource_second_response["resultType"], "input_required");

        let mut prompt_second_retry = prompt_retry.clone();
        prompt_second_retry
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("prompt retry parameters are an object")
            .insert(
                "requestState".to_owned(),
                prompt_response["requestState"].clone(),
            );
        let prompt_second_response =
            block_on(router.dispatch_stateless(&request_ctx, &prompt_second_retry))
                .expect("a second public prompts/get round reaches the resumed handler");
        assert_eq!(prompt_second_response["resultType"], "input_required");
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 3);
        assert_eq!(resource_final_calls.load(Ordering::SeqCst), 3);
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 3);

        let replay = block_on(router.dispatch_stateless(&request_ctx, &tool_retry))
            .expect_err("replaying only the already consumed tool state is refused");
        assert_eq!(replay.code, McpErrorCode::InvalidParams);
        assert_eq!(
            tool_final_calls.load(Ordering::SeqCst),
            3,
            "replay must fail before the tool handler is invoked again"
        );
    }

    #[test]
    fn modern_connection_disablements_block_final_handlers_before_mrtr_state_is_minted() {
        let tool_final_calls = Arc::new(AtomicUsize::new(0));
        let resource_final_calls = Arc::new(AtomicUsize::new(0));
        let prompt_final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::clone(&tool_final_calls),
            })
            .expect("tool registration succeeds");
        router.add_resource(InputRequiredResource {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&resource_final_calls),
        });
        router.add_prompt(InputRequiredPrompt {
            legacy_calls: Arc::new(AtomicUsize::new(0)),
            final_calls: Arc::clone(&prompt_final_calls),
        });

        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let tool_request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-tool",
                "arguments": {},
            })),
            611_i64,
        );
        let resource_request = JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "uri": "file:///input-required-resource",
            })),
            612_i64,
        );
        let prompt_request = JsonRpcRequest::new(
            "prompts/get",
            Some(serde_json::json!({
                "_meta": metadata,
                "name": "input-required-prompt",
            })),
            613_i64,
        );
        let request_bytes = [
            serde_json::to_vec(&tool_request).expect("tool request serializes"),
            serde_json::to_vec(&resource_request).expect("resource request serializes"),
            serde_json::to_vec(&prompt_request).expect("prompt request serializes"),
        ];

        let cx = Cx::for_testing();
        let allowed_connection = ModernConnection::new();
        let allowed_inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            611,
            InboundRequestTransport::Stdio,
            &allowed_connection,
        );
        let allowed_ctx = allowed_inbound.request_context();
        let allowed_cancellation = allowed_inbound
            .mrtr_continuation_cancellation()
            .expect("modern connection supplies continuation ownership");

        for request in [&tool_request, &resource_request, &prompt_request] {
            let result = block_on(router.dispatch_stateless_with_continuation_cancellation(
                &allowed_ctx,
                request,
                &allowed_cancellation,
            ))
            .expect("enabled final component reaches its handler");
            assert_eq!(result["resultType"], "input_required");
            assert!(
                result["requestState"].is_string(),
                "an enabled MRTR-capable handler mints framework-owned state"
            );
        }
        assert_eq!(tool_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resource_final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(prompt_final_calls.load(Ordering::SeqCst), 1);

        let denied_connection = ModernConnection::new();
        let denied_inbound = InboundRequestContext::with_modern_connection(
            cx,
            611,
            InboundRequestTransport::Stdio,
            &denied_connection,
        );
        let denied_ctx = denied_inbound.request_context();
        let denied_cancellation = denied_inbound
            .mrtr_continuation_cancellation()
            .expect("modern connection supplies continuation ownership");
        assert!(denied_ctx.disable_tool("input-required-tool"));
        assert!(denied_ctx.disable_resource("file:///input-required-resource"));
        assert!(denied_ctx.disable_prompt("input-required-prompt"));

        assert_eq!(
            serde_json::to_vec(&tool_request).expect("tool request remains serializable"),
            request_bytes[0],
            "connection state is the sole tool-request admission difference"
        );
        let tool_error = block_on(router.dispatch_stateless_with_continuation_cancellation(
            &denied_ctx,
            &tool_request,
            &denied_cancellation,
        ))
        .expect_err("a disabled tool cannot mint final MRTR state");
        assert_eq!(tool_error.code, McpErrorCode::MethodNotFound);

        assert_eq!(
            serde_json::to_vec(&resource_request).expect("resource request remains serializable"),
            request_bytes[1],
            "connection state is the sole resource-request admission difference"
        );
        let resource_error = block_on(router.dispatch_stateless_with_continuation_cancellation(
            &denied_ctx,
            &resource_request,
            &denied_cancellation,
        ))
        .expect_err("a disabled resource cannot mint final MRTR state");
        assert_eq!(resource_error.code, McpErrorCode::ResourceNotFound);

        assert_eq!(
            serde_json::to_vec(&prompt_request).expect("prompt request remains serializable"),
            request_bytes[2],
            "connection state is the sole prompt-request admission difference"
        );
        let prompt_error = block_on(router.dispatch_stateless_with_continuation_cancellation(
            &denied_ctx,
            &prompt_request,
            &denied_cancellation,
        ))
        .expect_err("a disabled prompt cannot mint final MRTR state");
        assert_eq!(prompt_error.code, McpErrorCode::PromptNotFound);

        assert_eq!(
            tool_final_calls.load(Ordering::SeqCst),
            1,
            "refused tool dispatch must not invoke the MRTR-capable handler"
        );
        assert_eq!(
            resource_final_calls.load(Ordering::SeqCst),
            1,
            "refused resource dispatch must not invoke the MRTR-capable handler"
        );
        assert_eq!(
            prompt_final_calls.load(Ordering::SeqCst),
            1,
            "refused prompt dispatch must not invoke the MRTR-capable handler"
        );
    }

    #[test]
    fn modern_final_catalogs_ignore_connection_disablements_while_exact_legacy_lists_remain_stateful()
     {
        let template_uri = "file:///input-required-template/{id}";
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("connection-scoped-tool"))
            .expect("tool registration succeeds");
        router.add_resource(NamedResource::new("file:///connection-scoped-resource"));
        router
            .add_resource_template_with_behavior(
                ResourceTemplate {
                    uri_template: template_uri.to_owned(),
                    name: "connection-scoped-template".to_owned(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: Vec::new(),
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("resource-template registration succeeds");
        router.add_prompt(NamedPrompt::new("connection-scoped-prompt"));

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            614,
            InboundRequestTransport::Stdio,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let list_request = |method, id| {
            JsonRpcRequest::new(
                method,
                Some(serde_json::json!({"_meta": metadata.clone()})),
                id,
            )
        };

        let tools =
            block_on(router.dispatch_stateless(&request_ctx, &list_request("tools/list", 614_i64)))
                .expect("enabled tool is discovered");
        let resources = block_on(router.dispatch_stateless(
            &request_ctx,
            &list_request("resources/list", 615_i64),
        ))
        .expect("enabled resource is discovered");
        let templates = block_on(router.dispatch_stateless(
            &request_ctx,
            &list_request("resources/templates/list", 616_i64),
        ))
        .expect("enabled resource template is discovered");
        let prompts = block_on(router.dispatch_stateless(
            &request_ctx,
            &list_request("prompts/list", 617_i64),
        ))
        .expect("enabled prompt is discovered");
        assert_eq!(tools["tools"][0]["name"], "connection-scoped-tool");
        assert_eq!(
            resources["resources"][0]["uri"],
            "file:///connection-scoped-resource"
        );
        assert_eq!(
            templates["resourceTemplates"][0]["uriTemplate"],
            template_uri
        );
        assert_eq!(prompts["prompts"][0]["name"], "connection-scoped-prompt");

        assert!(request_ctx.disable_tool("connection-scoped-tool"));
        assert!(request_ctx.disable_resource("file:///connection-scoped-resource"));
        assert!(request_ctx.disable_resource(template_uri));
        assert!(request_ctx.disable_prompt("connection-scoped-prompt"));

        let discovered_tools =
            block_on(router.dispatch_stateless(&request_ctx, &list_request("tools/list", 618_i64)))
                .expect("disabled tool remains in the immutable final catalog");
        let discovered_resources = block_on(router.dispatch_stateless(
            &request_ctx,
            &list_request("resources/list", 619_i64),
        ))
        .expect("disabled resource remains in the immutable final catalog");
        let discovered_templates = block_on(router.dispatch_stateless(
            &request_ctx,
            &list_request("resources/templates/list", 620_i64),
        ))
        .expect("disabled template remains in the immutable final catalog");
        let discovered_prompts = block_on(router.dispatch_stateless(
            &request_ctx,
            &list_request("prompts/list", 621_i64),
        ))
        .expect("disabled prompt remains in the immutable final catalog");
        assert_eq!(
            discovered_tools["tools"][0]["name"],
            "connection-scoped-tool"
        );
        assert_eq!(
            discovered_resources["resources"][0]["uri"],
            "file:///connection-scoped-resource"
        );
        assert_eq!(
            discovered_templates["resourceTemplates"][0]["uriTemplate"],
            template_uri
        );
        assert_eq!(
            discovered_prompts["prompts"][0]["name"],
            "connection-scoped-prompt"
        );

        let legacy_state = SessionState::new();
        let legacy_request_ctx =
            request_context(request_ctx.cx(), 622, Budget::INFINITE, &legacy_state);
        assert!(legacy_request_ctx.disable_tool("connection-scoped-tool"));
        assert!(legacy_request_ctx.disable_resource("file:///connection-scoped-resource"));
        assert!(legacy_request_ctx.disable_resource(template_uri));
        assert!(legacy_request_ctx.disable_prompt("connection-scoped-prompt"));

        let legacy_tools = router
            .handle_tools_list(
                &legacy_request_ctx,
                ListToolsParams {
                    cursor: None,
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&legacy_state),
            )
            .expect("exact legacy tools/list remains session-stateful");
        let legacy_resources = router
            .handle_resources_list(
                &legacy_request_ctx,
                ListResourcesParams {
                    cursor: None,
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&legacy_state),
            )
            .expect("exact legacy resources/list remains session-stateful");
        let legacy_templates = router
            .handle_resource_templates_list(
                &legacy_request_ctx,
                ListResourceTemplatesParams {
                    cursor: None,
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&legacy_state),
            )
            .expect("exact legacy resources/templates/list remains session-stateful");
        let legacy_prompts = router
            .handle_prompts_list(
                &legacy_request_ctx,
                ListPromptsParams {
                    cursor: None,
                    include_tags: None,
                    exclude_tags: None,
                },
                Some(&legacy_state),
            )
            .expect("exact legacy prompts/list remains session-stateful");
        assert!(legacy_tools.tools.is_empty());
        assert_eq!(legacy_resources.resources.len(), 0);
        assert!(legacy_templates.resource_templates.is_empty());
        assert!(legacy_prompts.prompts.is_empty());

        let later_inbound = InboundRequestContext::with_modern_connection(
            request_ctx.cx().clone(),
            630,
            InboundRequestTransport::Stdio,
            &connection,
        );
        let later_ctx = later_inbound.request_context();
        let read_error = block_on(router.dispatch_stateless(
            &later_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": metadata,
                    "uri": "file:///connection-scoped-resource",
                })),
                631_i64,
            ),
        ))
        .expect_err(
            "a later inbound on the same modern connection must refuse the disabled resource",
        );
        assert_eq!(read_error.code, McpErrorCode::ResourceNotFound);
        assert!(
            read_error.message.contains("disabled"),
            "the refused resource read must keep the session-disabled message: {read_error:?}"
        );
        let prompt_error = block_on(router.dispatch_stateless(
            &later_ctx,
            &JsonRpcRequest::new(
                "prompts/get",
                Some(serde_json::json!({
                    "_meta": metadata,
                    "name": "connection-scoped-prompt",
                })),
                632_i64,
            ),
        ))
        .expect_err(
            "a later inbound on the same modern connection must refuse the disabled prompt",
        );
        assert_eq!(prompt_error.code, McpErrorCode::PromptNotFound);
        assert!(
            prompt_error.message.contains("disabled"),
            "the refused prompt get must keep the session-disabled message: {prompt_error:?}"
        );
        let tool_error = block_on(router.dispatch_stateless(
            &later_ctx,
            &JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "_meta": metadata,
                    "name": "connection-scoped-tool",
                    "arguments": {},
                })),
                633_i64,
            ),
        ))
        .expect_err("a later inbound on the same modern connection must refuse the disabled tool");
        assert_eq!(tool_error.code, McpErrorCode::MethodNotFound);
        assert!(
            tool_error.message.contains("disabled"),
            "the refused tool call must keep the session-disabled message: {tool_error:?}"
        );
    }

    #[test]
    fn final_catalog_cursor_survives_a_changed_session_state() {
        let mut router = Router::new();
        router.set_list_page_size(Some(1));
        router
            .add_tool(NamedTool::new("first-connection-cursor-tool"))
            .expect("first tool registration succeeds");
        router
            .add_tool(NamedTool::new("second-connection-cursor-tool"))
            .expect("second tool registration succeeds");

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 622, Budget::INFINITE, &state);
        let initial = final_tools_list_request(None, None, None, 622_i64);
        let initial_bytes = serde_json::to_vec(&initial).expect("initial request serializes");
        let first_page = block_on(router.dispatch_stateless(&request_ctx, &initial))
            .expect("the initial final catalog page is admitted");
        let cursor = first_page["nextCursor"]
            .as_str()
            .expect("the first page has a continuation")
            .to_owned();

        assert!(request_ctx.disable_tool("first-connection-cursor-tool"));
        assert_eq!(
            serde_json::to_vec(&initial).expect("initial request remains serializable"),
            initial_bytes,
            "connection state cannot alter the request or its catalog continuation"
        );
        let continued = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(Some(&cursor), None, None, 623_i64),
        ))
        .expect("a disabled-component change cannot invalidate an immutable final cursor");
        assert_eq!(
            continued["tools"][0]["name"],
            "second-connection-cursor-tool"
        );
        assert!(continued.get("nextCursor").is_none());

        let refreshed = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_tools_list_request(None, None, None, 624_i64),
        ))
        .expect("a cursor-free request keeps the immutable final catalog");
        assert_eq!(
            refreshed["tools"][0]["name"],
            "first-connection-cursor-tool"
        );
    }

    #[test]
    fn final_catalog_cursor_continues_across_modern_connection_partitions() {
        let mut router = Router::new();
        router.set_list_page_size(Some(1));
        router
            .add_tool(NamedTool::new("first-cross-connection-cursor-tool"))
            .expect("first tool registration succeeds");
        router
            .add_tool(NamedTool::new("second-cross-connection-cursor-tool"))
            .expect("second tool registration succeeds");

        let cx = Cx::for_testing();
        let origin_connection = ModernConnection::new();
        let origin_inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            625,
            InboundRequestTransport::Stdio,
            &origin_connection,
        );
        let origin_context = origin_inbound.request_context();
        let continuation_connection = ModernConnection::new();
        let continuation_inbound = InboundRequestContext::with_modern_connection(
            cx,
            626,
            InboundRequestTransport::Stdio,
            &continuation_connection,
        );
        let continuation_context = continuation_inbound.request_context();
        let origin_partition = origin_context
            .session_cache_partition()
            .expect("a modern origin context has a durable partition")
            .0;
        let continuation_partition = continuation_context
            .session_cache_partition()
            .expect("a modern continuation context has a durable partition")
            .0;
        assert_ne!(
            origin_partition, continuation_partition,
            "distinct modern connections have distinct durable partitions"
        );

        let first_page = block_on(router.dispatch_stateless(
            &origin_context,
            &final_tools_list_request(None, None, None, 625_i64),
        ))
        .expect("the origin connection receives the first final catalog page");
        let cursor = first_page["nextCursor"]
            .as_str()
            .expect("the first page has a continuation")
            .to_owned();

        let continued = block_on(router.dispatch_stateless(
            &continuation_context,
            &final_tools_list_request(Some(&cursor), None, None, 626_i64),
        ))
        .expect("a final catalog cursor is not bound to its origin connection partition");
        assert_eq!(
            continued["tools"][0]["name"],
            "second-cross-connection-cursor-tool"
        );
        assert!(continued.get("nextCursor").is_none());
    }

    #[test]
    fn modern_connection_disconnect_is_the_only_changed_retry_dimension_and_cancels_state() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::new(AtomicUsize::new(0)),
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx.clone(),
            301,
            InboundRequestTransport::Stdio,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let continuation_cancellation = inbound
            .mrtr_continuation_cancellation()
            .expect("a modern connection supplies continuation ownership");
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let initial = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata.clone(),
                "name": "input-required-tool",
                "arguments": {},
            })),
            301_i64,
        );
        let initial_result = block_on(router.dispatch_stateless_with_continuation_cancellation(
            &request_ctx,
            &initial,
            &continuation_cancellation,
        ))
        .expect("the connected request mints a continuation");
        let retry = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata,
                "name": "input-required-tool",
                "arguments": {},
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": initial_result["requestState"].clone(),
            })),
            302_i64,
        );
        let retry_before_disconnect = serde_json::to_vec(&retry).expect("retry serializes");

        // The request, opaque state, durable partition, and response map all
        // remain unchanged. Peer disconnect is the sole changed dimension.
        connection.disconnect();
        let error = block_on(router.dispatch_stateless_with_continuation_cancellation(
            &request_ctx,
            &retry,
            &continuation_cancellation,
        ))
        .expect_err("disconnect must cancel the retained continuation");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(
            serde_json::to_vec(&retry).expect("retry serializes"),
            retry_before_disconnect,
            "disconnect cancellation cannot mutate client retry state"
        );
        assert_eq!(
            final_calls.load(Ordering::SeqCst),
            1,
            "a disconnected continuation cannot reach the resumed handler"
        );
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn task_capable_input_required_retry_does_not_require_tasks_capability() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(TaskCapableInputRequiredTool {
                final_calls: Arc::clone(&final_calls),
            })
            .expect("task-capable input-required tool registration succeeds");
        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            148,
            InboundRequestTransport::Memory,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let metadata_without_tasks = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let initial = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata_without_tasks.clone(),
                "name": "task-capable-input-required-tool",
                "arguments": {},
            })),
            148_i64,
        );
        let initial_result = block_on(router.dispatch_stateless(&request_ctx, &initial))
            .expect("a task-capable tool may return input_required without Tasks negotiation");
        let request_state = initial_result["requestState"]
            .as_str()
            .expect("framework result carries opaque task-capable state")
            .to_owned();
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let retry = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": metadata_without_tasks,
                "name": "task-capable-input-required-tool",
                "arguments": {},
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": request_state,
            })),
            149_i64,
        );
        let resumed = block_on(router.dispatch_stateless(&request_ctx, &retry))
            .expect("a task-capable input-required retry remains an ordinary MRTR operation");
        assert_eq!(resumed["resultType"], "input_required");
        assert_eq!(final_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn final_input_required_does_not_fallback_without_negotiation_metadata() {
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router
            .add_tool(InputRequiredTool {
                legacy_calls: Arc::clone(&legacy_calls),
                final_calls: Arc::clone(&final_calls),
            })
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let inbound = InboundRequestContext::with_modern_connection(
            cx,
            144,
            InboundRequestTransport::Memory,
            &connection,
        );
        let request_ctx = inbound.request_context();
        let baseline = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "input-required-tool",
                "arguments": {},
            })),
            144_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("tools/call parameters are an object")
            .remove("_meta");

        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            planted
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            "negotiation metadata is the sole planted dimension"
        );
        let catalog_before = serde_json::to_vec(&router.tools()).expect("catalog serializes");
        let planted_before = serde_json::to_vec(&planted).expect("request serializes");

        let accepted = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("negotiated final request is accepted");
        assert_eq!(accepted["resultType"], "input_required");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let error = block_on(router.dispatch_stateless(&request_ctx, &planted))
            .expect_err("one-field no-negotiation request is rejected instead of falling back");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&planted).expect("rejected request serializes"),
            planted_before,
            "rejection cannot mutate caller-owned no-negotiation parameters"
        );
        assert_eq!(
            serde_json::to_vec(&router.tools()).expect("catalog serializes"),
            catalog_before,
            "rejection cannot mutate the installed handler catalog"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
        let retried = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("the negotiated baseline remains accepted after rejection");
        assert_eq!(retried["resultType"], "input_required");
        assert_ne!(
            retried["requestState"], accepted["requestState"],
            "every framework-issued MRTR continuation has fresh opaque state"
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 2);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn final_prompts_get_dispatches_direct_request_owned_handler() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(DirectFinalPrompt {
            final_calls: Arc::clone(&final_calls),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 96, Budget::INFINITE, &state);
        let request = direct_final_prompt_request(96);
        let typed_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "prompts/get",
            request.params.as_ref(),
        )
        .expect("final prompts/get request decodes through the public core surface");

        let response = block_on(router.dispatch_stateless(&request_ctx, &request))
            .expect("final prompts/get reaches the direct final handler");

        assert_eq!(
            response.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(
            response["description"],
            serde_json::json!("direct final prompt description")
        );
        assert_eq!(response["messages"][0]["content"]["type"], "audio");
        assert_eq!(
            response["messages"][0]["content"]["_meta"]["com.example/direct-prompt"]["source"],
            "final-handler"
        );
        assert_eq!(
            response["messages"][0]["content"]["com.example/direct-field"],
            true
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let wire = serde_json::to_string(&response).expect("final prompt response serializes");
        let CoreResult::Final(FinalCoreResult::PromptsGet { result, .. }) = typed_request
            .decode_result(&wire)
            .expect("final prompts/get result decodes through the public core surface")
        else {
            panic!("prompts/get selects the exact final result");
        };
        assert_eq!(
            result.payload.description.as_deref(),
            Some("direct final prompt description")
        );
        assert!(matches!(
            result.payload.messages.as_slice(),
            [FinalPromptMessage {
                content: ContentBlock::Audio { data, mime_type, .. },
                ..
            }] if data == "aGVsbG8=" && mime_type == "audio/mpeg"
        ));
    }

    #[test]
    fn public_final_resource_uri_policy_admits_client_direct_https_and_rejects_only_policy_change_without_mutation()
     {
        let client_direct = HttpsCatalogResource {
            client_direct_https: true,
        };
        let server_mediated = HttpsCatalogResource {
            client_direct_https: false,
        };
        assert_eq!(
            client_direct.definition(),
            server_mediated.definition(),
            "the URI-use policy is the sole registration difference"
        );

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 971, Budget::INFINITE, &state);
        let final_metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let mut accepted_router = Router::new();
        accepted_router
            .add_resource_with_behavior(client_direct, crate::DuplicateBehavior::Replace)
            .expect("an explicitly client-direct HTTPS catalog resource is admitted");
        let accepted = block_on(accepted_router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/list",
                Some(serde_json::json!({"_meta": final_metadata.clone()})),
                971_i64,
            ),
        ))
        .expect("public final resource listing succeeds");
        assert_eq!(
            accepted["resources"][0]["uri"],
            "https://client.example.test/catalog.txt"
        );
        let direct_read = block_on(accepted_router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": final_metadata.clone(),
                    "uri": "https://client.example.test/catalog.txt",
                })),
                972_i64,
            ),
        ))
        .expect_err("client-direct HTTPS catalog entries are not MCP-read identities");
        assert_eq!(direct_read.code, McpErrorCode::InvalidParams);

        let mut rejected_router = Router::new();
        rejected_router.add_resource(NamedResource::new("mcp://uri-policy/unchanged"));
        let catalog_before =
            serde_json::to_vec(&rejected_router.resources()).expect("existing catalog serializes");
        let count_before = rejected_router.resources_count();
        let error = rejected_router
            .add_resource_with_behavior(server_mediated, crate::DuplicateBehavior::Replace)
            .expect_err("changing only to server-mediated policy rejects HTTPS registration");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            rejected_router.resources_count(),
            count_before,
            "rejected final admission cannot add or replace a resource handler"
        );
        assert_eq!(
            serde_json::to_vec(&rejected_router.resources()).expect("catalog remains serializable"),
            catalog_before,
            "rejected URI-use admission leaves the prior catalog unchanged"
        );
        let unchanged = block_on(rejected_router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/list",
                Some(serde_json::json!({"_meta": final_metadata})),
                972_i64,
            ),
        ))
        .expect("rejection leaves public final dispatch usable");
        assert_eq!(unchanged["resources"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            unchanged["resources"][0]["uri"],
            "mcp://uri-policy/unchanged"
        );
    }

    #[test]
    fn resource_uri_use_policy_leaves_exact_2024_resource_registration_unchanged() {
        let mut router = Router::new();
        router
            .add_legacy_resource_with_behavior(
                HttpsCatalogResource {
                    client_direct_https: false,
                },
                crate::DuplicateBehavior::Replace,
            )
            .expect("exact-2024 registration does not consult final URI-use admission");

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 976);
        let listed = router
            .handle_resources_list(&request_ctx, ListResourcesParams::default(), None)
            .expect("exact-2024 listing remains available");
        assert_eq!(listed.resources.len(), 1);
        assert_eq!(
            listed.resources[0].uri,
            "https://client.example.test/catalog.txt"
        );
    }

    #[test]
    fn public_final_uri_policy_rechecks_prompt_and_mrtr_resource_emissions() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let resumed_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(ClientDirectHttpsPrompt);
        router.add_resource(MrtrHttpsEmbeddedResource {
            initial_calls: Arc::clone(&initial_calls),
            resumed_calls: Arc::clone(&resumed_calls),
        });

        let cx = Cx::for_testing();
        let connection = ModernConnection::new();
        let metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let prompt = block_on(router.dispatch_stateless(
            &InboundRequestContext::with_modern_connection(
                cx.clone(),
                973,
                InboundRequestTransport::Memory,
                &connection,
            )
            .request_context(),
            &JsonRpcRequest::new(
                "prompts/get",
                Some(serde_json::json!({
                    "_meta": metadata.clone(),
                    "name": "client-direct-https-prompt",
                })),
                973_i64,
            ),
        ))
        .expect("a client-direct HTTPS resource link is emitted from the public prompt path");
        assert_eq!(
            prompt["messages"][0]["content"]["uri"],
            "https://client.example.test/prompt-link"
        );

        let initial = block_on(router.dispatch_stateless(
            &InboundRequestContext::with_modern_connection(
                cx.clone(),
                974,
                InboundRequestTransport::Memory,
                &connection,
            )
            .request_context(),
            &JsonRpcRequest::new(
                "resources/read",
                Some(serde_json::json!({
                    "_meta": metadata.clone(),
                    "uri": "mcp://uri-policy/mrtr",
                })),
                974_i64,
            ),
        ))
        .expect("initial public resource request mints MRTR state");
        assert_eq!(initial["resultType"], "input_required");
        let request_state = initial["requestState"]
            .as_str()
            .expect("framework minted MRTR state")
            .to_owned();
        let catalog_before = serde_json::to_vec(&router.resources())
            .expect("catalog serializes before the resumed rejection");
        let retry = JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": metadata,
                "uri": "mcp://uri-policy/mrtr",
                "inputResponses": {"roots": router_roots_response_wire()},
                "requestState": request_state,
            })),
            975_i64,
        );
        let error = block_on(router.dispatch_stateless(
            &InboundRequestContext::with_modern_connection(
                cx,
                975,
                InboundRequestTransport::Memory,
                &connection,
            )
            .request_context(),
            &retry,
        ))
        .expect_err(
            "an MRTR-resumed HTTPS embedded resource remains server-mediated and is refused",
        );
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(initial_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_vec(&router.resources()).expect("catalog remains serializable"),
            catalog_before,
            "rejected dynamic MRTR output cannot mutate registered resource state"
        );
    }

    #[test]
    fn final_prompt_arguments_are_validated_before_handler_and_legacy_is_unchanged() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(PromptArgumentBoundary {
            final_calls: Arc::clone(&final_calls),
            legacy_calls: Arc::clone(&legacy_calls),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 155, Budget::INFINITE, &state);
        let baseline = final_prompt_get_request(
            "prompt-argument-boundary",
            serde_json::json!({"topic": "release"}),
            155_i64,
        );
        let mut missing_required = baseline.clone();
        let removed_topic = missing_required
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("arguments"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("prompt arguments are an object")
            .remove("topic");
        assert_eq!(removed_topic, Some(serde_json::json!("release")));

        assert_eq!(baseline.method, missing_required.method);
        assert_eq!(baseline.id, missing_required.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            missing_required
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            "the required argument is the sole planted dimension"
        );
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            missing_required
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            "the required argument is the sole planted dimension"
        );

        let accepted = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("the complete required-argument request is accepted");
        assert_eq!(accepted["resultType"], "complete");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let missing_error = block_on(router.dispatch_stateless(&request_ctx, &missing_required))
            .expect_err("removing only a required prompt argument is rejected");
        assert_eq!(missing_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let unknown_error = block_on(router.dispatch_stateless(
            &request_ctx,
            &final_prompt_get_request("unknown-prompt", serde_json::json!({}), 156_i64),
        ))
        .expect_err("an unknown final prompt is an invalid-params protocol error");
        assert_eq!(unknown_error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let legacy = block_on(router.handle_prompts_get(
            &request_ctx,
            GetPromptParams {
                name: "prompt-argument-boundary".to_owned(),
                arguments: None,
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect("the exact legacy prompt path retains its existing argument behavior");
        let legacy_wire = serde_json::to_value(&legacy).expect("legacy prompt result serializes");
        assert_eq!(
            legacy_wire["messages"][0]["content"]["text"],
            "legacy prompt-argument-boundary result"
        );
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_prompt_argument_snapshot_is_immutable_after_registration() {
        let expose_admitted_argument = Arc::new(AtomicBool::new(true));
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(MutablePromptDefinition {
            expose_admitted_argument: Arc::clone(&expose_admitted_argument),
            final_calls: Arc::clone(&final_calls),
        });
        expose_admitted_argument.store(false, Ordering::SeqCst);

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 156, Budget::INFINITE, &state);
        let accepted = final_prompt_get_request(
            "mutable-prompt-definition",
            serde_json::json!({"topic": "release"}),
            156_i64,
        );
        let mut missing_required = accepted.clone();
        let removed = missing_required
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("arguments"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("prompt arguments are an object")
            .remove("topic");
        assert_eq!(removed, Some(serde_json::json!("release")));
        assert_eq!(accepted.method, missing_required.method);
        assert_eq!(accepted.id, missing_required.id);
        assert_eq!(
            accepted
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            missing_required
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            "the required argument is the sole planted dimension"
        );
        assert_eq!(
            accepted
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            missing_required
                .params
                .as_ref()
                .and_then(|params| params.get("name")),
            "the required argument is the sole planted dimension"
        );

        let catalog = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "prompts/list",
                Some(serde_json::json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                })),
                157_i64,
            ),
        ))
        .expect("the final prompt catalog retains its admitted argument metadata");
        assert_eq!(catalog["prompts"][0]["arguments"][0]["name"], "topic");
        assert_eq!(catalog["prompts"][0]["arguments"][0]["required"], true);

        let response = block_on(router.dispatch_stateless(&request_ctx, &accepted))
            .expect("the admitted final argument remains accepted after the legacy hook changes");
        assert_eq!(response["resultType"], "complete");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let error = block_on(router.dispatch_stateless(&request_ctx, &missing_required))
            .expect_err("removing only the admitted required argument is rejected");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_prompts_get_rejects_one_field_incompatible_result_type() {
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_prompt(DirectFinalPrompt {
            final_calls: Arc::clone(&final_calls),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 97, Budget::INFINITE, &state);
        let request = direct_final_prompt_request(97);
        let typed_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "prompts/get",
            request.params.as_ref(),
        )
        .expect("final prompts/get request decodes through the public core surface");
        let accepted = block_on(router.dispatch_stateless(&request_ctx, &request))
            .expect("the direct final prompt response is accepted");
        let accepted_wire = serde_json::to_string(&accepted).expect("accepted result serializes");
        let mut incompatible = accepted.clone();
        incompatible["resultType"] = serde_json::json!("input_required");

        let mut accepted_without_type = accepted.clone();
        let mut incompatible_without_type = incompatible.clone();
        let accepted_type = accepted_without_type
            .as_object_mut()
            .and_then(|object| object.remove("resultType"));
        let incompatible_type = incompatible_without_type
            .as_object_mut()
            .and_then(|object| object.remove("resultType"));
        assert_eq!(accepted_type, Some(serde_json::json!("complete")));
        assert_eq!(incompatible_type, Some(serde_json::json!("input_required")));
        assert_eq!(
            incompatible_without_type, accepted_without_type,
            "resultType is the sole incompatible result dimension"
        );

        let incompatible_wire =
            serde_json::to_string(&incompatible).expect("incompatible result serializes");
        assert!(matches!(
            typed_request.decode_result(&incompatible_wire),
            Err(fastmcp_protocol::CoreDispatchError::ResultCodec(_))
        ));
        assert_eq!(
            serde_json::to_string(&accepted).expect("accepted result remains serializable"),
            accepted_wire,
            "rejecting the one-field incompatible result cannot mutate the accepted response"
        );
        assert_eq!(
            final_calls.load(Ordering::SeqCst),
            1,
            "local result admission cannot invoke or mutate the direct prompt handler"
        );
        let reaccepted = typed_request
            .decode_result(&accepted_wire)
            .expect("the original direct final result remains accepted");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &reaccepted.encode().expect("reaccepted result encodes"),
            )
            .expect("reaccepted result is JSON"),
            accepted,
            "the incompatible result cannot alter the accepted final prompt contract"
        );
    }

    #[test]
    fn final_resources_read_dispatches_direct_handler_without_legacy_projection() {
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_resource(DirectFinalResource {
            legacy_calls: Arc::clone(&legacy_calls),
            final_calls: Arc::clone(&final_calls),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 98, Budget::INFINITE, &state);

        let legacy = block_on(router.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "file:///direct-final-resource".to_owned(),
                meta: None,
            },
            state.clone(),
            None,
            None,
        ))
        .expect("legacy resource requests retain their exact handler path");
        let [LegacyResourceContent::Text { text, .. }] = legacy.contents.as_slice() else {
            panic!("legacy resource result retains its exact text variant");
        };
        assert_eq!(text, "legacy resource result");
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(final_calls.load(Ordering::SeqCst), 0);

        let request = direct_final_resource_request(98);
        let typed_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "resources/read",
            request.params.as_ref(),
        )
        .expect("final resources/read request decodes through the public core surface");
        let response = block_on(router.dispatch_stateless(&request_ctx, &request))
            .expect("final resources/read reaches the direct final handler");

        assert_eq!(response["resultType"], "complete");
        assert_eq!(response["ttlMs"], 321);
        assert_eq!(response["cacheScope"], "public");
        assert_eq!(
            response["contents"][0]["text"],
            "direct final resource result"
        );
        assert_eq!(response["contents"][0]["mimeType"], "text/markdown");
        assert_eq!(
            response["contents"][0]["_meta"]["com.example/direct-resource"]["source"],
            "final-handler"
        );
        assert_eq!(response["contents"][0]["com.example/direct-field"], true);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);

        let wire = serde_json::to_string(&response).expect("final resource response serializes");
        let CoreResult::Final(FinalCoreResult::ResourcesRead { result, .. }) = typed_request
            .decode_result(&wire)
            .expect("final resource result decodes through the public core surface")
        else {
            panic!("resources/read selects the exact final result");
        };
        assert_eq!(result.payload.ttl_ms.as_str(), "321");
        assert_eq!(result.payload.cache_scope, CacheScope::Public);
        assert!(matches!(
            result.payload.contents.as_slice(),
            [EmbeddedResourceContents::Text { text, mime_type, .. }]
                if text == "direct final resource result"
                    && mime_type.as_deref() == Some("text/markdown")
        ));
    }

    #[test]
    fn resource_read_cache_hint_provenance_controls_router_policy_not_wire_value() {
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 981, Budget::INFINITE, &state);
        let request = JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "uri": "file:///sentinel-hint-resource",
            })),
            981_i64,
        );

        for (provenance, expected_ttl, expected_scope) in [
            (
                FinalResourceReadCacheHintProvenance::Explicit,
                DEFAULT_FINAL_RESOURCE_TTL_MS,
                "private",
            ),
            (
                FinalResourceReadCacheHintProvenance::RouterPolicy,
                23,
                "public",
            ),
        ] {
            let mut router = Router::new();
            router.set_final_cache_hint_policy(
                CacheTtl::milliseconds(17),
                CacheTtl::milliseconds(23),
                CacheScope::Public,
            );
            router.add_resource(SentinelHintResource { provenance });

            let response = block_on(router.dispatch_stateless(&request_ctx, &request))
                .expect("final resource result dispatches");
            assert_eq!(response["ttlMs"], expected_ttl);
            assert_eq!(response["cacheScope"], expected_scope);
        }
    }

    #[test]
    fn final_unknown_resource_is_invalid_params_with_exact_uri_and_legacy_is_unchanged() {
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_resource(DirectFinalResource {
            legacy_calls: Arc::clone(&legacy_calls),
            final_calls: Arc::clone(&final_calls),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 157, Budget::INFINITE, &state);
        let baseline = direct_final_resource_request(157);
        let mut unknown = baseline.clone();
        let replaced_uri = unknown
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("resource parameters are an object")
            .insert(
                "uri".to_owned(),
                serde_json::json!("file:///unknown-final-resource"),
            );
        assert_eq!(
            replaced_uri,
            Some(serde_json::json!("file:///direct-final-resource"))
        );

        assert_eq!(baseline.method, unknown.method);
        assert_eq!(baseline.id, unknown.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            unknown
                .params
                .as_ref()
                .and_then(|params| params.get("_meta")),
            "the resource URI is the sole planted dimension"
        );

        let accepted = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("the registered final resource is accepted");
        assert_eq!(accepted["resultType"], "complete");
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let error = block_on(router.dispatch_stateless(&request_ctx, &unknown))
            .expect_err("changing only the URI to an unknown resource is rejected");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(error.message, "Resource not found");
        assert_eq!(
            error.data,
            Some(serde_json::json!({"uri": "file:///unknown-final-resource"}))
        );
        assert_eq!(final_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);

        let legacy_error = block_on(router.handle_resources_read(
            &request_ctx,
            &ReadResourceParams {
                uri: "file:///unknown-final-resource".to_owned(),
                meta: None,
            },
            state,
            None,
            None,
        ))
        .expect_err("the exact legacy missing-resource error remains unchanged");
        assert_eq!(legacy_error.code, McpErrorCode::ResourceNotFound);
        assert_eq!(
            legacy_error.message,
            "Resource not found: file:///unknown-final-resource"
        );
    }

    #[test]
    fn final_resources_read_rejects_one_field_incompatible_result_type() {
        let legacy_calls = Arc::new(AtomicUsize::new(0));
        let final_calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_resource(DirectFinalResource {
            legacy_calls,
            final_calls: Arc::clone(&final_calls),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 99, Budget::INFINITE, &state);
        let request = direct_final_resource_request(99);
        let typed_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "resources/read",
            request.params.as_ref(),
        )
        .expect("final resources/read request decodes through the public core surface");
        let accepted = block_on(router.dispatch_stateless(&request_ctx, &request))
            .expect("the direct final resource response is accepted");
        let accepted_wire = serde_json::to_string(&accepted).expect("accepted result serializes");
        let mut incompatible = accepted.clone();
        incompatible["resultType"] = serde_json::json!("input_required");

        let mut accepted_without_type = accepted.clone();
        let mut incompatible_without_type = incompatible.clone();
        let accepted_type = accepted_without_type
            .as_object_mut()
            .and_then(|object| object.remove("resultType"));
        let incompatible_type = incompatible_without_type
            .as_object_mut()
            .and_then(|object| object.remove("resultType"));
        assert_eq!(accepted_type, Some(serde_json::json!("complete")));
        assert_eq!(incompatible_type, Some(serde_json::json!("input_required")));
        assert_eq!(
            incompatible_without_type, accepted_without_type,
            "resultType is the sole incompatible result dimension"
        );

        let incompatible_wire =
            serde_json::to_string(&incompatible).expect("incompatible result serializes");
        assert!(matches!(
            typed_request.decode_result(&incompatible_wire),
            Err(fastmcp_protocol::CoreDispatchError::ResultCodec(_))
        ));
        assert_eq!(
            serde_json::to_string(&accepted).expect("accepted result remains serializable"),
            accepted_wire,
            "rejecting the one-field incompatible result cannot mutate the accepted response"
        );
        assert_eq!(
            final_calls.load(Ordering::SeqCst),
            1,
            "local result admission cannot invoke or mutate the direct resource handler"
        );
        let reaccepted = typed_request
            .decode_result(&accepted_wire)
            .expect("the original direct final result remains accepted");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &reaccepted.encode().expect("reaccepted result encodes"),
            )
            .expect("reaccepted result is JSON"),
            accepted,
            "the incompatible result cannot alter the accepted final resource contract"
        );
    }

    #[test]
    fn final_catalog_preserves_resource_template_and_prompt_fields() {
        let resource_metadata = OpenMetadata::try_from_entries([(
            "com.example/resource".to_owned(),
            serde_json::json!({"source": "resource-handler"}),
        )])
        .expect("resource metadata is valid");
        let template_metadata = OpenMetadata::try_from_entries([(
            "com.example/template".to_owned(),
            serde_json::json!({"source": "template-handler"}),
        )])
        .expect("template metadata is valid");
        let prompt_metadata = OpenMetadata::try_from_entries([(
            "com.example/prompt".to_owned(),
            serde_json::json!({"source": "prompt-handler"}),
        )])
        .expect("prompt metadata is valid");
        let resource_annotations = Annotations {
            audience: None,
            priority: Some(0.25),
            last_modified: Some("2026-08-08T00:00:00Z".to_owned()),
            additional: BTreeMap::new(),
        };
        let template_annotations = Annotations {
            audience: None,
            priority: Some(0.75),
            last_modified: Some("2026-08-08T00:00:01Z".to_owned()),
            additional: BTreeMap::new(),
        };
        let resource_icon = RawIcon::try_with_details(
            "https://example.test/resource.png",
            Some("image/png".to_owned()),
            Some(vec!["32x32".to_owned()]),
            None,
        )
        .expect("resource icon is valid");
        let template_icon = RawIcon::try_with_details(
            "https://example.test/template.png",
            Some("image/png".to_owned()),
            Some(vec!["48x48".to_owned()]),
            None,
        )
        .expect("template icon is valid");
        let prompt_icon = RawIcon::try_with_details(
            "https://example.test/prompt.png",
            Some("image/png".to_owned()),
            Some(vec!["64x64".to_owned()]),
            None,
        )
        .expect("prompt icon is valid");

        let mut router = Router::new();
        router.add_resource(FinalCatalogResource {
            metadata: resource_metadata,
            icons: vec![resource_icon],
            annotations: resource_annotations,
        });
        router.add_resource(FinalCatalogResourceTemplate {
            metadata: template_metadata,
            icons: vec![template_icon],
            annotations: template_annotations,
        });
        router.add_prompt(FinalCatalogPrompt {
            metadata: prompt_metadata,
            icons: vec![prompt_icon],
        });

        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 96, Budget::INFINITE, &state);
        let final_metadata = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });

        let resources = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/list",
                Some(serde_json::json!({"_meta": final_metadata.clone()})),
                96_i64,
            ),
        ))
        .expect("final resource catalog is encoded");
        assert_eq!(resources["resources"][0]["title"], "Final Catalog Resource");
        assert_eq!(
            resources["resources"][0]["icons"][0]["src"],
            "https://example.test/resource.png"
        );
        assert_eq!(resources["resources"][0]["annotations"]["priority"], 0.25);
        assert_eq!(
            resources["resources"][0]["_meta"]["com.example/resource"]["source"],
            "resource-handler"
        );

        let templates = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "resources/templates/list",
                Some(serde_json::json!({"_meta": final_metadata.clone()})),
                97_i64,
            ),
        ))
        .expect("final resource-template catalog is encoded");
        assert_eq!(
            templates["resourceTemplates"][0]["title"],
            "Final Catalog Template"
        );
        assert_eq!(
            templates["resourceTemplates"][0]["icons"][0]["src"],
            "https://example.test/template.png"
        );
        assert_eq!(
            templates["resourceTemplates"][0]["annotations"]["priority"],
            0.75
        );
        assert_eq!(
            templates["resourceTemplates"][0]["_meta"]["com.example/template"]["source"],
            "template-handler"
        );

        let prompts = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new(
                "prompts/list",
                Some(serde_json::json!({"_meta": final_metadata})),
                98_i64,
            ),
        ))
        .expect("final prompt catalog is encoded");
        assert_eq!(prompts["prompts"][0]["title"], "Final Catalog Prompt");
        assert_eq!(
            prompts["prompts"][0]["icons"][0]["src"],
            "https://example.test/prompt.png"
        );
        assert_eq!(
            prompts["prompts"][0]["_meta"]["com.example/prompt"]["source"],
            "prompt-handler"
        );
        assert_eq!(prompts["prompts"][0]["arguments"][0]["required"], false);
    }

    #[test]
    fn final_resource_catalog_missing_metadata_is_non_mutating() {
        let metadata = OpenMetadata::try_from_entries([(
            "com.example/resource".to_owned(),
            serde_json::json!({"source": "resource-handler"}),
        )])
        .expect("resource metadata is valid");
        let icon = RawIcon::try_with_details(
            "https://example.test/resource.png",
            Some("image/png".to_owned()),
            None,
            None,
        )
        .expect("resource icon is valid");
        let mut router = Router::new();
        router.add_resource(FinalCatalogResource {
            metadata,
            icons: vec![icon],
            annotations: Annotations::default(),
        });
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 99, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            "resources/list",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            99_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("final resource-list parameters are an object")
            .remove("_meta");

        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .map(serde_json::Map::len),
            Some(1),
            "the final metadata object is the sole baseline parameter"
        );
        assert_eq!(
            planted
                .params
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .map(serde_json::Map::len),
            Some(0),
            "the planted request differs only by final metadata removal"
        );
        let catalog_before = serde_json::to_vec(&router.resources()).expect("catalog serializes");
        let planted_before = serde_json::to_vec(&planted).expect("request serializes");
        let baseline_result = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("final baseline is accepted");
        assert_eq!(
            baseline_result["resources"][0]["title"],
            "Final Catalog Resource"
        );

        let error = block_on(router.dispatch_stateless(&request_ctx, &planted))
            .expect_err("only final request metadata is refused");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&planted).expect("rejected request serializes"),
            planted_before,
            "the one-field rejection cannot mutate caller-owned input"
        );
        assert_eq!(
            serde_json::to_vec(&router.resources()).expect("catalog serializes"),
            catalog_before,
            "the one-field rejection cannot mutate the resource catalog"
        );
        assert_eq!(
            block_on(router.dispatch_stateless(&request_ctx, &baseline))
                .expect("the baseline remains accepted after rejection"),
            baseline_result,
            "the one-field rejection cannot alter final field preservation"
        );
    }

    #[test]
    fn core_request_decode_result_round_trips_final_catalog_and_read_cache_hints() {
        let metadata = OpenMetadata::try_from_entries([(
            "com.example/catalog".to_owned(),
            serde_json::json!({"source": "handler"}),
        )])
        .expect("valid final catalog metadata");
        let icon = RawIcon::try_with_details(
            "https://example.test/tool.png",
            Some("image/png".to_owned()),
            Some(vec!["48x48".to_owned()]),
            None,
        )
        .expect("valid final icon");
        let alternate_icon = RawIcon::try_with_details(
            "https://example.test/tool-dark.svg",
            Some("image/svg+xml".to_owned()),
            Some(vec!["any".to_owned()]),
            Some(fastmcp_protocol::common_types::IconTheme::Dark),
        )
        .expect("valid alternate final icon");
        let mut router = Router::new();
        let list_ttl: CacheTtl = serde_json::from_str("922337203685477580812345678901234567890")
            .expect("an arbitrary-width final list TTL is valid");
        let resource_read_ttl: CacheTtl =
            serde_json::from_str("184467440737095516160000000000000000000")
                .expect("an arbitrary-width final resource-read TTL is valid");
        router.set_final_cache_hint_policy(
            list_ttl.clone(),
            resource_read_ttl.clone(),
            CacheScope::Private,
        );
        router
            .add_tool(FinalCatalogTool {
                metadata,
                icons: vec![icon, alternate_icon],
            })
            .expect("final catalog tool registration succeeds");
        router.add_resource(NamedResource::new("file:///catalog-resource"));
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 93, Budget::INFINITE, &state);

        let legacy = router
            .handle_tools_list(&request_ctx, ListToolsParams::default(), None)
            .expect("legacy catalog remains available");
        let legacy_wire = serde_json::to_value(&legacy).expect("legacy catalog serializes");
        assert!(legacy_wire.get("ttlMs").is_none());
        assert!(legacy_wire.get("cacheScope").is_none());
        assert!(legacy_wire["tools"][0].get("icon").is_some());
        assert!(legacy_wire["tools"][0].get("version").is_some());
        assert!(legacy_wire["tools"][0].get("tags").is_some());

        let final_list_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        });
        let final_list_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "tools/list",
            Some(&final_list_params),
        )
        .expect("final catalog request decodes through the public core surface");
        let modern_list = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new("tools/list", Some(final_list_params), 93_i64),
        ))
        .expect("final catalog is projected through the exact model");
        assert_eq!(modern_list["resultType"], "complete");
        assert_eq!(modern_list["ttlMs"].to_string(), list_ttl.as_str());
        assert_eq!(modern_list["cacheScope"], "private");
        assert_eq!(modern_list["tools"][0]["title"], "Exact Final Catalog Tool");
        assert_eq!(
            modern_list["tools"][0]["annotations"]["title"],
            "Exact annotation title"
        );
        assert_eq!(
            modern_list["tools"][0]["icons"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(
            modern_list["tools"][0]["icons"][0]["sizes"],
            serde_json::json!(["48x48"])
        );
        assert_eq!(
            modern_list["tools"][0]["_meta"]["com.example/catalog"]["source"],
            "handler"
        );
        assert_eq!(modern_list["tools"][0]["outputSchema"]["type"], "object");
        assert!(modern_list["tools"][0].get("icon").is_none());
        assert!(modern_list["tools"][0].get("version").is_none());
        assert!(modern_list["tools"][0].get("tags").is_none());
        let modern_list_wire =
            serde_json::to_string(&modern_list).expect("final catalog response serializes");
        let CoreResult::Final(FinalCoreResult::ToolsList { result, .. }) = final_list_request
            .decode_result(&modern_list_wire)
            .expect("final catalog response decodes through the public core surface")
        else {
            panic!("tools/list selects the exact final catalog result");
        };
        assert_eq!(result.payload.ttl_ms.as_str(), list_ttl.as_str());
        assert_eq!(result.payload.cache_scope, CacheScope::Private);
        let final_tool = result
            .payload
            .tools
            .first()
            .expect("final catalog contains the registered tool");
        assert_eq!(
            final_tool.title.as_deref(),
            Some("Exact Final Catalog Tool")
        );
        assert_eq!(
            final_tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.title.as_deref()),
            Some("Exact annotation title")
        );
        assert_eq!(final_tool.icons.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            final_tool
                .icons
                .as_ref()
                .and_then(|icons| icons.first())
                .map(|icon| icon.src.as_str()),
            Some("https://example.test/tool.png")
        );
        assert_eq!(
            final_tool
                .meta
                .as_ref()
                .and_then(|metadata| metadata.get("com.example/catalog"))
                .and_then(|value| value.get("source"))
                .and_then(serde_json::Value::as_str),
            Some("handler")
        );
        assert_eq!(
            final_tool
                .output_schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("object")
        );

        let final_read_params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
            "uri": "file:///catalog-resource",
        });
        let final_read_request = CoreRequest::decode(
            ProtocolEra::Modern2026,
            "resources/read",
            Some(&final_read_params),
        )
        .expect("final resource-read request decodes through the public core surface");
        let modern_read = block_on(router.dispatch_stateless(
            &request_ctx,
            &JsonRpcRequest::new("resources/read", Some(final_read_params), 94_i64),
        ))
        .expect("final resource content is projected through the final model");
        assert_eq!(modern_read["ttlMs"].to_string(), resource_read_ttl.as_str());
        assert_eq!(modern_read["cacheScope"], "private");
        assert_eq!(
            modern_read["contents"][0]["uri"],
            "file:///catalog-resource"
        );
        assert_eq!(modern_read["contents"][0]["text"], "content");
        let modern_read_wire =
            serde_json::to_string(&modern_read).expect("final resource-read response serializes");
        let CoreResult::Final(FinalCoreResult::ResourcesRead { result, .. }) = final_read_request
            .decode_result(&modern_read_wire)
            .expect("final resource-read response decodes through the public core surface")
        else {
            panic!("resources/read selects the exact final read result");
        };
        assert_eq!(result.payload.ttl_ms.as_str(), resource_read_ttl.as_str());
        assert_eq!(result.payload.cache_scope, CacheScope::Private);
        assert!(matches!(
            result.payload.contents.as_slice(),
            [EmbeddedResourceContents::Text { text, .. }] if text == "content"
        ));
    }

    #[test]
    fn final_catalog_missing_metadata_is_non_mutating() {
        let mut router = Router::new();
        router
            .add_tool(NamedTool::new("metadata-guarded-tool"))
            .expect("tool registration succeeds");
        let cx = Cx::for_testing();
        let state = SessionState::new();
        let request_ctx = request_context(&cx, 95, Budget::INFINITE, &state);
        let baseline = JsonRpcRequest::new(
            "tools/list",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            95_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("final list parameters are an object")
            .remove("_meta");
        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.id, planted.id);
        let catalog_before = serde_json::to_vec(&router.tools()).expect("catalog serializes");
        let planted_before = serde_json::to_vec(&planted).expect("request serializes");

        let baseline_result = block_on(router.dispatch_stateless(&request_ctx, &baseline))
            .expect("the final baseline is accepted");
        let error = block_on(router.dispatch_stateless(&request_ctx, &planted))
            .expect_err("only missing final metadata is refused");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            serde_json::to_vec(&planted).expect("rejected request serializes"),
            planted_before,
            "the rejected one-field request remains unchanged"
        );
        assert_eq!(
            serde_json::to_vec(&router.tools()).expect("catalog serializes"),
            catalog_before,
            "the rejected one-field request cannot mutate the catalog"
        );
        assert_eq!(
            block_on(router.dispatch_stateless(&request_ctx, &baseline))
                .expect("the unchanged final baseline remains accepted"),
            baseline_result,
            "the rejection cannot alter final cache hints or catalog projection"
        );
    }

    #[test]
    fn srv_04_modern_owned_dispatch_runs_requests_concurrently() {
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let mut router = Router::new();
        router
            .add_tool(ConcurrentModernTool::new(
                Arc::clone(&started),
                Arc::clone(&completed),
            ))
            .expect("tool registration succeeds");
        let router = Arc::new(router);
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("the test runtime is available");
        let runtime_handle = runtime.handle();
        let mut first = spawn_owned_modern_request(
            &runtime_handle,
            Arc::clone(&router),
            401,
            "modern-one",
            "one",
            None,
        );
        let mut second =
            spawn_owned_modern_request(&runtime_handle, router, 402, "modern-two", "two", None);

        let (first, second) = runtime.block_on(async {
            let cx = Cx::current().expect("block_on installs an observer context");
            let first = first
                .recv(&cx)
                .await
                .expect("the first owner reports a terminal result");
            let second = second
                .recv(&cx)
                .await
                .expect("the second owner reports a terminal result");
            (first, second)
        });
        let first = first.expect("the first modern request completes");
        let second = second.expect("the second modern request completes");

        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(
            first.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(
            second.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        let mut completed = completed
            .lock()
            .expect("completion probe lock is not poisoned")
            .clone();
        completed.sort();
        assert_eq!(completed, vec!["one".to_string(), "two".to_string()]);
    }

    struct RegionOwnedTool {
        started: Mutex<Option<oneshot::Sender<Cx>>>,
        descendant_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ToolHandler for RegionOwnedTool {
        fn definition(&self) -> Tool {
            NamedTool::new("region-owned-tool").definition()
        }

        fn execution_mode(&self) -> crate::ToolExecutionMode {
            crate::ToolExecutionMode::Async
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            panic!("the modern request must use the async handler hook")
        }

        fn call_final_outcome_async_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            request_cx: &'a Cx,
            arguments: serde_json::Value,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                let request_cx = if arguments["ordinaryContext"] == true {
                    assert_eq!(ctx.region_id(), request_cx.region_id());
                    assert_eq!(ctx.task_id(), request_cx.task_id());
                    ctx.cx()
                } else {
                    request_cx
                };
                struct DescendantGuard(Arc<std::sync::atomic::AtomicBool>);
                impl Drop for DescendantGuard {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let dropped = Arc::clone(&self.descendant_dropped);
                let (ready_sender, mut ready) = oneshot::channel();
                let _descendant = request_cx
                    .spawn(move |descendant_cx| async move {
                        let _guard = DescendantGuard(dropped);
                        ready_sender
                            .send_blocking(())
                            .expect("handler waits for admission");
                        while descendant_cx.checkpoint().is_ok() {
                            yield_once().await;
                        }
                    })
                    .expect("the public handler Cx admits nested work");
                ready
                    .recv(request_cx)
                    .await
                    .expect("descendant actually started");
                self.started
                    .lock()
                    .expect("start sender lock")
                    .take()
                    .expect("handler invoked once")
                    .send_blocking(request_cx.clone())
                    .expect("test observes handler admission");
                if arguments["park"] == true {
                    // Keep the sender alive so only cancellation wakes this
                    // real runtime channel receive; no handler-side polling.
                    let (_sender, mut receiver) = oneshot::channel::<()>();
                    assert_eq!(
                        receiver.recv(request_cx).await,
                        Err(oneshot::RecvError::Cancelled)
                    );
                    return Outcome::Cancelled(asupersync::CancelReason::user(
                        "request child observed cancellation",
                    ));
                }
                Outcome::Ok(FinalToolOutcome::Complete(
                    crate::handler::promote_legacy_tool_content(vec![Content::text("completed")])
                        .expect("text result is valid"),
                ))
            })
        }
    }

    async fn check_modern_request_region(parent: Cx, action: &str, ordinary_context: bool) {
        let descendant_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started_sender, mut started) = oneshot::channel();
        let mut router = Router::new();
        router
            .add_tool(RegionOwnedTool {
                started: Mutex::new(Some(started_sender)),
                descendant_dropped: Arc::clone(&descendant_dropped),
            })
            .expect("region-owned tool registers");
        let request_ctx = McpContext::with_state(parent.clone(), 405, SessionState::new());
        // A derived pre-admission context can replace its token while still
        // sharing the mask. Mask release must wake the dispatch's token too.
        let mask_ctx = request_ctx
            .clone()
            .with_request_cancellation(fastmcp_core::McpRequestCancellation::new());
        let cancellation = request_ctx.request_cancellation();
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "region-owned-tool",
                "arguments": {"park": action != "complete", "ordinaryContext": ordinary_context},
            })),
            "region-request",
        );
        let (release, mut released) = oneshot::channel();
        let mut sibling = parent
            .spawn(move |sibling_cx| async move {
                released
                    .recv(&sibling_cx)
                    .await
                    .expect("sibling was not cancelled");
                sibling_cx.checkpoint().expect("parent region remains live");
                42
            })
            .expect("parent admits sibling");
        let mut dispatch =
            Box::pin(Arc::new(router).dispatch_stateless_owned(request_ctx, request));
        let handler_cx = if action == "complete" {
            let result = dispatch.as_mut().await.expect("request completes");
            assert_eq!(result["resultType"], "complete");
            assert_eq!(result["content"][0]["text"], "completed");
            assert!(
                descendant_dropped.load(Ordering::Acquire),
                "response cannot precede descendant cleanup"
            );
            started
                .recv(&parent)
                .await
                .expect("handler reported its Cx")
        } else {
            let mut admission = std::pin::pin!(started.recv(&parent));
            let handler_cx = std::future::poll_fn(|task_cx| {
                assert!(
                    dispatch.as_mut().poll(task_cx).is_pending(),
                    "parked request cannot finish before the selected action"
                );
                admission.as_mut().poll(task_cx)
            })
            .await
            .expect("handler and descendant started");
            assert!(!descendant_dropped.load(Ordering::Acquire));
            if matches!(action, "cancel" | "masked-cancel" | "masked-panic-cancel") {
                struct DispatchWake(AtomicUsize);
                impl std::task::Wake for DispatchWake {
                    fn wake(self: Arc<Self>) {
                        self.0.fetch_add(1, Ordering::SeqCst);
                    }
                }
                let wake = Arc::new(DispatchWake(AtomicUsize::new(0)));
                let waker = std::task::Waker::from(Arc::clone(&wake));
                if action == "cancel" {
                    let mut task_cx = std::task::Context::from_waker(&waker);
                    assert!(dispatch.as_mut().poll(&mut task_cx).is_pending());
                    let before_cancel = wake.0.load(Ordering::SeqCst);
                    let mut canceller = parent
                        .spawn(move |_| async move {
                            assert!(cancellation.cancel());
                        })
                        .expect("runtime admits cancellation controller");
                    canceller
                        .join(&parent)
                        .await
                        .expect("cancellation controller joins");
                    assert!(
                        wake.0.load(Ordering::SeqCst) > before_cancel,
                        "cancellation must wake the parked dispatch before it is polled again"
                    );
                } else {
                    assert!(cancellation.cancel());
                    let mut before_release = 0;
                    let masked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        mask_ctx
                            .masked(|| {
                                mask_ctx
                                    .masked(|| {
                                        let mut task_cx = std::task::Context::from_waker(&waker);
                                        for _ in 0..2 {
                                            assert!(
                                                dispatch.as_mut().poll(&mut task_cx).is_pending()
                                            );
                                        }
                                        before_release = wake.0.load(Ordering::SeqCst);
                                    })
                                    .expect("nested request mask enters");
                                assert_eq!(wake.0.load(Ordering::SeqCst), before_release);
                                if action == "masked-panic-cancel" {
                                    panic!("planted synchronous masked-section panic");
                                }
                            })
                            .expect("request may mask cancellation");
                    }));
                    assert_eq!(masked.is_err(), action == "masked-panic-cancel");
                    if let Err(payload) = masked {
                        assert_eq!(
                            payload.downcast_ref::<&str>(),
                            Some(&"planted synchronous masked-section panic"),
                            "a failed mask assertion must not count as the planted unwind"
                        );
                    }
                    assert!(
                        wake.0.load(Ordering::SeqCst) > before_release,
                        "outer mask exit must wake dispatch before it is polled again"
                    );
                    assert!(!descendant_dropped.load(Ordering::Acquire));
                }
                let error = dispatch
                    .as_mut()
                    .await
                    .expect_err("selected request is cancelled");
                assert_eq!(error.code, McpErrorCode::RequestCancelled);
                assert!(
                    descendant_dropped.load(Ordering::Acquire),
                    "cancellation result cannot precede descendant cleanup"
                );
            }
            handler_cx
        };
        assert_ne!(
            handler_cx.region_id(),
            parent.region_id(),
            "a task in the ambient region is not a request child region"
        );
        drop(dispatch);
        // Drop requests cleanup, but is not itself a quiescent close. Drive
        // the runtime until the independently observed descendant is gone.
        while !descendant_dropped.load(Ordering::Acquire) {
            yield_once().await;
        }
        assert!(
            !parent.is_cancel_requested(),
            "request cleanup cannot cancel parent"
        );
        release
            .send_blocking(())
            .expect("sibling still owns its receive");
        assert_eq!(sibling.join(&parent).await.expect("sibling joins"), 42);

        // Retaining the handler Cx cannot resurrect work after region close.
        let resurrected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let effect = Arc::clone(&resurrected);
        if let Ok(mut task) = handler_cx.spawn(move |_| async move {
            effect.store(true, Ordering::Release);
        }) {
            assert!(
                task.join(&parent).await.is_err(),
                "closed region rejects late work"
            );
        }
        assert!(!resurrected.load(Ordering::Acquire));
    }

    fn run_modern_request_region_check(action: &'static str, ordinary_context: bool) {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime builds");
        runtime.block_on(async move {
            let parent = Cx::current().expect("public runtime entry installs its context");
            asupersync::time::timeout(
                parent.now(),
                Duration::from_secs(5),
                check_modern_request_region(parent, action, ordinary_context),
            )
            .await
            .expect("request and descendants settle within five seconds");
        });
    }

    #[test]
    fn fnd_04_modern_request_region_drains_descendants_before_completion() {
        run_modern_request_region_check("complete", false);
    }

    #[test]
    fn fnd_04_modern_request_region_cancellation_isolates_sibling() {
        run_modern_request_region_check("cancel", false);
    }

    #[test]
    fn fnd_04_modern_request_region_drop_closes_descendants() {
        run_modern_request_region_check("drop", false);
    }

    #[test]
    fn fnd_04_modern_request_region_mask_defers_cancellation_without_repolling_completion() {
        run_modern_request_region_check("masked-cancel", false);
    }

    #[test]
    fn fnd_04_modern_request_region_mask_unwind_wakes_deferred_cancellation() {
        run_modern_request_region_check("masked-panic-cancel", false);
    }

    #[test]
    fn fnd_04_modern_request_context_drains_descendants_before_completion() {
        run_modern_request_region_check("complete", true);
    }

    #[test]
    fn fnd_04_modern_request_context_cancellation_isolates_sibling() {
        run_modern_request_region_check("cancel", true);
    }

    #[test]
    fn fnd_04_modern_request_context_drop_closes_descendants() {
        run_modern_request_region_check("drop", true);
    }

    #[test]
    fn fnd_04_modern_request_region_unavailable_runtime_never_dispatches() {
        struct ObservedTool(Arc<AtomicUsize>);
        impl ToolHandler for ObservedTool {
            fn definition(&self) -> Tool {
                NamedTool::new("observed-runtime").definition()
            }

            fn execution_mode(&self) -> crate::ToolExecutionMode {
                crate::ToolExecutionMode::Async
            }

            fn call(&self, ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                ctx.set_state("called", true);
                Ok(vec![Content::text("runtime admitted")])
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.add_tool(ObservedTool(Arc::clone(&calls))).unwrap();
        let router = Arc::new(router);
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "observed-runtime",
                "arguments": {},
            })),
            "runtime-admission",
        );
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let state = SessionState::new();
        let retained = runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::with_state(cx.clone(), 406, state.clone());
            let result = Arc::clone(&router)
                .dispatch_stateless_owned(ctx, request.clone())
                .await
                .expect("live runtime dispatches");
            assert_eq!(result["content"][0]["text"], "runtime admitted");
            cx
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.get::<bool>("called"), Some(true));
        drop(runtime);
        let untouched = SessionState::new();
        let error = fastmcp_core::block_on(Arc::clone(&router).dispatch_stateless_owned(
            McpContext::with_state(retained, 406, untouched.clone()),
            request.clone(),
        ))
        .expect_err("dead runtime must not select direct dispatch");
        assert_eq!(
            error.message,
            "modern request runtime is no longer available"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(untouched.get::<bool>("called"), None);

        // A detached test context has never held runtime authority. Preserve
        // the existing direct-call surface, rather than conflating it with a
        // retained handle whose runtime was destroyed.
        let result = fastmcp_core::block_on(router.dispatch_stateless_owned(
            McpContext::with_state(Cx::for_testing(), 406, untouched.clone()),
            request,
        ))
        .expect("detached direct caller still dispatches");
        assert_eq!(result["content"][0]["text"], "runtime admitted");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(untouched.get::<bool>("called"), Some(true));
    }

    #[test]
    fn srv_04_modern_owned_cancellation_does_not_change_sibling() {
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let mut router = Router::new();
        router
            .add_tool(ConcurrentModernTool::new(
                Arc::clone(&started),
                Arc::clone(&completed),
            ))
            .expect("tool registration succeeds");
        let router = Arc::new(router);
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("the test runtime is available");
        let runtime_handle = runtime.handle();
        let (cancel_control_sender, mut cancel_control) = oneshot::channel();
        let mut cancelled = spawn_owned_modern_request(
            &runtime_handle,
            Arc::clone(&router),
            403,
            "modern-cancelled",
            "cancelled",
            Some(cancel_control_sender),
        );
        let mut sibling = spawn_owned_modern_request(
            &runtime_handle,
            router,
            404,
            "modern-sibling",
            "sibling",
            None,
        );

        let (cancelled, sibling) = runtime.block_on(async {
            let observer_cx = Cx::current().expect("block_on installs an observer context");
            let cancelled_cx = cancel_control
                .recv(&observer_cx)
                .await
                .expect("the request owner exposes its cancellation context");
            // Bounded so a dispatch rejection fails the test loudly instead of
            // spinning this observer loop forever.
            let admission_deadline = std::time::Instant::now() + Duration::from_secs(10);
            while started.load(Ordering::SeqCst) < 2 {
                assert!(
                    std::time::Instant::now() < admission_deadline,
                    "both owned modern requests must start before the admission deadline"
                );
                yield_once().await;
            }
            cancelled_cx.cancel_with(CancelKind::User, Some("test single-request cancellation"));
            let cancelled = cancelled
                .recv(&observer_cx)
                .await
                .expect("the cancelled owner reports a terminal result");
            let sibling = sibling
                .recv(&observer_cx)
                .await
                .expect("the sibling owner reports a terminal result");
            (cancelled, sibling)
        });

        let cancelled = cancelled.expect_err("only the selected request is cancelled");
        assert_eq!(cancelled.code, McpErrorCode::RequestCancelled);
        let sibling = sibling.expect("the sibling completes despite peer cancellation");
        assert_eq!(
            sibling.get("resultType"),
            Some(&serde_json::json!("complete"))
        );
        assert_eq!(
            completed
                .lock()
                .expect("completion probe lock is not poisoned")
                .as_slice(),
            ["sibling"],
            "cancelling one request cannot add, remove, or alter sibling completion"
        );
    }

    #[test]
    fn mount_result_with_warning_and_no_error_is_successful() {
        let result = MountResult {
            tools: 0,
            resources: 0,
            resource_templates: 0,
            prompts: 0,
            warnings: vec!["something".to_string()],
            errors: vec![],
        };
        assert!(result.is_success());
        assert!(!result.has_components());
    }

    struct AppsLinkedTool;

    impl ToolHandler for AppsLinkedTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "apps-linked-tool".to_owned(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn final_definition(&self) -> Option<FinalTool> {
            let metadata = fastmcp_protocol::McpAppsToolMetadata::try_new(
                Some(
                    AbsoluteUri::parse("ui://weather/dashboard").expect("fixed Apps URI is valid"),
                ),
                Some(vec![fastmcp_protocol::McpAppsToolVisibility::App]),
            )
            .expect("fixed Apps metadata is valid")
            .to_open_metadata()
            .expect("fixed Apps metadata serializes");
            Some(FinalTool {
                name: "apps-linked-tool".to_owned(),
                title: None,
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: Some(metadata),
            })
        }

        fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(Vec::new())
        }
    }

    struct AppsBoundResource {
        mime_type: &'static str,
    }

    impl ResourceHandler for AppsBoundResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "ui://weather/dashboard".to_owned(),
                name: "weather-dashboard".to_owned(),
                description: None,
                mime_type: Some(self.mime_type.to_owned()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn apps_tool_registration_requires_a_registered_html_ui_resource() {
        let mut router = Router::new();
        router
            .add_mcp_apps_ui_resource_with_behavior(
                AppsBoundResource {
                    mime_type: fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("matching Apps HTML resource registers first");
        router
            .add_mcp_apps_tool_with_behavior(AppsLinkedTool, crate::DuplicateBehavior::Error)
            .expect("tool binding to the registered Apps HTML resource is admitted");
        assert!(router.tools().is_empty(), "Apps tools are final-only");
        assert!(
            router
                .resolve_tool_for_era("apps-linked-tool", Some(ProtocolEra::Legacy2024),)
                .is_none()
        );
        assert!(
            router
                .resolve_tool_for_era("apps-linked-tool", Some(ProtocolEra::Modern2026),)
                .is_some()
        );
    }

    #[test]
    fn apps_tool_registration_rejects_only_a_non_html_ui_resource_without_mutation() {
        let mut router = Router::new();
        router
            .add_final_resource_with_behavior(
                AppsBoundResource {
                    mime_type: "text/plain",
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("the one-field MIME variant remains a valid final resource");

        let error = router
            .add_mcp_apps_tool_with_behavior(AppsLinkedTool, crate::DuplicateBehavior::Error)
            .expect_err("only replacing the Apps HTML MIME type rejects the linked tool");
        assert_eq!(error.code, McpErrorCode::InvalidRequest);
        assert!(
            router.tools().is_empty(),
            "rejected Apps linkage cannot add the tool to the legacy or final catalog"
        );
    }

    #[test]
    fn generic_registration_rejects_apps_ui_resources_and_tools_without_mutation() {
        let mut router = Router::new();
        let resource_error = router
            .add_resource_with_behavior(
                AppsBoundResource {
                    mime_type: fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect_err("generic registration must not expose an Apps View to exact 2024");
        assert_eq!(resource_error.code, McpErrorCode::InvalidRequest);
        assert_eq!(router.resources().len(), 0);

        router
            .add_mcp_apps_ui_resource_with_behavior(
                AppsBoundResource {
                    mime_type: fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("the negotiated final-only Apps resource registers");
        let tool_error = router
            .add_tool_with_behavior(AppsLinkedTool, crate::DuplicateBehavior::Error)
            .expect_err("generic registration must not publish Apps metadata without opt-in");
        assert_eq!(tool_error.code, McpErrorCode::InvalidRequest);
        assert!(router.tools().is_empty());
    }

    #[test]
    fn apps_binding_mounts_reject_dangling_tools_replacements_and_prefixes_atomically() {
        let mut source = Router::new();
        source
            .add_mcp_apps_ui_resource_with_behavior(
                AppsBoundResource {
                    mime_type: fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("source Apps resource registers");
        source
            .add_mcp_apps_tool_with_behavior(AppsLinkedTool, crate::DuplicateBehavior::Error)
            .expect("source Apps tool registers");

        let mut tools_only_destination = Router::new();
        let tools_only = tools_only_destination.mount_tools(source, None);
        assert!(!tools_only.is_success());
        assert!(tools_only_destination.tools().is_empty());

        let mut destination = Router::new();
        destination
            .add_mcp_apps_ui_resource_with_behavior(
                AppsBoundResource {
                    mime_type: fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("destination Apps resource registers");
        destination
            .add_mcp_apps_tool_with_behavior(AppsLinkedTool, crate::DuplicateBehavior::Error)
            .expect("destination Apps tool registers");

        let mut incompatible_resource = Router::new();
        incompatible_resource
            .add_final_resource_with_behavior(
                AppsBoundResource {
                    mime_type: "text/plain",
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("non-Apps final resource is independently admissible");
        let replacement = destination.mount_resources_with_behavior(
            incompatible_resource,
            None,
            crate::DuplicateBehavior::Replace,
        );
        assert!(!replacement.is_success());
        assert_eq!(
            destination
                .final_resources
                .get("ui://weather/dashboard")
                .expect("rejected replacement retains the HTML resource")
                .definition
                .mime_type
                .as_deref(),
            Some(fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE)
        );

        let mut prefixed_source = Router::new();
        prefixed_source
            .add_mcp_apps_ui_resource_with_behavior(
                AppsBoundResource {
                    mime_type: fastmcp_protocol::MCP_APPS_HTML_MIME_TYPE,
                },
                crate::DuplicateBehavior::Error,
            )
            .expect("prefixed source Apps resource registers");
        prefixed_source
            .add_mcp_apps_tool_with_behavior(AppsLinkedTool, crate::DuplicateBehavior::Error)
            .expect("prefixed source Apps tool registers");
        let mut prefixed_destination = Router::new();
        let prefixed = prefixed_destination.mount(prefixed_source, Some("peer"));
        assert!(!prefixed.is_success());
        assert!(prefixed_destination.tools().is_empty());
        assert_eq!(prefixed_destination.resources().len(), 0);
    }
}
