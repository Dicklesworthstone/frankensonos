//! Authenticated, request-owned forwarding of modern core capabilities.
//!
//! The application supplies ONE managed upstream login and explicitly registers
//! the returned handlers. This is service-account delegation, not on-behalf-of
//! authentication: protect the gateway with its own authorization policy and
//! register only capabilities that downstream users may exercise with that
//! login. Incoming credentials and request metadata never become upstream HTTP
//! headers. The endpoint, refresh policy and credential custody stay inside
//! `ManagedOAuthSession`.
//!
//! Catalogs are collected completely, with the managed client's page, byte,
//! credential-generation and invalidation bounds, before any handlers are
//! returned. Registration still belongs to the normal server builder/router.
//! By default each invocation performs exactly one managed core POST on the
//! request-owned `Cx`. The opt-in [`interaction`] adapter can resolve upstream
//! input-required rounds through an explicitly supplied host callback, within
//! one bounded operation. Neither path retries a failed operation, holds a route
//! mutex, or starts a runtime. Native response admission, clean finite-SSE EOF,
//! expiry and cancellation checks remain in the managed client.
//!
//! The default provider advertises no reverse-input or extension capabilities.
//! Host-resolved interactions advertise only locally configured input support;
//! they do not relay upstream requestState to downstream clients. This provider
//! does not claim legacy execution, transparent downstream MRTR, or Tasks relay.
//! Unexpected input-required or extension results are never flattened.
//!
//! ```ignore
//! use fastmcp_server::providers::managed_oauth::ManagedOAuthProvider;
//! // `login` is an application-provisioned ManagedOAuthSession; `cx` belongs
//! // to the application's runtime. No token is copied into the server context.
//! let provider = ManagedOAuthProvider::new(login).with_namespace("upstream")?;
//! for tool in provider.tools(cx).await? {
//!     builder = builder.tool(tool);
//! }
//! for resource in provider.resources(cx).await? {
//!     builder = builder.resource(resource);
//! }
//! for prompt in provider.prompts(cx).await? {
//!     builder = builder.prompt(prompt);
//! }
//! ```

/// Authenticated, reversibly routed resource templates.
pub mod dynamic;
/// Opt-in, request-owned host resolution of upstream input-required workflows.
pub mod interaction;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::Cx;
use fastmcp_client::http_auth::managed::ManagedOAuthSession;
use fastmcp_client::http_auth::rpc::catalog::{
    ManagedCatalogClient, ManagedCatalogError, ManagedCatalogLimits,
};
use fastmcp_client::http_auth::rpc::{ManagedCoreError, ManagedCoreEvent, ManagedCoreLimits};
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult, Outcome};
use fastmcp_protocol::common_types::{OpenMetadata, RawIcon};
use fastmcp_protocol::protocol_policy::ProtocolEra;
use fastmcp_protocol::{
    CompleteResult, Content, CoreRequest, CoreResult, FinalCallToolResult,
    FinalCoreResult, FinalGetPromptResult, FinalPrompt, FinalReadResourceResult,
    FinalResource, FinalTool, Prompt, PromptMessage, RequestId, Resource,
    ResourceContent, ServerNotification, Tool,
    FINAL_CLIENT_CAPABILITIES_META_KEY, FINAL_PROTOCOL_VERSION,
    FINAL_PROTOCOL_VERSION_META_KEY,
};
use serde_json::{Value, json};

use crate::handler::{
    BoxFuture, FinalMethodOutcome, FinalResourceReadCacheHintProvenance,
    FinalToolOutcome, FinalToolSchemaAuthority, PromptHandler, ResourceHandler,
    ToolExecutionMode, ToolHandler, UpstreamFinalToolSchemaRegistration, UriParams,
};

const UPSTREAM_FAILURE: &str = "Authenticated upstream request failed";
const UNEXPECTED_RESULT: &str = "Authenticated upstream returned an unsupported result";
const MODERN_ASYNC_ONLY: &str = "Managed OAuth handlers require modern asynchronous dispatch";

/// An explicitly provisioned upstream and independently bounded catalog/call policies.
/// Clones share the same login and correlation-ID allocator, not response caches.
#[derive(Clone)]
pub struct ManagedOAuthProvider {
    session: ManagedOAuthSession,
    forwarder: Arc<Forwarder>,
    catalog_limits: ManagedCatalogLimits,
    namespace: Option<String>,
}

impl ManagedOAuthProvider {
    /// Construction performs no I/O. Registering the returned handlers delegates
    /// this login's authority; it does not forward the gateway caller's identity.
    pub fn new(session: ManagedOAuthSession) -> Self {
        Self {
            forwarder: Arc::new(Forwarder {
                backend: Arc::new(NativeBackend(session.clone())),
                next_id: Arc::new(AtomicU64::new(1)),
                limits: ManagedCoreLimits::default(),
            }),
            session,
            catalog_limits: ManagedCatalogLimits::default(),
            namespace: None,
        }
    }

    /// Selects bounds already validated by the managed client. Use this before
    /// sharing provider clones; existing clones and handlers retain their policy.
    /// An explicitly installed input handler is preserved with the new bounds.
    pub fn with_limits(mut self, calls: ManagedCoreLimits, catalogs: ManagedCatalogLimits) -> Self {
        self.forwarder = Arc::new(Forwarder {
            backend: Arc::clone(&self.forwarder.backend),
            next_id: Arc::clone(&self.forwarder.next_id),
            limits: calls,
        });
        self.catalog_limits = catalogs;
        self
    }

