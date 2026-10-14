//! Machine-authenticated catalogs using the existing exact managed handlers.
//!
//! This implementation shares the private catalog constructors with dynamic
//! routing. Applications use `providers::ClientCredentialsProvider`.

mod interaction;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use asupersync::Cx;
use fastmcp_client::http_auth::discovery::OAuthDiscoveryError;
use fastmcp_client::http_auth::discovery::client_credentials::{
    ClientCredentialsClient, ClientCredentialsError,
};
use fastmcp_client::http_auth::discovery::client_credentials::rpc::{
    ClientCredentialsCoreCall, ClientCredentialsCoreError,
};
use fastmcp_client::http_auth::discovery::client_credentials::rpc::catalog::{
    ClientCredentialsCatalogClient, ClientCredentialsCatalogError, ClientCredentialsCatalogLimits,
};
use fastmcp_client::http_auth::rpc::{ManagedCoreEvent, ManagedCoreLimits};
use fastmcp_client::http_auth::rpc::catalog::ManagedCatalogError;
use fastmcp_core::{McpContext, McpError, McpResult};
use fastmcp_protocol::{CoreRequest, CoreResult, FinalCoreResult, RequestId};
use serde_json::json;

use interaction::{InteractiveMachineResponse, MachineInputs, interaction_error};
use super::{ManagedOAuthResourceTemplate, build_templates};
use super::super::{
    BoxFuture, CoreBackend, Forwarder, ManagedOAuthPrompt, ManagedOAuthResource, ManagedOAuthTool,
    UNEXPECTED_RESULT, allocate_request_id, build_prompts, build_resources, build_tools,
    check_cx, core_request, forward_notification, upstream_error,
};

const MACHINE_FAILURE: &str = "Machine-authenticated upstream request failed";
const CATALOG_FAILURE: &str = "Machine-authenticated upstream catalog acquisition failed";

/// Expose an explicitly provisioned machine identity through ordinary MCP handlers.
///
/// Unlike `ManagedOAuthProvider`, this does not require an interactive login.
/// Supply an already configured `ClientCredentialsClient`: its issuer, endpoint,
/// scopes, authentication method, credential expiry and revocation remain its
/// own authority. All supported machine authentication methods use the same path.
/// Each upstream operation performs that client's same-token authenticated
/// discovery before dispatch; neither this provider nor its catalog bypasses it.
///
/// This is service-account delegation, NOT on-behalf-of authentication. Protect
/// the gateway with its own authorization policy and register only capabilities
/// downstream callers may exercise as this machine. Downstream credentials and
/// metadata are never used as upstream HTTP headers. Nested tool arguments stay
/// application data. A provider/handler clone keeps the credential owner alive;
/// the application's explicit client close still retires that shared owner.
///
/// Catalog acquisition returns a whole collection or an error, never partial
/// handlers. Each category has its own bounded collection; separate acquisitions
/// are not an atomic multi-category snapshot. Namespace rewriting, schema
/// authority, exact results, resource URI matching and completion routing reuse
/// the same handlers as the interactive provider, rather than a parallel router.
///
/// By default only complete modern core results are forwarded. Explicitly
/// installing `with_input_handler` enables bounded host-resolved input-required
/// rounds, not transparent downstream continuation state. Neither path installs
/// legacy execution, Tasks or subscriptions. No failed POST is retried and no
/// runtime is created.
///
/// ```ignore
/// use fastmcp_server::providers::ClientCredentialsProvider;
/// let provider = ClientCredentialsProvider::new(machine_client).with_namespace("service")?;
/// for tool in provider.tools(cx).await? {
///     builder = builder.tool(tool);
/// }
/// for prompt in provider.prompts(cx).await? {
///     let name = prompt.catalog_definition().name.clone();
///     let completion = prompt.completion_handler()?;
///     builder = builder.prompt(prompt).prompt_completion_handler(name, completion);
/// }
/// ```
#[derive(Clone)]
pub struct ClientCredentialsProvider {
    source: Arc<dyn MachineSource>,
    forwarder: Arc<Forwarder>,
    catalog_limits: ClientCredentialsCatalogLimits,
    namespace: Option<String>,
}

impl ClientCredentialsProvider {
    /// Construction performs no discovery, grant acquisition or network work.
    pub fn new(client: ClientCredentialsClient) -> Self {
        Self::from_source(Arc::new(NativeMachineSource(client)))
    }

    fn from_source(source: Arc<dyn MachineSource>) -> Self {
        let next_id = Arc::new(AtomicU64::new(1));
        Self {
            forwarder: Arc::new(Forwarder {
                backend: Arc::new(MachineBackend {
                    source: Arc::clone(&source),
                    next_id: Arc::clone(&next_id),
                    inputs: None,
                }),
                next_id,
                limits: ManagedCoreLimits::default(),
            }),
            source,
            catalog_limits: ClientCredentialsCatalogLimits::default(),
            namespace: None,
        }
    }