    /// Prefixes local tool and prompt names, never upstream names or resource URIs.
    /// Namespace admission happens before catalog or credential acquisition.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> McpResult<Self> {
        let namespace = namespace.into();
        if namespace.is_empty() || namespace.len() > 64
            || !namespace.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(McpError::invalid_params("Invalid managed OAuth provider namespace"));
        }
        self.namespace = Some(namespace);
        Ok(self)
    }

    /// Collects all tools or returns an error without a partial handler list.
    /// Schemas and final-only catalog metadata stay exact, including outputSchema.
    /// No remote advertisement is installed into a router by this method.
    pub async fn tools(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthTool>> {
        let pages = self.catalog(cx, "tools/list").await?;
        let mut entries = Vec::new();
        for page in pages {
            // FinalCoreResult carries STRUCT variants { result, diagnostic }; its
            // siblings LegacyCoreResult / FinalCoreRequest / LegacyCoreRequest /
            // McpAppsHostRequest all use the tuple form for the same variant name,
            // which is where the original shape came from. `diagnostic` is dropped
            // deliberately: this fn returns Vec<ManagedOAuthTool> and has nowhere to
            // carry a peer diagnostic. Propagating it upstream is a real question for
            // a FORWARDING provider and needs the managed-OAuth ownership decision,
            // not a signature change smuggled in under a compile fix.
            let CoreResult::Final(FinalCoreResult::ToolsList {
                result: page,
                diagnostic: _,
            }) = page
            else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(page.payload.tools);
        }
        build_tools(Arc::clone(&self.forwarder), entries, self.namespace.as_deref())
    }

    /// Collects the complete concrete-resource catalog before returning handlers.
    /// URIs are retained verbatim: a tool namespace never changes resource routing.
    /// Duplicate URIs fail the entire acquisition rather than shadowing a route.
    /// Templates and resource subscriptions are not installed by this method.
    pub async fn resources(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthResource>> {
        let pages = self.catalog(cx, "resources/list").await?;
        let mut entries = Vec::new();
        for page in pages {
            let CoreResult::Final(FinalCoreResult::ResourcesList { result: page, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(page.payload.resources);
        }
        build_resources(Arc::clone(&self.forwarder), entries)
    }

    /// Collects all prompts before exposing any registrable handlers.
    /// Final argument titles and absent-versus-false `required` values remain
    /// intact. A namespace changes only the published name, not the upstream
    /// request name or caller-supplied argument values.
    pub async fn prompts(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthPrompt>> {
        let pages = self.catalog(cx, "prompts/list").await?;
        let mut entries = Vec::new();
        for page in pages {
            let CoreResult::Final(FinalCoreResult::PromptsList { result: page, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(page.payload.prompts);
        }
        build_prompts(Arc::clone(&self.forwarder), entries, self.namespace.as_deref())
    }

    async fn catalog(&self, cx: &Cx, method: &str) -> McpResult<Vec<CoreResult>> {
        check_cx(cx)?;
        let request = core_request(method, json!({}), None)?;
        let collector = ManagedCatalogClient::new(self.session.clone(), self.catalog_limits);
        let collected = collector.collect(
            cx,
            request,
            || self.forwarder.allocate_id().map_err(|_| ManagedCatalogError::AbortedByHost),
            // No logging/subscription capabilities are advertised. A catalog
            // changed during materialization must not become a partial snapshot.
            |_| Err(ManagedCatalogError::AbortedByHost),
        ).await.map_err(|_| McpError::invalid_request("Authenticated upstream catalog acquisition failed"))?;
        check_cx(cx)?;
        Ok(collected.into_pages())
    }
}

/// One prompt from a fully collected authenticated upstream catalog.
/// Published names may be namespaced; execution always retains the original
/// upstream identity and returns the final prompt result without projection.
pub struct ManagedOAuthPrompt {
    forwarder: Arc<Forwarder>,
    upstream_name: String,
    definition: FinalPrompt,
}

impl ManagedOAuthPrompt {
    /// The immutable final catalog definition, including exact argument metadata.
    pub fn catalog_definition(&self) -> &FinalPrompt { &self.definition }

    async fn invoke(&self, ctx: &McpContext, cx: &Cx, arguments: HashMap<String, String>)
        -> McpResult<CompleteResult<FinalGetPromptResult>>
    {
        match self.forwarder.execute(
            ctx, cx, "prompts/get", json!({"name": self.upstream_name, "arguments": arguments}),
        ).await? {
            FinalCoreResult::PromptsGet { result, .. } => Ok(result),
            _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
        }
    }
}

impl PromptHandler for ManagedOAuthPrompt {
    // Legacy shape is registration-only. Modern catalogs use final_definition.
    fn definition(&self) -> Prompt {
        Prompt {
            name: self.definition.name.clone(),
            description: self.definition.description.clone(),
            arguments: self.definition.arguments.as_ref().map(|arguments| {
                arguments.iter().map(|argument| fastmcp_protocol::PromptArgument {
                    name: argument.name.clone(),
                    description: argument.description.clone(),
                    required: argument.required.unwrap_or(false),
                }).collect()
            }).unwrap_or_default(),
            icon: None, version: None, tags: Vec::new(),
        }
    }
    fn final_definition(&self) -> Option<FinalPrompt> { Some(self.definition.clone()) }
    fn final_title(&self) -> Option<&str> { self.definition.title.as_deref() }
    fn final_icons(&self) -> Option<&[RawIcon]> { self.definition.icons.as_deref() }
    fn final_metadata(&self) -> Option<&OpenMetadata> { self.definition.meta.as_ref() }
    fn get(&self, _ctx: &McpContext, _arguments: HashMap<String, String>) -> McpResult<Vec<PromptMessage>> {
        Err(McpError::invalid_request(MODERN_ASYNC_ONLY))
    }
    fn get_final_async<'a>(&'a self, ctx: &'a McpContext, arguments: HashMap<String, String>)
        -> BoxFuture<'a, McpOutcome<CompleteResult<FinalGetPromptResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), arguments).await) })
    }
    fn get_final_async_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalGetPromptResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, arguments).await) })
    }
    fn get_final_outcome_async<'a>(&'a self, ctx: &'a McpContext, arguments: HashMap<String, String>)
        -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), arguments).await.map(FinalMethodOutcome::Complete)) })
    }
    fn get_final_outcome_async_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, arguments).await.map(FinalMethodOutcome::Complete)) })
    }
}

fn build_prompts(forwarder: Arc<Forwarder>, entries: Vec<FinalPrompt>, namespace: Option<&str>)
    -> McpResult<Vec<ManagedOAuthPrompt>>
{
    let mut names = HashSet::new();
    let mut prompts = Vec::with_capacity(entries.len());
    for mut definition in entries {
        if !names.insert(definition.name.clone()) {
            return Err(McpError::invalid_request("Authenticated upstream catalog contains duplicate prompt names"));
        }
        let upstream_name = definition.name.clone();
        definition.name = published_name(namespace, &upstream_name)?;
        prompts.push(ManagedOAuthPrompt { forwarder: Arc::clone(&forwarder), upstream_name, definition });
    }
    Ok(prompts)
}

/// One concrete resource from a fully collected authenticated upstream catalog.
/// Its URI is an immutable route identity, not an arbitrary authenticated fetch
/// target. Reads use the managed MCP endpoint, never a direct fetch of that URI.
pub struct ManagedOAuthResource {
    forwarder: Arc<Forwarder>,
    definition: FinalResource,
}

impl ManagedOAuthResource {
    /// The exact final catalog entry, including size, annotations and metadata.
    pub fn catalog_definition(&self) -> &FinalResource { &self.definition }

    async fn invoke(&self, ctx: &McpContext, cx: &Cx, uri: &str)
        -> McpResult<CompleteResult<FinalReadResourceResult>>
    {
        // Direct callers must not widen a registered handler's authority by
        // supplying another URI. Refuse before allocating an ID or doing I/O.
        if uri != self.definition.uri.as_str() {
            return Err(McpError::invalid_params("Managed OAuth resource URI does not match its route"));
        }
        match self.forwarder.execute(ctx, cx, "resources/read", json!({"uri": uri})).await? {
            FinalCoreResult::ResourcesRead { result, .. } => Ok(result),
            _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
        }
    }
}

impl ResourceHandler for ManagedOAuthResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: self.definition.uri.as_str().to_owned(),
            name: self.definition.name.clone(),
            description: self.definition.description.clone(),
            mime_type: self.definition.mime_type.clone(),
            icon: None, version: None, tags: Vec::new(),
        }
    }
    fn final_definition(&self) -> Option<FinalResource> { Some(self.definition.clone()) }
    fn final_title(&self) -> Option<&str> { self.definition.title.as_deref() }
    fn final_icons(&self) -> Option<&[RawIcon]> { self.definition.icons.as_deref() }
    fn final_metadata(&self) -> Option<&OpenMetadata> { self.definition.meta.as_ref() }
    fn final_resource_read_cache_hint_provenance(&self) -> FinalResourceReadCacheHintProvenance {
        // Otherwise the router replaces the upstream TTL/scope with its defaults.
        FinalResourceReadCacheHintProvenance::Explicit
    }
    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Err(McpError::invalid_request(MODERN_ASYNC_ONLY))
    }
    fn on_subscribe(&self, _ctx: &McpContext, _uri: &str) -> McpResult<()> {
        Err(McpError::invalid_request("Managed OAuth resource subscriptions are not installed"))
    }
    fn on_unsubscribe(&self, _ctx: &McpContext, _uri: &str) -> McpResult<()> {
        Err(McpError::invalid_request("Managed OAuth resource subscriptions are not installed"))
    }
    fn read_final_async<'a>(&'a self, ctx: &'a McpContext)
        -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), self.definition.uri.as_str()).await) })
    }
    fn read_final_async_with_uri<'a>(&'a self, ctx: &'a McpContext, uri: &'a str, _params: &'a UriParams)
        -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), uri).await) })
    }
    fn read_final_outcome_async<'a>(&'a self, ctx: &'a McpContext)
        -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>>
    {
        Box::pin(async move {
            outcome(self.invoke(ctx, ctx.cx(), self.definition.uri.as_str()).await.map(FinalMethodOutcome::Complete))
        })
    }
    fn read_final_outcome_async_with_uri<'a>(&'a self, ctx: &'a McpContext, uri: &'a str, _params: &'a UriParams)
        -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), uri).await.map(FinalMethodOutcome::Complete)) })
    }
    fn read_final_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, _params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, uri).await) })
    }
    fn read_final_outcome_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, _params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, uri).await.map(FinalMethodOutcome::Complete)) })
    }
}

fn build_resources(forwarder: Arc<Forwarder>, entries: Vec<FinalResource>)
    -> McpResult<Vec<ManagedOAuthResource>>
{
    let mut uris = HashSet::new();
    let mut resources = Vec::with_capacity(entries.len());
    for definition in entries {
        if !uris.insert(definition.uri.as_str().to_owned()) {
            return Err(McpError::invalid_request("Authenticated upstream catalog contains duplicate resource URIs"));
        }
        resources.push(ManagedOAuthResource { forwarder: Arc::clone(&forwarder), definition });
    }
    Ok(resources)
}

/// A tool admitted from a fully collected authenticated upstream catalog.
/// Its private construction prevents an arbitrary local definition from minting
/// the server's sealed exact-proxy schema registration.
pub struct ManagedOAuthTool {
    forwarder: Arc<Forwarder>,
    upstream_name: String,
    definition: FinalTool,
}

impl ManagedOAuthTool {
    /// The immutable final catalog entry published by this handler.
    pub fn catalog_definition(&self) -> &FinalTool { &self.definition }

    async fn invoke(&self, ctx: &McpContext, cx: &Cx, arguments: Value)
        -> McpResult<CompleteResult<FinalCallToolResult>>
    {
        let result = self.forwarder.execute(
            ctx, cx, "tools/call", json!({"name": self.upstream_name, "arguments": arguments}),
        ).await?;
        tool_result(result)
    }
}