    /// Policies are already validated by the native client. Existing clones and
    /// registered handlers retain their policy. All clones still share one ID
    /// allocator for BOTH discovery and operation IDs, including catalog pages.
    /// An explicitly installed input handler is retained with the new limits.
    pub fn with_limits(
        mut self, calls: ManagedCoreLimits, catalogs: ClientCredentialsCatalogLimits,
    ) -> Self {
        self.forwarder = Arc::new(Forwarder {
            backend: Arc::clone(&self.forwarder.backend),
            next_id: Arc::clone(&self.forwarder.next_id),
            limits: calls,
        });
        self.catalog_limits = catalogs;
        self
    }

    /// Prefix local tool/prompt names only. Resource identities and upstream
    /// request names are unchanged. Invalid namespaces fail before any grant.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> McpResult<Self> {
        let namespace = namespace.into();
        if namespace.is_empty() || namespace.len() > 64
            || !namespace.bytes().all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(McpError::invalid_params("Invalid machine provider namespace"));
        }
        self.namespace = Some(namespace);
        Ok(self)
    }

    /// Fully collect and validate the tool catalog before exposing handlers.
    /// Exact upstream input/output schemas remain the normal proxy authority.
    pub async fn tools(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthTool>> {
        let mut entries = Vec::new();
        for page in self.catalog(cx, "tools/list").await? {
            let CoreResult::Final(FinalCoreResult::ToolsList { result, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(result.payload.tools);
        }
        build_tools(Arc::clone(&self.forwarder), entries, self.namespace.as_deref())
    }

    /// Fully collect concrete resources. Duplicate URIs reject the whole result.
    /// Reads remain MCP operations against the configured endpoint, not URL fetches.
    pub async fn resources(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthResource>> {
        let mut entries = Vec::new();
        for page in self.catalog(cx, "resources/list").await? {
            let CoreResult::Final(FinalCoreResult::ResourcesList { result, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(result.payload.resources);
        }
        build_resources(Arc::clone(&self.forwarder), entries)
    }

    /// Fully collect prompts, retaining exact argument and catalog metadata.
    /// Returned prompts also provide their route-bound `completion_handler()`.
    pub async fn prompts(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthPrompt>> {
        let mut entries = Vec::new();
        for page in self.catalog(cx, "prompts/list").await? {
            let CoreResult::Final(FinalCoreResult::PromptsList { result, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(result.payload.prompts);
        }
        build_prompts(Arc::clone(&self.forwarder), entries, self.namespace.as_deref())
    }

    /// Fully collect reversible resource templates, rejecting duplicate,
    /// client-direct HTTPS and non-reversible routes before exposing any handler.
    /// Returned templates also provide their route-bound completion handler.
    pub async fn resource_templates(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthResourceTemplate>> {
        let mut entries = Vec::new();
        for page in self.catalog(cx, "resources/templates/list").await? {
            let CoreResult::Final(FinalCoreResult::ResourceTemplatesList { result, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(result.payload.resource_templates);
        }
        build_templates(Arc::clone(&self.forwarder), entries)
    }

    async fn catalog(&self, cx: &Cx, method: &'static str) -> McpResult<Vec<CoreResult>> {
        check_cx(cx)?;
        let result = self.source.collect(cx, method, &self.forwarder.next_id, self.catalog_limits).await;
        check_cx(cx)?;
        result
    }
}

// Both seams are private. The only production constructor installs the native
// machine client; synthetic catalogs cannot mint public schema registrations.
struct MachineCall {
    request: CoreRequest,
    discovery_id: RequestId,
    request_id: RequestId,
    limits: ManagedCoreLimits,
    inputs: Option<Arc<MachineInputs>>,
}

trait MachineSource: Send + Sync {
    fn collect<'a>(
        &'a self, cx: &'a Cx, method: &'static str, ids: &'a AtomicU64,
        limits: ClientCredentialsCatalogLimits,
    ) -> BoxFuture<'a, McpResult<Vec<CoreResult>>>;

    fn start<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, call: MachineCall,
    ) -> BoxFuture<'a, McpResult<Box<dyn MachineResponse>>>;
}

trait MachineResponse: Send {
    fn next_event<'a>(&'a mut self, cx: &'a Cx)
        -> BoxFuture<'a, McpResult<Option<ManagedCoreEvent>>>;
}

struct NativeMachineSource(ClientCredentialsClient);

impl MachineSource for NativeMachineSource {
    fn collect<'a>(
        &'a self, cx: &'a Cx, method: &'static str, ids: &'a AtomicU64,
        limits: ClientCredentialsCatalogLimits,
    ) -> BoxFuture<'a, McpResult<Vec<CoreResult>>> {
        Box::pin(async move {
            let request = core_request(method, json!({}), None)?;
            let collector = ClientCredentialsCatalogClient::new(self.0.clone(), limits);
            let pages = collector.collect(
                cx, request,
                || next_pair(ids).map_err(|_| ClientCredentialsCatalogError::Catalog(
                    ManagedCatalogError::AbortedByHost,
                )),
                // Empty capability metadata grants no notification stream.
                // Invalidation during collection must not publish a partial catalog.
                |_| Err(ClientCredentialsCatalogError::Catalog(ManagedCatalogError::AbortedByHost)),
            ).await.map_err(catalog_error)?;
            Ok(pages.into_pages())
        })
    }

    fn start<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, call: MachineCall,
    ) -> BoxFuture<'a, McpResult<Box<dyn MachineResponse>>> {
        Box::pin(async move {
            let MachineCall { request, discovery_id, request_id, limits, inputs } = call;
            let cancellation = ctx.request_cancellation();
            if let Some(inputs) = inputs {
                let operation = self.0.start_core_interaction_with_cancellation(
                    cx, &cancellation, request, discovery_id, request_id, inputs.policy.limits(limits)?,
                ).await.map_err(interaction_error)?;
                // Preserve request identity/auth/quota/lease while giving host
                // callbacks the exact Cx selected by request-owned dispatch.
                return Ok(Box::new(InteractiveMachineResponse::new(
                    operation, ctx.clone().with_request_cx(cx.clone()), inputs,
                )) as Box<dyn MachineResponse>);
            }
            let call = self.0.request_core_with_cancellation(
                cx, &cancellation, request, discovery_id, request_id, limits,
            ).await.map_err(machine_error)?;
            Ok(Box::new(NativeMachineResponse(call)) as Box<dyn MachineResponse>)
        })
    }
}

struct NativeMachineResponse(ClientCredentialsCoreCall);
impl MachineResponse for NativeMachineResponse {
    fn next_event<'a>(&'a mut self, cx: &'a Cx)
        -> BoxFuture<'a, McpResult<Option<ManagedCoreEvent>>>
    {
        Box::pin(async move { self.0.next_event(cx).await.map_err(machine_error) })
    }
}

struct MachineBackend {
    source: Arc<dyn MachineSource>,
    next_id: Arc<AtomicU64>,
    inputs: Option<Arc<MachineInputs>>,
}

impl CoreBackend for MachineBackend {
    fn execute<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, request: CoreRequest,
        id: RequestId, limits: ManagedCoreLimits,
    ) -> BoxFuture<'a, McpResult<FinalCoreResult>> {
        Box::pin(async move {
            ctx.checkpoint()?;
            check_cx(cx)?;
            let (request, inputs) = match &self.inputs {
                Some(inputs) => match inputs.policy.select_request(&request)? {
                    Some(selected) => (selected, Some(Arc::clone(inputs))),
                    // Completion is not an input-required method. Do not send
                    // extra capabilities or disable it when a host is installed.
                    None => (request, None),
                },
                None => (request, None),
            };
            // The operation's ID already came from Forwarder. Discovery needs
            // another ID from that same allocator, never a fixed or reused ID.
            let discovery_id = allocate_request_id(&self.next_id)?;
            let mut response = self.source.start(ctx, cx, MachineCall {
                request, discovery_id, request_id: id, limits, inputs,
            }).await?;
            loop {
                ctx.checkpoint()?;
                check_cx(cx)?;
                let event = response.next_event(cx).await?;
                // Do not publish progress or terminal data after cancellation.
                ctx.checkpoint()?;
                check_cx(cx)?;
                match event {
                    Some(ManagedCoreEvent::Notification(notification)) => {
                        forward_notification(ctx, *notification)?;
                    }
                    Some(ManagedCoreEvent::Result(result)) => match *result {
                        CoreResult::Final(result) => return Ok(result),
                        _ => return Err(McpError::invalid_request(UNEXPECTED_RESULT)),
                    },
                    None => return Err(McpError::invalid_request(MACHINE_FAILURE)),
                }
            }
        })
    }
}

fn next_pair(ids: &AtomicU64) -> McpResult<(RequestId, RequestId)> {
    // If the second allocation fails, the first is deliberately not reusable.
    Ok((allocate_request_id(ids)?, allocate_request_id(ids)?))
}

fn machine_error(error: ClientCredentialsCoreError) -> McpError {
    match error {
        ClientCredentialsCoreError::Protocol(error) => upstream_error(error),
        ClientCredentialsCoreError::Authentication(ClientCredentialsError::Discovery(
            OAuthDiscoveryError::Cancelled | OAuthDiscoveryError::TimedOut,
        )) => McpError::request_cancelled(),
        ClientCredentialsCoreError::Authentication(_) => McpError::invalid_request(MACHINE_FAILURE),
    }
}

fn catalog_error(error: ClientCredentialsCatalogError) -> McpError {
    match error {
        ClientCredentialsCatalogError::Core(error) => machine_error(error),
        ClientCredentialsCatalogError::Catalog(_) => McpError::invalid_request(CATALOG_FAILURE),
    }
}