impl ToolHandler for ManagedOAuthTool {
    // Required legacy registration shape only; no lossy legacy execution is offered.
    fn definition(&self) -> Tool {
        Tool {
            name: self.definition.name.clone(),
            description: self.definition.description.clone(),
            input_schema: self.definition.input_schema.clone(),
            output_schema: self.definition.output_schema.clone(),
            icon: None, version: None, tags: Vec::new(), annotations: None,
        }
    }
    fn final_definition(&self) -> Option<FinalTool> { Some(self.definition.clone()) }
    fn final_title(&self) -> Option<&str> { self.definition.title.as_deref() }
    fn final_icons(&self) -> Option<&[RawIcon]> { self.definition.icons.as_deref() }
    fn final_metadata(&self) -> Option<&OpenMetadata> { self.definition.meta.as_ref() }
    fn output_schema(&self) -> Option<Value> { self.definition.output_schema.clone() }
    fn final_tool_schema_authority(&self) -> FinalToolSchemaAuthority { FinalToolSchemaAuthority::Upstream }
    fn upstream_final_tool_schema_registration(&self) -> Option<UpstreamFinalToolSchemaRegistration> {
        Some(UpstreamFinalToolSchemaRegistration::exact_proxy())
    }
    fn execution_mode(&self) -> ToolExecutionMode { ToolExecutionMode::Async }
    fn call(&self, _ctx: &McpContext, _arguments: Value) -> McpResult<Vec<Content>> {
        Err(McpError::invalid_request(MODERN_ASYNC_ONLY))
    }
    fn call_final_async<'a>(&'a self, ctx: &'a McpContext, arguments: Value)
        -> BoxFuture<'a, McpOutcome<CompleteResult<FinalCallToolResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), arguments).await) })
    }
    fn call_final_async_in_request<'a>(&'a self, ctx: &'a McpContext, cx: &'a Cx, arguments: Value)
        -> BoxFuture<'a, McpOutcome<CompleteResult<FinalCallToolResult>>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, cx, arguments).await) })
    }
    fn call_final_outcome_async<'a>(&'a self, ctx: &'a McpContext, arguments: Value)
        -> BoxFuture<'a, McpOutcome<FinalToolOutcome>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), arguments).await.map(FinalToolOutcome::Complete)) })
    }
    fn call_final_outcome_async_in_request<'a>(&'a self, ctx: &'a McpContext, cx: &'a Cx, arguments: Value)
        -> BoxFuture<'a, McpOutcome<FinalToolOutcome>>
    {
        Box::pin(async move { outcome(self.invoke(ctx, cx, arguments).await.map(FinalToolOutcome::Complete)) })
    }
}

fn build_tools(forwarder: Arc<Forwarder>, entries: Vec<FinalTool>, namespace: Option<&str>)
    -> McpResult<Vec<ManagedOAuthTool>>
{
    let mut names = HashSet::new();
    let mut tools = Vec::with_capacity(entries.len());
    for mut definition in entries {
        if !names.insert(definition.name.clone()) {
            return Err(McpError::invalid_request("Authenticated upstream catalog contains duplicate tool names"));
        }
        let upstream_name = definition.name.clone();
        definition.name = published_name(namespace, &upstream_name)?;
        tools.push(ManagedOAuthTool { forwarder: Arc::clone(&forwarder), upstream_name, definition });
    }
    Ok(tools)
}

fn published_name(namespace: Option<&str>, original: &str) -> McpResult<String> {
    let name = namespace.map_or_else(|| original.to_owned(), |prefix| format!("{prefix}/{original}"));
    if original.is_empty() || name.len() > 128 {
        return Err(McpError::invalid_request("Managed OAuth provider name exceeds its bound"));
    }
    Ok(name)
}

fn core_request(method: &str, mut parameters: Value, progress: Option<Value>) -> McpResult<CoreRequest> {
    let object = parameters.as_object_mut().ok_or_else(|| McpError::invalid_params("Invalid upstream parameters"))?;
    let mut metadata = serde_json::Map::new();
    metadata.insert(FINAL_PROTOCOL_VERSION_META_KEY.to_owned(), json!(FINAL_PROTOCOL_VERSION));
    metadata.insert(FINAL_CLIENT_CAPABILITIES_META_KEY.to_owned(), json!({}));
    if let Some(progress) = progress { metadata.insert("progressToken".to_owned(), progress); }
    // Replace, never merge, caller metadata. Nested tool arguments remain data.
    object.insert("_meta".to_owned(), Value::Object(metadata));
    CoreRequest::decode(ProtocolEra::Modern2026, method, Some(&parameters))
        .map_err(|_| McpError::invalid_params("Invalid managed OAuth core request"))
}

struct Forwarder {
    backend: Arc<dyn CoreBackend>,
    next_id: Arc<AtomicU64>,
    limits: ManagedCoreLimits,
}

fn allocate_request_id(next_id: &AtomicU64) -> McpResult<RequestId> {
    let id = next_id
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| McpError::internal_error("Managed OAuth request ID space exhausted"))?;
    Ok(RequestId::String(format!("managed-provider-{id}")))
}

impl Forwarder {
    fn allocate_id(&self) -> McpResult<RequestId> {
        allocate_request_id(&self.next_id)
    }

    async fn execute(&self, ctx: &McpContext, cx: &Cx, method: &str, parameters: Value)
        -> McpResult<FinalCoreResult>
    {
        ctx.checkpoint()?;
        check_cx(cx)?;
        let progress = ctx.progress_marker().map(serde_json::to_value).transpose()
            .map_err(|_| McpError::invalid_params("Invalid upstream progress marker"))?;
        let request = core_request(method, parameters, progress)?;
        let id = self.allocate_id()?;
        let result = self.backend.execute(ctx, cx, request, id, self.limits).await;
        // A late success cannot cross either cancellation boundary.
        check_cx(cx)?;
        ctx.checkpoint()?;
        result
    }
}

// Production construction installs NativeBackend or the opt-in interaction
// adapter. Test injection is private and cannot grant a schema bypass.
trait CoreBackend: Send + Sync {
    fn execute<'a>(&'a self, ctx: &'a McpContext, cx: &'a Cx, request: CoreRequest,
        id: RequestId, limits: ManagedCoreLimits) -> BoxFuture<'a, McpResult<FinalCoreResult>>;
}

struct NativeBackend(ManagedOAuthSession);

impl CoreBackend for NativeBackend {
    fn execute<'a>(&'a self, ctx: &'a McpContext, cx: &'a Cx, request: CoreRequest,
        id: RequestId, limits: ManagedCoreLimits) -> BoxFuture<'a, McpResult<FinalCoreResult>>
    {
        Box::pin(async move {
            let cancellation = ctx.request_cancellation();
            let mut call = self.0.request_core_with_cancellation(cx, &cancellation, request, id, limits)
                .await.map_err(upstream_error)?;
            while let Some(event) = call.next_event(cx).await.map_err(upstream_error)? {
                ctx.checkpoint()?;
                match event {
                    ManagedCoreEvent::Result(result) => return match *result {
                        CoreResult::Final(result) => Ok(result),
                        _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
                    },
                    ManagedCoreEvent::Notification(notification) => forward_notification(ctx, *notification)?,
                }
            }
            Err(McpError::invalid_request(UNEXPECTED_RESULT))
        })
    }
}

fn forward_notification(ctx: &McpContext, notification: ServerNotification) -> McpResult<()> {
    let ServerNotification::Progress(progress) = notification else {
        // No log-level or subscription metadata was advertised. Never relay an
        // unsolicited log containing the upstream service account's private data.
        return Err(McpError::invalid_request("Unexpected authenticated upstream notification"));
    };
    let exact = |value| serde_json::to_value(value).and_then(serde_json::from_value::<serde_json::Number>)
        .map_err(|_| McpError::invalid_request("Invalid upstream progress number"));
    let amount = exact(progress.progress)?;
    let total = progress.total.map(exact).transpose()?;
    ctx.report_progress_exact(amount, total, progress.message.as_deref());
    Ok(())
}

fn upstream_error(error: ManagedCoreError) -> McpError {
    match error {
        ManagedCoreError::Cancelled | ManagedCoreError::TimedOut => McpError::request_cancelled(),
        _ => McpError::invalid_request(UPSTREAM_FAILURE),
    }
}
fn tool_result(result: FinalCoreResult) -> McpResult<CompleteResult<FinalCallToolResult>> {
    match result {
        // Struct variant, same reasoning as the tools/list site above.
        FinalCoreResult::ToolsCall {
            result,
            diagnostic: _,
        } => Ok(result),
        _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
    }
}
fn check_cx(cx: &Cx) -> McpResult<()> {
    cx.checkpoint().map_err(|_| McpError::request_cancelled())
}
fn outcome<T>(result: McpResult<T>) -> McpOutcome<T> {
    match result { Ok(value) => Outcome::Ok(value), Err(error) => Outcome::Err(error) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use fastmcp_core::block_on;
    use fastmcp_protocol::ResultMeta;

    struct Backend {
        calls: Mutex<Vec<(RequestId, Value)>>,
        responses: Mutex<VecDeque<FinalCoreResult>>,
        cancel_on_return: bool,
    }
    impl CoreBackend for Backend {
        fn execute<'a>(&'a self, ctx: &'a McpContext, _cx: &'a Cx, request: CoreRequest,
            id: RequestId, _limits: ManagedCoreLimits) -> BoxFuture<'a, McpResult<FinalCoreResult>>
        {
            Box::pin(async move {
                self.calls.lock().unwrap().push((id, request.encode_params().unwrap().unwrap()));
                if self.cancel_on_return { ctx.request_cancellation().cancel(); }
                self.responses.lock().unwrap().pop_front().ok_or_else(|| McpError::internal_error("test backend exhausted"))
            })
        }
    }
    fn definition() -> FinalTool {
        serde_json::from_value(json!({
            "name":"lookup", "title":"Exact title", "description":"Remote tool",
            "inputSchema":{"type":"object","properties":{"key":{"type":"string"}}},
            "outputSchema":{"type":"object","properties":{"answer":{"type":"integer"}}},
            "annotations":{"title":"Annotation title","readOnlyHint":true},
            "_meta":{"com.example/source":{"revision":7}}
        })).unwrap()
    }
    fn response(error: bool) -> FinalCoreResult {
        // Struct variant, third instance of the sibling-enum shape carry-over.
        // CONSTRUCTION, not a pattern: `diagnostic` needs a VALUE here, so the
        // `diagnostic: _` used at the two match sites does not apply. `None` is
        // the in-tree idiom for "no peer diagnostic" (fastmcp-client/src/lib.rs
        // :39172, :39415) and this canned test response exercises no diagnostic
        // path -- it preserves the behaviour the tuple form had.
        FinalCoreResult::ToolsCall {
            result: CompleteResult::new(
                FinalCallToolResult {
                    content: Vec::new(),
                    is_error: error,
                    structured_content: Some(json!({"answer":42})),
                },
                ResultMeta::empty(),
            ),
            diagnostic: None,
        }
    }
    fn fixture(responses: Vec<FinalCoreResult>, cancel: bool) -> (ManagedOAuthTool, Arc<Backend>) {
        let backend = Arc::new(Backend { calls: Mutex::new(Vec::new()), responses: Mutex::new(responses.into()), cancel_on_return: cancel });
        let forwarder = Arc::new(Forwarder { backend: backend.clone(), next_id: Arc::new(AtomicU64::new(1)), limits: ManagedCoreLimits::default() });
        let tool = build_tools(forwarder, vec![definition()], Some("remote")).unwrap().pop().unwrap();
        (tool, backend)
    }

    #[test]
    fn authenticated_tool_keeps_exact_catalog_and_upstream_schema_authority() {
        let (tool, _) = fixture(vec![], false);
        let mut expected = definition();
        expected.name = "remote/lookup".to_owned();
        assert_eq!(serde_json::to_value(tool.final_definition().unwrap()).unwrap(), serde_json::to_value(expected).unwrap());
        assert!(tool.upstream_final_tool_schema_registration().is_some());
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Async);
        assert!(!tool.declares_final_tasks());
        assert!(!tool.declares_final_mrtr());
    }

    #[test]
    fn owned_tool_call_rewrites_only_the_name_and_preserves_structured_result() {
        let (tool, backend) = fixture(vec![response(false)], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let result = block_on(tool.call_final_async_in_request(&ctx, &cx, json!({"key":"abc","_meta":{"user_data":true}}))).unwrap();
        assert_eq!(result.payload.structured_content, Some(json!({"answer":42})));
        assert!(!result.payload.is_error);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["name"], "lookup");
        assert_eq!(calls[0].1["arguments"]["_meta"], json!({"user_data":true}));
        assert_eq!(calls[0].1["_meta"][FINAL_CLIENT_CAPABILITIES_META_KEY], json!({}));
    }

    #[test]
    fn tool_error_result_is_not_converted_to_success_or_retried() {
        let (tool, backend) = fixture(vec![response(true)], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let result = block_on(tool.call_final_async_in_request(&ctx, &cx, json!({}))).unwrap();
        assert!(result.payload.is_error);
        assert_eq!(result.payload.structured_content, Some(json!({"answer":42})));
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn metadata_is_rebuilt_instead_of_forwarding_credentials_or_extensions() {
        let request = core_request("tools/call", json!({"name":"lookup","arguments":{},"_meta":{
            "authorization":"Bearer secret", "io.modelcontextprotocol/clientCapabilities":{"extensions":{"evil":{}}}
        }}), None).unwrap();
        let params = request.encode_params().unwrap().unwrap();
        assert_eq!(params["_meta"].as_object().unwrap().len(), 2);
        assert!(!serde_json::to_string(&params).unwrap().contains("secret"));
        assert_eq!(params["_meta"][FINAL_CLIENT_CAPABILITIES_META_KEY], json!({}));
    }

    #[test]
    fn malformed_arguments_never_enter_the_backend() {
        let (tool, backend) = fixture(vec![response(false)], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        assert!(block_on(tool.call_final_async_in_request(&ctx, &cx, json!([1,2]))).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_before_dispatch_never_enters_the_backend() {
        let (tool, backend) = fixture(vec![response(false)], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        ctx.request_cancellation().cancel();
        assert!(block_on(tool.call_final_async_in_request(&ctx, &cx, json!({}))).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn late_success_is_refused_after_request_cancellation() {
        let (tool, backend) = fixture(vec![response(false)], true);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        assert!(block_on(tool.call_final_async_in_request(&ctx, &cx, json!({}))).is_err());
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn legacy_calls_do_not_start_modern_network_work() {
        let (tool, backend) = fixture(vec![response(false)], false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(tool.call(&ctx, json!({})).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_catalog_entries_are_refused_without_partial_handlers() {
        let (tool, backend) = fixture(vec![], false);
        assert!(build_tools(tool.forwarder, vec![definition(), definition()], None).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn request_ids_are_unique_and_exhaustion_never_wraps() {
        let (tool, _) = fixture(vec![], false);
        let first = tool.forwarder.allocate_id().unwrap();
        let second = tool.forwarder.allocate_id().unwrap();
        assert!(!first.correlates_with(&second));
        tool.forwarder.next_id.store(u64::MAX, Ordering::Relaxed);
        assert!(tool.forwarder.allocate_id().is_err());
        assert_eq!(tool.forwarder.next_id.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn oversized_names_are_rejected_instead_of_truncated() {
        assert!(published_name(Some("namespace"), &"x".repeat(128)).is_err());
        assert_eq!(published_name(Some("ns"), "tool").unwrap(), "ns/tool");
        assert!(published_name(None, "").is_err());
    }

    #[test]
    fn upstream_failures_are_redacted() {
        let message = upstream_error(ManagedCoreError::InvalidResponse).to_string();
        assert!(message.contains(UPSTREAM_FAILURE));
        assert!(!message.contains("token"));
    }

    fn resource_definition(uri: &str) -> FinalResource {
        serde_json::from_value(json!({
            "uri": uri, "name": "Report", "title": "Quarterly report",
            "description": "Remote document", "mimeType": "text/plain", "size": 42,
            "icons": [{"src":"https://example.com/report.png","mimeType":"image/png"}],
            "annotations": {"audience":["assistant"],"priority":0.5},
            "_meta": {"com.example/source":{"revision":9}}
        })).unwrap()
    }

    fn resource_response() -> FinalCoreResult {
        let request = core_request("resources/read", json!({"uri":"file:///report"}), None).unwrap();
        let CoreResult::Final(result) = request.decode_result(r#"{
            "resultType":"complete",
            "contents":[{"uri":"file:///report","text":"first"},{"uri":"file:///attachment","blob":"AAEC"}],
            "ttlMs":1234,"cacheScope":"private","_meta":{"com.example/revision":9},
            "x-exact":{"z":900719925474099312345,"a":1.20e+4}
        }"#).unwrap() else { panic!("expected final resource result") };
        result
    }

    fn resource_fixture(responses: Vec<FinalCoreResult>, cancel: bool)
        -> (ManagedOAuthResource, Arc<Backend>)
    {
        let (tool, backend) = fixture(responses, cancel);
        let resource = build_resources(tool.forwarder, vec![resource_definition("file:///report")])
            .unwrap().pop().unwrap();
        (resource, backend)
    }

    #[test]
    fn authenticated_resource_preserves_catalog_and_explicit_cache_authority() {
        let (resource, _) = resource_fixture(vec![], false);
        assert_eq!(
            serde_json::to_value(resource.final_definition().unwrap()).unwrap(),
            serde_json::to_value(resource_definition("file:///report")).unwrap(),
        );
        assert_eq!(resource.definition().uri, "file:///report");
        assert_eq!(resource.final_resource_read_cache_hint_provenance(), FinalResourceReadCacheHintProvenance::Explicit);
        assert!(!resource.declares_final_mrtr());
        assert!(resource.template().is_none());
    }

    #[test]
    fn owned_resource_read_preserves_contents_cache_hints_metadata_and_exact_extras() {
        let expected = CoreResult::Final(resource_response()).encode().unwrap();
        let (resource, backend) = resource_fixture(vec![resource_response()], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let result = block_on(resource.read_final_async_with_uri_in_request(
            &ctx, &cx, "file:///report", &UriParams::new(),
        )).unwrap();
        let actual = CoreResult::Final(FinalCoreResult::ResourcesRead { result, diagnostic: None }).encode().unwrap();
        assert_eq!(actual, expected);
        assert!(actual.contains("900719925474099312345") && actual.contains("1.20e+4"));
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["uri"], "file:///report");
        assert_eq!(calls[0].1["_meta"][FINAL_CLIENT_CAPABILITIES_META_KEY], json!({}));
    }

    #[test]
    fn resource_uri_substitution_refuses_before_id_allocation_or_io() {
        let (resource, backend) = resource_fixture(vec![resource_response()], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        assert!(block_on(resource.read_final_async_with_uri_in_request(
            &ctx, &cx, "file:///another-principal", &UriParams::new(),
        )).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(backend.responses.lock().unwrap().len(), 1);
        assert_eq!(resource.forwarder.next_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resource_catalog_collision_uses_uri_not_display_name() {
        let (resource, _) = resource_fixture(vec![], false);
        let first = resource_definition("file:///report");
        let different_uri = resource_definition("file:///other");
        assert_eq!(build_resources(Arc::clone(&resource.forwarder), vec![first.clone(), different_uri]).unwrap().len(), 2);
        let mut same_uri = first.clone();
        same_uri.name = "Another name".to_owned();
        assert!(build_resources(resource.forwarder, vec![first, same_uri]).is_err());
    }

    #[test]
    fn resource_async_entry_points_all_preserve_the_final_result() {
        let (resource, backend) = resource_fixture(vec![resource_response(); 5], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let params = UriParams::from([("unused".to_owned(), "value".to_owned())]);
        assert!(block_on(resource.read_final_async(&ctx)).is_ok());
        assert!(block_on(resource.read_final_async_with_uri(&ctx, "file:///report", &params)).is_ok());
        assert!(matches!(block_on(resource.read_final_outcome_async(&ctx)).unwrap(), FinalMethodOutcome::Complete(_)));
        assert!(matches!(block_on(resource.read_final_outcome_async_with_uri(&ctx, "file:///report", &params)).unwrap(), FinalMethodOutcome::Complete(_)));
        assert!(matches!(block_on(resource.read_final_outcome_async_with_uri_in_request(&ctx, &cx, "file:///report", &params)).unwrap(), FinalMethodOutcome::Complete(_)));
        assert_eq!(backend.calls.lock().unwrap().len(), 5);
    }

    #[test]
    fn resource_input_required_is_not_flattened_or_retried() {
        let request = core_request("resources/read", json!({"uri":"file:///report"}), None).unwrap();
        let CoreResult::Final(input) = request.decode_result(r#"{"resultType":"input_required","requestState":"opaque"}"#).unwrap()
            else { panic!("expected final input-required result") };
        for response in [input, response(false)] {
            let (resource, backend) = resource_fixture(vec![response, resource_response()], false);
            let cx = Cx::for_testing();
            let ctx = McpContext::new(cx.clone(), 1);
            assert!(block_on(resource.read_final_outcome_async_with_uri_in_request(
                &ctx, &cx, "file:///report", &UriParams::new(),
            )).is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), 1);
            assert_eq!(backend.responses.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn resource_pre_cancel_and_late_cancel_prevent_result_delivery() {
        for cancel_on_return in [false, true] {
            let (resource, backend) = resource_fixture(vec![resource_response()], cancel_on_return);
            let ctx = McpContext::new(Cx::for_testing(), 1);
            if !cancel_on_return { ctx.request_cancellation().cancel(); }
            assert!(block_on(resource.read_final_async(&ctx)).is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), usize::from(cancel_on_return));
        }
    }

    #[test]
    fn resource_legacy_and_subscription_calls_never_start_network_work() {
        let (resource, backend) = resource_fixture(vec![resource_response()], false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(resource.read(&ctx).is_err());
        assert!(resource.on_subscribe(&ctx, "file:///report").is_err());
        assert!(resource.on_unsubscribe(&ctx, "file:///report").is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    fn prompt_definition() -> FinalPrompt {
        serde_json::from_value(json!({
            "name":"summarize", "title":"Summarize a report", "description":"Remote prompt",
            "arguments":[
                {"name":"report","title":"Report text","required":true},
                {"name":"style","title":"Writing style"},
                {"name":"language","title":"Output language","required":false}
            ],
            "icons":[{"src":"https://example.com/prompt.png","mimeType":"image/png"}],
            "_meta":{"com.example/prompt":{"revision":3}}
        })).unwrap()
    }

    fn prompt_response() -> FinalCoreResult {
        let request = core_request("prompts/get", json!({"name":"summarize","arguments":{"report":"text"}}), None).unwrap();
        let CoreResult::Final(result) = request.decode_result(r#"{
            "resultType":"complete","description":"An exact upstream prompt",
            "messages":[
                {"role":"user","content":{"type":"text","text":"Summarize this","_meta":{"com.example/block":true}}},
                {"role":"assistant","content":{"type":"text","text":"Ready"}}
            ],
            "_meta":{"com.example/revision":3},
            "x-exact":{"z":900719925474099312345,"a":1.20e+4}
        }"#).unwrap() else { panic!("expected final prompt result") };
        result
    }

    fn prompt_fixture(responses: Vec<FinalCoreResult>, cancel: bool)
        -> (ManagedOAuthPrompt, Arc<Backend>)
    {
        let (tool, backend) = fixture(responses, cancel);
        let prompt = build_prompts(tool.forwarder, vec![prompt_definition()], Some("remote"))
            .unwrap().pop().unwrap();
        (prompt, backend)
    }

    #[test]
    fn authenticated_prompt_catalog_retains_argument_titles_and_required_presence() {
        let (prompt, _) = prompt_fixture(vec![], false);
        let mut expected = prompt_definition();
        expected.name = "remote/summarize".to_owned();
        let actual = serde_json::to_value(prompt.final_definition().unwrap()).unwrap();
        assert_eq!(actual, serde_json::to_value(expected).unwrap());
        assert_eq!(actual["arguments"][0]["title"], "Report text");
        assert!(actual["arguments"][1].get("required").is_none());
        assert_eq!(actual["arguments"][2]["required"], false);
        assert_eq!(prompt.definition().name, "remote/summarize");
        assert!(!prompt.declares_final_mrtr());
    }

    #[test]
    fn owned_prompt_get_rewrites_only_name_and_preserves_full_final_result() {
        let expected = CoreResult::Final(prompt_response()).encode().unwrap();
        let (prompt, backend) = prompt_fixture(vec![prompt_response()], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let arguments = HashMap::from([
            ("report".to_owned(), "Unicode: 日本語\n{not metadata}".to_owned()),
            ("_meta".to_owned(), "ordinary prompt data".to_owned()),
        ]);
        let result = block_on(prompt.get_final_async_in_request(&ctx, &cx, arguments.clone())).unwrap();
        let actual = CoreResult::Final(FinalCoreResult::PromptsGet { result, diagnostic: None }).encode().unwrap();
        assert_eq!(actual, expected);
        assert!(actual.contains("900719925474099312345") && actual.contains("1.20e+4"));
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["name"], "summarize");
        assert_eq!(calls[0].1["arguments"], serde_json::to_value(arguments).unwrap());
        assert_eq!(calls[0].1["_meta"][FINAL_CLIENT_CAPABILITIES_META_KEY], json!({}));
    }

    #[test]
    fn prompt_async_entry_points_return_complete_outcomes_without_legacy_projection() {
        let (prompt, backend) = prompt_fixture(vec![prompt_response(); 3], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let arguments = HashMap::from([("report".to_owned(), "text".to_owned())]);
        assert!(block_on(prompt.get_final_async(&ctx, arguments.clone())).is_ok());
        assert!(matches!(block_on(prompt.get_final_outcome_async(&ctx, arguments.clone())).unwrap(), FinalMethodOutcome::Complete(_)));
        assert!(matches!(block_on(prompt.get_final_outcome_async_in_request(&ctx, &cx, arguments)).unwrap(), FinalMethodOutcome::Complete(_)));
        assert_eq!(backend.calls.lock().unwrap().len(), 3);
    }

    #[test]
    fn prompt_catalog_duplicate_and_oversized_names_fail_without_partial_handlers() {
        let (prompt, backend) = prompt_fixture(vec![], false);
        assert!(build_prompts(Arc::clone(&prompt.forwarder), vec![prompt_definition(), prompt_definition()], None).is_err());
        let mut oversized = prompt_definition();
        oversized.name = "x".repeat(128);
        assert!(build_prompts(prompt.forwarder, vec![prompt_definition(), oversized], Some("remote")).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_namespace_cannot_hide_an_empty_upstream_tool_or_prompt_name() {
        let (tool, _) = fixture(vec![], false);
        let mut bad_tool = definition();
        bad_tool.name.clear();
        let mut bad_prompt = prompt_definition();
        bad_prompt.name.clear();
        assert!(build_tools(Arc::clone(&tool.forwarder), vec![bad_tool], Some("remote")).is_err());
        assert!(build_prompts(tool.forwarder, vec![bad_prompt], Some("remote")).is_err());
    }

    #[test]
    fn prompt_input_required_and_wrong_method_results_never_trigger_retry() {
        let request = core_request("prompts/get", json!({"name":"summarize"}), None).unwrap();
        let CoreResult::Final(input) = request.decode_result(r#"{"resultType":"input_required","requestState":"opaque"}"#).unwrap()
            else { panic!("expected final input-required result") };
        for response in [input, resource_response()] {
            let (prompt, backend) = prompt_fixture(vec![response, prompt_response()], false);
            let cx = Cx::for_testing();
            let ctx = McpContext::new(cx.clone(), 1);
            let arguments = HashMap::from([("report".to_owned(), "text".to_owned())]);
            assert!(block_on(prompt.get_final_outcome_async_in_request(&ctx, &cx, arguments)).is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), 1);
            assert_eq!(backend.responses.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn prompt_pre_cancel_and_late_cancel_prevent_result_delivery() {
        for cancel_on_return in [false, true] {
            let (prompt, backend) = prompt_fixture(vec![prompt_response()], cancel_on_return);
            let ctx = McpContext::new(Cx::for_testing(), 1);
            if !cancel_on_return { ctx.request_cancellation().cancel(); }
            let arguments = HashMap::from([("report".to_owned(), "text".to_owned())]);
            assert!(block_on(prompt.get_final_async(&ctx, arguments)).is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), usize::from(cancel_on_return));
        }
    }

    #[test]
    fn prompt_legacy_sync_and_async_calls_do_not_start_network_work() {
        let (prompt, backend) = prompt_fixture(vec![prompt_response()], false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(prompt.get(&ctx, HashMap::new()).is_err());
        assert!(block_on(prompt.get_async(&ctx, HashMap::new())).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn tool_resource_and_prompt_handlers_share_one_correlation_id_allocator() {
        let (tool, backend) = fixture(vec![response(false), resource_response(), prompt_response()], false);
        let resource = build_resources(Arc::clone(&tool.forwarder), vec![resource_definition("file:///report")])
            .unwrap().pop().unwrap();
        let prompt = build_prompts(Arc::clone(&tool.forwarder), vec![prompt_definition()], None)
            .unwrap().pop().unwrap();
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(tool.call_final_async(&ctx, json!({}))).is_ok());
        assert!(block_on(resource.read_final_async(&ctx)).is_ok());
        assert!(block_on(prompt.get_final_async(&ctx, HashMap::from([("report".to_owned(), "text".to_owned())]))).is_ok());
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        for (index, (id, _)) in calls.iter().enumerate() {
            assert!(calls[..index].iter().all(|(previous, _)| !id.correlates_with(previous)));
        }
    }
}
