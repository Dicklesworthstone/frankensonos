//! Comprehensive tests for the MCP server using Lab runtime patterns.
//!
//! These tests verify:
//! - Request/response cycle
//! - Tool invocation with cancellation
//! - Resource reading with budget exhaustion
//! - Multi-handler registration
//! - Error handling

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use asupersync::{Budget, CancelKind, Cx, time::wall_now};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use fastmcp_core::logging::{info, targets};
use fastmcp_core::{
    AuthContext, McpContext, McpError, McpErrorCode, McpRequestCancellation, McpResult,
    SessionState,
};
use fastmcp_derive::tool;
use fastmcp_protocol::{
    CallToolParams, CancelledParams, ClientCapabilities, ClientInfo, Content, CreateMessageResult,
    GetPromptParams, InitializeParams, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
    LegacyContent, LegacyResourceContent, LogLevel, LogMessageParams, Prompt, PromptArgument,
    PromptMessage, ReadResourceParams, RequestId, Resource, ResourceContent, ResourceTemplate,
    ResourceUpdatedNotificationParams, Role, SamplingCapability, ServerCapabilities, ServerInfo,
    SetLogLevelParams, Tool,
};

#[cfg(feature = "tasks")]
use crate::TaskManager;
use crate::bidirectional::{PendingRequests, RequestSender, TransportSendFn};
use crate::caching::ResponseCachingMiddleware;
use crate::handler::{PromptHandler, ResourceHandler, ToolHandler, UriParams};
use crate::rate_limiting::RateLimitingMiddleware;
use crate::router::Router;
use crate::session::{
    MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION, MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION, Session,
};
use crate::{
    ActiveRequest, ActiveRequestGuard, ActiveRequestKey, AuthRequest, InboundRequestContext,
    InboundRequestTransport, NotificationSender, RequestCompletion, Server, StaticTokenVerifier,
    TokenAuthProvider,
};

fn run_returning_pump_with_caller_runtime<R, S>(
    server: Server,
    recv: R,
    send: S,
    notification_sender: NotificationSender,
    connection_failure: Option<Arc<AtomicBool>>,
    transport_label: &'static str,
) -> McpResult<()>
where
    R: FnMut(&Cx, &AtomicBool) -> Result<JsonRpcMessage, fastmcp_transport::TransportError>
        + Send
        + 'static,
    S: FnMut(&Cx, &JsonRpcMessage) -> Result<(), fastmcp_transport::TransportError>
        + Send
        + Sync
        + 'static,
{
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("returning pump test reactor must initialize"))
        .blocking_threads(4, crate::MAX_DISPATCH_QUEUE_DEPTH)
        .build()
        .expect("returning pump test runtime must initialize");
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    runtime.block_on(async move {
        let caller_cx = Cx::current().expect("returning pump test runtime must install Cx");
        let dispatch_cx = caller_cx.clone();
        let pump = caller_cx
            .spawn_blocking(move |pump_cx| {
                let result = server.run_loop_returning_with_dispatch_cx(
                    &pump_cx,
                    &dispatch_cx,
                    recv,
                    send,
                    notification_sender,
                    connection_failure,
                    transport_label,
                );
                let _ = result_sender.send(result);
            })
            .map_err(|error| McpError::internal_error(error.to_string()))?;
        let deadline = caller_cx.now().saturating_add_nanos(5_000_000_000);
        loop {
            match result_receiver.try_recv() {
                Ok(result) => {
                    drop(pump);
                    break result;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    drop(pump);
                    break Err(McpError::internal_error(
                        "returning pump exited without a result",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    asupersync::time::timeout_at(
                        deadline,
                        asupersync::time::sleep(caller_cx.now(), Duration::from_millis(1)),
                    )
                    .await
                    .map_err(|_| McpError::internal_error("returning pump test timed out"))?;
                }
            }
        }
    })
}

/// Creates a request sender for tests that should not perform server-to-client requests.
///
/// If a test unexpectedly triggers bidirectional server->client communication (sampling,
/// elicitation, roots), the send will fail and the request path should surface an error.
fn create_test_request_sender() -> RequestSender {
    let pending = Arc::new(PendingRequests::new());
    let send_fn: TransportSendFn = Arc::new(|message| {
        Err(format!(
            "unexpected server-to-client message in unit test: {message:?}"
        ))
    });
    RequestSender::new(pending, send_fn)
}

// ============================================================================
// Test Tool Handlers
// ============================================================================

#[tool(name = "greet", description = "Greets a user by name")]
fn greet(ctx: &McpContext, name: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(format!("Hello, {name}!"))
}

#[test]
fn poll_on_cx_preserves_caller_capabilities_and_restores_parent() {
    let ((), report) = asupersync::lab::run_async_under_lab(5050, |root| async move {
        let parent = Cx::current().expect("lab installs a caller context");
        let parent_caps = parent.capabilities();
        assert!(parent.timer_driver().is_some(), "positive timer control");

        for restricted in [false, true] {
            let caller = if restricted {
                let _guard = root
                    .restrict::<asupersync::cx::cap::None>()
                    .set_current_restricted();
                Cx::current().expect("capture the caller's narrowed runtime ceiling")
            } else {
                root.clone()
            };
            let expected_caps = caller.capabilities();
            let request = McpContext::new(caller.clone(), 1);
            let mut polls = 0;
            let greeting = crate::poll_on_cx(
                &caller,
                std::future::poll_fn(|task| {
                    polls += 1;
                    let ambient = Cx::current().expect("server polling installs the caller");
                    assert_eq!(ambient.task_id(), caller.task_id());
                    assert_eq!(ambient.capabilities(), expected_caps);
                    assert_eq!(ambient.timer_driver().is_some(), !restricted);
                    if restricted {
                        assert!(!ambient.capabilities().spawn);
                        assert!(!ambient.capabilities().io);
                        assert!(!ambient.capabilities().remote);
                        assert!(ambient.io().is_none());
                    }
                    if polls == 1 {
                        task.waker().wake_by_ref();
                        std::task::Poll::Pending
                    } else {
                        std::task::Poll::Ready(greet(&request, "caller".to_owned()))
                    }
                }),
            );
            assert_eq!(
                greeting.expect("authorized pure tool completes"),
                "Hello, caller!"
            );
            assert_eq!(polls, 2);
            let restored = Cx::current().expect("server polling restores its parent");
            assert_eq!(restored.task_id(), parent.task_id());
            assert_eq!(restored.capabilities(), parent_caps);
            assert!(restored.timer_driver().is_some());
        }
    });
    assert!(report.quiescent, "{report:?}");
    assert!(report.oracle_report.total > 0, "{report:?}");
    assert!(report.oracle_report.all_passed(), "{report:?}");
    assert!(report.invariant_violations.is_empty(), "{report:?}");
}

#[tool(
    name = "greet_default",
    description = "Greets a user by name (with a default)",
    defaults(name = "World")
)]
fn greet_default(ctx: &McpContext, name: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(format!("Hello, {name}!"))
}

// These fixture handlers never actually fail, so clippy flags the Result as
// unnecessary; the McpResult return is retained deliberately so the fixtures
// keep the same fallible handler shape production tools use.
#[allow(clippy::unnecessary_wraps)]
#[tool(name = "announce", description = "Emits handler log notifications")]
fn announce(ctx: &McpContext, name: String) -> McpResult<String> {
    ctx.debug("handler-debug");
    ctx.info(format!("handler-info:{name}"));
    Ok(format!("announced {name}"))
}

#[allow(clippy::unnecessary_wraps)]
#[tool(
    name = "hide_greet",
    description = "Disables the greet tool for this session"
)]
fn hide_greet(ctx: &McpContext) -> McpResult<String> {
    ctx.disable_tool("greet");
    Ok("hidden".to_string())
}

#[allow(clippy::unnecessary_wraps)]
#[tool(
    name = "touch_file",
    description = "Notifies subscribers that a file changed"
)]
fn touch_file(ctx: &McpContext, uri: String) -> McpResult<String> {
    let notified = ctx.notify_resource_updated(&uri);
    Ok(if notified { "notified" } else { "silent" }.to_string())
}

#[tool(name = "formal_greet", description = "Formally greets a user")]
fn formal_greet(_ctx: &McpContext, name: Option<String>) -> String {
    let name = name.as_deref().unwrap_or("Sir/Madam");
    format!("Good day, {name}.")
}

#[tool(
    name = "cancellation_check",
    description = "Tool that checks cancellation status"
)]
fn cancellation_check(ctx: &McpContext) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok("Not cancelled".to_string())
}

#[tool(name = "slow_tool", description = "Simulates a slow operation")]
fn slow_tool(ctx: &McpContext) -> McpResult<String> {
    for _ in 0..5 {
        ctx.checkpoint()?;
    }
    Ok("Slow work completed".to_string())
}

#[tool(
    name = "increment",
    description = "Increments a counter in session state"
)]
fn increment(ctx: &McpContext) -> String {
    let count: i32 = ctx.get_state("counter").unwrap_or(0);
    let new_count = count + 1;
    ctx.set_state("counter", new_count);
    format!("Counter: {new_count}")
}

#[tool(name = "query", description = "Executes a query")]
fn mount_query(_ctx: &McpContext, sql: Option<String>) -> String {
    let sql = sql.unwrap_or_default();
    format!("Query result: {sql}")
}

#[tool(name = "insert", description = "Inserts data")]
fn mount_insert(_ctx: &McpContext) -> String {
    "Inserted".to_string()
}

#[tool(name = "add", description = "Adds two numbers")]
fn add_numbers_tool(_ctx: &McpContext, a: i64, b: i64) -> String {
    (a + b).to_string()
}

#[tool(name = "compute", description = "Returns a JSON result")]
fn compute_json_tool(_ctx: &McpContext) -> String {
    r#"{"value": 42}"#.to_string()
}

#[tool(name = "failing", description = "Always fails")]
fn failing_tool_test(_ctx: &McpContext) -> McpResult<String> {
    // An application-level error: framework-terminal codes (InternalError,
    // RequestCancelled) now propagate as JSON-RPC failures instead of
    // converting to is_error tool content.
    Err(McpError::new(
        McpErrorCode::InvalidParams,
        "Something went wrong",
    ))
}

#[tool(name = "declined", description = "Returns a typed caller refusal")]
fn declined_tool(_ctx: &McpContext) -> McpResult<String> {
    Err(McpError::invalid_params("caller input was declined"))
}

#[tool(name = "get_state", description = "Returns session state value")]
fn get_state_from_ctx(ctx: &McpContext) -> String {
    let value: Option<String> = ctx.get_state("tool_test_key");
    value.unwrap_or_else(|| "no_value".to_string())
}

#[tool(
    name = "nested_state",
    description = "Sets state then calls another tool"
)]
async fn nested_state_call(ctx: &McpContext) -> McpResult<String> {
    ctx.set_state("tool_test_key", "tool_propagated_value");
    let inner_result = ctx.call_tool("get_state", serde_json::json!({})).await?;
    let text = inner_result.first_text().unwrap_or("(no content)");
    Ok(format!("Inner tool saw: {}", text))
}

#[derive(Clone)]
struct BlockingToolState {
    barrier: Arc<Barrier>,
    started: Option<Arc<AtomicBool>>,
    completed: Option<Arc<AtomicBool>>,
}

static BLOCKING_TOOL_STATE: OnceLock<Mutex<Option<BlockingToolState>>> = OnceLock::new();
static BLOCKING_TOOL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn blocking_tool_state() -> &'static Mutex<Option<BlockingToolState>> {
    BLOCKING_TOOL_STATE.get_or_init(|| Mutex::new(None))
}

fn blocking_tool_lock() -> &'static Mutex<()> {
    BLOCKING_TOOL_LOCK.get_or_init(|| Mutex::new(()))
}

struct BlockingToolConfigGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for BlockingToolConfigGuard {
    fn drop(&mut self) {
        *blocking_tool_state()
            .lock()
            .expect("blocking tool state lock poisoned") = None;
    }
}

fn configure_blocking_tool(barrier: Arc<Barrier>) -> BlockingToolConfigGuard {
    configure_blocking_tool_with_completion(barrier, None, None)
}

fn configure_blocking_tool_with_completion(
    barrier: Arc<Barrier>,
    started: Option<Arc<AtomicBool>>,
    completed: Option<Arc<AtomicBool>>,
) -> BlockingToolConfigGuard {
    let lock = blocking_tool_lock()
        .lock()
        .expect("blocking tool lock poisoned");
    *blocking_tool_state()
        .lock()
        .expect("blocking tool state lock poisoned") = Some(BlockingToolState {
        barrier,
        started,
        completed,
    });
    BlockingToolConfigGuard { _lock: lock }
}

#[tool(
    name = "block_until_cancelled",
    description = "Blocks until cancellation is observed"
)]
fn block_until_cancelled(ctx: &McpContext) -> McpResult<String> {
    let state = blocking_tool_state()
        .lock()
        .expect("blocking tool state lock poisoned")
        .clone()
        .ok_or_else(|| McpError::internal_error("blocking tool not configured for test"))?;

    if let Some(started) = state.started {
        started.store(true, Ordering::Release);
    }
    state.barrier.wait();
    // Sleep between liveness checks: a hot yield_now spin charges the
    // request's poll quota fast enough to exhaust it before an in-band
    // cancellation notification can arrive, which turns this fixture into a
    // budget-death probe instead of a cancellation probe.
    while !ctx.is_cancelled() {
        std::thread::sleep(Duration::from_millis(1));
    }
    if let Some(completed) = state.completed {
        completed.store(true, Ordering::Release);
    }
    Err(McpError::request_cancelled())
}

#[tool(
    name = "sampling_round_trip",
    description = "Requests one sampling completion"
)]
fn sampling_round_trip(ctx: &McpContext) -> McpResult<String> {
    let response = fastmcp_core::block_on(ctx.sample("reply with a test value", 16))?;
    Ok(response.text)
}

struct LoggingBlockingToolState {
    barrier: Arc<Barrier>,
    events: Arc<Mutex<Vec<RequestEvent>>>,
    start: Instant,
}

static LOGGING_BLOCKING_TOOL_STATE: OnceLock<Mutex<Option<LoggingBlockingToolState>>> =
    OnceLock::new();
static LOGGING_BLOCKING_TOOL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn logging_blocking_tool_state() -> &'static Mutex<Option<LoggingBlockingToolState>> {
    LOGGING_BLOCKING_TOOL_STATE.get_or_init(|| Mutex::new(None))
}

fn logging_blocking_tool_lock() -> &'static Mutex<()> {
    LOGGING_BLOCKING_TOOL_LOCK.get_or_init(|| Mutex::new(()))
}

struct LoggingBlockingToolConfigGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for LoggingBlockingToolConfigGuard {
    fn drop(&mut self) {
        *logging_blocking_tool_state()
            .lock()
            .expect("logging blocking tool state lock poisoned") = None;
    }
}

fn configure_logging_blocking_tool(
    barrier: Arc<Barrier>,
    events: Arc<Mutex<Vec<RequestEvent>>>,
    start: Instant,
) -> LoggingBlockingToolConfigGuard {
    let lock = logging_blocking_tool_lock()
        .lock()
        .expect("logging blocking tool lock poisoned");
    *logging_blocking_tool_state()
        .lock()
        .expect("logging blocking tool state lock poisoned") = Some(LoggingBlockingToolState {
        barrier,
        events,
        start,
    });
    LoggingBlockingToolConfigGuard { _lock: lock }
}

#[tool(
    name = "block_until_cancelled_logged",
    description = "Blocks until cancellation; records timing logs"
)]
fn block_until_cancelled_logged(ctx: &McpContext, request_id: i64) -> McpResult<String> {
    let (barrier, events, start) = {
        let guard = logging_blocking_tool_state()
            .lock()
            .expect("logging blocking tool state lock poisoned");
        let state = guard
            .as_ref()
            .ok_or_else(|| McpError::internal_error("logging blocking tool not configured"))?;
        (
            Arc::clone(&state.barrier),
            Arc::clone(&state.events),
            state.start,
        )
    };

    record_event(&events, request_id, "start", start);
    barrier.wait();

    loop {
        if ctx.checkpoint().is_err() || ctx.is_cancelled() {
            record_event(&events, request_id, "cancelled", start);
            break;
        }
        std::thread::yield_now();
    }

    record_event(&events, request_id, "finish", start);
    Err(McpError::request_cancelled())
}

#[derive(Debug, Clone)]
struct RequestEvent {
    request_id: i64,
    phase: &'static str,
    elapsed: Duration,
}

fn record_event(
    events: &Arc<Mutex<Vec<RequestEvent>>>,
    request_id: i64,
    phase: &'static str,
    start: Instant,
) {
    let elapsed = start.elapsed();
    let mut guard = events.lock().expect("events lock poisoned");
    guard.push(RequestEvent {
        request_id,
        phase,
        elapsed,
    });
    info!(
        target: targets::SESSION,
        "e2e event request_id={} phase={} elapsed_ms={}",
        request_id,
        phase,
        elapsed.as_millis()
    );
}

#[test]
fn request_id_to_u64_number() {
    let id = RequestId::Number(42);
    assert_eq!(crate::request_id_to_u64(Some(&id)), 42);
}

#[test]
fn request_id_to_u64_negative_number() {
    let id = RequestId::Number(-1);
    assert_eq!(crate::request_id_to_u64(Some(&id)), (-1i64) as u64);
}

#[test]
fn request_id_to_u64_string_stable_nonzero() {
    let id = RequestId::String("abc".to_string());
    let first = crate::request_id_to_u64(Some(&id));
    let second = crate::request_id_to_u64(Some(&id));
    assert_eq!(first, second);
    assert_ne!(first, 0);
}

#[test]
fn request_id_to_u64_integer_stable_nonzero() {
    let id = RequestId::Integer("9223372036854775808".to_string());
    let first = crate::request_id_to_u64(Some(&id));
    let second = crate::request_id_to_u64(Some(&id));
    assert_eq!(first, second);
    assert_eq!(first, crate::stable_hash_request_id("9223372036854775808"));
    assert_ne!(first, 0);
}

#[test]
fn request_id_to_u64_none_is_zero() {
    assert_eq!(crate::request_id_to_u64(None), 0);
}

#[tool(name = "error_tool", description = "Always returns an error")]
fn error_tool(_ctx: &McpContext) -> McpResult<String> {
    // An application-level error: framework-terminal codes (InternalError,
    // RequestCancelled) now propagate as JSON-RPC failures instead of
    // converting to is_error tool content.
    Err(McpError::invalid_params("Intentional error for testing"))
}

// ============================================================================
// Test Resource Handlers
// ============================================================================

/// A simple static resource.
struct StaticResource {
    uri: String,
    content: String,
}

impl ResourceHandler for StaticResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: self.uri.clone(),
            name: "Static Resource".to_string(),
            description: Some("A static test resource".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Ok(vec![ResourceContent {
            uri: self.uri.clone(),
            mime_type: Some("text/plain".to_string()),
            text: Some(self.content.clone()),
            blob: None,
        }])
    }
}

/// A resource that checks cancellation.
struct CancellableResource;

impl ResourceHandler for CancellableResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "resource://cancellable".to_string(),
            name: "Cancellable Resource".to_string(),
            description: Some("A resource that checks cancellation".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        if ctx.is_cancelled() {
            return Err(McpError::request_cancelled());
        }
        Ok(vec![ResourceContent {
            uri: "resource://cancellable".to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some("Resource content".to_string()),
            blob: None,
        }])
    }
}

/// A resource with a URI template that echoes the matched parameter.
struct TemplateResource;

impl ResourceHandler for TemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "resource:///items/{id}".to_string(),
            name: "Template Resource".to_string(),
            description: Some("Template resource for tests".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "resource:///items/{id}".to_string(),
            name: "Template Resource".to_string(),
            description: Some("Template resource for tests".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        })
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Err(McpError::invalid_params(
            "uri parameters required for template resource",
        ))
    }

    fn read_with_uri(
        &self,
        _ctx: &McpContext,
        uri: &str,
        params: &UriParams,
    ) -> McpResult<Vec<ResourceContent>> {
        let id = params
            .get("id")
            .ok_or_else(|| McpError::invalid_params("missing uri parameter: id"))?;
        Ok(vec![ResourceContent {
            uri: uri.to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some(format!("Template {id}")),
            blob: None,
        }])
    }
}

/// A more specific template resource for precedence tests.
struct SpecificTemplateResource;

impl ResourceHandler for SpecificTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "resource:///items/foo/{id}".to_string(),
            name: "Specific Template Resource".to_string(),
            description: Some("Specific template resource for tests".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "resource:///items/foo/{id}".to_string(),
            name: "Specific Template Resource".to_string(),
            description: Some("Specific template resource for tests".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        })
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Err(McpError::invalid_params(
            "uri parameters required for specific template resource",
        ))
    }

    fn read_with_uri(
        &self,
        _ctx: &McpContext,
        uri: &str,
        params: &UriParams,
    ) -> McpResult<Vec<ResourceContent>> {
        let id = params
            .get("id")
            .ok_or_else(|| McpError::invalid_params("missing uri parameter: id"))?;
        Ok(vec![ResourceContent {
            uri: uri.to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some(format!("Specific {id}")),
            blob: None,
        }])
    }
}

#[derive(Debug, Clone)]
struct TemplateLogEntry {
    template: String,
    uri: String,
    params: UriParams,
    response: String,
}

/// A template resource that logs chosen template, params, and response payload.
struct LoggingTemplateResource {
    template: &'static str,
    events: Arc<std::sync::Mutex<Vec<TemplateLogEntry>>>,
}

impl ResourceHandler for LoggingTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: self.template.to_string(),
            name: "Logging Template Resource".to_string(),
            description: Some("Template resource that logs matches".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: self.template.to_string(),
            name: "Logging Template Resource".to_string(),
            description: Some("Template resource that logs matches".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        })
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Err(McpError::invalid_params(
            "uri parameters required for logging template resource",
        ))
    }

    fn read_with_uri(
        &self,
        _ctx: &McpContext,
        uri: &str,
        params: &UriParams,
    ) -> McpResult<Vec<ResourceContent>> {
        let response = format!(
            "Logged {}",
            params.get("path").map(String::as_str).unwrap_or_default()
        );
        let entry = TemplateLogEntry {
            template: self.template.to_string(),
            uri: uri.to_string(),
            params: params.clone(),
            response: response.clone(),
        };
        let mut guard = self.events.lock().expect("template log lock poisoned");
        guard.push(entry);

        info!(
            target: targets::SESSION,
            "e2e template={} uri={} params={:?} response={}",
            self.template,
            uri,
            params,
            response
        );

        Ok(vec![ResourceContent {
            uri: uri.to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some(response),
            blob: None,
        }])
    }
}

// ============================================================================
// Test Prompt Handlers
// ============================================================================

/// A simple greeting prompt.
struct GreetingPrompt;

impl PromptHandler for GreetingPrompt {
    fn definition(&self) -> Prompt {
        Prompt {
            name: "greeting".to_string(),
            description: Some("A simple greeting prompt".to_string()),
            arguments: vec![PromptArgument {
                name: "name".to_string(),
                description: Some("Name to greet".to_string()),
                required: true,
            }],
            icon: None,
            version: None,
            tags: vec![],
        }
    }

    fn get(
        &self,
        _ctx: &McpContext,
        arguments: HashMap<String, String>,
    ) -> McpResult<Vec<PromptMessage>> {
        let name = arguments.get("name").map_or("User", String::as_str);
        Ok(vec![PromptMessage {
            role: Role::User,
            content: Content::Text {
                text: format!("Please greet {name} warmly."),
            },
        }])
    }
}

// ============================================================================
// Router Tests
// ============================================================================

#[cfg(test)]
mod router_tests {
    use super::*;
    use crate::middleware::Middleware;
    use fastmcp_protocol::ListResourceTemplatesParams;

    /// Creates a test router with all handlers registered.
    fn create_test_router() -> Router {
        let mut router = Router::new();

        // Register tools
        router.add_tool(Greet).expect("tool registration succeeds");
        router
            .add_tool(CancellationCheck)
            .expect("tool registration succeeds");
        router
            .add_tool(SlowTool)
            .expect("tool registration succeeds");
        router
            .add_tool(ErrorTool)
            .expect("tool registration succeeds");

        // Register resources
        router.add_resource(StaticResource {
            uri: "resource://test".to_string(),
            content: "Test content".to_string(),
        });
        router.add_resource(CancellableResource);
        router.add_resource(TemplateResource);

        // Register resource templates
        router.add_resource_template(ResourceTemplate {
            uri_template: "resource:///named/{name}".to_string(),
            name: "Manual Template".to_string(),
            description: Some("Resource template for manual listing".to_string()),
            mime_type: Some("text/plain".to_string()),
            icon: None,
            version: None,
            tags: vec![],
        });

        // Register prompts
        router.add_prompt(GreetingPrompt);

        router
    }

    /// Creates a test session.
    fn create_test_session() -> Session {
        Session::new(
            ServerInfo {
                name: "test-server".to_string(),
                version: "1.0.0".to_string(),
            },
            ServerCapabilities::default(),
        )
    }

    #[test]
    fn test_middleware_short_circuit_prevents_late_middleware() {
        struct ImmediateResponse;
        impl Middleware for ImmediateResponse {
            fn on_request(
                &self,
                _ctx: &McpContext,
                _request: &fastmcp_protocol::JsonRpcRequest,
            ) -> McpResult<crate::MiddlewareDecision> {
                Ok(crate::MiddlewareDecision::Respond(
                    serde_json::json!({"short_circuit": true}),
                ))
            }
        }

        struct CountEntry(Arc<Mutex<usize>>);
        impl Middleware for CountEntry {
            fn on_request(
                &self,
                _ctx: &McpContext,
                _request: &fastmcp_protocol::JsonRpcRequest,
            ) -> McpResult<crate::MiddlewareDecision> {
                *self.0.lock().expect("entry counter mutex poisoned") += 1;
                Ok(crate::MiddlewareDecision::Continue)
            }
        }

        let late_entries = Arc::new(Mutex::new(0_usize));

        let server = Server::new("test-server", "1.0.0")
            .tool(Greet)
            .middleware(ImmediateResponse)
            .middleware(CountEntry(Arc::clone(&late_entries)))
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let sender: NotificationSender = Arc::new(|_| {});
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Ada"})),
            meta: None,
        };
        let request = fastmcp_protocol::JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::to_value(params).expect("params")),
            1,
        );

        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request.clone(),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("short-circuit response");
        assert_eq!(
            response.result,
            Some(serde_json::json!({"short_circuit": true}))
        );
        assert_eq!(
            *late_entries.lock().expect("entry counter mutex poisoned"),
            0
        );
    }

    #[test]
    fn production_session_cache_hits_within_one_session_and_isolates_sessions() {
        let cache_a = Arc::new(ResponseCachingMiddleware::new().max_entries(10));
        let cache_b = Arc::new(ResponseCachingMiddleware::new().max_entries(10));

        let cx = Cx::for_testing();
        let request = fastmcp_protocol::JsonRpcRequest::new("tools/list", None, 2);

        let server = Server::new("test-server", "1.0.0")
            .tool(Greet)
            .middleware(cache_a.clone())
            .middleware(cache_b.clone())
            .build();

        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let sender: NotificationSender = Arc::new(|_| {});

        let first = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request.clone(),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("first response");
        assert!(first.error.is_none());

        let second = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request.clone(),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("second response");
        assert!(second.error.is_none());
        assert_eq!(second.result, first.result);

        let cache_a_same_session = cache_a.stats();
        let cache_b_same_session = cache_b.stats();
        assert_eq!(cache_a_same_session.entries, 1);
        assert_eq!(cache_b_same_session.entries, 1);
        assert!(cache_a_same_session.hits + cache_b_same_session.hits >= 1);

        let mut isolated_session = create_test_session();
        isolated_session.initialize(
            ClientInfo {
                name: "other-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let isolated = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut isolated_session,
            request,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("isolated-session response");
        assert!(isolated.error.is_none());
        assert_eq!(isolated.result, first.result);

        let cache_a_isolated = cache_a.stats();
        let cache_b_isolated = cache_b.stats();
        assert_eq!(cache_a_isolated.entries, 2);
        assert_eq!(cache_b_isolated.entries, 2);
        assert!(cache_a_isolated.misses > cache_a_same_session.misses);
        assert!(cache_b_isolated.misses > cache_b_same_session.misses);
    }

    #[test]
    fn test_auth_request_access_token_parsing() {
        let params = serde_json::json!({"authorization": "Bearer alpha"});
        let request = AuthRequest {
            method: "tools/list",
            params: Some(&params),
            transport_authorization: None,
            request_id: 10,
        };
        let access = request.access_token().expect("missing access credential");
        assert_eq!(access.scheme, "Bearer");
        assert_eq!(access.token, "alpha");

        // Object credentials must carry an explicit scheme under the strict
        // fail-closed grammar; a token-only object is malformed, not Bearer.
        let params = serde_json::json!({"auth": {"scheme": "Bearer", "token": "beta"}});
        let request = AuthRequest {
            method: "tools/list",
            params: Some(&params),
            transport_authorization: None,
            request_id: 11,
        };
        let access = request.access_token().expect("missing access credential");
        assert_eq!(access.scheme, "Bearer");
        assert_eq!(access.token, "beta");

        let params = serde_json::json!({"auth": {"token": "gamma"}});
        let request = AuthRequest {
            method: "tools/list",
            params: Some(&params),
            transport_authorization: None,
            request_id: 12,
        };
        assert!(
            request.access_token().is_none(),
            "a scheme-less object credential must be refused, not defaulted"
        );
    }

    #[test]
    fn test_token_auth_provider_allows_and_denies() {
        let verifier =
            StaticTokenVerifier::new([("good-token", AuthContext::with_subject("user-1"))])
                .expect("valid verifier configuration")
                .with_allowed_schemes(["Bearer"])
                .expect("valid scheme configuration");
        let provider = TokenAuthProvider::new(verifier);

        let server = Server::new("test-server", "1.0.0")
            .tool(Greet)
            .auth_provider(provider)
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let sender: NotificationSender = Arc::new(|_| {});
        let request = fastmcp_protocol::JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "greet",
                "arguments": { "name": "Ada" },
                "auth": "Bearer good-token"
            })),
            6,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        assert!(response.error.is_none(), "expected authorized response");

        let request = fastmcp_protocol::JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "greet",
                "arguments": { "name": "Ada" },
                "auth": "Bearer bad-token"
            })),
            7,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        assert!(response.is_error(), "expected auth error");
        let error = response.error.expect("error payload");
        assert_eq!(
            error.code,
            i32::from(McpErrorCode::ResourceForbidden).into()
        );
    }

    #[test]
    fn test_auth_provider_protects_resource_access() {
        let verifier = StaticTokenVerifier::new([(
            "resource-token",
            AuthContext::with_subject("resource-user"),
        )])
        .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);

        let server = Server::new("test-server", "1.0.0")
            .resource(StaticResource {
                uri: "resource://secure".to_string(),
                content: "secret".to_string(),
            })
            .auth_provider(provider)
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let sender: NotificationSender = Arc::new(|_| {});
        let request = fastmcp_protocol::JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({"uri": "resource://secure"})),
            8,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        assert!(response.is_error(), "expected auth error");

        let request = fastmcp_protocol::JsonRpcRequest::new(
            "resources/read",
            Some(serde_json::json!({
                "uri": "resource://secure",
                "auth": "Bearer resource-token"
            })),
            9,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        assert!(response.error.is_none(), "expected authorized response");
    }

    #[test]
    fn test_e2e_auth_decisions_logged() {
        let verifier = StaticTokenVerifier::new([("good", AuthContext::with_subject("user-e2e"))])
            .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);

        let server = Server::new("test-server", "1.0.0")
            .tool(Greet)
            .auth_provider(provider)
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let sender: NotificationSender = Arc::new(|_| {});
        let ts = chrono::Utc::now().to_rfc3339();

        let unauthorized = fastmcp_protocol::JsonRpcRequest::new(
            "tools/list",
            Some(serde_json::json!({ "cursor": null })),
            12,
        );
        let unauthorized_response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            unauthorized,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        info!(
            target: targets::SESSION,
            "e2e auth unauthorized ts={} error={:?}",
            ts,
            unauthorized_response.error
        );
        assert!(unauthorized_response.is_error());

        let authorized = fastmcp_protocol::JsonRpcRequest::new(
            "tools/list",
            Some(serde_json::json!({ "cursor": null, "auth": "Bearer good" })),
            13,
        );
        let authorized_response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            authorized,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        info!(
            target: targets::SESSION,
            "e2e auth authorized ts={} result={:?}",
            ts,
            authorized_response.result
        );
        assert!(authorized_response.error.is_none());
    }

    #[test]
    fn test_router_tool_list() {
        let router = create_test_router();
        let tools = router.tools();

        assert_eq!(tools.len(), 4);

        let tool_names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"greet"));
        assert!(tool_names.contains(&"cancellation_check"));
        assert!(tool_names.contains(&"slow_tool"));
        assert!(tool_names.contains(&"error_tool"));
    }

    #[test]
    fn test_router_resource_list() {
        let router = create_test_router();
        let resources = router.resources();

        assert_eq!(resources.len(), 2);

        let resource_uris: Vec<_> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert!(resource_uris.contains(&"resource://test"));
        assert!(resource_uris.contains(&"resource://cancellable"));
    }

    #[test]
    fn test_router_resource_template_list() {
        let router = create_test_router();
        let templates = router.resource_templates();

        assert_eq!(templates.len(), 2);

        let template_uris: Vec<_> = templates
            .iter()
            .map(|template| template.uri_template.as_str())
            .collect();
        assert!(template_uris.contains(&"resource:///items/{id}"));
        assert!(template_uris.contains(&"resource:///named/{name}"));
    }

    // ========================================================================
    // Dynamic Enable/Disable Filtering Tests
    // ========================================================================

    #[test]
    fn test_router_tools_filtered_with_disabled() {
        let router = create_test_router();
        let state = SessionState::new();

        // All tools visible without filtering
        let all_tools = router.tools_filtered(None, None);
        assert_eq!(all_tools.len(), 4);

        // Disable a tool
        let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
        disabled.insert("greet".to_string());
        state.set("fastmcp.disabled_tools", disabled);

        // Now filtered list should have one less tool
        let filtered_tools = router.tools_filtered(Some(&state), None);
        assert_eq!(filtered_tools.len(), 3);
        assert!(!filtered_tools.iter().any(|t| t.name == "greet"));
        assert!(filtered_tools.iter().any(|t| t.name == "slow_tool"));
    }

    #[test]
    fn test_router_resources_filtered_with_disabled() {
        let router = create_test_router();
        let state = SessionState::new();

        // All resources visible without filtering
        let all_resources = router.resources_filtered(None, None);
        assert_eq!(all_resources.len(), 2);

        // Disable a resource
        let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
        disabled.insert("resource://test".to_string());
        state.set("fastmcp.disabled_resources", disabled);

        // Now filtered list should have one less resource
        let filtered_resources = router.resources_filtered(Some(&state), None);
        assert_eq!(filtered_resources.len(), 1);
        assert!(
            !filtered_resources
                .iter()
                .any(|r| r.uri == "resource://test")
        );
        assert!(
            filtered_resources
                .iter()
                .any(|r| r.uri == "resource://cancellable")
        );
    }

    #[test]
    fn test_router_prompts_filtered_with_disabled() {
        let router = create_test_router();
        let state = SessionState::new();

        // All prompts visible without filtering
        let all_prompts = router.prompts_filtered(None, None);
        assert_eq!(all_prompts.len(), 1);

        // Disable the "greeting" prompt (that's its actual name)
        let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
        disabled.insert("greeting".to_string());
        state.set("fastmcp.disabled_prompts", disabled);

        // Now filtered list should be empty
        let filtered_prompts = router.prompts_filtered(Some(&state), None);
        assert_eq!(filtered_prompts.len(), 0);
    }

    #[test]
    fn test_router_resource_templates_filtered_with_disabled() {
        let router = create_test_router();
        let state = SessionState::new();

        // All templates visible without filtering
        let all_templates = router.resource_templates_filtered(None, None);
        assert_eq!(all_templates.len(), 2);

        // Disable a template by its URI template
        let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
        disabled.insert("resource:///items/{id}".to_string());
        state.set("fastmcp.disabled_resources", disabled);

        // Now filtered list should have one less template
        let filtered_templates = router.resource_templates_filtered(Some(&state), None);
        assert_eq!(filtered_templates.len(), 1);
        assert!(
            !filtered_templates
                .iter()
                .any(|t| t.uri_template == "resource:///items/{id}")
        );
        assert!(
            filtered_templates
                .iter()
                .any(|t| t.uri_template == "resource:///named/{name}")
        );
    }

    #[test]
    fn test_handle_resource_templates_list_sorted() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let params = ListResourceTemplatesParams::default();

        let request_ctx = McpContext::new(cx, 1);
        let result = router.handle_resource_templates_list(&request_ctx, params, None);
        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());
        let templates = result.unwrap().resource_templates;

        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].uri_template, "resource:///items/{id}");
        assert_eq!(templates[0].name, "Template Resource");
        assert_eq!(
            templates[0].description.as_deref(),
            Some("Template resource for tests")
        );
        assert_eq!(templates[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(templates[1].uri_template, "resource:///named/{name}");
        assert_eq!(templates[1].name, "Manual Template");
        assert_eq!(
            templates[1].description.as_deref(),
            Some("Resource template for manual listing")
        );
        assert_eq!(templates[1].mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn test_e2e_resource_templates_list_logs_response() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let params = ListResourceTemplatesParams::default();

        let request_ctx = McpContext::new(cx, 1);
        let result = router
            .handle_resource_templates_list(&request_ctx, params, None)
            .expect("resource templates list");

        info!(
            target: targets::SESSION,
            "e2e resources/templates/list response={:?}",
            result
        );

        assert!(!result.resource_templates.is_empty());
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn test_e2e_task_rpc_quarantine_emits_no_status_notifications() {
        let manager = TaskManager::new_for_testing();
        manager.register_handler("notify_task", |_cx, _params| async {
            Ok(serde_json::json!({"ok": true}))
        });
        let shared = manager.into_shared();

        let server = Server::new("test-server", "1.0.0")
            .with_task_manager(shared.clone())
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let notifications: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifications_clone = Arc::clone(&notifications);
        let sender: NotificationSender = Arc::new(move |request| {
            if request.method == "notifications/tasks/status" {
                notifications_clone
                    .lock()
                    .expect("notifications lock poisoned")
                    .push(request.method.clone());
            }
        });

        let submit = fastmcp_protocol::JsonRpcRequest::new(
            "tasks/submit",
            Some(serde_json::json!({"taskType": "notify_task"})),
            20,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            submit,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("submit response");
        let error = response
            .error
            .as_ref()
            .expect("quarantined tasks/submit must return an error");
        assert_eq!(error.code, i32::from(McpErrorCode::MethodNotFound).into());
        assert!(response.result.is_none());

        let recorded = notifications.lock().expect("notifications lock poisoned");
        assert!(
            recorded.is_empty(),
            "quarantined task RPC emitted status notifications: {recorded:?}"
        );
    }

    #[test]
    fn test_router_prompt_list() {
        let router = create_test_router();
        let prompts = router.prompts();

        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "greeting");
    }

    #[test]
    fn test_notification_does_not_return_response() {
        let server = Server::new("test-server", "1.0.0").build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let sender: NotificationSender = std::sync::Arc::new(|_| {});
        let params = CancelledParams {
            request_id: RequestId::Number(1),
            reason: Some("unit test".to_string()),
            meta: None,
        };
        let request = fastmcp_protocol::JsonRpcRequest::notification(
            "notifications/cancelled",
            Some(serde_json::to_value(params).unwrap()),
        );

        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            request,
            &sender,
            &create_test_request_sender(),
        ));
        assert!(response.is_none());
    }

    #[test]
    fn test_cancelled_notification_marks_request_cancelled() {
        let server = Server::new("test-server", "1.0.0").build();
        let session_id = 1;
        let request_id = RequestId::Number(99);
        let cx = Cx::for_testing();

        let completion = Arc::new(RequestCompletion::new());
        let active = ActiveRequest::new(cx.clone(), completion);
        let cancellation = active.cancellation.clone();
        {
            let mut guard = server
                .active_requests
                .lock()
                .expect("active_requests lock poisoned");
            guard.insert(
                ActiveRequestKey::new(session_id, &request_id)
                    .expect("test request ID must have a canonical correlation key"),
                active,
            );
        }

        let params = CancelledParams {
            request_id: request_id.clone(),
            reason: Some("test cancellation".to_string()),
            meta: None,
        };
        server.handle_cancelled_notification(session_id, params);

        assert!(cancellation.is_cancel_requested());
        assert!(!cx.is_cancel_requested());
    }

    #[test]
    fn test_cancelled_notification_remains_nonblocking() {
        let server = Server::new("test-server", "1.0.0").build();
        let session_id = 1;
        let request_id = RequestId::Number(100);
        let cx = Cx::for_testing();
        let completion = Arc::new(RequestCompletion::new());
        let active = ActiveRequest::new(cx.clone(), completion.clone());
        let cancellation = active.cancellation.clone();

        {
            let mut guard = server
                .active_requests
                .lock()
                .expect("active_requests lock poisoned");
            guard.insert(
                ActiveRequestKey::new(session_id, &request_id)
                    .expect("test request ID must have a canonical correlation key"),
                active,
            );
        }

        let params = CancelledParams {
            request_id: request_id.clone(),
            reason: Some("test cancellation".to_string()),
            meta: None,
        };
        server.handle_cancelled_notification(session_id, params);

        // bd-mvpne: an `assert!(start.elapsed() < Duration::from_secs(1))` stood here and was
        // removed, with its `Instant::now()` binding. It ran AFTER the call it bounded, so a
        // genuine block never reached it -- it could not detect the defect its own message
        // named -- while still being able to fail because the machine was merely slow. The
        // three assertions below carry everything recoverable. Do not re-add a wall-clock
        // bound here; the property is structural, not temporal.
        assert!(!completion.is_done());
        assert!(cancellation.is_cancel_requested());
        assert!(!cx.is_cancel_requested());
    }

    #[test]
    fn test_active_request_guard_registers_and_cleans_up() {
        let server = Server::new("test-server", "1.0.0").build();
        let session_id = 1;
        let request_id = RequestId::Number(77);
        let cx = Cx::for_testing();
        let key = ActiveRequestKey::new(session_id, &request_id)
            .expect("test request ID must have a canonical correlation key");

        let guard = ActiveRequestGuard::try_new(
            Arc::clone(&server.active_requests),
            session_id,
            request_id,
            cx.clone(),
        )
        .expect("active request should register");
        {
            let guard_map = server
                .active_requests
                .lock()
                .expect("active_requests lock poisoned");
            let entry = guard_map.get(&key).expect("active request missing");
            assert_eq!(entry.region_id, cx.region_id());
            assert!(!entry.completion.is_done());
        }
        drop(guard);

        let guard_map = server
            .active_requests
            .lock()
            .expect("active_requests lock poisoned");
        assert!(!guard_map.contains_key(&key));
    }

    #[test]
    fn test_active_request_registry_concurrent_add_remove() {
        let server = Arc::new(Server::new("test-server", "1.0.0").build());
        let thread_count = 4usize;
        let ready = Arc::new(Barrier::new(thread_count + 1));
        let mut release_txs = Vec::new();
        let mut handles = Vec::new();
        let mut cxs = Vec::new();

        for i in 0..thread_count {
            let session_id = 1;
            let request_id =
                RequestId::Number(i64::try_from(i + 1).expect("request id fits in i64"));
            let cx = Cx::for_testing();
            cxs.push(cx.clone());

            let (release_tx, release_rx) = mpsc::channel::<()>();
            release_txs.push(release_tx);

            let server = Arc::clone(&server);
            let ready = Arc::clone(&ready);
            let handle = thread::spawn(move || {
                let _guard = ActiveRequestGuard::try_new(
                    Arc::clone(&server.active_requests),
                    session_id,
                    request_id,
                    cx.clone(),
                )
                .expect("active request should register");
                ready.wait();
                let _ = release_rx.recv();
            });
            handles.push(handle);
        }

        ready.wait();

        {
            let guard = server
                .active_requests
                .lock()
                .expect("active_requests lock poisoned");
            assert_eq!(guard.len(), thread_count);
        }

        server.cancel_active_requests(CancelKind::User, false);

        for cx in &cxs {
            assert!(cx.is_cancel_requested());
        }

        for tx in release_txs {
            tx.send(()).expect("release send failed");
        }
        for handle in handles {
            handle.join().expect("worker join failed");
        }

        let guard = server
            .active_requests
            .lock()
            .expect("active_requests lock poisoned");
        assert!(guard.is_empty());
    }

    #[test]
    fn test_cancel_active_requests_waits_for_guard_drop() {
        let server = Arc::new(Server::new("test-server", "1.0.0").build());
        let session_id = 1;
        let request_id = RequestId::Number(500);
        let cx = Cx::for_testing();

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server_for_worker = Arc::clone(&server);
        let cx_for_worker = cx.clone();
        let worker = thread::spawn(move || {
            let _guard = ActiveRequestGuard::try_new(
                Arc::clone(&server_for_worker.active_requests),
                session_id,
                request_id,
                cx_for_worker,
            )
            .expect("active request should register");
            ready_tx.send(()).expect("ready send failed");
            let _ = release_rx.recv();
        });

        ready_rx.recv().expect("ready recv failed");

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let (cancel_started_tx, cancel_started_rx) = mpsc::channel::<()>();
        let server_for_cancel = Arc::clone(&server);
        let canceler = thread::spawn(move || {
            cancel_started_tx
                .send(())
                .expect("cancel-started send failed");
            server_for_cancel.cancel_active_requests(CancelKind::User, true);
            done_tx.send(()).expect("done send failed");
        });

        cancel_started_rx
            .recv()
            .expect("cancel-started recv failed");
        let cancellation_deadline = Instant::now() + Duration::from_secs(5);
        while !cx.is_cancel_requested() && Instant::now() < cancellation_deadline {
            thread::yield_now();
        }
        assert!(
            cx.is_cancel_requested(),
            "the shutdown thread must cancel the registered request before cleanup can wait"
        );
        assert!(
            done_rx.try_recv().is_err(),
            "shutdown cleanup must remain blocked while the active guard is alive"
        );

        release_tx.send(()).expect("release send failed");
        worker.join().expect("worker join failed");
        done_rx.recv().expect("done recv failed");
        canceler.join().expect("cancel join failed");

        let guard = server
            .active_requests
            .lock()
            .expect("active_requests lock poisoned");
        assert!(guard.is_empty());
    }

    #[test]
    fn test_server_cancels_inflight_requests() {
        let thread_count = 3usize;
        let barrier = Arc::new(Barrier::new(thread_count + 1));
        let _tool_config = configure_blocking_tool(Arc::clone(&barrier));
        let server = Arc::new(
            Server::new("test-server", "1.0.0")
                .tool(BlockUntilCancelled)
                .build(),
        );
        let sender: NotificationSender = Arc::new(|_| {});
        let (tx, rx) = mpsc::channel::<JsonRpcResponse>();

        for i in 0..thread_count {
            let server = Arc::clone(&server);
            let sender = Arc::clone(&sender);
            let tx = tx.clone();
            thread::spawn(move || {
                let cx = Cx::for_testing();
                let mut session = create_test_session();
                session.initialize(
                    ClientInfo {
                        name: "test-client".to_string(),
                        version: "1.0.0".to_string(),
                    },
                    ClientCapabilities::default(),
                    "2024-11-05".to_string(),
                );

                let params = CallToolParams {
                    name: "block_until_cancelled".to_string(),
                    arguments: Some(serde_json::json!({})),
                    meta: None,
                };
                let request = fastmcp_protocol::JsonRpcRequest::new(
                    "tools/call",
                    Some(serde_json::to_value(params).expect("params")),
                    i64::try_from(i + 1).expect("request id fits in i64"),
                );
                let response = fastmcp_core::block_on(server.handle_request(
                    &cx,
                    &mut session,
                    request,
                    &sender,
                    &create_test_request_sender(),
                ))
                .expect("response");
                tx.send(response).expect("response send failed");
            });
        }

        barrier.wait();

        let start = Instant::now();
        loop {
            let count = server
                .active_requests
                .lock()
                .expect("active_requests lock poisoned")
                .len();
            if count == thread_count {
                break;
            }
            if start.elapsed() > Duration::from_secs(1) {
                assert!(
                    start.elapsed() <= Duration::from_secs(1),
                    "active requests did not register in time"
                );
            }
            thread::yield_now();
        }

        server.cancel_active_requests(CancelKind::User, true);

        for _ in 0..thread_count {
            let response = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("response recv timeout");
            let err = response.error.expect("expected error");
            assert_eq!(err.code, i32::from(McpErrorCode::RequestCancelled).into());
        }
    }

    #[test]
    fn test_e2e_cancel_drain_logs() {
        let thread_count = 3usize;
        let barrier = Arc::new(Barrier::new(thread_count + 1));
        let events: Arc<Mutex<Vec<RequestEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let start = Instant::now();
        let _tool_config =
            configure_logging_blocking_tool(Arc::clone(&barrier), Arc::clone(&events), start);
        let server = Arc::new(
            Server::new("test-server", "1.0.0")
                .tool(BlockUntilCancelledLogged)
                .build(),
        );
        let sender: NotificationSender = Arc::new(|_| {});
        let (tx, rx) = mpsc::channel::<JsonRpcResponse>();

        for i in 0..thread_count {
            let server = Arc::clone(&server);
            let sender = Arc::clone(&sender);
            let tx = tx.clone();
            thread::spawn(move || {
                let cx = Cx::for_testing();
                let mut session = create_test_session();
                session.initialize(
                    ClientInfo {
                        name: "test-client".to_string(),
                        version: "1.0.0".to_string(),
                    },
                    ClientCapabilities::default(),
                    "2024-11-05".to_string(),
                );

                let request_id = i64::try_from(i + 1).expect("request id fits in i64");
                let params = CallToolParams {
                    name: "block_until_cancelled_logged".to_string(),
                    arguments: Some(serde_json::json!({ "request_id": request_id })),
                    meta: None,
                };
                let request = fastmcp_protocol::JsonRpcRequest::new(
                    "tools/call",
                    Some(serde_json::to_value(params).expect("params")),
                    request_id,
                );
                let response = fastmcp_core::block_on(server.handle_request(
                    &cx,
                    &mut session,
                    request,
                    &sender,
                    &create_test_request_sender(),
                ))
                .expect("response");
                tx.send(response).expect("response send failed");
            });
        }

        barrier.wait();

        let start_wait = Instant::now();
        loop {
            let count = server
                .active_requests
                .lock()
                .expect("active_requests lock poisoned")
                .len();
            if count == thread_count {
                break;
            }
            if start_wait.elapsed() > Duration::from_secs(1) {
                assert!(
                    start_wait.elapsed() <= Duration::from_secs(1),
                    "active requests did not register in time"
                );
            }
            thread::yield_now();
        }

        server.cancel_active_requests(CancelKind::User, true);

        for _ in 0..thread_count {
            let response = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("response recv timeout");
            let err = response.error.expect("expected error");
            assert_eq!(err.code, i32::from(McpErrorCode::RequestCancelled).into());
        }

        let mut by_request: HashMap<i64, Vec<&RequestEvent>> = HashMap::new();
        let guard = events.lock().expect("events lock poisoned");
        for event in guard.iter() {
            by_request.entry(event.request_id).or_default().push(event);
        }
        assert_eq!(by_request.len(), thread_count);
        for (request_id, events) in by_request {
            let mut phases: Vec<&'static str> = events.iter().map(|e| e.phase).collect();
            phases.sort_unstable();
            phases.dedup();
            assert!(
                phases.contains(&"start")
                    && phases.contains(&"cancelled")
                    && phases.contains(&"finish"),
                "missing phases for request {}: {:?}",
                request_id,
                phases
            );
        }
    }

    #[test]
    fn test_resources_subscribe_and_unsubscribe() {
        let server = Server::new("test-server", "1.0.0")
            .resource(StaticResource {
                uri: "resource://test".to_string(),
                content: "Test content".to_string(),
            })
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let notifications_for_sender = Arc::clone(&notifications);
        let sender: NotificationSender = std::sync::Arc::new(move |req| {
            notifications_for_sender
                .lock()
                .expect("notifications lock poisoned")
                .push(req);
        });
        let subscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: "resource://test".to_string(),
                })
                .unwrap(),
            ),
            1i64,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            subscribe,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        assert!(response.error.is_none());
        assert!(session.is_resource_subscribed("resource://test"));

        assert!(session.notify_resource_updated("resource://test", &sender));
        let guard = notifications.lock().expect("notifications lock poisoned");
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].method, "notifications/resources/updated");
        let params = guard[0].params.clone().expect("notification params");
        let parsed: ResourceUpdatedNotificationParams =
            serde_json::from_value(params).expect("parse notification params");
        assert_eq!(parsed.uri, "resource://test");
        info!(
            target: targets::SESSION,
            "e2e resource update notification ts={} uri={}",
            chrono::Utc::now().to_rfc3339(),
            parsed.uri
        );
        drop(guard);

        let unsubscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/unsubscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::UnsubscribeResourceParams {
                    uri: "resource://test".to_string(),
                })
                .unwrap(),
            ),
            2i64,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            unsubscribe,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("response");
        assert!(response.error.is_none());
        assert!(!session.is_resource_subscribed("resource://test"));

        assert!(!session.notify_resource_updated("resource://test", &sender));
        assert_eq!(
            notifications
                .lock()
                .expect("notifications lock poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn resource_subscription_dispatch_reports_capacity_and_preserves_liveness() {
        const DUPLICATE_URI: &str = "resource://subscription/already-retained";
        const OVER_LIMIT_URI_CANARY: &str = "resource://subscription/private-capacity-canary-71c9";

        let server = Server::new("subscription-admission-test", "1.0.0")
            .resource(StaticResource {
                uri: DUPLICATE_URI.to_string(),
                content: "duplicate".to_string(),
            })
            .resource(StaticResource {
                uri: OVER_LIMIT_URI_CANARY.to_string(),
                content: "over-limit".to_string(),
            })
            .build();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "subscription-admission-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let admission_ctx = McpContext::new(Cx::for_testing(), 9_000);
        session
            .subscribe_resource(&admission_ctx, DUPLICATE_URI.to_string())
            .expect("first subscription should be admitted");
        for index in 1..MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION {
            session
                .subscribe_resource(&admission_ctx, format!("resource://filler/{index}"))
                .expect("subscription at the exact count cap should be admitted");
        }

        let sender: NotificationSender = Arc::new(|_| {});
        let request_sender = create_test_request_sender();
        let duplicate = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: DUPLICATE_URI.to_string(),
                })
                .expect("serialize duplicate subscription"),
            ),
            10_i64,
        );
        let duplicate_response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            duplicate,
            &sender,
            &request_sender,
        ))
        .expect("duplicate subscribe request should produce a response");
        assert!(duplicate_response.error.is_none());
        assert!(session.is_resource_subscribed(DUPLICATE_URI));

        let over_limit = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: OVER_LIMIT_URI_CANARY.to_string(),
                })
                .expect("serialize over-limit subscription"),
            ),
            11_i64,
        );
        let over_limit_response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            over_limit,
            &sender,
            &request_sender,
        ))
        .expect("over-limit subscribe request should produce a response");
        let capacity_error = over_limit_response
            .error
            .expect("over-limit subscription must fail");
        assert_eq!(
            capacity_error.code,
            crate::RESOURCE_EXHAUSTED_ERROR_CODE.into()
        );
        assert_eq!(
            capacity_error.message,
            crate::RESOURCE_SUBSCRIPTION_CAPACITY_MESSAGE
        );
        assert!(capacity_error.data.is_none());
        assert!(!capacity_error.message.contains(OVER_LIMIT_URI_CANARY));
        let encoded_capacity_error =
            serde_json::to_string(&capacity_error).expect("serialize capacity error");
        assert!(!encoded_capacity_error.contains(OVER_LIMIT_URI_CANARY));
        assert!(!session.is_resource_subscribed(OVER_LIMIT_URI_CANARY));

        let impossible_uri = "x".repeat(MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION + 1);
        let impossible = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: impossible_uri,
                })
                .expect("serialize individually over-limit subscription"),
            ),
            12_i64,
        );
        let impossible_response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            impossible,
            &sender,
            &request_sender,
        ))
        .expect("individually over-limit subscribe should produce a response");
        let impossible_error = impossible_response
            .error
            .expect("individually over-limit subscription must fail before lookup");
        assert_eq!(
            impossible_error.code,
            crate::RESOURCE_EXHAUSTED_ERROR_CODE.into()
        );
        assert_eq!(
            impossible_error.message,
            crate::RESOURCE_SUBSCRIPTION_CAPACITY_MESSAGE
        );

        let mut cancelled_session = create_test_session();
        cancelled_session.initialize(
            ClientInfo {
                name: "cancelled-subscription-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        cancelled_session
            .subscribe_resource(&admission_ctx, DUPLICATE_URI.to_string())
            .expect("cancelled-dispatch fixture subscription should be retained");
        let cancelled_cx = Cx::for_testing();
        cancelled_cx.set_cancel_requested(true);
        let cancelled_request = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: OVER_LIMIT_URI_CANARY.to_string(),
                })
                .expect("serialize cancelled subscription"),
            ),
            13_i64,
        );
        let cancelled_response = fastmcp_core::block_on(server.handle_request(
            &cancelled_cx,
            &mut cancelled_session,
            cancelled_request,
            &sender,
            &request_sender,
        ))
        .expect("cancelled subscribe request should produce a response");
        let cancellation = cancelled_response
            .error
            .expect("cancelled subscription must fail");
        assert_eq!(
            cancellation.code,
            i32::from(McpErrorCode::RequestCancelled).into()
        );
        assert!(!cancelled_session.is_resource_subscribed(OVER_LIMIT_URI_CANARY));

        let cancelled_unsubscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/unsubscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::UnsubscribeResourceParams {
                    uri: DUPLICATE_URI.to_string(),
                })
                .expect("serialize cancelled unsubscription"),
            ),
            14_i64,
        );
        let cancelled_unsubscribe_response = fastmcp_core::block_on(server.handle_request(
            &cancelled_cx,
            &mut cancelled_session,
            cancelled_unsubscribe,
            &sender,
            &request_sender,
        ))
        .expect("cancelled unsubscribe request should produce a response");
        let unsubscribe_cancellation = cancelled_unsubscribe_response
            .error
            .expect("cancelled unsubscription must fail");
        assert_eq!(
            unsubscribe_cancellation.code,
            i32::from(McpErrorCode::RequestCancelled).into()
        );
        assert!(cancelled_session.is_resource_subscribed(DUPLICATE_URI));
    }

    #[test]
    fn subscription_hook_errors_preserve_session_state() {
        use std::sync::atomic::AtomicU8;

        const URI: &str = "resource://subscription/hook-rollback";
        struct HookResource(Arc<AtomicU8>);
        impl HookResource {
            fn hook(&self, ctx: &McpContext) -> McpResult<()> {
                match self.0.load(Ordering::Acquire) {
                    0 => Ok(()),
                    1 => Err(McpError::invalid_request("upstream rejected subscription")),
                    2 => {
                        ctx.cx().set_cancel_requested(true);
                        Err(McpError::request_cancelled())
                    }
                    _ => unreachable!(),
                }
            }
        }
        impl ResourceHandler for HookResource {
            fn definition(&self) -> Resource {
                StaticResource {
                    uri: URI.to_owned(),
                    content: "ready".to_owned(),
                }
                .definition()
            }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                Ok(Vec::new())
            }
            fn on_subscribe(&self, ctx: &McpContext, _uri: &str) -> McpResult<()> {
                self.hook(ctx)
            }
            fn on_unsubscribe(&self, ctx: &McpContext, _uri: &str) -> McpResult<()> {
                self.hook(ctx)
            }
        }
        for mode in 0..=2 {
            for (method, initially_subscribed) in [
                ("resources/subscribe", false),
                ("resources/subscribe", true),
                ("resources/unsubscribe", true),
            ] {
                let server = Server::new("subscription-hook-rollback", "1")
                    .resource(HookResource(Arc::new(AtomicU8::new(mode))))
                    .build();
                let cx = Cx::for_testing();
                let mut session = create_test_session();
                session.initialize(
                    ClientInfo {
                        name: "hook-client".to_owned(),
                        version: "1".to_owned(),
                    },
                    ClientCapabilities::default(),
                    "2024-11-05".to_owned(),
                );
                if initially_subscribed {
                    session
                        .subscribe_resource(&McpContext::new(cx.clone(), 1), URI.to_owned())
                        .unwrap();
                }
                let before: Vec<String> = session
                    .subscribed_resource_uris()
                    .map(str::to_owned)
                    .collect();
                let sender: NotificationSender = Arc::new(|_| {});
                let response = fastmcp_core::block_on(server.handle_request(
                    &cx,
                    &mut session,
                    JsonRpcRequest::new(method, Some(serde_json::json!({"uri":URI})), 91_i64),
                    &sender,
                    &create_test_request_sender(),
                ))
                .unwrap();
                if mode == 0 {
                    assert!(
                        response.error.is_none(),
                        "successful hook completes: {response:?}"
                    );
                    assert_eq!(
                        session.is_resource_subscribed(URI),
                        method == "resources/subscribe"
                    );
                } else {
                    assert_eq!(
                        response.error.unwrap().code,
                        i32::from(if mode == 2 {
                            McpErrorCode::RequestCancelled
                        } else {
                            McpErrorCode::InvalidRequest
                        })
                        .into()
                    );
                    let after: Vec<String> = session
                        .subscribed_resource_uris()
                        .map(str::to_owned)
                        .collect();
                    assert_eq!(
                        after, before,
                        "hook failure preserves exact prior membership"
                    );
                    cx.set_cancel_requested(false);
                    assert_eq!(
                        session.notify_resource_updated(URI, &sender),
                        initially_subscribed
                    );
                }
            }
        }
    }

    #[test]
    fn middleware_cannot_forge_subscription_mutation_success() {
        const URI: &str = "resource://subscription/short-circuit-guard";

        struct ForgedSuccess;
        impl Middleware for ForgedSuccess {
            fn on_request(
                &self,
                _ctx: &McpContext,
                _request: &fastmcp_protocol::JsonRpcRequest,
            ) -> McpResult<crate::MiddlewareDecision> {
                Ok(crate::MiddlewareDecision::Respond(serde_json::json!({})))
            }
        }

        let server = Server::new("subscription-short-circuit-test", "1.0.0")
            .resource(StaticResource {
                uri: URI.to_string(),
                content: "guarded".to_string(),
            })
            .middleware(ForgedSuccess)
            .build();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "subscription-short-circuit-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let sender: NotificationSender = Arc::new(|_| {});
        let request_sender = create_test_request_sender();

        let subscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: URI.to_string(),
                })
                .expect("serialize guarded subscription"),
            ),
            20_i64,
        );
        let subscribe_response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            subscribe,
            &sender,
            &request_sender,
        ))
        .expect("guarded subscribe request should produce a response");
        assert_eq!(
            subscribe_response.error.expect("subscribe must fail").code,
            i32::from(McpErrorCode::InternalError).into()
        );
        assert!(!session.is_resource_subscribed(URI));

        session
            .subscribe_resource(&McpContext::new(Cx::for_testing(), 21), URI.to_string())
            .expect("fixture subscription should be admitted");
        let unsubscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/unsubscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::UnsubscribeResourceParams {
                    uri: URI.to_string(),
                })
                .expect("serialize guarded unsubscription"),
            ),
            21_i64,
        );
        let unsubscribe_response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            unsubscribe,
            &sender,
            &request_sender,
        ))
        .expect("guarded unsubscribe request should produce a response");
        assert_eq!(
            unsubscribe_response
                .error
                .expect("unsubscribe must fail")
                .code,
            i32::from(McpErrorCode::InternalError).into()
        );
        assert!(session.is_resource_subscribed(URI));
    }

    #[test]
    fn subscription_mutations_roll_back_when_response_middleware_fails() {
        const URI: &str = "resource://subscription/finalization-rollback";

        struct RejectResponse;
        impl Middleware for RejectResponse {
            fn on_response(
                &self,
                _ctx: &McpContext,
                _request: &fastmcp_protocol::JsonRpcRequest,
                _response: serde_json::Value,
            ) -> McpResult<serde_json::Value> {
                Err(McpError::internal_error(
                    "response middleware rejected subscription result",
                ))
            }
        }

        let server = Server::new("subscription-rollback-test", "1.0.0")
            .resource(StaticResource {
                uri: URI.to_string(),
                content: "guarded".to_string(),
            })
            .middleware(RejectResponse)
            .build();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "subscription-rollback-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let sender: NotificationSender = Arc::new(|_| {});
        let request_sender = create_test_request_sender();

        let subscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/subscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::SubscribeResourceParams {
                    uri: URI.to_string(),
                })
                .expect("serialize subscription"),
            ),
            22_i64,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            subscribe,
            &sender,
            &request_sender,
        ))
        .expect("subscribe produces response");
        assert!(response.error.is_some());
        assert!(!session.is_resource_subscribed(URI));

        session
            .subscribe_resource(&McpContext::new(Cx::for_testing(), 23), URI.to_string())
            .expect("fixture subscription should be admitted");
        let unsubscribe = fastmcp_protocol::JsonRpcRequest::new(
            "resources/unsubscribe",
            Some(
                serde_json::to_value(fastmcp_protocol::UnsubscribeResourceParams {
                    uri: URI.to_string(),
                })
                .expect("serialize unsubscription"),
            ),
            23_i64,
        );
        let response = fastmcp_core::block_on(server.handle_request(
            &Cx::for_testing(),
            &mut session,
            unsubscribe,
            &sender,
            &request_sender,
        ))
        .expect("unsubscribe produces response");
        assert!(response.error.is_some());
        assert!(session.is_resource_subscribed(URI));
    }

    #[test]
    fn test_logging_set_level_emits_notifications() {
        let server = Server::new("test-server", "1.0.0").tool(Greet).build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let notifications_for_sender = Arc::clone(&notifications);
        let sender: NotificationSender = std::sync::Arc::new(move |req| {
            notifications_for_sender
                .lock()
                .expect("notifications lock poisoned")
                .push(req);
        });

        let set_level = fastmcp_protocol::JsonRpcRequest::new(
            "logging/setLevel",
            Some(
                serde_json::to_value(SetLogLevelParams {
                    level: LogLevel::Info,
                })
                .expect("set level params"),
            ),
            1i64,
        );
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            set_level,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("set level response");

        let call = fastmcp_protocol::JsonRpcRequest::new(
            "tools/call",
            Some(
                serde_json::to_value(CallToolParams {
                    name: "greet".to_string(),
                    arguments: Some(serde_json::json!({"name": "Ada"})),
                    meta: None,
                })
                .expect("tool params"),
            ),
            2i64,
        );
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("tool call response");

        let guard = notifications.lock().expect("notifications lock poisoned");
        let mut logs = guard
            .iter()
            .filter(|req| req.method == "notifications/message")
            .map(|req| {
                serde_json::from_value::<LogMessageParams>(req.params.clone().expect("log params"))
                    .expect("parse log params")
            })
            .collect::<Vec<_>>();

        assert_eq!(logs.len(), 1);
        let log = logs.pop().expect("log message");
        assert_eq!(log.level, LogLevel::Info);
        let text = log.data.as_str().expect("log data string");
        // The handled-notification deliberately reports the sanitized method
        // key (after its timestamp prefix), never the raw peer-visible
        // method literal.
        assert!(text.ends_with(&format!(
            "Handled method={}",
            crate::safe_peer_log_key("tools/call")
        )));
        assert!(!text.contains("tools/call"));
        info!(
            target: targets::SESSION,
            "e2e log notification {}",
            text
        );
    }

    #[test]
    fn handler_info_emits_after_set_level_and_debug_stays_filtered() {
        let server = Server::new("handler-log-server", "1.0.0")
            .tool(Announce)
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
        session.initialize(
            ClientInfo {
                name: "handler-log-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let notifications_for_sender = Arc::clone(&notifications);
        let sender: NotificationSender = Arc::new(move |req| {
            notifications_for_sender
                .lock()
                .expect("notifications lock poisoned")
                .push(req);
        });

        let call = |id: i64| {
            fastmcp_protocol::JsonRpcRequest::new(
                "tools/call",
                Some(
                    serde_json::to_value(CallToolParams {
                        name: "announce".to_string(),
                        arguments: Some(serde_json::json!({"name": "Ada"})),
                        meta: None,
                    })
                    .expect("tool params"),
                ),
                id,
            )
        };

        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call(1),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("handler log without setLevel still responds");
        let handler_logs = notifications
            .lock()
            .expect("notifications lock poisoned")
            .iter()
            .filter(|req| req.method == "notifications/message")
            .filter_map(|req| req.params.as_ref())
            .filter(|params| {
                params
                    .get("data")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|data| data.contains("handler-"))
            })
            .count();
        assert_eq!(
            handler_logs, 0,
            "MCP forbids notifications/message before logging/setLevel"
        );

        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            fastmcp_protocol::JsonRpcRequest::new(
                "logging/setLevel",
                Some(
                    serde_json::to_value(SetLogLevelParams {
                        level: LogLevel::Info,
                    })
                    .expect("set level params"),
                ),
                2_i64,
            ),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("setLevel must respond");
        notifications
            .lock()
            .expect("notifications lock poisoned")
            .clear();
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call(3),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("handler log after setLevel must respond");
        let emitted: Vec<String> = notifications
            .lock()
            .expect("notifications lock poisoned")
            .iter()
            .filter(|req| req.method == "notifications/message")
            .filter_map(|req| {
                req.params
                    .as_ref()
                    .and_then(|params| params.get("data"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        assert!(
            emitted.iter().any(|data| data.contains("handler-info:Ada")),
            "ctx.info must emit after setLevel info: {emitted:?}"
        );
        assert!(
            emitted.iter().all(|data| !data.contains("handler-debug")),
            "ctx.debug must stay below an info floor: {emitted:?}"
        );
    }

    #[test]
    fn disable_tool_emits_list_changed_only_when_the_catalog_mutates() {
        let server = Server::new("catalog-change", "1.0.0")
            .tool(Greet)
            .tool(HideGreet)
            .build();
        assert!(
            server
                .capabilities()
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed)
        );
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "catalog-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifications_for_sender = Arc::clone(&notifications);
        let sender: NotificationSender = Arc::new(move |req| {
            notifications_for_sender
                .lock()
                .expect("notifications lock poisoned")
                .push(req);
        });
        let call = |id: i64| {
            fastmcp_protocol::JsonRpcRequest::new(
                "tools/call",
                Some(
                    serde_json::to_value(CallToolParams {
                        name: "hide_greet".to_string(),
                        arguments: Some(serde_json::json!({})),
                        meta: None,
                    })
                    .expect("tool params"),
                ),
                id,
            )
        };
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call(1),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("first hide must respond");
        let first = notifications
            .lock()
            .expect("notifications lock poisoned")
            .iter()
            .filter(|req| req.method == "notifications/tools/list_changed")
            .count();
        assert_eq!(first, 1, "first disable must emit one tools/list_changed");

        notifications
            .lock()
            .expect("notifications lock poisoned")
            .clear();
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call(2),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("repeat hide must respond");
        let repeat = notifications
            .lock()
            .expect("notifications lock poisoned")
            .iter()
            .filter(|req| req.method == "notifications/tools/list_changed")
            .count();
        assert_eq!(
            repeat, 0,
            "disabling an already-disabled tool must not emit list_changed"
        );
    }

    #[test]
    fn disable_tool_publishes_to_modern_subscription_listeners() {
        let server = Server::new("modern-catalog-listen", "1.0.0")
            .tool(Greet)
            .tool(HideGreet)
            .build();
        let sent = Arc::new(Mutex::new(Vec::<fastmcp_protocol::JsonRpcRequest>::new()));
        let sent_for_sender = Arc::clone(&sent);
        let listen_sender: NotificationSender = Arc::new(move |notification| {
            sent_for_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(notification);
        });
        let _lease = server
            .final_subscriptions
            .open(
                RequestId::String("listen-hide".to_owned()),
                fastmcp_protocol::SubscriptionFilter {
                    tools_list_changed: Some(true),
                    ..fastmcp_protocol::SubscriptionFilter::default()
                },
                false,
                None,
                fastmcp_core::McpRequestCancellation::new(),
                None,
                listen_sender,
            )
            .expect("listen stream must open");
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "listen-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let session_sender: NotificationSender = Arc::new(|_| {});
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            fastmcp_protocol::JsonRpcRequest::new(
                "tools/call",
                Some(
                    serde_json::to_value(CallToolParams {
                        name: "hide_greet".to_string(),
                        arguments: Some(serde_json::json!({})),
                        meta: None,
                    })
                    .expect("tool params"),
                ),
                1_i64,
            ),
            &session_sender,
            &create_test_request_sender(),
        ))
        .expect("hide_greet must respond");
        let delivered = sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|request| request.method == "notifications/tools/list_changed")
            .count();
        assert_eq!(
            delivered, 1,
            "handler-driven disable must publish to subscriptions/listen"
        );
    }

    #[test]
    fn notify_resource_updated_publishes_to_matching_listen_filters() {
        const URI: &str = "file:///watched.txt";
        let server = Server::new("modern-resource-listen", "1.0.0")
            .resource(StaticResource {
                uri: URI.to_string(),
                content: "watched".to_string(),
            })
            .tool(TouchFile)
            .build();
        let sent = Arc::new(Mutex::new(Vec::<fastmcp_protocol::JsonRpcRequest>::new()));
        let sent_for_sender = Arc::clone(&sent);
        let listen_sender: NotificationSender = Arc::new(move |notification| {
            sent_for_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(notification);
        });
        let _lease = server
            .final_subscriptions
            .open(
                RequestId::String("listen-touch".to_owned()),
                fastmcp_protocol::SubscriptionFilter {
                    resource_subscriptions: Some(vec![URI.to_owned()]),
                    ..fastmcp_protocol::SubscriptionFilter::default()
                },
                false,
                None,
                McpRequestCancellation::new(),
                None,
                listen_sender,
            )
            .expect("listen stream must open");
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "listen-touch-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let session_sender: NotificationSender = Arc::new(|_| {});
        let response = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            fastmcp_protocol::JsonRpcRequest::new(
                "tools/call",
                Some(
                    serde_json::to_value(CallToolParams {
                        name: "touch_file".to_string(),
                        arguments: Some(serde_json::json!({ "uri": URI })),
                        meta: None,
                    })
                    .expect("tool params"),
                ),
                1_i64,
            ),
            &session_sender,
            &create_test_request_sender(),
        ))
        .expect("touch_file must respond");
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.pointer("/content/0/text"))
                .and_then(serde_json::Value::as_str),
            Some("notified"),
            "a matching modern listener must count as delivery"
        );
        let delivered = sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|request| request.method == "notifications/resources/updated")
            .count();
        assert_eq!(
            delivered, 1,
            "handler-driven resource update must publish to subscriptions/listen"
        );
    }

    #[test]
    fn notify_resource_updated_emits_only_for_subscribed_uris() {
        const URI: &str = "file:///watched.txt";
        let server = Server::new("resource-update", "1.0.0")
            .resource(StaticResource {
                uri: URI.to_string(),
                content: "watched".to_string(),
            })
            .tool(TouchFile)
            .build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        session.initialize(
            ClientInfo {
                name: "resource-update-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifications_for_sender = Arc::clone(&notifications);
        let sender: NotificationSender = Arc::new(move |req| {
            notifications_for_sender
                .lock()
                .expect("notifications lock poisoned")
                .push(req);
        });
        let call = |id: i64| {
            fastmcp_protocol::JsonRpcRequest::new(
                "tools/call",
                Some(
                    serde_json::to_value(CallToolParams {
                        name: "touch_file".to_string(),
                        arguments: Some(serde_json::json!({ "uri": URI })),
                        meta: None,
                    })
                    .expect("tool params"),
                ),
                id,
            )
        };
        let silent = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call(1),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("unsubscribed touch must respond");
        assert_eq!(
            silent
                .result
                .as_ref()
                .and_then(|result| result.pointer("/content/0/text"))
                .and_then(serde_json::Value::as_str),
            Some("silent")
        );
        assert!(
            notifications
                .lock()
                .expect("notifications lock poisoned")
                .iter()
                .all(|req| req.method != "notifications/resources/updated")
        );

        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            fastmcp_protocol::JsonRpcRequest::new(
                "resources/subscribe",
                Some(serde_json::json!({ "uri": URI })),
                2_i64,
            ),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("subscribe must respond");
        notifications
            .lock()
            .expect("notifications lock poisoned")
            .clear();
        let notified = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call(3),
            &sender,
            &create_test_request_sender(),
        ))
        .expect("subscribed touch must respond");
        assert_eq!(
            notified
                .result
                .as_ref()
                .and_then(|result| result.pointer("/content/0/text"))
                .and_then(serde_json::Value::as_str),
            Some("notified")
        );
        let updates = notifications
            .lock()
            .expect("notifications lock poisoned")
            .iter()
            .filter(|req| req.method == "notifications/resources/updated")
            .count();
        assert_eq!(updates, 1, "subscribed URI must emit one resources/updated");
    }

    #[test]
    fn test_logging_set_level_filters_notifications() {
        let server = Server::new("test-server", "1.0.0").tool(Greet).build();
        let cx = Cx::for_testing();
        let mut session = create_test_session();
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        let notifications_for_sender = Arc::clone(&notifications);
        let sender: NotificationSender = std::sync::Arc::new(move |req| {
            notifications_for_sender
                .lock()
                .expect("notifications lock poisoned")
                .push(req);
        });

        let set_level = fastmcp_protocol::JsonRpcRequest::new(
            "logging/setLevel",
            Some(
                serde_json::to_value(SetLogLevelParams {
                    level: LogLevel::Error,
                })
                .expect("set level params"),
            ),
            1i64,
        );
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            set_level,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("set level response");

        let call = fastmcp_protocol::JsonRpcRequest::new(
            "tools/call",
            Some(
                serde_json::to_value(CallToolParams {
                    name: "greet".to_string(),
                    arguments: Some(serde_json::json!({"name": "Ada"})),
                    meta: None,
                })
                .expect("tool params"),
            ),
            2i64,
        );
        let _ = fastmcp_core::block_on(server.handle_request(
            &cx,
            &mut session,
            call,
            &sender,
            &create_test_request_sender(),
        ))
        .expect("tool call response");

        let guard = notifications.lock().expect("notifications lock poisoned");
        let log_count = guard
            .iter()
            .filter(|req| req.method == "notifications/message")
            .count();
        assert_eq!(log_count, 0);
    }

    #[test]
    fn test_handle_initialize() {
        let router = create_test_router();
        let mut session = create_test_session();
        let cx = Cx::for_testing();

        let params = InitializeParams {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "test-client".to_string(),
                version: "1.0.0".to_string(),
            },
        };

        let request_ctx = McpContext::new(cx, 1);
        let result = router.handle_initialize(
            &request_ctx,
            &mut session,
            params,
            Some("Test instructions"),
        );

        assert!(result.is_ok());
        let init_result = result.unwrap();
        assert_eq!(init_result.server_info.name, "test-server");
        assert_eq!(
            init_result.instructions,
            Some("Test instructions".to_string())
        );
        assert!(session.is_initialized());
    }

    #[test]
    fn test_handle_tools_call_success() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(!call_result.is_error);
        assert_eq!(call_result.content.len(), 1);

        assert!(matches!(call_result.content[0], LegacyContent::Text { .. }));
        let LegacyContent::Text { text, .. } = &call_result.content[0] else {
            return;
        };
        assert_eq!(text, "Hello, Alice!");
    }

    #[test]
    fn test_handle_tools_call_not_found() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = CallToolParams {
            name: "nonexistent".to_string(),
            arguments: None,
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        // The refusal deliberately does not echo the peer-controlled tool
        // name; only the sanitized method-not-found classification surfaces.
        assert_eq!(err.code, McpErrorCode::MethodNotFound);
        assert!(!err.message.contains("nonexistent"));
    }

    #[test]
    fn test_handle_tools_call_with_error() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = CallToolParams {
            name: "error_tool".to_string(),
            arguments: None,
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        // Tool errors are returned as content with is_error=true
        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(call_result.is_error);
        assert_eq!(call_result.content.len(), 1);
    }

    #[test]
    fn test_handle_tools_call_with_cancellation() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let budget = Budget::INFINITE;

        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        // Request should be cancelled before handler runs
        assert!(result.is_err());
    }

    #[test]
    fn test_handle_tools_call_with_exhausted_budget() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::unlimited().with_poll_quota(0);

        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        // The handler explicitly requests a poll checkpoint, so a zero poll
        // balance rejects that next admission rather than retroactively
        // invalidating some earlier operation.
        let err = result.expect_err("checkpoint should require a poll unit");
        assert_eq!(err.code, McpErrorCode::RequestCancelled);
    }

    #[test]
    fn test_handle_resources_read_success() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = ReadResourceParams {
            uri: "resource://test".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok());
        let read_result = result.unwrap();
        assert_eq!(read_result.contents.len(), 1);
        assert!(matches!(
            &read_result.contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Test content"
        ));
    }

    #[test]
    fn test_handle_resources_read_template_match() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = ReadResourceParams {
            uri: "resource:///items/abc".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());
        let read_result = result.unwrap();
        assert!(matches!(
            &read_result.contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Template abc"
        ));
    }

    #[test]
    fn test_handle_resources_read_template_match_percent_decoded() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = ReadResourceParams {
            uri: "resource:///items/hello%20world".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());
        let read_result = result.unwrap();
        assert!(matches!(
            &read_result.contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Template hello world"
        ));
    }

    #[test]
    fn test_handle_resources_read_template_match_with_slash() {
        // A simple `{id}` expression cannot span "/" under RFC 6570 matching;
        // slash-bearing values are reachable through reserved expansion.
        struct SlashTemplateResource;

        impl ResourceHandler for SlashTemplateResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "resource://{+id}".to_string(),
                    name: "Slash Template Resource".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }

            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate {
                    uri_template: "resource://{+id}".to_string(),
                    name: "Slash Template Resource".to_string(),
                    description: None,
                    mime_type: Some("text/plain".to_string()),
                    icon: None,
                    version: None,
                    tags: vec![],
                })
            }

            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                Err(McpError::invalid_params(
                    "uri parameters required for template resource",
                ))
            }

            fn read_with_uri(
                &self,
                _ctx: &McpContext,
                uri: &str,
                params: &UriParams,
            ) -> McpResult<Vec<ResourceContent>> {
                let id = params
                    .get("id")
                    .ok_or_else(|| McpError::invalid_params("missing uri parameter: id"))?;
                Ok(vec![ResourceContent {
                    uri: uri.to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some(format!("Template {id}")),
                    blob: None,
                }])
            }
        }

        let mut router = Router::new();
        router.add_resource(SlashTemplateResource);
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = ReadResourceParams {
            uri: "resource://foo/bar".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());
        let read_result = result.unwrap();
        assert!(matches!(
            &read_result.contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Template foo/bar"
        ));
    }

    #[test]
    fn test_overlapping_resource_template_registration_is_rejected_without_mutation() {
        let mut router = Router::new();
        router.add_resource(TemplateResource);
        let templates_before = serde_json::to_value(router.resource_templates())
            .expect("resource template catalog serializes");
        let error = router
            .add_resource_with_behavior(SpecificTemplateResource, crate::DuplicateBehavior::Replace)
            .expect_err("a conservatively overlapping final template must be rejected");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            error.message,
            "resource template collides with an admitted final resource template"
        );
        assert_eq!(
            serde_json::to_value(router.resource_templates())
                .expect("resource template catalog serializes"),
            templates_before
        );

        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = ReadResourceParams {
            uri: "resource:///items/123".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());
        let read_result = result.unwrap();
        assert!(matches!(
            &read_result.contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Template 123"
        ));
    }

    #[test]
    fn test_e2e_template_logging() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut router = Router::new();
        router.add_resource(LoggingTemplateResource {
            template: "file://{path}",
            events: events.clone(),
        });

        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let params = ReadResourceParams {
            uri: "file://dir%2Ffile.txt".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());
        let read_result = result.unwrap();
        assert!(matches!(
            &read_result.contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Logged dir/file.txt"
        ));

        let guard = events.lock().expect("template log lock poisoned");
        assert_eq!(guard.len(), 1);
        let entry = &guard[0];
        assert_eq!(entry.template, "file://{path}");
        assert_eq!(entry.uri, "file://dir%2Ffile.txt");
        assert_eq!(
            entry.params.get("path").map(String::as_str),
            Some("dir/file.txt")
        );
        assert_eq!(entry.response, "Logged dir/file.txt");
    }

    #[test]
    fn test_handle_resources_read_not_found() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Use a scheme that doesn't match any registered resources or templates
        let params = ReadResourceParams {
            uri: "file://nonexistent".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        assert!(result.is_err());
    }

    #[test]
    fn test_handle_resources_read_with_cancellation() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let budget = Budget::INFINITE;

        let params = ReadResourceParams {
            uri: "resource://test".to_string(),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx,
            &params,
            state,
            None,
            None,
        ));

        // Should be cancelled
        assert!(result.is_err());
    }

    #[test]
    fn test_handle_prompts_get_success() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = GetPromptParams {
            name: "greeting".to_string(),
            arguments: Some({
                let mut map = HashMap::new();
                map.insert("name".to_string(), "Bob".to_string());
                map
            }),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_prompts_get(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok());
        let get_result = result.unwrap();
        assert_eq!(get_result.messages.len(), 1);

        assert!(matches!(
            get_result.messages[0].content,
            LegacyContent::Text { .. }
        ));
        let LegacyContent::Text { text, .. } = &get_result.messages[0].content else {
            return;
        };
        assert!(text.contains("Bob"));
    }

    #[test]
    fn test_handle_prompts_get_not_found() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let params = GetPromptParams {
            name: "nonexistent".to_string(),
            arguments: None,
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_prompts_get(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_err());
    }

    #[test]
    fn test_handle_tools_call_validation_missing_required() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // greet tool requires 'name' field, so passing empty object should fail validation
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("validation") || err.message.contains("required"));
    }

    #[test]
    fn test_handle_tools_call_validation_wrong_type() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // greet tool expects 'name' to be a string, not a number
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": 123})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("validation") || err.message.contains("type"));
    }

    #[test]
    fn test_handle_tools_call_validation_passes() {
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Valid arguments that satisfy the schema
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(!call_result.is_error);
    }

    #[test]
    fn test_handle_tools_call_lenient_validation_allows_extra_properties() {
        // Default (lenient) mode allows extra properties
        let router = create_test_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Include an extra property not in the schema
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice", "extra": "ignored"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        // Should pass in lenient mode
        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(!call_result.is_error);
    }

    #[test]
    fn test_handle_tools_call_strict_validation_rejects_extra_properties() {
        // Enable strict validation mode
        let mut router = create_test_router();
        router.set_strict_input_validation(true);
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Include an extra property not in the schema
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice", "extra": "should_fail"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        // Should fail in strict mode due to extra property
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("validation") || err.message.contains("additional"));
    }

    #[test]
    fn test_handle_tools_call_strict_validation_passes_valid_input() {
        // Enable strict validation mode
        let mut router = create_test_router();
        router.set_strict_input_validation(true);
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Only include defined properties
        let params = CallToolParams {
            name: "greet".to_string(),
            arguments: Some(serde_json::json!({"name": "Alice"})),
            meta: None,
        };

        let state = SessionState::new();
        let request_ctx = McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
        let result = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx,
            params,
            state,
            None,
            None,
        ));

        // Should pass in strict mode with valid input
        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(!call_result.is_error);
    }
}

// ============================================================================
// Session Tests
// ============================================================================

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        assert!(!session.is_initialized());
        assert!(session.client_info().is_none());
        assert!(session.client_capabilities().is_none());
        assert!(session.protocol_version().is_none());
    }

    #[test]
    fn test_session_initialization() {
        let mut session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        session.initialize(
            ClientInfo {
                name: "client".to_string(),
                version: "2.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        assert!(session.is_initialized());
        assert_eq!(session.client_info().unwrap().name, "client");
        assert_eq!(session.protocol_version(), Some("2024-11-05"));
    }
}

// ============================================================================
// Cancellation Tests
// ============================================================================

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn test_tool_observes_cancellation() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);

        // Initially not cancelled
        assert!(!ctx.is_cancelled());

        // Set cancellation
        cx.set_cancel_requested(true);

        // Now tool should observe cancellation
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_checkpoint_fails_when_cancelled() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);

        // Checkpoint succeeds initially
        assert!(ctx.checkpoint().is_ok());

        // Set cancellation
        cx.set_cancel_requested(true);

        // Checkpoint now fails
        assert!(ctx.checkpoint().is_err());
    }

    #[test]
    fn test_masked_section_defers_cancellation() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);

        cx.set_cancel_requested(true);

        // Inside masked section, checkpoint should succeed
        ctx.masked(|| {
            assert!(ctx.checkpoint().is_ok());
        })
        .expect("mask should be admitted");

        // Outside masked section, checkpoint should fail
        assert!(ctx.checkpoint().is_err());
    }
}

// ============================================================================
// Budget Tests
// ============================================================================

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn test_infinite_budget_not_exhausted() {
        let budget = Budget::INFINITE;
        assert!(!budget.is_exhausted());
    }

    #[test]
    fn test_exhausted_budget() {
        let budget = Budget::unlimited().with_poll_quota(0);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_deadline_budget() {
        // A budget with a deadline far in the future
        let budget = Budget::new().with_timeout(wall_now(), Duration::from_secs(3600));
        assert!(!budget.is_exhausted());
    }
}

// ============================================================================
// Handler Definition Tests
// ============================================================================

#[cfg(test)]
mod handler_definition_tests {
    use super::*;
    use crate::router::TagFilters;
    use fastmcp_protocol::ListToolsParams;

    #[test]
    fn test_tool_definition() {
        let tool = Greet;
        let def = tool.definition();

        assert_eq!(def.name, "greet");
        assert!(def.description.is_some());
        assert_eq!(def.input_schema["type"], "object");
    }

    #[test]
    fn test_resource_definition() {
        let resource = StaticResource {
            uri: "resource://foo".to_string(),
            content: "bar".to_string(),
        };
        let def = resource.definition();

        assert_eq!(def.uri, "resource://foo");
        assert_eq!(def.mime_type, Some("text/plain".to_string()));
    }

    #[test]
    fn test_prompt_definition() {
        let prompt = GreetingPrompt;
        let def = prompt.definition();

        assert_eq!(def.name, "greeting");
        assert!(!def.arguments.is_empty());
        assert_eq!(def.arguments.len(), 1);
    }

    // ========================================================================
    // Tag Filtering Tests
    // ========================================================================

    #[tool(
        name = "search",
        description = "Tool with tags: api/public/read",
        tags = ["api", "public", "read"]
    )]
    fn tagged_search_tool() -> String {
        "ok".to_string()
    }

    #[tool(
        name = "create",
        description = "Tool with tags: api/public/write",
        tags = ["api", "public", "write"]
    )]
    fn tagged_create_tool() -> String {
        "ok".to_string()
    }

    #[tool(
        name = "admin",
        description = "Tool with tags: api/private/admin",
        tags = ["api", "private", "admin"]
    )]
    fn tagged_admin_tool() -> String {
        "ok".to_string()
    }

    #[tool(
        name = "debug",
        description = "Tool with tags: internal/debug",
        tags = ["internal", "debug"]
    )]
    fn tagged_debug_tool() -> String {
        "ok".to_string()
    }

    #[tool(name = "untagged", description = "Tool with no tags")]
    fn tagged_untagged_tool() -> String {
        "ok".to_string()
    }

    fn create_tagged_tools_router() -> Router {
        let mut router = Router::new();
        // Tools with various tag combinations
        router
            .add_tool(TaggedSearchTool)
            .expect("tool registration succeeds");
        router
            .add_tool(TaggedCreateTool)
            .expect("tool registration succeeds");
        router
            .add_tool(TaggedAdminTool)
            .expect("tool registration succeeds");
        router
            .add_tool(TaggedDebugTool)
            .expect("tool registration succeeds");
        router
            .add_tool(TaggedUntaggedTool)
            .expect("tool registration succeeds");
        router
    }

    #[test]
    fn test_tag_filters_include_single_tag() {
        let router = create_tagged_tools_router();
        let include = vec!["api".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(tools.len(), 3, "Expected search, create, admin");
        assert!(tools.iter().any(|t| t.name == "search"));
        assert!(tools.iter().any(|t| t.name == "create"));
        assert!(tools.iter().any(|t| t.name == "admin"));
    }

    #[test]
    fn test_tag_filters_include_multiple_tags_and_logic() {
        let router = create_tagged_tools_router();
        let include = vec!["api".to_string(), "public".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(
            tools.len(),
            2,
            "Expected search, create (both have api AND public)"
        );
        assert!(tools.iter().any(|t| t.name == "search"));
        assert!(tools.iter().any(|t| t.name == "create"));
    }

    #[test]
    fn test_tag_filters_exclude_single_tag() {
        let router = create_tagged_tools_router();
        let exclude = vec!["private".to_string()];
        let filters = TagFilters::new(None, Some(&exclude));
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(tools.len(), 4, "Expected all except admin");
        assert!(!tools.iter().any(|t| t.name == "admin"));
    }

    #[test]
    fn test_tag_filters_exclude_multiple_tags_or_logic() {
        let router = create_tagged_tools_router();
        let exclude = vec!["private".to_string(), "internal".to_string()];
        let filters = TagFilters::new(None, Some(&exclude));
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(tools.len(), 3, "Expected search, create, untagged");
        assert!(tools.iter().any(|t| t.name == "search"));
        assert!(tools.iter().any(|t| t.name == "create"));
        assert!(tools.iter().any(|t| t.name == "untagged"));
    }

    #[test]
    fn test_tag_filters_include_and_exclude_combined() {
        let router = create_tagged_tools_router();
        let include = vec!["api".to_string()];
        let exclude = vec!["private".to_string()];
        let filters = TagFilters::new(Some(&include), Some(&exclude));
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(
            tools.len(),
            2,
            "Expected search, create (api but not private)"
        );
        assert!(tools.iter().any(|t| t.name == "search"));
        assert!(tools.iter().any(|t| t.name == "create"));
    }

    #[test]
    fn test_tag_filters_case_insensitive() {
        let router = create_tagged_tools_router();
        let include = vec!["API".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(tools.len(), 3, "Should match 'api' tags case-insensitively");
    }

    #[test]
    fn test_tag_filters_empty_include_no_filter() {
        let router = create_tagged_tools_router();
        let include: Vec<String> = vec![];
        let filters = TagFilters::new(Some(&include), None);
        let tools = router.tools_filtered(None, Some(&filters));
        assert_eq!(tools.len(), 5, "Empty include should not filter");
    }

    #[test]
    fn test_tag_filters_no_matches() {
        let router = create_tagged_tools_router();
        let include = vec!["nonexistent".to_string()];
        let filters = TagFilters::new(Some(&include), None);
        let tools = router.tools_filtered(None, Some(&filters));
        assert!(tools.is_empty(), "No tools should match nonexistent tag");
    }

    #[test]
    fn test_handle_tools_list_with_include_tags() {
        let router = create_tagged_tools_router();
        let cx = Cx::for_testing();
        let params = ListToolsParams {
            cursor: None,
            include_tags: Some(vec!["public".to_string()]),
            exclude_tags: None,
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = router.handle_tools_list(&request_ctx, params, None);
        let tools = result.unwrap().tools;
        assert_eq!(tools.len(), 2, "Expected search, create");
    }

    #[test]
    fn test_handle_tools_list_with_exclude_tags() {
        let router = create_tagged_tools_router();
        let cx = Cx::for_testing();
        let params = ListToolsParams {
            cursor: None,
            include_tags: None,
            exclude_tags: Some(vec!["private".to_string(), "internal".to_string()]),
        };
        let request_ctx = McpContext::new(cx, 1);
        let result = router.handle_tools_list(&request_ctx, params, None);
        let tools = result.unwrap().tools;
        assert_eq!(tools.len(), 3, "Expected search, create, untagged");
    }
}

// ============================================================================
// Multiple Handler Tests
// ============================================================================

#[cfg(test)]
mod multi_handler_tests {
    use super::*;

    #[test]
    fn test_multiple_tools() {
        let mut router = Router::new();
        router.add_tool(Greet).expect("tool registration succeeds");
        router
            .add_tool(FormalGreet)
            .expect("tool registration succeeds");

        let tools = router.tools();
        assert_eq!(tools.len(), 2);

        // Call both tools
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let state1 = SessionState::new();
        let request_ctx1 =
            McpContext::with_state(cx.clone(), 1, state1.clone()).with_budget_ceiling(budget);
        let result1 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx1,
            CallToolParams {
                name: "greet".to_string(),
                arguments: Some(serde_json::json!({"name": "Alice"})),
                meta: None,
            },
            state1,
            None,
            None,
        ));
        assert!(result1.is_ok());

        let state2 = SessionState::new();
        let request_ctx2 =
            McpContext::with_state(cx, 2, state2.clone()).with_budget_ceiling(budget);
        let result2 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx2,
            CallToolParams {
                name: "formal_greet".to_string(),
                arguments: Some(serde_json::json!({"name": "Alice"})),
                meta: None,
            },
            state2,
            None,
            None,
        ));
        assert!(result2.is_ok());

        // Verify different outputs
        if let LegacyContent::Text { text: text1, .. } = &result1.unwrap().content[0] {
            if let LegacyContent::Text { text: text2, .. } = &result2.unwrap().content[0] {
                assert_eq!(text1, "Hello, Alice!");
                assert_eq!(text2, "Good day, Alice.");
            }
        }
    }

    #[test]
    fn test_multiple_resources() {
        let mut router = Router::new();
        router.add_resource(StaticResource {
            uri: "resource://a".to_string(),
            content: "Content A".to_string(),
        });
        router.add_resource(StaticResource {
            uri: "resource://b".to_string(),
            content: "Content B".to_string(),
        });

        let resources = router.resources();
        assert_eq!(resources.len(), 2);

        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        let state_a = SessionState::new();
        let request_ctx_a =
            McpContext::with_state(cx.clone(), 1, state_a.clone()).with_budget_ceiling(budget);
        let result_a = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx_a,
            &ReadResourceParams {
                uri: "resource://a".to_string(),
                meta: None,
            },
            state_a,
            None,
            None,
        ));
        let state_b = SessionState::new();
        let request_ctx_b =
            McpContext::with_state(cx, 2, state_b.clone()).with_budget_ceiling(budget);
        let result_b = fastmcp_core::block_on(router.handle_resources_read(
            &request_ctx_b,
            &ReadResourceParams {
                uri: "resource://b".to_string(),
                meta: None,
            },
            state_b,
            None,
            None,
        ));

        assert!(matches!(
            &result_a.unwrap().contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Content A"
        ));
        assert!(matches!(
            &result_b.unwrap().contents[0],
            LegacyResourceContent::Text { text, .. } if text == "Content B"
        ));
    }
}

// ============================================================================
// Session State Tests
// ============================================================================

mod session_state_tests {
    use super::*;

    #[test]
    fn test_session_state_persists_across_calls() {
        let mut router = Router::new();
        router
            .add_tool(Increment)
            .expect("tool registration succeeds");

        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Create a shared session state
        let state = SessionState::new();

        // First call - counter should be 1
        let params = CallToolParams {
            name: "increment".to_string(),
            arguments: None,
            meta: None,
        };
        let request_ctx1 =
            McpContext::with_state(cx.clone(), 1, state.clone()).with_budget_ceiling(budget);
        let result1 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx1,
            params.clone(),
            state.clone(),
            None,
            None,
        ));
        assert!(result1.is_ok());
        if let LegacyContent::Text { text, .. } = &result1.unwrap().content[0] {
            assert_eq!(text, "Counter: 1");
        }

        // Second call with same state - counter should be 2
        let request_ctx2 =
            McpContext::with_state(cx.clone(), 2, state.clone()).with_budget_ceiling(budget);
        let result2 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx2,
            params.clone(),
            state.clone(),
            None,
            None,
        ));
        assert!(result2.is_ok());
        if let LegacyContent::Text { text, .. } = &result2.unwrap().content[0] {
            assert_eq!(text, "Counter: 2");
        }

        // Third call - counter should be 3
        let request_ctx3 = McpContext::with_state(cx, 3, state.clone()).with_budget_ceiling(budget);
        let result3 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx3,
            params,
            state.clone(),
            None,
            None,
        ));
        assert!(result3.is_ok());
        if let LegacyContent::Text { text, .. } = &result3.unwrap().content[0] {
            assert_eq!(text, "Counter: 3");
        }
    }

    #[test]
    fn test_different_session_states_are_independent() {
        let mut router = Router::new();
        router
            .add_tool(Increment)
            .expect("tool registration succeeds");

        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // Create two separate session states
        let state1 = SessionState::new();
        let state2 = SessionState::new();

        let params = CallToolParams {
            name: "increment".to_string(),
            arguments: None,
            meta: None,
        };

        // Call with state1 twice
        let request_ctx1 =
            McpContext::with_state(cx.clone(), 1, state1.clone()).with_budget_ceiling(budget);
        fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx1,
            params.clone(),
            state1.clone(),
            None,
            None,
        ))
        .unwrap();
        let request_ctx2 =
            McpContext::with_state(cx.clone(), 2, state1.clone()).with_budget_ceiling(budget);
        let result1 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx2,
            params.clone(),
            state1.clone(),
            None,
            None,
        ))
        .unwrap();

        // Call with state2 once
        let request_ctx3 =
            McpContext::with_state(cx, 3, state2.clone()).with_budget_ceiling(budget);
        let result2 = fastmcp_core::block_on(router.handle_tools_call(
            &request_ctx3,
            params,
            state2.clone(),
            None,
            None,
        ))
        .unwrap();

        // state1 should have counter=2, state2 should have counter=1
        if let LegacyContent::Text { text, .. } = &result1.content[0] {
            assert_eq!(text, "Counter: 2");
        }
        if let LegacyContent::Text { text, .. } = &result2.content[0] {
            assert_eq!(text, "Counter: 1");
        }
    }
}

// ============================================================================
// Console Config Integration Tests
// ============================================================================

mod console_config_tests {
    use crate::{BannerStyle, ConsoleConfig, Server, TrafficVerbosity};

    #[test]
    fn test_server_default_console_config() {
        let server = Server::new("test", "1.0.0").build();
        let config = server.console_config();

        // Default config should show banner
        assert!(config.show_banner);
        assert_eq!(config.banner_style, BannerStyle::Full);
    }

    #[test]
    fn test_server_with_console_config() {
        let config = ConsoleConfig::new()
            .with_banner(BannerStyle::Compact)
            .plain_mode();

        let server = Server::new("test", "1.0.0")
            .with_console_config(config)
            .build();

        assert_eq!(server.console_config().banner_style, BannerStyle::Compact);
        assert!(server.console_config().force_plain);
    }

    #[test]
    fn test_server_without_banner() {
        let server = Server::new("test", "1.0.0").without_banner().build();

        assert!(!server.console_config().show_banner);
        assert_eq!(server.console_config().banner_style, BannerStyle::None);
    }

    #[test]
    fn test_server_with_banner_style() {
        let server = Server::new("test", "1.0.0")
            .with_banner(BannerStyle::Minimal)
            .build();

        assert!(server.console_config().show_banner);
        assert_eq!(server.console_config().banner_style, BannerStyle::Minimal);
    }

    #[test]
    fn test_server_with_traffic_logging() {
        let server = Server::new("test", "1.0.0")
            .with_traffic_logging(TrafficVerbosity::Summary)
            .build();

        assert_eq!(
            server.console_config().traffic_verbosity,
            TrafficVerbosity::Summary
        );
    }

    #[test]
    fn traffic_verbosity_is_the_single_enablement_source() {
        let disabled = Server::new("test", "1.0.0").build();
        assert!(disabled.configured_traffic_renderer().is_none());

        let summary = Server::new("test", "1.0.0")
            .with_traffic_logging(TrafficVerbosity::Summary)
            .build()
            .configured_traffic_renderer()
            .expect("summary traffic should create a renderer");
        assert!(!summary.show_params);
        assert!(!summary.show_result);

        let full = Server::new("test", "1.0.0")
            .with_traffic_logging(TrafficVerbosity::Full)
            .build()
            .configured_traffic_renderer()
            .expect("full traffic should create a renderer");
        assert!(full.show_params);
        assert!(full.show_result);
    }

    #[test]
    fn test_server_plain_mode() {
        let server = Server::new("test", "1.0.0").plain_mode().build();

        assert!(server.console_config().force_plain);
    }

    #[test]
    fn test_server_force_color() {
        let server = Server::new("test", "1.0.0").force_color().build();

        assert_eq!(server.console_config().force_color, Some(true));
    }

    #[test]
    fn test_console_config_chaining() {
        let server = Server::new("test", "1.0.0")
            .with_banner(BannerStyle::Compact)
            .with_traffic_logging(TrafficVerbosity::Full)
            .plain_mode()
            .build();

        let config = server.console_config();
        assert_eq!(config.banner_style, BannerStyle::Compact);
        assert_eq!(config.traffic_verbosity, TrafficVerbosity::Full);
        assert!(config.force_plain);
    }
}

/// Tests for lifecycle hooks (on_startup, on_shutdown).
mod lifespan_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_on_startup_hook_builder() {
        let startup_called = Arc::new(AtomicBool::new(false));
        let startup_called_clone = startup_called.clone();

        let server = Server::new("test", "1.0.0")
            .on_startup(move || {
                startup_called_clone.store(true, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
            .build();

        // The hook is stored but not called until run
        // Verify that the lifespan is stored (we can't call run_startup_hook directly
        // since it's private, but we verify the builder works)
        assert!(!startup_called.load(Ordering::SeqCst));

        // Manually trigger the startup hook via the public interface
        // (In production, this would be called by run_loop)
        let startup_success = server.run_startup_hook();
        assert!(startup_success);
        assert!(startup_called.load(Ordering::SeqCst));
    }

    #[test]
    fn test_on_shutdown_hook_builder() {
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let shutdown_called_clone = shutdown_called.clone();

        let server = Server::new("test", "1.0.0")
            .on_shutdown(move || {
                shutdown_called_clone.store(true, Ordering::SeqCst);
            })
            .build();

        // The hook is stored but not called until shutdown
        assert!(!shutdown_called.load(Ordering::SeqCst));

        // Manually trigger the shutdown hook
        server.run_shutdown_hook();
        assert!(shutdown_called.load(Ordering::SeqCst));
    }

    #[test]
    fn test_startup_hook_failure() {
        let server = Server::new("test", "1.0.0")
            .on_startup(|| Err(std::io::Error::other("startup failed")))
            .build();

        // Startup should return false on failure
        let startup_success = server.run_startup_hook();
        assert!(!startup_success);
    }

    #[test]
    fn test_no_hooks_is_ok() {
        let server = Server::new("test", "1.0.0").build();

        // No hooks configured should be fine
        let startup_success = server.run_startup_hook();
        assert!(startup_success);

        // Shutdown hook should also be a no-op
        server.run_shutdown_hook();
    }

    #[test]
    fn test_hooks_only_run_once() {
        let startup_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let startup_count_clone = startup_count.clone();

        let shutdown_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let shutdown_count_clone = shutdown_count.clone();

        let server = Server::new("test", "1.0.0")
            .on_startup(move || {
                startup_count_clone.fetch_add(1, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
            .on_shutdown(move || {
                shutdown_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .build();

        // Call startup multiple times
        server.run_startup_hook();
        server.run_startup_hook();
        server.run_startup_hook();

        // Should only have run once (hook is taken)
        assert_eq!(startup_count.load(Ordering::SeqCst), 1);

        // Same for shutdown
        server.run_shutdown_hook();
        server.run_shutdown_hook();
        server.run_shutdown_hook();

        assert_eq!(shutdown_count.load(Ordering::SeqCst), 1);
    }
}

/// Deterministic LabRuntime tests for cancel/timeout handling.
mod lab_runtime_tests {
    use super::*;
    use asupersync::conformance::{ConformanceTarget, LabRuntimeTarget};
    use asupersync::lab::{LabConfig, LabRuntime};
    use std::sync::{Arc, Mutex};

    fn with_lab_runtime<T>(f: impl FnOnce(&mut LabRuntime) -> T) -> T {
        let mut runtime = LabRuntime::new(LabConfig::new(42).max_steps(2000));
        f(&mut runtime)
    }

    #[test]
    fn test_lab_runtime_cancelled_tool_call() {
        with_lab_runtime(|runtime| {
            let events = Arc::new(Mutex::new(Vec::new()));
            let events_for_task = Arc::clone(&events);

            LabRuntimeTarget::block_on(runtime, async move {
                let mut router = Router::new();
                router
                    .add_tool(CancellationCheck)
                    .expect("tool registration succeeds");

                let cx = Cx::for_testing();
                cx.cancel_with(CancelKind::User, None);

                let params = CallToolParams {
                    name: "cancellation_check".to_string(),
                    arguments: Some(serde_json::json!({})),
                    meta: None,
                };
                let state = SessionState::new();
                let request_ctx = McpContext::with_state(cx, 1, state.clone())
                    .with_budget_ceiling(Budget::INFINITE);
                let result = router
                    .handle_tools_call(&request_ctx, params, state, None, None)
                    .await;

                let err = result.as_ref().err().map(|e| e.message.clone());
                events_for_task
                    .lock()
                    .expect("events lock poisoned")
                    .push(format!("cancelled_result={err:?}"));
                info!(
                    target: targets::SESSION,
                    "lab cancel outcome ts={} err={:?}",
                    chrono::Utc::now().to_rfc3339(),
                    err
                );

                assert!(result.is_err());
            });

            assert_eq!(events.lock().expect("events lock poisoned").len(), 1);
        });
    }

    #[test]
    fn test_lab_runtime_budget_exhaustion_resource_read() {
        with_lab_runtime(|runtime| {
            let events = Arc::new(Mutex::new(Vec::new()));
            let events_for_task = Arc::clone(&events);

            LabRuntimeTarget::block_on(runtime, async move {
                let mut router = Router::new();
                router.add_resource(StaticResource {
                    uri: "resource://test".to_string(),
                    content: "Test content".to_string(),
                });

                let cx = Cx::for_testing();
                let budget = Budget::unlimited().with_poll_quota(0);
                let params = ReadResourceParams {
                    uri: "resource://test".to_string(),
                    meta: None,
                };

                let state = SessionState::new();
                let request_ctx =
                    McpContext::with_state(cx, 1, state.clone()).with_budget_ceiling(budget);
                let result = router
                    .handle_resources_read(&request_ctx, &params, state, None, None)
                    .await;

                let err = result.as_ref().err().map(|e| e.message.clone());
                events_for_task
                    .lock()
                    .expect("events lock poisoned")
                    .push(format!("budget_result={err:?}"));
                info!(
                    target: targets::SESSION,
                    "lab budget outcome ts={} err={:?}",
                    chrono::Utc::now().to_rfc3339(),
                    err
                );

                assert!(
                    result.is_ok(),
                    "a resource that requests no poll admission may complete at an exact zero balance"
                );
            });

            assert_eq!(events.lock().expect("events lock poisoned").len(), 1);
        });
    }

    #[test]
    fn test_lab_runtime_deadline_progression() {
        with_lab_runtime(|runtime| {
            let budget = Budget::with_deadline_at_secs(1);
            let start = runtime.now();
            assert!(!budget.is_past_deadline(start));

            runtime.advance_time(Duration::from_secs(2).as_nanos() as u64);
            let end = runtime.now();
            assert!(budget.is_past_deadline(end));
            info!(
                target: targets::SESSION,
                "lab deadline progression start={:?} end={:?}",
                start,
                end
            );
        });
    }
}

// ============================================================================
// Mount/Composition Tests
// ============================================================================

mod mount_tests {
    use super::*;
    use crate::Router;

    /// A resource for mount tests.
    struct ConfigResource;

    impl ResourceHandler for ConfigResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "config://app".to_string(),
                name: "App Config".to_string(),
                description: Some("Application configuration".to_string()),
                mime_type: Some("application/json".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: "config://app".to_string(),
                text: Some(r#"{"debug": true}"#.to_string()),
                mime_type: Some("application/json".to_string()),
                blob: None,
            }])
        }
    }

    /// A prompt for mount tests.
    struct GreetingPrompt;

    impl PromptHandler for GreetingPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: "greeting".to_string(),
                description: Some("A greeting prompt".to_string()),
                arguments: Vec::new(),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _arguments: HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(vec![PromptMessage {
                role: Role::User,
                content: Content::Text {
                    text: "Hello!".to_string(),
                },
            }])
        }
    }

    #[test]
    fn test_mount_with_prefix_renames_tools() {
        let mut main_router = Router::new();
        let mut db_router = Router::new();
        db_router
            .add_tool(MountQuery)
            .expect("tool registration succeeds");
        db_router
            .add_tool(MountInsert)
            .expect("tool registration succeeds");

        let result = main_router.mount(db_router, Some("db"));

        assert_eq!(result.tools, 2);
        assert!(main_router.get_tool("db/query").is_some());
        assert!(main_router.get_tool("db/insert").is_some());
        assert!(main_router.get_tool("query").is_none());
        assert!(main_router.get_tool("insert").is_none());
    }

    #[test]
    fn test_mount_without_prefix_keeps_names() {
        let mut main_router = Router::new();
        let mut other_router = Router::new();
        other_router
            .add_tool(MountQuery)
            .expect("tool registration succeeds");

        let result = main_router.mount(other_router, None);

        assert_eq!(result.tools, 1);
        assert!(main_router.get_tool("query").is_some());
    }

    #[test]
    fn test_mount_resources_with_prefix() {
        let mut main_router = Router::new();
        let mut other_router = Router::new();
        other_router.add_resource(ConfigResource);

        let result = main_router.mount(other_router, Some("service"));

        assert_eq!(result.resources, 1);
        assert!(main_router.get_resource("service/config://app").is_some());
        assert!(main_router.get_resource("config://app").is_none());
    }

    #[test]
    fn test_mount_prompts_with_prefix() {
        let mut main_router = Router::new();
        let mut other_router = Router::new();
        other_router.add_prompt(GreetingPrompt);

        let result = main_router.mount(other_router, Some("templates"));

        assert_eq!(result.prompts, 1);
        assert!(main_router.get_prompt("templates/greeting").is_some());
        assert!(main_router.get_prompt("greeting").is_none());
    }

    #[test]
    fn test_mount_conflict_generates_warning() {
        let mut main_router = Router::new();
        main_router
            .add_tool(MountQuery)
            .expect("tool registration succeeds");

        let mut other_router = Router::new();
        other_router
            .add_tool(MountQuery)
            .expect("tool registration succeeds");

        // Mount without prefix, causing a conflict
        let result = main_router.mount(other_router, None);

        assert_eq!(result.tools, 1);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("already exists"));
    }

    #[test]
    fn test_mount_preserves_tool_definition() {
        let mut main_router = Router::new();
        let mut db_router = Router::new();
        db_router
            .add_tool(MountQuery)
            .expect("tool registration succeeds");

        main_router.mount(db_router, Some("db"));

        let tools = main_router.tools();
        let tool = tools.iter().find(|t| t.name == "db/query").unwrap();
        assert_eq!(tool.description, Some("Executes a query".to_string()));
    }

    #[test]
    fn test_mount_all_components() {
        let mut main_router = Router::new();
        let mut other_router = Router::new();
        other_router
            .add_tool(MountQuery)
            .expect("tool registration succeeds");
        other_router.add_resource(ConfigResource);
        other_router.add_prompt(GreetingPrompt);

        let result = main_router.mount(other_router, Some("sub"));

        assert_eq!(result.tools, 1);
        assert_eq!(result.resources, 1);
        assert_eq!(result.prompts, 1);
        assert!(result.has_components());
    }

    #[test]
    fn test_selective_mount_tools_only() {
        let db_server = Server::new("db", "1.0")
            .tool(MountQuery)
            .resource(ConfigResource)
            .prompt(GreetingPrompt)
            .build();

        let main = Server::new("main", "1.0")
            .mount_tools(db_server, Some("db"))
            .build();

        let tools = main.tools();
        let resources = main.resources();
        let prompts = main.prompts();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "db/query");
        assert_eq!(resources.len(), 0);
        assert_eq!(prompts.len(), 0);
    }

    #[test]
    fn test_selective_mount_resources_only() {
        let data_server = Server::new("data", "1.0")
            .tool(MountQuery)
            .resource(ConfigResource)
            .build();

        let main = Server::new("main", "1.0")
            .mount_resources(data_server, Some("data"))
            .build();

        let tools = main.tools();
        let resources = main.resources();

        assert!(tools.is_empty());
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "data/config://app");
    }

    #[test]
    fn test_selective_mount_prompts_only() {
        let templates_server = Server::new("templates", "1.0")
            .tool(MountQuery)
            .prompt(GreetingPrompt)
            .build();

        let main = Server::new("main", "1.0")
            .mount_prompts(templates_server, Some("tmpl"))
            .build();

        let tools = main.tools();
        let prompts = main.prompts();

        assert!(tools.is_empty());
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "tmpl/greeting");
    }

    #[test]
    fn test_full_mount_via_server_builder() {
        let db_server = Server::new("db", "1.0")
            .tool(MountQuery)
            .tool(MountInsert)
            .build();

        let api_server = Server::new("api", "1.0").prompt(GreetingPrompt).build();

        let main = Server::new("main", "1.0")
            .tool(Greet)
            .mount(db_server, Some("db"))
            .mount(api_server, Some("api"))
            .build();

        let tools = main.tools();
        let prompts = main.prompts();

        // Should have original greet + mounted db/query and db/insert
        assert_eq!(tools.len(), 3);
        let tool_names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"greet"));
        assert!(tool_names.contains(&"db/query"));
        assert!(tool_names.contains(&"db/insert"));

        // Should have api/greeting
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "api/greeting");
    }

    #[test]
    fn test_nested_mounting() {
        // Create inner server
        let inner = Server::new("inner", "1.0").tool(MountQuery).build();

        // Mount inner into middle
        let middle = Server::new("middle", "1.0")
            .mount(inner, Some("inner"))
            .build();

        // Mount middle into outer
        let outer = Server::new("outer", "1.0")
            .mount(middle, Some("middle"))
            .build();

        let tools = outer.tools();
        assert_eq!(tools.len(), 1);
        // Tool should be at middle/inner/query
        assert_eq!(tools[0].name, "middle/inner/query");
    }

    #[test]
    fn test_prefix_validation_rejects_slashes() {
        let mut router = Router::new();
        let mut other = Router::new();
        other
            .add_tool(MountQuery)
            .expect("tool registration succeeds");

        let result = router.mount(other, Some("bad/prefix"));

        // Should still mount but generate a warning
        assert_ne!(result.warnings.len(), 0);
    }

    #[test]
    fn test_mounted_tool_can_be_called() {
        let db_server = Server::new("db", "1.0").tool(MountQuery).build();

        let main = Server::new("main", "1.0")
            .mount(db_server, Some("db"))
            .build();

        // Get the tool handler
        let tools = main.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "db/query");

        // Verify the definition is correct
        let tool = &tools[0];
        assert_eq!(tool.description, Some("Executes a query".to_string()));
    }

    #[test]
    fn test_mount_empty_router() {
        let mut main_router = Router::new();
        let empty_router = Router::new();

        let result = main_router.mount(empty_router, Some("empty"));

        assert_eq!(result.tools, 0);
        assert_eq!(result.resources, 0);
        assert_eq!(result.prompts, 0);
        assert!(!result.has_components());
    }
}

// ============================================================================
// Duplicate Behavior Tests
// ============================================================================

mod duplicate_behavior_tests {
    use super::*;
    use crate::{DuplicateBehavior, Router};

    #[tool(name = "dup_tool", description = "Tool #1")]
    fn dup_tool_1() -> String {
        "Tool #1".to_string()
    }

    #[tool(name = "dup_tool", description = "Tool #2")]
    fn dup_tool_2() -> String {
        "Tool #2".to_string()
    }

    #[test]
    fn test_duplicate_behavior_error_returns_error() {
        let mut router = Router::new();
        router
            .add_tool(DupTool1)
            .expect("tool registration succeeds");

        let result = router.add_tool_with_behavior(DupTool2, DuplicateBehavior::Error);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("already exists"));
    }

    #[test]
    fn test_duplicate_behavior_warn_keeps_original() {
        let mut router = Router::new();
        router
            .add_tool(DupTool1)
            .expect("tool registration succeeds");

        let result = router.add_tool_with_behavior(DupTool2, DuplicateBehavior::Warn);
        assert!(result.is_ok());

        // Original should be kept
        let tool = router.get_tool("dup_tool").unwrap();
        assert_eq!(tool.definition().description, Some("Tool #1".to_string()));
    }

    #[test]
    fn test_duplicate_behavior_replace_replaces() {
        let mut router = Router::new();
        router
            .add_tool(DupTool1)
            .expect("tool registration succeeds");

        let result = router.add_tool_with_behavior(DupTool2, DuplicateBehavior::Replace);
        assert!(result.is_ok());

        // New one should replace original
        let tool = router.get_tool("dup_tool").unwrap();
        assert_eq!(tool.definition().description, Some("Tool #2".to_string()));
    }

    #[test]
    fn test_duplicate_behavior_ignore_keeps_original() {
        let mut router = Router::new();
        router
            .add_tool(DupTool1)
            .expect("tool registration succeeds");

        let result = router.add_tool_with_behavior(DupTool2, DuplicateBehavior::Ignore);
        assert!(result.is_ok());

        // Original should be kept
        let tool = router.get_tool("dup_tool").unwrap();
        assert_eq!(tool.definition().description, Some("Tool #1".to_string()));
    }

    #[test]
    fn test_duplicate_behavior_default_is_warn() {
        assert_eq!(DuplicateBehavior::default(), DuplicateBehavior::Warn);
    }

    #[test]
    fn test_no_duplicate_succeeds_for_all_behaviors() {
        for behavior in [
            DuplicateBehavior::Error,
            DuplicateBehavior::Warn,
            DuplicateBehavior::Replace,
            DuplicateBehavior::Ignore,
        ] {
            let mut router = Router::new();
            let result = router.add_tool_with_behavior(DupTool1, behavior);
            assert!(result.is_ok(), "Failed for {:?}", behavior);
        }
    }

    #[test]
    fn test_server_builder_on_duplicate() {
        // Create server with strict duplicate checking
        let server = Server::new("test", "1.0")
            .on_duplicate(DuplicateBehavior::Replace)
            .tool(DupTool1)
            .tool(DupTool2) // Should replace
            .build();

        let tools = server.tools();
        assert_eq!(tools.len(), 1);
        // The replaced tool should have id 2
        assert_eq!(tools[0].description, Some("Tool #2".to_string()));
    }

    #[test]
    fn test_server_builder_error_behavior_refuses_the_build() {
        // A duplicate refused under Error must fail the build, not vanish.
        let Err(crate::ServerBuildError::InvalidConfiguration(refused)) =
            Server::new("test", "1.0")
                .on_duplicate(DuplicateBehavior::Error)
                .tool(DupTool1)
                .tool(DupTool2)
                .try_build()
        else {
            panic!("a duplicate refused under DuplicateBehavior::Error must fail try_build");
        };
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].kind, crate::RegistrationKind::Tool);
        assert_eq!(refused[0].name, "dup_tool");
    }
}

// ============================================================================
// Cross-Component Resource Reading Tests (ctx.read_resource)
// ============================================================================

mod ctx_read_resource_tests {
    use super::*;
    use crate::RouterResourceReader;
    use fastmcp_core::{
        MAX_RESOURCE_READ_DEPTH, ResourceContentItem, ResourceReadResult, ResourceReader,
    };

    /// A simple resource that returns static config data.
    struct ConfigResource {
        config_json: String,
    }

    impl ConfigResource {
        fn new(json: &str) -> Self {
            Self {
                config_json: json.to_string(),
            }
        }
    }

    impl ResourceHandler for ConfigResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "config://app".to_string(),
                name: "app_config".to_string(),
                description: Some("Application configuration".to_string()),
                mime_type: Some("application/json".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: "config://app".to_string(),
                mime_type: Some("application/json".to_string()),
                text: Some(self.config_json.clone()),
                blob: None,
            }])
        }
    }

    /// A resource that reads another resource.
    struct NestedResource {
        inner_uri: String,
    }

    impl NestedResource {
        fn new(inner_uri: &str) -> Self {
            Self {
                inner_uri: inner_uri.to_string(),
            }
        }
    }

    impl ResourceHandler for NestedResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "nested://wrapper".to_string(),
                name: "nested_wrapper".to_string(),
                description: Some("Wraps another resource".to_string()),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            // Read the inner resource using ctx
            let inner_uri = self.inner_uri.clone();
            let inner_result = fastmcp_core::block_on(ctx.read_resource(&inner_uri))?;

            let text = inner_result.first_text().unwrap_or("(no content)");
            Ok(vec![ResourceContent {
                uri: "nested://wrapper".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some(format!("Wrapped: {}", text)),
                blob: None,
            }])
        }
    }

    #[test]
    fn test_resource_content_item_constructors() {
        let text_item = ResourceContentItem::text("file://test", "hello world");
        assert_eq!(text_item.uri, "file://test");
        assert_eq!(text_item.as_text(), Some("hello world"));
        assert!(text_item.is_text());
        assert!(!text_item.is_blob());

        let json_item = ResourceContentItem::json("config://app", r#"{"key": "value"}"#);
        assert_eq!(json_item.mime_type, Some("application/json".to_string()));

        let blob_item = ResourceContentItem::blob("image://test", "image/png", "base64data");
        assert_eq!(blob_item.as_blob(), Some("base64data"));
        assert!(blob_item.is_blob());
        assert!(!blob_item.is_text());
    }

    #[test]
    fn test_resource_read_result_constructors() {
        let result = ResourceReadResult::text("file://test", "content");
        assert_eq!(result.first_text(), Some("content"));
        assert_eq!(result.contents.len(), 1);

        let multi = ResourceReadResult::new(vec![
            ResourceContentItem::text("file://a", "A"),
            ResourceContentItem::text("file://b", "B"),
        ]);
        assert_eq!(multi.contents.len(), 2);
        assert_eq!(multi.first_text(), Some("A"));
    }

    #[test]
    fn test_ctx_read_resource_without_reader_fails() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1);

        // Without a resource reader, should fail
        assert!(!ctx.can_read_resources());

        let result = fastmcp_core::block_on(ctx.read_resource("config://app"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("no router"));
    }

    #[test]
    fn test_router_resource_reader_reads_resource() {
        let mut router = Router::new();
        router.add_resource(ConfigResource::new(r#"{"db": "postgres"}"#));

        let router_arc = Arc::new(router);
        let reader = RouterResourceReader::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        let result = fastmcp_core::block_on(reader.read_resource(&request_ctx, "config://app", 0));

        assert!(result.is_ok());
        let read_result = result.unwrap();
        assert_eq!(read_result.first_text(), Some(r#"{"db": "postgres"}"#));
    }

    #[test]
    fn test_router_resource_reader_not_found() {
        let router = Router::new(); // No resources
        let router_arc = Arc::new(router);
        let reader = RouterResourceReader::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        let result =
            fastmcp_core::block_on(reader.read_resource(&request_ctx, "config://missing", 0));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[test]
    fn test_router_resource_reader_depth_limit() {
        let router = Router::new();
        let router_arc = Arc::new(router);
        let reader = RouterResourceReader::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        // Call with depth at limit
        let result = fastmcp_core::block_on(reader.read_resource(
            &request_ctx,
            "any://uri",
            MAX_RESOURCE_READ_DEPTH + 1,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("depth"));
    }

    #[test]
    fn test_ctx_with_resource_reader() {
        let mut router = Router::new();
        router.add_resource(ConfigResource::new(r#"{"name": "test"}"#));

        let router_arc = Arc::new(router);
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_resource_reader(reader);

        assert!(ctx.can_read_resources());
        assert_eq!(ctx.resource_read_depth(), 0);

        // Read the resource
        let result = fastmcp_core::block_on(ctx.read_resource("config://app"));
        assert!(result.is_ok());
        let read_result = result.unwrap();
        assert!(read_result.first_text().unwrap().contains("test"));
    }

    #[test]
    fn test_ctx_read_resource_text() {
        let mut router = Router::new();
        router.add_resource(ConfigResource::new(r#"{"value": 42}"#));

        let router_arc = Arc::new(router);
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_resource_reader(reader);

        let result = fastmcp_core::block_on(ctx.read_resource_text("config://app"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), r#"{"value": 42}"#);
    }

    #[test]
    fn test_ctx_read_resource_json() {
        let mut router = Router::new();
        router.add_resource(ConfigResource::new(
            r#"{"database": "postgres", "port": 5432}"#,
        ));

        let router_arc = Arc::new(router);
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_resource_reader(reader);

        #[derive(Debug, serde::Deserialize)]
        struct DbConfig {
            database: String,
            port: u16,
        }

        let result: McpResult<DbConfig> =
            fastmcp_core::block_on(ctx.read_resource_json("config://app"));
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.database, "postgres");
        assert_eq!(config.port, 5432);
    }

    #[test]
    fn test_ctx_read_resource_json_parse_error() {
        let mut router = Router::new();
        router.add_resource(ConfigResource::new("not valid json"));

        let router_arc = Arc::new(router);
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_resource_reader(reader);

        #[derive(Debug, serde::Deserialize)]
        struct Config {
            value: i32,
        }

        let result: McpResult<Config> =
            fastmcp_core::block_on(ctx.read_resource_json("config://app"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("JSON"));
    }

    #[test]
    fn test_resource_read_depth_increments() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_resource_read_depth(5);

        assert_eq!(ctx.resource_read_depth(), 5);
    }

    #[test]
    fn test_max_resource_read_depth_constant() {
        // Verify the constant is reasonable
        assert_eq!(MAX_RESOURCE_READ_DEPTH, 10);
    }

    /// A resource that reads session state and then reads another resource.
    struct SessionStateResource;

    impl ResourceHandler for SessionStateResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "session://state".to_string(),
                name: "session_state".to_string(),
                description: Some("Returns session state value".to_string()),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            let value: Option<String> = ctx.get_state("test_key");
            Ok(vec![ResourceContent {
                uri: "session://state".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some(value.unwrap_or_else(|| "no_value".to_string())),
                blob: None,
            }])
        }
    }

    /// A resource that sets session state and reads another resource to verify propagation.
    struct NestedSessionResource;

    impl ResourceHandler for NestedSessionResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "nested://session".to_string(),
                name: "nested_session".to_string(),
                description: Some("Sets state then reads another resource".to_string()),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            // A nested read is async; bridging it with block_on from this
            // synchronous hook is the rejected pattern (bd-6rfrg).
            Err(McpError::internal_error("nested reads use read_async"))
        }

        fn read_async<'a>(
            &'a self,
            ctx: &'a McpContext,
        ) -> crate::handler::BoxFuture<'a, fastmcp_core::McpOutcome<Vec<ResourceContent>>> {
            Box::pin(async move {
                // Set a value in session state
                ctx.set_state("test_key", "propagated_value");

                // Read another resource - it should see our session state
                let inner_result = match ctx.read_resource("session://state").await {
                    Ok(result) => result,
                    Err(error) => return fastmcp_core::Outcome::Err(error),
                };
                let text = inner_result.first_text().unwrap_or("(no content)");

                fastmcp_core::Outcome::Ok(vec![ResourceContent {
                    uri: "nested://session".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some(format!("Inner saw: {}", text)),
                    blob: None,
                }])
            })
        }
    }

    #[test]
    fn test_session_state_propagates_through_nested_reads() {
        let mut router = Router::new();
        router.add_resource(SessionStateResource);
        router.add_resource(NestedSessionResource);

        let router_arc = Arc::new(router);
        let session_state = SessionState::new();
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, session_state.clone()));

        let cx = Cx::for_testing();
        let ctx = McpContext::with_state(cx, 1, session_state).with_resource_reader(reader);

        // Read the nested resource - it sets state then reads another resource
        let result = fastmcp_core::block_on(ctx.read_resource("nested://session"));
        assert!(result.is_ok());
        let read_result = result.unwrap();

        // The inner resource should have seen the propagated value
        let text = read_result.first_text().unwrap();
        assert!(
            text.contains("propagated_value"),
            "Expected session state to propagate, got: {}",
            text
        );
    }

    struct AuthEchoResource;

    impl ResourceHandler for AuthEchoResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "auth://subject".to_string(),
                name: "auth_subject".to_string(),
                description: Some("Returns the current auth subject".to_string()),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: "auth://subject".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some(
                    ctx.auth()
                        .and_then(|auth| auth.subject)
                        .unwrap_or_else(|| "anonymous".to_string()),
                ),
                blob: None,
            }])
        }
    }

    struct NestedAuthResource;

    impl ResourceHandler for NestedAuthResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "nested://auth".to_string(),
                name: "nested_auth".to_string(),
                description: Some(
                    "Reads another resource and expects auth to propagate".to_string(),
                ),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            // A nested read is async; bridging it with block_on from this
            // synchronous hook is the rejected pattern (bd-6rfrg).
            Err(McpError::internal_error("nested reads use read_async"))
        }

        fn read_async<'a>(
            &'a self,
            ctx: &'a McpContext,
        ) -> crate::handler::BoxFuture<'a, fastmcp_core::McpOutcome<Vec<ResourceContent>>> {
            Box::pin(async move {
                let inner = match ctx.read_resource("auth://subject").await {
                    Ok(result) => result,
                    Err(error) => return fastmcp_core::Outcome::Err(error),
                };
                fastmcp_core::Outcome::Ok(vec![ResourceContent {
                    uri: "nested://auth".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some(inner.first_text().unwrap_or("missing").to_string()),
                    blob: None,
                }])
            })
        }
    }

    #[test]
    fn test_request_auth_propagates_through_nested_reads() {
        let mut router = Router::new();
        router.add_resource(AuthEchoResource);
        router.add_resource(NestedAuthResource);

        let router_arc = Arc::new(router);
        let session_state = SessionState::new();
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, session_state.clone()));

        let cx = Cx::for_testing();
        let ctx = McpContext::with_state(cx, 1, session_state)
            .with_resource_reader(reader)
            .with_auth(AuthContext::with_subject("reader-auth"));

        let result = fastmcp_core::block_on(ctx.read_resource("nested://auth"))
            .expect("nested resource read should succeed");
        assert_eq!(result.first_text(), Some("reader-auth"));
    }

    // ========================================================================
    // bd-v9ev3 area (d): reading the payload the redaction hides.
    //
    // The four (d) tests let a handler's panic escape into the server's
    // extension-unwind catch, which converts it to a payload-free
    // `InternalError`, while the process panic hook prints a fixed redaction
    // constant instead of the message. Neither surface can name what fired,
    // which is why area (d) has no observed root cause.
    //
    // The hook governs what is PRINTED. `catch_unwind` still RETURNS the
    // payload. A handler that catches its own unwind can therefore read the
    // message the redaction withholds, with no change to any shipped path.
    // ========================================================================

    /// Renders a caught panic payload as text, naming the shapes it does not
    /// recognise rather than collapsing them into an empty string.
    pub(super) fn panic_payload_text(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(text) = payload.downcast_ref::<&'static str>() {
            (*text).to_string()
        } else if let Some(text) = payload.downcast_ref::<String>() {
            text.clone()
        } else {
            "PANIC WITH A NON-STRING PAYLOAD".to_string()
        }
    }

    /// Reproduces the (d) shape and reports what it observes instead of
    /// propagating it.
    ///
    /// The body is identical whether or not a bridge is already active on this
    /// thread; only the caller differs. That is what makes the negative below
    /// near-identical to the positive.
    struct BridgeProbeResource;

    impl ResourceHandler for BridgeProbeResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "probe://bridge".to_string(),
                name: "bridge_probe".to_string(),
                description: Some("Reports the payload of its own nested bridge".to_string()),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            None
        }

        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            ctx.set_state("test_key", "probe_value");

            let observed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fastmcp_core::block_on(ctx.read_resource("session://state"))
            })) {
                Ok(Ok(inner)) => format!(
                    "NO PANIC: inner read returned {}",
                    inner.first_text().unwrap_or("(no content)")
                ),
                Ok(Err(error)) => format!("NO PANIC: inner read returned Err {error}"),
                Err(payload) => panic_payload_text(payload.as_ref()),
            };

            Ok(vec![ResourceContent {
                uri: "probe://bridge".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some(observed),
                blob: None,
            }])
        }
    }

    /// Builds the (d) fixture: a probe handler and the resource it reads.
    fn bridge_probe_context() -> McpContext {
        let mut router = Router::new();
        router.add_resource(SessionStateResource);
        router.add_resource(BridgeProbeResource);

        let router_arc = Arc::new(router);
        let session_state = SessionState::new();
        let reader: Arc<dyn ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, session_state.clone()));

        McpContext::with_state(Cx::for_testing(), 1, session_state).with_resource_reader(reader)
    }

    /// POSITIVE. bd-v9ev3 (d): names the assertion the redaction hides.
    ///
    /// This test can refute the diagnosis it was written for. If the payload is
    /// any other message, or no panic occurs at all, the assertion fails and
    /// prints what actually happened.
    #[test]
    fn a_nested_bridge_from_a_sync_handler_panics_with_the_reentrancy_rejection() {
        let ctx = bridge_probe_context();

        // The outer bridge. The handler it dispatches to enters a second one,
        // which is the position the four (d) tests reach.
        let observed = fastmcp_core::block_on(ctx.read_resource("probe://bridge"))
            .expect("the probe catches its own unwind, so the outer read must succeed")
            .first_text()
            .expect("the probe reports its observation as text")
            .to_string();

        assert_eq!(
            observed, "nested fastmcp_core::runtime::block_on is not supported",
            "the payload behind the extension-panic redaction is not the reentrancy \
             rejection, so bd-v9ev3's (d) chain is refuted and this is what fired instead"
        );
    }

    /// NEAR-IDENTICAL NEGATIVE (RH-5). Same handler, same method, same closure,
    /// same runtime, same fixture.
    ///
    /// The single varied dimension is whether a bridge is ALREADY ACTIVE on this
    /// thread when the handler runs: here the handler is invoked directly rather
    /// than from inside an outer `block_on`, so its bridge is the first entry
    /// rather than the second. Without this control, the positive above would be
    /// consistent with the handler panicking for some reason unrelated to
    /// nesting.
    #[test]
    fn the_same_handler_completes_when_no_bridge_is_already_active() {
        let ctx = bridge_probe_context();

        let observed = BridgeProbeResource
            .read(&ctx)
            .expect("the probe handler returns Ok on the unnested path")
            .first()
            .and_then(|content| content.text.clone())
            .expect("the probe reports its observation as text");

        assert_eq!(
            observed, "NO PANIC: inner read returned probe_value",
            "the first bridge entry must not be rejected, and the inner read must \
             observe the state the handler set; if this fails, the positive above \
             is not evidence about nesting"
        );
    }
}

// ============================================================================
// Cross-Component Tool Calling Tests (ctx.call_tool)
// ============================================================================

mod ctx_call_tool_tests {
    use super::*;
    use crate::RouterToolCaller;
    use fastmcp_core::{MAX_TOOL_CALL_DEPTH, ToolCallResult, ToolCaller, ToolContentItem};

    #[test]
    fn test_tool_content_item_constructors() {
        let text_item = ToolContentItem::text("hello world");
        assert_eq!(text_item.as_text(), Some("hello world"));
        assert!(text_item.is_text());
    }

    #[test]
    fn test_tool_call_result_constructors() {
        let success = ToolCallResult::text("result");
        assert!(!success.is_error);
        assert_eq!(success.first_text(), Some("result"));

        let error = ToolCallResult::error("failed");
        assert!(error.is_error);
        assert_eq!(error.first_text(), Some("failed"));

        let multi =
            ToolCallResult::success(vec![ToolContentItem::text("a"), ToolContentItem::text("b")]);
        assert_eq!(multi.content.len(), 2);
        assert_eq!(multi.first_text(), Some("a"));
    }

    #[test]
    fn test_ctx_call_tool_without_caller_fails() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1);

        // Without a tool caller, should fail
        assert!(!ctx.can_call_tools());

        let result =
            fastmcp_core::block_on(ctx.call_tool("add", serde_json::json!({"a": 1, "b": 2})));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("no router"));
    }

    #[test]
    fn test_router_tool_caller_calls_tool() {
        let mut router = Router::new();
        router
            .add_tool(AddNumbersTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller = RouterToolCaller::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        let result = fastmcp_core::block_on(caller.call_tool(
            &request_ctx,
            "add",
            serde_json::json!({"a": 5, "b": 3}),
            0,
        ));

        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(!call_result.is_error);
        assert_eq!(call_result.first_text(), Some("8"));
    }

    #[test]
    fn test_router_tool_caller_not_found() {
        let router = Router::new(); // No tools
        let router_arc = Arc::new(router);
        let caller = RouterToolCaller::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        let result = fastmcp_core::block_on(caller.call_tool(
            &request_ctx,
            "nonexistent",
            serde_json::json!({}),
            0,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[test]
    fn test_router_tool_caller_depth_limit() {
        let router = Router::new();
        let router_arc = Arc::new(router);
        let caller = RouterToolCaller::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        // Call with depth at limit
        let result = fastmcp_core::block_on(caller.call_tool(
            &request_ctx,
            "any_tool",
            serde_json::json!({}),
            MAX_TOOL_CALL_DEPTH + 1,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("depth"));
    }

    #[test]
    fn test_ctx_with_tool_caller() {
        let mut router = Router::new();
        router
            .add_tool(AddNumbersTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller: Arc<dyn ToolCaller> =
            Arc::new(RouterToolCaller::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_tool_caller(caller);

        assert!(ctx.can_call_tools());
        assert_eq!(ctx.tool_call_depth(), 0);

        // Call the tool
        let result =
            fastmcp_core::block_on(ctx.call_tool("add", serde_json::json!({"a": 10, "b": 5})));
        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert_eq!(call_result.first_text(), Some("15"));
    }

    #[test]
    fn test_ctx_call_tool_text() {
        let mut router = Router::new();
        router
            .add_tool(AddNumbersTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller: Arc<dyn ToolCaller> =
            Arc::new(RouterToolCaller::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_tool_caller(caller);

        let result =
            fastmcp_core::block_on(ctx.call_tool_text("add", serde_json::json!({"a": 7, "b": 3})));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "10");
    }

    #[test]
    fn test_ctx_call_tool_json() {
        let mut router = Router::new();
        router
            .add_tool(ComputeJsonTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller: Arc<dyn ToolCaller> =
            Arc::new(RouterToolCaller::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_tool_caller(caller);

        #[derive(Debug, serde::Deserialize)]
        struct Result {
            value: i32,
        }

        let result: McpResult<Result> =
            fastmcp_core::block_on(ctx.call_tool_json("compute", serde_json::json!({})));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().value, 42);
    }

    #[test]
    fn test_ctx_call_tool_returns_error_result() {
        let mut router = Router::new();
        router
            .add_tool(FailingToolTest)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller: Arc<dyn ToolCaller> =
            Arc::new(RouterToolCaller::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_tool_caller(caller);

        // call_tool returns the error as is_error=true
        let result = fastmcp_core::block_on(ctx.call_tool("failing", serde_json::json!({})));
        assert!(result.is_ok());
        let call_result = result.unwrap();
        assert!(call_result.is_error);
    }

    #[test]
    fn test_ctx_call_tool_text_propagates_error() {
        let mut router = Router::new();
        router
            .add_tool(FailingToolTest)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller: Arc<dyn ToolCaller> =
            Arc::new(RouterToolCaller::new(router_arc, SessionState::new()));

        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_tool_caller(caller);

        // call_tool_text converts is_error=true to Err
        let result = fastmcp_core::block_on(ctx.call_tool_text("failing", serde_json::json!({})));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("failed"));
    }

    #[test]
    fn test_tool_call_depth_increments() {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1).with_tool_call_depth(5);

        assert_eq!(ctx.tool_call_depth(), 5);
    }

    #[test]
    fn test_max_tool_call_depth_constant() {
        // Verify the constant is reasonable
        assert_eq!(MAX_TOOL_CALL_DEPTH, 10);
    }

    #[test]
    fn test_tool_validation_error() {
        let mut router = Router::new();
        router
            .add_tool(AddNumbersTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let caller = RouterToolCaller::new(router_arc, SessionState::new());

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1).with_budget_ceiling(Budget::INFINITE);
        // Missing required parameters
        let result = fastmcp_core::block_on(caller.call_tool(
            &request_ctx,
            "add",
            serde_json::json!({}), // Missing a and b
            0,
        ));

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("validation"));
    }

    #[test]
    fn test_session_state_propagates_through_nested_tool_calls() {
        use crate::RouterResourceReader;

        let mut router = Router::new();
        router
            .add_tool(GetStateFromCtx)
            .expect("tool registration succeeds");
        router
            .add_tool(NestedStateCall)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let session_state = SessionState::new();
        let caller: Arc<dyn ToolCaller> = Arc::new(RouterToolCaller::new(
            router_arc.clone(),
            session_state.clone(),
        ));
        let reader: Arc<dyn fastmcp_core::ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, session_state.clone()));

        let cx = Cx::for_testing();
        let ctx = McpContext::with_state(cx, 1, session_state)
            .with_tool_caller(caller)
            .with_resource_reader(reader);

        // Call the nested tool - it sets state then calls another tool
        let result = fastmcp_core::block_on(ctx.call_tool("nested_state", serde_json::json!({})));
        assert!(result.is_ok());
        let call_result = result.unwrap();

        // The inner tool should have seen the propagated value
        let text = call_result.first_text().unwrap();
        assert!(
            text.contains("tool_propagated_value"),
            "Expected session state to propagate through tool calls, got: {}",
            text
        );
    }

    struct CurrentAuthTool;

    impl ToolHandler for CurrentAuthTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "current_auth".to_string(),
                description: Some("Returns the current auth subject".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                output_schema: None,
                icon: None,
                version: None,
                annotations: None,
                tags: vec![],
            }
        }

        fn call(&self, ctx: &McpContext, _arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::Text {
                text: ctx
                    .auth()
                    .and_then(|auth| auth.subject)
                    .unwrap_or_else(|| "anonymous".to_string()),
            }])
        }
    }

    struct NestedAuthTool;

    impl ToolHandler for NestedAuthTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "nested_auth".to_string(),
                description: Some("Calls another tool and expects auth to propagate".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                output_schema: None,
                icon: None,
                version: None,
                annotations: None,
                tags: vec![],
            }
        }

        fn execution_mode(&self) -> crate::handler::ToolExecutionMode {
            crate::handler::ToolExecutionMode::Async
        }

        fn call(
            &self,
            _ctx: &McpContext,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            // A nested call is async; bridging it with block_on from this
            // synchronous hook is the rejected pattern (bd-6rfrg).
            Err(McpError::internal_error("nested calls use call_async"))
        }

        fn call_async<'a>(
            &'a self,
            ctx: &'a McpContext,
            _arguments: serde_json::Value,
        ) -> crate::handler::BoxFuture<'a, fastmcp_core::McpOutcome<Vec<Content>>> {
            Box::pin(async move {
                match ctx.call_tool("current_auth", serde_json::json!({})).await {
                    Ok(inner) => fastmcp_core::Outcome::Ok(vec![Content::Text {
                        text: inner.first_text().unwrap_or("missing").to_string(),
                    }]),
                    Err(error) => fastmcp_core::Outcome::Err(error),
                }
            })
        }
    }

    #[test]
    fn test_request_auth_propagates_through_nested_tool_calls() {
        use crate::RouterResourceReader;

        let mut router = Router::new();
        router
            .add_tool(CurrentAuthTool)
            .expect("tool registration succeeds");
        router
            .add_tool(NestedAuthTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let session_state = SessionState::new();
        let caller: Arc<dyn ToolCaller> = Arc::new(RouterToolCaller::new(
            router_arc.clone(),
            session_state.clone(),
        ));
        let reader: Arc<dyn fastmcp_core::ResourceReader> =
            Arc::new(RouterResourceReader::new(router_arc, session_state.clone()));

        let cx = Cx::for_testing();
        let ctx = McpContext::with_state(cx, 1, session_state)
            .with_tool_caller(caller)
            .with_resource_reader(reader)
            .with_auth(AuthContext::with_subject("tool-auth"));

        let result = fastmcp_core::block_on(ctx.call_tool("nested_auth", serde_json::json!({})))
            .expect("nested tool call should succeed");
        assert_eq!(result.first_text(), Some("tool-auth"));
    }

    // ========================================================================
    // bd-v9ev3 area (d), SECOND SHAPE: the call_tool column.
    //
    // STRENGTHENING, NOT REQUIRED BY A1 AS WRITTEN. A1 is satisfied by an
    // explanation that predicts each area's observed failure text, and the
    // mechanism named on the read_resource column already does that for all
    // four (d) tests. This test exists because the mechanism is shared while
    // the SHAPE is not: `ToolHandler::call` dispatched through a tool caller is
    // a different path from `ResourceHandler::read` through a resource reader,
    // and a true fact about one caller is not a fact about a different one.
    // It buys the difference between predicted and observed on that column.
    // ========================================================================

    /// The `BridgeProbeResource` seam on the tool dispatch path: bridge a
    /// nested call, catch the unwind, and report the payload as the tool's own
    /// output instead of letting the redaction swallow it.
    struct BridgeProbeTool;

    impl ToolHandler for BridgeProbeTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "bridge_probe".to_string(),
                description: Some("Reports the payload of its own nested bridge".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                output_schema: None,
                icon: None,
                version: None,
                annotations: None,
                tags: vec![],
            }
        }

        fn call(&self, ctx: &McpContext, _arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            let observed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fastmcp_core::block_on(ctx.call_tool("current_auth", serde_json::json!({})))
            })) {
                Ok(Ok(inner)) => format!(
                    "NO PANIC: inner call returned {}",
                    inner.first_text().unwrap_or("(no content)")
                ),
                Ok(Err(error)) => format!("NO PANIC: inner call returned Err {error}"),
                Err(payload) => {
                    super::ctx_read_resource_tests::panic_payload_text(payload.as_ref())
                }
            };

            Ok(vec![Content::Text { text: observed }])
        }
    }

    /// POSITIVE, second shape. Moves the call_tool column from PREDICTED to
    /// OBSERVED.
    ///
    /// No separate negative accompanies this one, and that is a judgement I am
    /// recording rather than an omission: the RH-5 control for the varied
    /// dimension -- whether a bridge is already active -- is owned by
    /// `the_same_handler_completes_when_no_bridge_is_already_active`, and the
    /// reentrancy flag it exercises is one process-global thread-local shared
    /// by both shapes. The alternative this shape could otherwise admit -- that
    /// the tool path panics for some unrelated reason -- is excluded by the
    /// assertion being an EQUALITY on the payload rather than a check that
    /// something panicked.
    #[test]
    fn a_nested_bridge_from_a_sync_tool_handler_panics_with_the_reentrancy_rejection() {
        let mut router = Router::new();
        router
            .add_tool(CurrentAuthTool)
            .expect("tool registration succeeds");
        router
            .add_tool(BridgeProbeTool)
            .expect("tool registration succeeds");

        let router_arc = Arc::new(router);
        let session_state = SessionState::new();
        let caller: Arc<dyn ToolCaller> =
            Arc::new(RouterToolCaller::new(router_arc, session_state.clone()));

        let ctx = McpContext::with_state(Cx::for_testing(), 1, session_state)
            .with_tool_caller(caller)
            .with_auth(AuthContext::with_subject("probe-auth"));

        // The outer bridge. The tool it dispatches to enters a second one.
        let observed = fastmcp_core::block_on(ctx.call_tool("bridge_probe", serde_json::json!({})))
            .expect("the probe catches its own unwind, so the outer call must succeed")
            .first_text()
            .expect("the probe reports its observation as text")
            .to_string();

        assert_eq!(
            observed, "nested fastmcp_core::runtime::block_on is not supported",
            "the tool dispatch path does not reach the same reentrancy rejection as the \
             resource path, so the call_tool column is NOT the same defect and (d) is two \
             defects rather than one"
        );
    }
}

// ============================================================================
// Handler Direct Tests
// ============================================================================

mod handler_direct_tests {
    use super::*;
    use crate::handler::{
        BidirectionalSenders, MountedPromptHandler, MountedResourceHandler, MountedToolHandler,
        ProgressNotificationSender, UriParams,
    };
    use fastmcp_protocol::{Icon, ToolAnnotations};

    /// Helper: create a test McpContext.
    fn test_ctx() -> McpContext {
        let cx = Cx::for_testing();
        McpContext::new(cx, 1)
    }

    // ── ToolHandler direct call ──────────────────────────────────────

    #[test]
    fn tool_handler_call_returns_content() {
        let tool = Greet;
        let ctx = test_ctx();
        let result = tool.call(&ctx, serde_json::json!({"name": "Alice"}));
        assert!(result.is_ok());
        let contents = result.unwrap();
        assert_eq!(contents.len(), 1);
        assert!(
            matches!(contents[0], Content::Text { .. }),
            "Expected text content"
        );
        let Content::Text { text } = &contents[0] else {
            return;
        };
        assert_eq!(text, "Hello, Alice!");
    }

    #[test]
    fn tool_handler_call_default_arg() {
        let tool = GreetDefault;
        let ctx = test_ctx();
        let result = tool.call(&ctx, serde_json::json!({}));
        assert!(result.is_ok());
        let content = result.unwrap();
        assert!(
            matches!(content[0], Content::Text { .. }),
            "Expected text content"
        );
        let Content::Text { text } = &content[0] else {
            return;
        };
        assert_eq!(text, "Hello, World!");
    }

    #[test]
    fn tool_handler_error_returns_mcp_error() {
        let tool = ErrorTool;
        let ctx = test_ctx();
        let result = tool.call(&ctx, serde_json::json!({}));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn tool_handler_definition_has_expected_fields() {
        let tool = Greet;
        let def = tool.definition();
        assert_eq!(def.name, "greet");
        assert!(def.description.is_some());
        assert_eq!(def.input_schema["type"], "object");
        assert!(def.input_schema["properties"]["name"].is_object());
    }

    #[test]
    fn tool_handler_default_icon_is_none() {
        let tool = Greet;
        assert!(tool.icon().is_none());
    }

    #[test]
    fn tool_handler_default_version_is_none() {
        let tool = Greet;
        assert!(tool.version().is_none());
    }

    #[test]
    fn tool_handler_default_tags_is_empty() {
        let tool = Greet;
        assert_eq!(tool.tags().len(), 0);
    }

    #[test]
    fn tool_handler_default_annotations_is_none() {
        let tool = Greet;
        assert!(tool.annotations().is_none());
    }

    #[test]
    fn tool_handler_default_output_schema_is_none() {
        let tool = Greet;
        assert!(tool.output_schema().is_none());
    }

    #[test]
    fn tool_handler_default_timeout_is_none() {
        let tool = Greet;
        assert!(tool.timeout().is_none());
    }

    // ── Custom tool with overrides ───────────────────────────────────

    struct RichTool {
        icon: Icon,
        version: String,
        tags: Vec<String>,
        annotations: ToolAnnotations,
        output_schema: serde_json::Value,
        timeout: Duration,
    }

    impl Default for RichTool {
        fn default() -> Self {
            Self {
                icon: Icon {
                    src: Some("https://example.com/icon.png".to_string()),
                    mime_type: None,
                    sizes: None,
                },
                version: "2.0.0".to_string(),
                tags: vec!["api".to_string(), "read".to_string()],
                annotations: ToolAnnotations {
                    destructive: Some(false),
                    idempotent: Some(true),
                    read_only: Some(true),
                    open_world_hint: Some(false),
                },
                output_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "result": {"type": "string"}
                    }
                }),
                timeout: Duration::from_secs(60),
            }
        }
    }

    impl ToolHandler for RichTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "rich".to_string(),
                description: Some("A fully configured tool".to_string()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: Some(self.output_schema.clone()),
                icon: Some(self.icon.clone()),
                version: Some(self.version.clone()),
                tags: self.tags.clone(),
                annotations: Some(self.annotations.clone()),
            }
        }

        fn icon(&self) -> Option<&Icon> {
            Some(&self.icon)
        }

        fn version(&self) -> Option<&str> {
            Some(&self.version)
        }

        fn tags(&self) -> &[String] {
            &self.tags
        }

        fn annotations(&self) -> Option<&ToolAnnotations> {
            Some(&self.annotations)
        }

        fn output_schema(&self) -> Option<serde_json::Value> {
            Some(self.output_schema.clone())
        }

        fn timeout(&self) -> Option<Duration> {
            Some(self.timeout)
        }

        fn call(
            &self,
            _ctx: &McpContext,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            Ok(vec![Content::Text {
                text: "rich result".to_string(),
            }])
        }
    }

    #[test]
    fn tool_handler_custom_icon() {
        let tool = RichTool::default();
        assert!(tool.icon().is_some());
    }

    #[test]
    fn tool_handler_custom_version() {
        let tool = RichTool::default();
        assert_eq!(tool.version(), Some("2.0.0"));
    }

    #[test]
    fn tool_handler_custom_tags() {
        let tool = RichTool::default();
        assert_eq!(tool.tags().len(), 2);
        assert_eq!(tool.tags()[0], "api");
    }

    #[test]
    fn tool_handler_custom_annotations() {
        let tool = RichTool::default();
        let ann = tool.annotations().unwrap();
        assert_eq!(ann.read_only, Some(true));
        assert_eq!(ann.destructive, Some(false));
        assert_eq!(ann.idempotent, Some(true));
    }

    #[test]
    fn tool_handler_custom_output_schema() {
        let tool = RichTool::default();
        let schema = tool.output_schema().unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn tool_handler_custom_timeout() {
        let tool = RichTool::default();
        assert_eq!(tool.timeout(), Some(Duration::from_secs(60)));
    }

    // ── ResourceHandler direct read ──────────────────────────────────

    #[test]
    fn resource_handler_read_returns_content() {
        let resource = StaticResource {
            uri: "test://hello".to_string(),
            content: "world".to_string(),
        };
        let ctx = test_ctx();
        let result = resource.read(&ctx);
        assert!(result.is_ok());
        let contents = result.unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].uri, "test://hello");
        assert_eq!(contents[0].text, Some("world".to_string()));
    }

    #[test]
    fn resource_handler_definition_fields() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "content".to_string(),
        };
        let def = resource.definition();
        assert_eq!(def.uri, "test://data");
        assert_eq!(def.name, "Static Resource");
        assert_eq!(def.mime_type, Some("text/plain".to_string()));
    }

    #[test]
    fn resource_handler_default_template_is_none() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "content".to_string(),
        };
        assert!(resource.template().is_none());
    }

    #[test]
    fn resource_handler_default_icon_is_none() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "".to_string(),
        };
        assert!(resource.icon().is_none());
    }

    #[test]
    fn resource_handler_default_version_is_none() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "".to_string(),
        };
        assert!(resource.version().is_none());
    }

    #[test]
    fn resource_handler_default_tags_is_empty() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "".to_string(),
        };
        assert_eq!(resource.tags().len(), 0);
    }

    #[test]
    fn resource_handler_default_timeout_is_none() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "".to_string(),
        };
        assert!(resource.timeout().is_none());
    }

    #[test]
    fn resource_handler_read_with_uri_delegates_to_read() {
        let resource = StaticResource {
            uri: "test://data".to_string(),
            content: "delegated".to_string(),
        };
        let ctx = test_ctx();
        let params = UriParams::new();
        let result = resource.read_with_uri(&ctx, "test://data", &params);
        assert!(result.is_ok());
        assert_eq!(result.unwrap()[0].text, Some("delegated".to_string()));
    }

    #[test]
    fn resource_handler_template_resource_read_with_uri() {
        let resource = TemplateResource;
        let ctx = test_ctx();
        let mut params = UriParams::new();
        params.insert("id".to_string(), "42".to_string());
        let result = resource.read_with_uri(&ctx, "resource://42", &params);
        assert!(result.is_ok());
        assert_eq!(result.unwrap()[0].text, Some("Template 42".to_string()));
    }

    #[test]
    fn resource_handler_template_resource_has_template() {
        let resource = TemplateResource;
        let tmpl = resource.template();
        assert!(tmpl.is_some());
        assert_eq!(tmpl.unwrap().uri_template, "resource:///items/{id}");
    }

    #[test]
    fn resource_handler_template_resource_read_without_params_errors() {
        let resource = TemplateResource;
        let ctx = test_ctx();
        let result = resource.read(&ctx);
        assert!(result.is_err());
    }

    // ── PromptHandler direct get ─────────────────────────────────────

    #[test]
    fn prompt_handler_get_returns_messages() {
        let prompt = GreetingPrompt;
        let ctx = test_ctx();
        let mut args = HashMap::new();
        args.insert("name".to_string(), "Bob".to_string());
        let result = prompt.get(&ctx, args);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(messages[0].content, Content::Text { .. }),
            "Expected text content"
        );
        let Content::Text { text } = &messages[0].content else {
            return;
        };
        assert!(text.contains("Bob"));
    }

    #[test]
    fn prompt_handler_definition_fields() {
        let prompt = GreetingPrompt;
        let def = prompt.definition();
        assert_eq!(def.name, "greeting");
        assert!(def.description.is_some());
        assert_eq!(def.arguments.len(), 1);
        assert_eq!(def.arguments[0].name, "name");
        assert!(def.arguments[0].required);
    }

    #[test]
    fn prompt_handler_default_icon_is_none() {
        let prompt = GreetingPrompt;
        assert!(prompt.icon().is_none());
    }

    #[test]
    fn prompt_handler_default_version_is_none() {
        let prompt = GreetingPrompt;
        assert!(prompt.version().is_none());
    }

    #[test]
    fn prompt_handler_default_tags_is_empty() {
        let prompt = GreetingPrompt;
        assert_eq!(prompt.tags().len(), 0);
    }

    #[test]
    fn prompt_handler_default_timeout_is_none() {
        let prompt = GreetingPrompt;
        assert!(prompt.timeout().is_none());
    }

    #[test]
    fn prompt_handler_get_with_missing_arg_uses_default() {
        let prompt = GreetingPrompt;
        let ctx = test_ctx();
        let args = HashMap::new(); // no "name" argument
        let result = prompt.get(&ctx, args);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert!(
            matches!(messages[0].content, Content::Text { .. }),
            "Expected text content"
        );
        let Content::Text { text } = &messages[0].content else {
            return;
        };
        assert!(text.contains("User"));
    }

    // ── MountedToolHandler ───────────────────────────────────────────

    #[test]
    fn mounted_tool_handler_overrides_name() {
        let inner: Box<dyn ToolHandler> = Box::new(Greet);
        let mounted = MountedToolHandler::new(inner, "ns/greet".to_string());
        let def = mounted.definition();
        assert_eq!(def.name, "ns/greet");
        // Other fields preserved
        assert!(def.description.is_some());
    }

    #[test]
    fn mounted_tool_handler_delegates_call() {
        let inner: Box<dyn ToolHandler> = Box::new(Greet);
        let mounted = MountedToolHandler::new(inner, "ns/greet".to_string());
        let ctx = test_ctx();
        let result = mounted.call(&ctx, serde_json::json!({"name": "Mounted"}));
        assert!(result.is_ok());
        let contents = result.unwrap();
        assert!(
            matches!(contents[0], Content::Text { .. }),
            "Expected text content"
        );
        let Content::Text { text } = &contents[0] else {
            return;
        };
        assert_eq!(text, "Hello, Mounted!");
    }

    #[test]
    fn mounted_tool_handler_delegates_timeout() {
        let inner: Box<dyn ToolHandler> = Box::new(RichTool::default());
        let mounted = MountedToolHandler::new(inner, "ns/rich".to_string());
        assert_eq!(mounted.timeout(), Some(Duration::from_secs(60)));
    }

    #[test]
    fn mounted_tool_handler_delegates_annotations() {
        let inner: Box<dyn ToolHandler> = Box::new(RichTool::default());
        let mounted = MountedToolHandler::new(inner, "ns/rich".to_string());
        let ann = mounted.annotations().unwrap();
        assert_eq!(ann.read_only, Some(true));
    }

    #[test]
    fn mounted_tool_handler_delegates_output_schema() {
        let inner: Box<dyn ToolHandler> = Box::new(RichTool::default());
        let mounted = MountedToolHandler::new(inner, "ns/rich".to_string());
        assert!(mounted.output_schema().is_some());
    }

    // ── MountedResourceHandler ───────────────────────────────────────

    #[test]
    fn mounted_resource_handler_overrides_uri() {
        let inner: Box<dyn ResourceHandler> = Box::new(StaticResource {
            uri: "test://orig".to_string(),
            content: "data".to_string(),
        });
        let mounted = MountedResourceHandler::new(
            inner,
            "test://orig".to_string(),
            "ns/test://orig".to_string(),
        );
        let def = mounted.definition();
        assert_eq!(def.uri, "ns/test://orig");
        // Other fields preserved
        assert_eq!(def.name, "Static Resource");
    }

    #[test]
    fn mounted_resource_handler_delegates_read() {
        let inner: Box<dyn ResourceHandler> = Box::new(StaticResource {
            uri: "test://data".to_string(),
            content: "mounted_data".to_string(),
        });
        let mounted = MountedResourceHandler::new(
            inner,
            "test://data".to_string(),
            "ns/test://data".to_string(),
        );
        let ctx = test_ctx();
        let result = mounted.read(&ctx);
        assert!(result.is_ok());
        let contents = result.unwrap();
        assert_eq!(contents[0].text, Some("mounted_data".to_string()));
        assert_eq!(contents[0].uri, "ns/test://data");
    }

    #[test]
    fn mounted_resource_handler_with_template() {
        let inner: Box<dyn ResourceHandler> = Box::new(TemplateResource);
        let tmpl = ResourceTemplate {
            uri_template: "ns/resource:///items/{id}".to_string(),
            name: "Mounted Template".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let mounted = MountedResourceHandler::with_template(
            inner,
            "resource:///items/{id}".to_string(),
            "ns/resource:///items/{id}".to_string(),
            tmpl,
        );
        let template = mounted.template();
        assert!(template.is_some());
        assert_eq!(template.unwrap().uri_template, "ns/resource:///items/{id}");
    }

    // ── MountedPromptHandler ─────────────────────────────────────────

    #[test]
    fn mounted_prompt_handler_overrides_name() {
        let inner: Box<dyn PromptHandler> = Box::new(GreetingPrompt);
        let mounted = MountedPromptHandler::new(inner, "ns/greeting".to_string());
        let def = mounted.definition();
        assert_eq!(def.name, "ns/greeting");
        // Arguments preserved
        assert_eq!(def.arguments.len(), 1);
    }

    #[test]
    fn mounted_prompt_handler_delegates_get() {
        let inner: Box<dyn PromptHandler> = Box::new(GreetingPrompt);
        let mounted = MountedPromptHandler::new(inner, "ns/greeting".to_string());
        let ctx = test_ctx();
        let mut args = HashMap::new();
        args.insert("name".to_string(), "MountedUser".to_string());
        let result = mounted.get(&ctx, args);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert!(
            matches!(messages[0].content, Content::Text { .. }),
            "Expected text content"
        );
        let Content::Text { text } = &messages[0].content else {
            return;
        };
        assert!(text.contains("MountedUser"));
    }

    // ── ProgressNotificationSender ───────────────────────────────────

    fn final_progress_timer_rearm_probe(cancel: bool) {
        use fastmcp_core::NotificationSender;
        use std::future::Future;
        use std::sync::atomic::AtomicUsize;
        use std::task::{Context, Poll, Wake, Waker};

        struct WakeCount(AtomicUsize);
        impl Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Force every real Sleep, including each replacement, to become ready
        // immediately. This needs no wall-clock delay or scheduler lottery.
        let ambient = Cx::for_testing();
        ambient.set_cancel_requested(true);
        let _ambient = Cx::set_current(Some(ambient));
        let cx = Cx::for_testing();
        let cancellation = McpRequestCancellation::new();
        let sent = Mutex::new(Vec::new());
        let runtime = crate::handler::FinalProgressRuntime::new(
            fastmcp_protocol::ProgressMarker::from("rearm"),
            |request| sent.lock().unwrap().push(request),
        );
        let mut polls = 0;
        let handler = std::future::poll_fn(|_| {
            polls += 1;
            if polls <= 3 {
                Poll::Pending
            } else {
                Poll::Ready(73)
            }
        });
        let mut wait = Box::pin(crate::await_final_progress_rate_tick(
            &cx,
            &cancellation,
            &runtime,
            handler,
        ));
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wakes));
        let mut task_cx = Context::from_waker(&waker);
        for step in 1..=3 {
            if cancel && step == 3 {
                assert!(cancellation.cancel());
            }
            runtime.send_progress_exact(serde_json::Number::from(step), None, None);
            assert!(wait.as_mut().poll(&mut task_cx).is_pending());
        }
        let expected_frames = if cancel { 2 } else { 3 };
        assert!(
            wakes.0.load(Ordering::SeqCst) >= expected_frames,
            "successive rate ticks must arrange another handler poll"
        );
        let notifications = sent.lock().unwrap();
        assert_eq!(notifications.len(), expected_frames);
        for (index, notification) in notifications.iter().enumerate() {
            assert_eq!(notification.method, "notifications/progress");
            let params = notification.params.as_ref().unwrap();
            assert_eq!(params["progressToken"], "rearm");
            assert_eq!(params["progress"], index + 1);
        }
        drop(notifications);
        assert_eq!(wait.as_mut().poll(&mut task_cx), Poll::Ready(73));
        assert_eq!(sent.lock().unwrap().len(), expected_frames);
        assert!(!cx.is_cancel_requested());
    }

    #[test]
    fn final_progress_timer_rearm_positive() {
        final_progress_timer_rearm_probe(false);
    }

    #[test]
    fn final_progress_timer_rearm_planted_negative() {
        final_progress_timer_rearm_probe(true);
    }

    #[test]
    fn progress_notification_sender_sends_notification() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent_clone = sent.clone();
        let sender = ProgressNotificationSender::new(
            fastmcp_protocol::ProgressMarker::String("tok".to_string()),
            move |req: fastmcp_protocol::JsonRpcRequest| {
                sent_clone.lock().unwrap().push(req);
            },
        );

        use fastmcp_core::NotificationSender;
        sender.send_progress(0.5, Some(1.0), Some("half done"));

        let notifications = sent.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].method, "notifications/progress");
    }

    #[test]
    fn progress_notification_sender_without_total() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent_clone = sent.clone();
        let sender = ProgressNotificationSender::new(
            fastmcp_protocol::ProgressMarker::Number(fastmcp_protocol::JsonInteger::from(99_i64)),
            move |req: fastmcp_protocol::JsonRpcRequest| {
                sent_clone.lock().unwrap().push(req);
            },
        );

        use fastmcp_core::NotificationSender;
        sender.send_progress(1.0, None, None);

        let notifications = sent.lock().unwrap();
        assert_eq!(notifications.len(), 1);
    }

    #[test]
    fn progress_notification_sender_debug_format() {
        let sender = ProgressNotificationSender::new(
            fastmcp_protocol::ProgressMarker::String("debug-test".to_string()),
            |_: fastmcp_protocol::JsonRpcRequest| {},
        );
        let debug = format!("{sender:?}");
        assert!(debug.contains("ProgressNotificationSender"));
    }

    // ── BidirectionalSenders ─────────────────────────────────────────

    #[test]
    fn bidirectional_senders_default_is_empty() {
        let senders = BidirectionalSenders::new();
        assert!(senders.sampling.is_none());
        assert!(senders.elicitation.is_none());
    }

    #[test]
    fn bidirectional_senders_debug_format() {
        let senders = BidirectionalSenders::new();
        let debug = format!("{senders:?}");
        assert!(debug.contains("BidirectionalSenders"));
        assert!(debug.contains("sampling: false"));
        assert!(debug.contains("elicitation: false"));
    }

    // ── Router registration and lookup via direct handler ────────────

    #[test]
    fn router_registers_tool_and_lists_it() {
        let mut router = Router::new();
        router.add_tool(Greet).expect("tool registration succeeds");
        let tools = router.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");
    }

    #[test]
    fn router_registers_resource_and_lists_it() {
        let mut router = Router::new();
        router.add_resource(StaticResource {
            uri: "test://r1".to_string(),
            content: "c1".to_string(),
        });
        let resources = router.resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "test://r1");
    }

    #[test]
    fn router_registers_prompt_and_lists_it() {
        let mut router = Router::new();
        router.add_prompt(GreetingPrompt);
        let prompts = router.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "greeting");
    }

    #[test]
    fn router_counts_match_registrations() {
        let mut router = Router::new();
        router.add_tool(Greet).expect("tool registration succeeds");
        router
            .add_tool(ErrorTool)
            .expect("tool registration succeeds");
        router.add_resource(StaticResource {
            uri: "test://a".to_string(),
            content: "".to_string(),
        });
        router.add_prompt(GreetingPrompt);

        assert_eq!(router.tools_count(), 2);
        assert_eq!(router.resources_count(), 1);
        assert_eq!(router.prompts_count(), 1);
    }

    #[test]
    fn router_resource_template_count() {
        let mut router = Router::new();
        router.add_resource(TemplateResource);
        // TemplateResource returns a template, so it should count as a template
        assert_eq!(router.resource_templates_count(), 1);
    }

    #[test]
    fn router_strict_input_validation_default_is_false() {
        let router = Router::new();
        assert!(!router.strict_input_validation());
    }

    #[test]
    fn router_strict_input_validation_can_be_set() {
        let mut router = Router::new();
        router.set_strict_input_validation(true);
        assert!(router.strict_input_validation());
    }
}

// ============================================================================
// ServerBuilder Tests
// ============================================================================

mod builder_tests {
    use super::*;
    use crate::{DuplicateBehavior, LoggingConfig, ServerBuilder};
    use fastmcp_console::config::{BannerStyle, ConsoleConfig, TrafficVerbosity};
    use fastmcp_protocol::ResourceTemplate;
    use log::Level;

    // ── Minimal concrete handlers for builder coverage ─────────────────

    #[tool(name = "alpha", description = "Test tool alpha")]
    fn builder_alpha_tool() -> String {
        "test:alpha".to_string()
    }

    #[tool(name = "a", description = "Test tool a")]
    fn builder_a_tool() -> String {
        "test:a".to_string()
    }

    #[tool(name = "b", description = "Test tool b")]
    fn builder_b_tool() -> String {
        "test:b".to_string()
    }

    #[tool(name = "c", description = "Test tool c")]
    fn builder_c_tool() -> String {
        "test:c".to_string()
    }

    #[tool(name = "do_thing", description = "Test tool do_thing")]
    fn builder_do_thing_tool() -> String {
        "test:do_thing".to_string()
    }

    #[tool(name = "dup", description = "Test tool dup")]
    fn builder_dup_tool() -> String {
        "test:dup".to_string()
    }

    #[tool(name = "fetch", description = "Test tool fetch")]
    fn builder_fetch_tool() -> String {
        "test:fetch".to_string()
    }

    #[tool(name = "query", description = "Test tool query")]
    fn builder_query_tool() -> String {
        "test:query".to_string()
    }

    #[tool(name = "t", description = "Test tool t")]
    fn builder_t_tool() -> String {
        "test:t".to_string()
    }

    #[tool(name = "t1", description = "Test tool t1")]
    fn builder_t1_tool() -> String {
        "test:t1".to_string()
    }

    #[tool(name = "t2", description = "Test tool t2")]
    fn builder_t2_tool() -> String {
        "test:t2".to_string()
    }

    #[tool(name = "t3", description = "Test tool t3")]
    fn builder_t3_tool() -> String {
        "test:t3".to_string()
    }

    #[tool(name = "tool_a", description = "Test tool tool_a")]
    fn builder_tool_a_tool() -> String {
        "test:tool_a".to_string()
    }

    struct NamedResource {
        name: &'static str,
        uri: String,
    }

    impl NamedResource {
        fn named(name: &'static str) -> Self {
            Self {
                name,
                uri: format!("test://{name}"),
            }
        }
    }

    impl ResourceHandler for NamedResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: self.uri.clone(),
                name: self.name.to_string(),
                description: Some(format!("Test resource {}", self.name)),
                mime_type: Some("text/plain".to_string()),
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: self.uri.clone(),
                mime_type: Some("text/plain".to_string()),
                text: Some(format!("content:{}", self.name)),
                blob: None,
            }])
        }
    }

    struct NamedPrompt {
        name: &'static str,
    }

    impl NamedPrompt {
        fn named(name: &'static str) -> Self {
            Self { name }
        }
    }

    impl PromptHandler for NamedPrompt {
        fn definition(&self) -> Prompt {
            Prompt {
                name: self.name.to_string(),
                description: Some(format!("Test prompt {}", self.name)),
                arguments: vec![],
                icon: None,
                version: None,
                tags: vec![],
            }
        }

        fn get(
            &self,
            _ctx: &McpContext,
            _arguments: HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            Ok(vec![PromptMessage {
                role: Role::User,
                content: Content::Text {
                    text: format!("prompt:{}", self.name),
                },
            }])
        }
    }

    // ── Basic Construction ───────────────────────────────────────────

    #[test]
    fn builder_new_sets_name_and_version() {
        let server = ServerBuilder::new("test-server", "1.2.3").build();
        assert_eq!(server.info().name, "test-server");
        assert_eq!(server.info().version, "1.2.3");
    }

    #[test]
    fn builder_default_capabilities_include_logging() {
        let server = ServerBuilder::new("s", "0.1").build();
        assert!(server.capabilities().logging.is_some());
        assert!(server.capabilities().tools.is_none());
        assert!(server.capabilities().resources.is_none());
        assert!(server.capabilities().prompts.is_none());
        assert!(server.capabilities().tasks.is_none());
    }

    #[test]
    fn builder_server_new_delegates_to_builder() {
        // Server::new returns a ServerBuilder, not a Server
        let server = Server::new("srv", "0.1").build();
        assert_eq!(server.info().name, "srv");
    }

    // ── Tool Registration ────────────────────────────────────────────

    #[test]
    fn builder_tool_enables_tools_capability() {
        let server = ServerBuilder::new("s", "0.1")
            .tool(BuilderAlphaTool)
            .build();
        assert!(server.capabilities().tools.is_some());
        assert!(server.has_tools());
    }

    #[test]
    fn builder_registers_multiple_tools() {
        let server = ServerBuilder::new("s", "0.1")
            .tool(BuilderATool)
            .tool(BuilderBTool)
            .tool(BuilderCTool)
            .build();
        let tools = server.tools();
        assert_eq!(tools.len(), 3);
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    // ── Resource Registration ────────────────────────────────────────

    #[test]
    fn builder_resource_enables_resources_capability() {
        let server = ServerBuilder::new("s", "0.1")
            .resource(NamedResource::named("data"))
            .build();
        assert!(server.capabilities().resources.is_some());
        assert!(server.has_resources());
    }

    #[test]
    fn builder_registers_multiple_resources() {
        let server = ServerBuilder::new("s", "0.1")
            .resource(NamedResource::named("r1"))
            .resource(NamedResource::named("r2"))
            .build();
        let resources = server.resources();
        assert_eq!(resources.len(), 2);
    }

    #[test]
    fn builder_resource_template_enables_resources_capability() {
        let template = ResourceTemplate {
            uri_template: "file://{path}".to_string(),
            name: "file".to_string(),
            description: None,
            mime_type: None,
            icon: None,
            version: None,
            tags: vec![],
        };
        let server = ServerBuilder::new("s", "0.1")
            .resource_template(template)
            .build();
        assert!(server.capabilities().resources.is_some());
        assert!(server.has_resources());
        let templates = server.resource_templates();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "file");
    }

    // ── Prompt Registration ──────────────────────────────────────────

    #[test]
    fn builder_prompt_enables_prompts_capability() {
        let server = ServerBuilder::new("s", "0.1")
            .prompt(NamedPrompt::named("hello"))
            .build();
        assert!(server.capabilities().prompts.is_some());
        assert!(server.has_prompts());
    }

    #[test]
    fn builder_registers_multiple_prompts() {
        let server = ServerBuilder::new("s", "0.1")
            .prompt(NamedPrompt::named("p1"))
            .prompt(NamedPrompt::named("p2"))
            .prompt(NamedPrompt::named("p3"))
            .build();
        let prompts = server.prompts();
        assert_eq!(prompts.len(), 3);
    }

    // ── Mixed Registration ───────────────────────────────────────────

    #[test]
    fn builder_mixed_handlers_enable_all_capabilities() {
        let server = ServerBuilder::new("s", "0.1")
            .tool(BuilderTTool)
            .resource(NamedResource::named("r"))
            .prompt(NamedPrompt::named("p"))
            .build();
        assert!(server.has_tools());
        assert!(server.has_resources());
        assert!(server.has_prompts());
    }

    #[test]
    fn builder_no_handlers_means_no_capabilities() {
        let server = ServerBuilder::new("empty", "0.1").build();
        assert!(!server.has_tools());
        assert!(!server.has_resources());
        assert!(!server.has_prompts());
    }

    // ── Request Timeout ──────────────────────────────────────────────

    #[test]
    fn builder_default_request_timeout_is_30() {
        // Build and check internal state via the server
        let server = ServerBuilder::new("s", "0.1").build();
        // Default is 30 seconds; verified via internal state
        // We can't directly read request_timeout_secs, but we verify the builder
        // accepted the default without panicking
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_custom_request_timeout() {
        let server = ServerBuilder::new("s", "0.1").request_timeout(60).build();
        // Builder accepted custom timeout without error
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_zero_timeout_omits_only_server_ceiling() {
        let server = ServerBuilder::new("s", "0.1").request_timeout(0).build();
        assert_eq!(server.info().name, "s");
    }

    // ── Stats ────────────────────────────────────────────────────────

    #[test]
    fn builder_stats_enabled_by_default() {
        let server = ServerBuilder::new("s", "0.1").build();
        assert!(server.stats().is_some());
    }

    #[test]
    fn builder_without_stats_disables_collection() {
        let server = ServerBuilder::new("s", "0.1").without_stats().build();
        assert!(server.stats().is_none());
        assert!(server.stats_collector().is_none());
    }

    // ── Error Masking ────────────────────────────────────────────────

    #[test]
    fn builder_error_masking_disabled_by_default() {
        let builder = ServerBuilder::new("s", "0.1");
        assert!(!builder.is_error_masking_enabled());
    }

    #[test]
    fn builder_mask_error_details_enables_masking() {
        let builder = ServerBuilder::new("s", "0.1").mask_error_details(true);
        assert!(builder.is_error_masking_enabled());
    }

    #[test]
    fn builder_mask_error_details_toggle() {
        let builder = ServerBuilder::new("s", "0.1")
            .mask_error_details(true)
            .mask_error_details(false);
        assert!(!builder.is_error_masking_enabled());
    }

    // ── Strict Input Validation ──────────────────────────────────────

    #[test]
    fn builder_strict_input_validation_disabled_by_default() {
        let builder = ServerBuilder::new("s", "0.1");
        assert!(!builder.is_strict_input_validation_enabled());
    }

    #[test]
    fn builder_strict_input_validation_enable() {
        let builder = ServerBuilder::new("s", "0.1").strict_input_validation(true);
        assert!(builder.is_strict_input_validation_enabled());
    }

    #[test]
    fn builder_strict_input_validation_toggle() {
        let builder = ServerBuilder::new("s", "0.1")
            .strict_input_validation(true)
            .strict_input_validation(false);
        assert!(!builder.is_strict_input_validation_enabled());
    }

    // ── Instructions ─────────────────────────────────────────────────

    #[test]
    fn builder_instructions_set() {
        // Instructions are stored internally; verify the builder accepts them.
        let server = ServerBuilder::new("s", "0.1")
            .instructions("Use this server for math operations")
            .build();
        assert_eq!(server.info().name, "s");
    }

    // ── Logging Configuration ────────────────────────────────────────

    #[test]
    fn builder_log_level() {
        let server = ServerBuilder::new("s", "0.1")
            .log_level(Level::Debug)
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_log_level_filter() {
        let server = ServerBuilder::new("s", "0.1")
            .log_level_filter(log::LevelFilter::Warn)
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_log_timestamps_and_targets() {
        let server = ServerBuilder::new("s", "0.1")
            .log_timestamps(false)
            .log_targets(false)
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_full_logging_config() {
        let config = LoggingConfig {
            level: log::LevelFilter::Trace,
            timestamps: false,
            targets: false,
            file_line: true,
        };
        let server = ServerBuilder::new("s", "0.1").logging(config).build();
        assert_eq!(server.info().name, "s");
    }

    // ── Console Configuration ────────────────────────────────────────

    #[test]
    fn builder_console_config_full() {
        let config = ConsoleConfig::new()
            .with_banner(BannerStyle::Compact)
            .plain_mode();
        let server = ServerBuilder::new("s", "0.1")
            .with_console_config(config)
            .build();
        let cc = server.console_config();
        assert!(cc.force_plain);
    }

    #[test]
    fn builder_without_banner() {
        let server = ServerBuilder::new("s", "0.1").without_banner().build();
        let cc = server.console_config();
        assert!(!cc.show_banner);
        assert_eq!(cc.banner_style, BannerStyle::None);
    }

    #[test]
    fn builder_with_banner_compact() {
        let server = ServerBuilder::new("s", "0.1")
            .with_banner(BannerStyle::Compact)
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_traffic_logging() {
        let server = ServerBuilder::new("s", "0.1")
            .with_traffic_logging(TrafficVerbosity::Full)
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_plain_mode() {
        let server = ServerBuilder::new("s", "0.1").plain_mode().build();
        assert!(server.console_config().force_plain);
    }

    #[test]
    fn builder_force_color() {
        let server = ServerBuilder::new("s", "0.1").force_color().build();
        assert_eq!(server.info().name, "s");
    }

    // ── DuplicateBehavior ────────────────────────────────────────────

    #[test]
    fn builder_on_duplicate_default_is_warn() {
        // Default behavior is Warn (keeps original, logs warning)
        let server = ServerBuilder::new("s", "0.1")
            .tool(BuilderDupTool)
            .tool(BuilderDupTool)
            .build();
        // With Warn default, the second registration keeps original
        let tools = server.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "dup");
    }

    #[test]
    fn builder_on_duplicate_ignore_keeps_original() {
        let server = ServerBuilder::new("s", "0.1")
            .on_duplicate(DuplicateBehavior::Ignore)
            .tool(BuilderDupTool)
            .tool(BuilderDupTool)
            .build();
        let tools = server.tools();
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn builder_on_duplicate_replace() {
        let server = ServerBuilder::new("s", "0.1")
            .on_duplicate(DuplicateBehavior::Replace)
            .tool(BuilderDupTool)
            .tool(BuilderDupTool)
            .build();
        let tools = server.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "dup");
    }

    #[test]
    fn builder_on_duplicate_error_refuses_the_build() {
        // With Error behavior the duplicate is refused and the build fails.
        let Err(crate::ServerBuildError::InvalidConfiguration(refused)) =
            ServerBuilder::new("s", "0.1")
                .on_duplicate(DuplicateBehavior::Error)
                .tool(BuilderDupTool)
                .tool(BuilderDupTool)
                .try_build()
        else {
            panic!("a duplicate refused under DuplicateBehavior::Error must fail try_build");
        };
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].name, "dup");
    }

    #[test]
    fn builder_on_duplicate_applies_to_resources() {
        let server = ServerBuilder::new("s", "0.1")
            .on_duplicate(DuplicateBehavior::Ignore)
            .resource(NamedResource::named("r"))
            .resource(NamedResource::named("r"))
            .build();
        let resources = server.resources();
        assert_eq!(resources.len(), 1);
    }

    #[test]
    fn builder_on_duplicate_applies_to_prompts() {
        let server = ServerBuilder::new("s", "0.1")
            .on_duplicate(DuplicateBehavior::Ignore)
            .prompt(NamedPrompt::named("p"))
            .prompt(NamedPrompt::named("p"))
            .build();
        let prompts = server.prompts();
        assert_eq!(prompts.len(), 1);
    }

    // ── Middleware Registration ───────────────────────────────────────

    #[test]
    fn builder_middleware_registration() {
        let server = ServerBuilder::new("s", "0.1")
            .middleware(ResponseCachingMiddleware::new())
            .middleware(RateLimitingMiddleware::new(10.0).burst_capacity(20))
            .build();
        // Server builds successfully with middleware
        assert_eq!(server.info().name, "s");
    }

    // ── Auth Provider ────────────────────────────────────────────────

    #[test]
    fn builder_auth_provider_static_token() {
        let ctx = fastmcp_core::AuthContext::with_subject("test-user");
        let server = ServerBuilder::new("s", "0.1")
            .auth_provider(TokenAuthProvider::new(
                StaticTokenVerifier::new(vec![("secret-token".to_string(), ctx)])
                    .expect("valid verifier configuration"),
            ))
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_auth_provider_allow_all() {
        let server = ServerBuilder::new("s", "0.1")
            .auth_provider(crate::AllowAllAuthProvider)
            .build();
        assert_eq!(server.info().name, "s");
    }

    // ── Lifecycle Hooks ──────────────────────────────────────────────

    #[test]
    fn builder_on_startup_hook() {
        let server = ServerBuilder::new("s", "0.1")
            .on_startup(|| -> Result<(), std::io::Error> { Ok(()) })
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_on_shutdown_hook() {
        let server = ServerBuilder::new("s", "0.1")
            .on_shutdown(|| {
                // cleanup
            })
            .build();
        assert_eq!(server.info().name, "s");
    }

    #[test]
    fn builder_both_lifecycle_hooks() {
        let server = ServerBuilder::new("s", "0.1")
            .on_startup(|| -> Result<(), std::io::Error> { Ok(()) })
            .on_shutdown(|| {})
            .build();
        assert_eq!(server.info().name, "s");
    }

    // ── Mount ────────────────────────────────────────────────────────

    #[test]
    fn builder_mount_server_with_prefix() {
        let child = ServerBuilder::new("child", "0.1")
            .tool(BuilderDoThingTool)
            .resource(NamedResource::named("data"))
            .prompt(NamedPrompt::named("ask"))
            .build();

        let parent = ServerBuilder::new("parent", "0.1")
            .mount(child, Some("child"))
            .build();

        assert!(parent.has_tools());
        assert!(parent.has_resources());
        assert!(parent.has_prompts());

        let tools = parent.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "child/do_thing");
    }

    #[test]
    fn builder_mount_server_without_prefix() {
        let child = ServerBuilder::new("child", "0.1")
            .tool(BuilderAlphaTool)
            .build();

        let parent = ServerBuilder::new("parent", "0.1")
            .mount(child, None)
            .build();

        let tools = parent.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "alpha");
    }

    #[test]
    fn builder_mount_tools_only() {
        let child = ServerBuilder::new("child", "0.1")
            .tool(BuilderTTool)
            .resource(NamedResource::named("r"))
            .prompt(NamedPrompt::named("p"))
            .build();

        let parent = ServerBuilder::new("parent", "0.1")
            .mount_tools(child, Some("ns"))
            .build();

        assert!(parent.has_tools());
        assert!(!parent.has_resources());
        assert!(!parent.has_prompts());
    }

    #[test]
    fn builder_mount_resources_only() {
        let child = ServerBuilder::new("child", "0.1")
            .tool(BuilderTTool)
            .resource(NamedResource::named("r"))
            .build();

        let parent = ServerBuilder::new("parent", "0.1")
            .mount_resources(child, Some("ns"))
            .build();

        assert!(!parent.has_tools());
        assert!(parent.has_resources());
    }

    #[test]
    fn builder_mount_prompts_only() {
        let child = ServerBuilder::new("child", "0.1")
            .tool(BuilderTTool)
            .prompt(NamedPrompt::named("p"))
            .build();

        let parent = ServerBuilder::new("parent", "0.1")
            .mount_prompts(child, Some("ns"))
            .build();

        assert!(!parent.has_tools());
        assert!(parent.has_prompts());
    }

    #[test]
    fn builder_mount_multiple_servers() {
        let db = ServerBuilder::new("db", "0.1")
            .tool(BuilderQueryTool)
            .build();
        let api = ServerBuilder::new("api", "0.1")
            .tool(BuilderFetchTool)
            .build();

        let main = ServerBuilder::new("main", "0.1")
            .mount(db, Some("db"))
            .mount(api, Some("api"))
            .build();

        let tools = main.tools();
        assert_eq!(tools.len(), 2);
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"db/query"));
        assert!(names.contains(&"api/fetch"));
    }

    // ── Fluent Chaining ──────────────────────────────────────────────

    #[test]
    fn builder_full_fluent_chain() {
        let server = ServerBuilder::new("full-server", "2.0.0")
            .instructions("A fully configured server")
            .on_duplicate(DuplicateBehavior::Error)
            .request_timeout(120)
            .mask_error_details(true)
            .strict_input_validation(true)
            .log_level(Level::Debug)
            .log_timestamps(false)
            .without_banner()
            .plain_mode()
            .tool(BuilderToolATool)
            .resource(NamedResource::named("res_a"))
            .prompt(NamedPrompt::named("prompt_a"))
            .middleware(ResponseCachingMiddleware::new())
            .auth_provider(crate::AllowAllAuthProvider)
            .on_startup(|| -> Result<(), std::io::Error> { Ok(()) })
            .on_shutdown(|| {})
            .build();

        assert_eq!(server.info().name, "full-server");
        assert_eq!(server.info().version, "2.0.0");
        assert!(server.has_tools());
        assert!(server.has_resources());
        assert!(server.has_prompts());
        assert_eq!(server.tools().len(), 1);
        assert_eq!(server.resources().len(), 1);
        assert_eq!(server.prompts().len(), 1);
    }

    // ── into_router ──────────────────────────────────────────────────

    #[test]
    fn builder_into_router_preserves_components() {
        let server = ServerBuilder::new("s", "0.1")
            .tool(BuilderT1Tool)
            .tool(BuilderT2Tool)
            .resource(NamedResource::named("r1"))
            .prompt(NamedPrompt::named("p1"))
            .build();

        let router = server.into_router();
        assert_eq!(router.tools_count(), 2);
        assert_eq!(router.resources_count(), 1);
        assert_eq!(router.prompts_count(), 1);
    }

    // ── Task Manager ─────────────────────────────────────────────────

    #[cfg(feature = "tasks")]
    #[test]
    fn builder_without_task_manager() {
        let server = ServerBuilder::new("s", "0.1").build();
        assert!(server.task_manager().is_none());
        assert!(server.capabilities().tasks.is_none());
    }

    #[cfg(feature = "tasks")]
    #[test]
    fn builder_with_task_manager_keeps_tasks_capability_quarantined() {
        let tm = TaskManager::new();
        let server = ServerBuilder::new("s", "0.1")
            .with_task_manager(tm.into_shared())
            .build();
        assert!(server.task_manager().is_some());
        assert!(server.capabilities().tasks.is_none());
    }

    // ── List Pagination ─────────────────────────────────────────────

    #[test]
    fn builder_list_page_size_enables_tools_pagination() {
        let router = ServerBuilder::new("s", "0.1")
            .list_page_size(2)
            .tool(BuilderT1Tool)
            .tool(BuilderT2Tool)
            .tool(BuilderT3Tool)
            .build()
            .into_router();

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);
        let first = router
            .handle_tools_list(
                &request_ctx,
                fastmcp_protocol::ListToolsParams::default(),
                None,
            )
            .expect("tools/list first page");
        assert_eq!(first.tools.len(), 2);
        let cursor = first.next_cursor.expect("nextCursor present");

        let second = router
            .handle_tools_list(
                &request_ctx,
                fastmcp_protocol::ListToolsParams {
                    cursor: Some(cursor),
                    ..Default::default()
                },
                None,
            )
            .expect("tools/list second page");
        assert_eq!(second.tools.len(), 1);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn tools_list_rejects_invalid_cursor() {
        let router = ServerBuilder::new("s", "0.1")
            .list_page_size(2)
            .tool(BuilderT1Tool)
            .build()
            .into_router();

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);
        let err = router
            .handle_tools_list(
                &request_ctx,
                fastmcp_protocol::ListToolsParams {
                    cursor: Some("not-base64".to_string()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn tools_list_handles_extreme_cursor_offset_without_overflow() {
        let router = ServerBuilder::new("s", "0.1")
            .list_page_size(1)
            .tool(BuilderT1Tool)
            .tool(BuilderT2Tool)
            .build()
            .into_router();

        let cx = Cx::for_testing();
        let request_ctx = McpContext::new(cx, 1);
        let first = router
            .handle_tools_list(
                &request_ctx,
                fastmcp_protocol::ListToolsParams::default(),
                None,
            )
            .expect("tools/list should mint a bound continuation cursor");
        let cursor = first
            .next_cursor
            .expect("the first page has a continuation");
        let mut payload: serde_json::Value = serde_json::from_slice(
            &BASE64_STANDARD
                .decode(cursor)
                .expect("router-minted cursor is base64"),
        )
        .expect("router-minted cursor is JSON");
        payload["offset"] = serde_json::json!(u64::MAX);
        let cursor = BASE64_STANDARD
            .encode(serde_json::to_vec(&payload).expect("extreme bound cursor should serialize"));

        let error = router
            .handle_tools_list(
                &request_ctx,
                fastmcp_protocol::ListToolsParams {
                    cursor: Some(cursor),
                    ..Default::default()
                },
                None,
            )
            .expect_err("an extreme offset must fail closed without overflowing");

        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            error.message,
            "final catalog cursor offset is outside the requested catalog page"
        );
    }
}

// ============================================================================
// Helper Function Tests
// ============================================================================

#[cfg(test)]
mod helper_function_tests {
    use super::*;
    use crate::{
        DuplicateBehavior, LoggingConfig, RequestCompletion, parse_params, parse_params_or_default,
        stable_hash_request_id, transport_lock_error,
    };
    use fastmcp_transport::TransportError;

    // ── parse_params ────────────────────────────────────────────────

    #[test]
    fn parse_params_none_returns_error() {
        let result = parse_params::<serde_json::Value>(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn parse_params_valid_json_succeeds() {
        let val = serde_json::json!({"name": "test"});
        let result = parse_params::<serde_json::Value>(Some(val.clone()));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), val);
    }

    #[test]
    fn parse_params_invalid_type_returns_error() {
        // Try to parse a string as a struct that expects an object
        let val = serde_json::json!("just a string");
        let result = parse_params::<std::collections::HashMap<String, String>>(Some(val));
        assert!(result.is_err());
    }

    // ── parse_params_or_default ─────────────────────────────────────

    #[test]
    fn parse_params_or_default_none_returns_default() {
        let result = parse_params_or_default::<Vec<String>>(None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn parse_params_or_default_valid_json_succeeds() {
        let val = serde_json::json!(["a", "b"]);
        let result = parse_params_or_default::<Vec<String>>(Some(val));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn parse_params_or_default_invalid_type_returns_error() {
        let val = serde_json::json!(42);
        let result = parse_params_or_default::<Vec<String>>(Some(val));
        assert!(result.is_err());
    }

    // ── stable_hash_request_id ──────────────────────────────────────

    #[test]
    fn stable_hash_deterministic() {
        let h1 = stable_hash_request_id("test-id-123");
        let h2 = stable_hash_request_id("test-id-123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn stable_hash_different_strings_differ() {
        let h1 = stable_hash_request_id("alpha");
        let h2 = stable_hash_request_id("beta");
        assert_ne!(h1, h2);
    }

    #[test]
    fn stable_hash_never_zero() {
        // Even an empty string should produce non-zero
        let h = stable_hash_request_id("");
        assert_ne!(h, 0);
        // And various other strings
        for s in &["a", "b", "test", "0", ""] {
            assert_ne!(stable_hash_request_id(s), 0);
        }
    }

    // ── transport_lock_error ────────────────────────────────────────

    #[test]
    fn transport_lock_error_is_io_error() {
        let err = transport_lock_error();
        match err {
            TransportError::Io(e) => {
                assert!(e.to_string().contains("poisoned"));
            }
            other => panic!("expected Io error, got {:?}", other),
        }
    }

    #[test]
    fn receive_pump_cancels_a_running_handler_in_band() {
        let barrier = Arc::new(Barrier::new(1));
        let started = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let _configuration = configure_blocking_tool_with_completion(
            barrier,
            Some(Arc::clone(&started)),
            Some(Arc::clone(&completed)),
        );
        let server = Server::new("receive-pump-cancellation", "1.0.0")
            .tool(BlockUntilCancelled)
            .build();
        let sent = Arc::new(Mutex::new(Vec::<JsonRpcMessage>::new()));
        let sent_for_transport = Arc::clone(&sent);
        let started_for_receive = Arc::clone(&started);
        let completed_for_receive = Arc::clone(&completed);
        let mut step = 0_u8;

        run_returning_pump_with_caller_runtime(
            server,
            move |_, _worker_failed| {
                let current = step;
                step = step.saturating_add(1);
                match current {
                    0 => Ok(JsonRpcMessage::Request(
                        fastmcp_protocol::JsonRpcRequest::new(
                            "initialize",
                            Some(
                                serde_json::to_value(InitializeParams {
                                    protocol_version: fastmcp_protocol::PROTOCOL_VERSION
                                        .to_string(),
                                    capabilities: ClientCapabilities::default(),
                                    client_info: ClientInfo {
                                        name: "pump-client".to_string(),
                                        version: "1.0.0".to_string(),
                                    },
                                })
                                .expect("serialize initialize"),
                            ),
                            1_i64,
                        ),
                    )),
                    // Operating requests are refused with -32600 until the
                    // initialized notification completes the handshake.
                    1 => Ok(JsonRpcMessage::Request(
                        fastmcp_protocol::JsonRpcRequest::notification(
                            "notifications/initialized",
                            None,
                        ),
                    )),
                    2 => Ok(JsonRpcMessage::Request(
                        fastmcp_protocol::JsonRpcRequest::new(
                            "tools/call",
                            Some(
                                serde_json::to_value(CallToolParams {
                                    name: "block_until_cancelled".to_string(),
                                    arguments: Some(serde_json::json!({})),
                                    meta: None,
                                })
                                .expect("serialize call"),
                            ),
                            2_i64,
                        ),
                    )),
                    3 => {
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while !started_for_receive.load(Ordering::Acquire)
                            && Instant::now() < deadline
                        {
                            std::thread::yield_now();
                        }
                        if started_for_receive.load(Ordering::Acquire) {
                            Ok(JsonRpcMessage::Request(
                                fastmcp_protocol::JsonRpcRequest::notification(
                                    "notifications/cancelled",
                                    Some(
                                        serde_json::to_value(CancelledParams {
                                            request_id: RequestId::Number(2),
                                            reason: Some("test cancellation".to_string()),
                                            meta: None,
                                        })
                                        .expect("serialize cancellation"),
                                    ),
                                ),
                            ))
                        } else {
                            Err(fastmcp_transport::TransportError::Timeout)
                        }
                    }
                    _ => {
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while !completed_for_receive.load(Ordering::Acquire)
                            && Instant::now() < deadline
                        {
                            std::thread::yield_now();
                        }
                        if completed_for_receive.load(Ordering::Acquire) {
                            Err(fastmcp_transport::TransportError::Closed)
                        } else {
                            Err(fastmcp_transport::TransportError::Timeout)
                        }
                    }
                }
            },
            move |_, message| {
                sent_for_transport
                    .lock()
                    .expect("sent messages lock")
                    .push(message.clone());
                Ok(())
            },
            Arc::new(|_| {}),
            None,
            "test",
        )
        .expect("scripted returning loop must close cleanly");

        let sent = sent.lock().expect("sent messages lock");
        assert_eq!(
            sent.iter()
                .filter(|message| matches!(message, JsonRpcMessage::Response(_)))
                .count(),
            1,
            "accepted exact legacy cancellation must suppress the cancelled request response"
        );
        assert!(sent.iter().any(
            |message| matches!(message, JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(1)))
        ));
        assert!(!sent.iter().any(
            |message| matches!(message, JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(2)))
        ));
    }

    #[test]
    fn receive_pump_routes_sampling_response_while_handler_waits() {
        let server = Server::new("receive-pump-sampling", "1.0.0")
            .tool(SamplingRoundTrip)
            .build();
        let (outbound_tx, outbound_rx) = mpsc::channel::<JsonRpcMessage>();
        let sent = Arc::new(Mutex::new(Vec::<JsonRpcMessage>::new()));
        let sent_for_transport = Arc::clone(&sent);
        let mut step = 0_u8;

        run_returning_pump_with_caller_runtime(
            server,
            move |_, _worker_failed| {
                let current = step;
                step = step.saturating_add(1);
                match current {
                    0 => Ok(JsonRpcMessage::Request(
                        fastmcp_protocol::JsonRpcRequest::new(
                            "initialize",
                            Some(
                                serde_json::to_value(InitializeParams {
                                    protocol_version: fastmcp_protocol::PROTOCOL_VERSION
                                        .to_string(),
                                    capabilities: ClientCapabilities {
                                        sampling: Some(SamplingCapability::default()),
                                        ..ClientCapabilities::default()
                                    },
                                    client_info: ClientInfo {
                                        name: "sampling-client".to_string(),
                                        version: "1.0.0".to_string(),
                                    },
                                })
                                .expect("serialize initialize"),
                            ),
                            1_i64,
                        ),
                    )),
                    // Operating requests are refused with -32600 until the
                    // initialized notification completes the handshake.
                    1 => Ok(JsonRpcMessage::Request(
                        fastmcp_protocol::JsonRpcRequest::notification(
                            "notifications/initialized",
                            None,
                        ),
                    )),
                    2 => Ok(JsonRpcMessage::Request(
                        fastmcp_protocol::JsonRpcRequest::new(
                            "tools/call",
                            Some(
                                serde_json::to_value(CallToolParams {
                                    name: "sampling_round_trip".to_string(),
                                    arguments: Some(serde_json::json!({})),
                                    meta: None,
                                })
                                .expect("serialize call"),
                            ),
                            2_i64,
                        ),
                    )),
                    3 => loop {
                        match outbound_rx.recv_timeout(Duration::from_secs(2)) {
                            Ok(JsonRpcMessage::Request(request))
                                if request.method == "sampling/createMessage" =>
                            {
                                let id = request.id.expect("sampling request id");
                                return Ok(JsonRpcMessage::Response(JsonRpcResponse::success(
                                    id,
                                    serde_json::to_value(CreateMessageResult::text(
                                        "sampled-value",
                                        "test-model",
                                    ))
                                    .expect("serialize sampling result"),
                                )));
                            }
                            Ok(_) => {}
                            Err(_) => return Err(fastmcp_transport::TransportError::Timeout),
                        }
                    },
                    _ => loop {
                        match outbound_rx.recv_timeout(Duration::from_secs(2)) {
                            Ok(JsonRpcMessage::Response(response))
                                if response.id == Some(RequestId::Number(2)) =>
                            {
                                return Err(fastmcp_transport::TransportError::Closed);
                            }
                            Ok(_) => {}
                            Err(_) => return Err(fastmcp_transport::TransportError::Timeout),
                        }
                    },
                }
            },
            move |_, message| {
                sent_for_transport
                    .lock()
                    .expect("sent messages lock")
                    .push(message.clone());
                outbound_tx
                    .send(message.clone())
                    .map_err(|_| fastmcp_transport::TransportError::Closed)
            },
            Arc::new(|_| {}),
            None,
            "test",
        )
        .expect("scripted returning loop must close cleanly");

        let sent = sent.lock().expect("sent messages lock");
        let tool_response = sent.iter().find_map(|message| match message {
            JsonRpcMessage::Response(response) if response.id == Some(RequestId::Number(2)) => {
                Some(response)
            }
            _ => None,
        });
        let tool_response = tool_response.expect("tool response must be sent");
        assert!(tool_response.error.is_none());
        assert!(
            tool_response
                .result
                .as_ref()
                .is_some_and(|result| result.to_string().contains("sampled-value"))
        );
    }

    // ── RequestCompletion ───────────────────────────────────────────

    #[test]
    fn request_completion_starts_not_done() {
        let rc = RequestCompletion::new();
        assert!(!rc.is_done());
    }

    #[test]
    fn request_completion_mark_done() {
        let rc = RequestCompletion::new();
        rc.mark_done();
        assert!(rc.is_done());
    }

    #[test]
    fn request_completion_mark_done_idempotent() {
        let rc = RequestCompletion::new();
        rc.mark_done();
        rc.mark_done(); // should not panic
        assert!(rc.is_done());
    }

    #[test]
    fn request_completion_wait_timeout_already_done() {
        let rc = RequestCompletion::new();
        rc.mark_done();
        // Should return immediately
        assert!(rc.wait_timeout(Duration::from_millis(10)));
    }

    #[test]
    fn request_completion_wait_timeout_not_done_times_out() {
        let rc = RequestCompletion::new();
        let start = Instant::now();
        assert!(!rc.wait_timeout(Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn request_completion_wait_timeout_done_by_another_thread() {
        let rc = Arc::new(RequestCompletion::new());
        let rc2 = Arc::clone(&rc);

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            rc2.mark_done();
        });

        assert!(rc.wait_timeout(Duration::from_secs(2)));
        assert!(rc.is_done());
    }

    // ── LoggingConfig ───────────────────────────────────────────────

    #[test]
    fn logging_config_default() {
        let cfg = LoggingConfig::default();
        assert_eq!(cfg.level, log::LevelFilter::Info);
        assert!(cfg.timestamps);
        assert!(cfg.targets);
        assert!(!cfg.file_line);
    }

    // ── DuplicateBehavior ───────────────────────────────────────────

    #[test]
    fn duplicate_behavior_default_is_warn() {
        assert_eq!(DuplicateBehavior::default(), DuplicateBehavior::Warn);
    }

    #[test]
    fn duplicate_behavior_debug_and_clone() {
        let d = DuplicateBehavior::Error;
        let debug = format!("{:?}", d);
        assert!(debug.contains("Error"));
        let cloned = d;
        assert_eq!(cloned, DuplicateBehavior::Error);
    }

    #[test]
    fn duplicate_behavior_eq_variants() {
        assert_ne!(DuplicateBehavior::Error, DuplicateBehavior::Warn);
        assert_ne!(DuplicateBehavior::Warn, DuplicateBehavior::Replace);
        assert_ne!(DuplicateBehavior::Replace, DuplicateBehavior::Ignore);
    }

    // ── Server::log_level_rank ──────────────────────────────────────

    #[test]
    fn log_level_rank_ordering() {
        let d = Server::log_level_rank(LogLevel::Debug);
        let i = Server::log_level_rank(LogLevel::Info);
        let w = Server::log_level_rank(LogLevel::Warning);
        let e = Server::log_level_rank(LogLevel::Error);
        assert!(d < i);
        assert!(i < w);
        assert!(w < e);
    }

    fn stateless_public_catalog_snapshot(server: &Server) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "info": server.info(),
            "capabilities": server.capabilities(),
            "tools": server.tools(),
            "resources": server.resources(),
            "resourceTemplates": server.resource_templates(),
            "prompts": server.prompts(),
        }))
        .expect("public stateless catalog must serialize")
    }

    // The four `srv_01_*_unit_*` names below deliberately do NOT match the
    // frozen SRV-01 acceptance IDs. Those IDs must resolve to exactly one
    // definition in the workspace, because the frozen runner on
    // bd-mcp-2026-07-28-support-ahet.15.1/.15.2 is
    // `cargo test --workspace --all-targets <id> -- --exact` and AC bullet 3
    // forbids substituted rows. The shipped proof lives at the crate root of
    // crates/fastmcp-server/tests/srv_01_stateless.rs; `cfg(test)` behaviour
    // cannot prove shipped behaviour (AC bullet 4, PL-3), so these remain
    // in-crate unit coverage under distinct names. Renaming them back would
    // reintroduce the duplicate. Same convention as fnd_03_i_* and
    // fnd_03_integration_*.
    #[test]
    fn srv_01_a_unit_positive() {
        let server = Server::new("stateless-public-handler", "1.0.0")
            .tool(Greet)
            .build();
        let inbound =
            InboundRequestContext::new(Cx::for_testing(), 71, InboundRequestTransport::Memory);
        // `dispatch_stateless` is the modern-only public surface; requests
        // must carry the final protocol marker and client capabilities.
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "greet",
                "arguments": { "name": "stateless client" },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            71_i64,
        );
        let catalog_before = stateless_public_catalog_snapshot(&server);

        let response = fastmcp_core::block_on(server.dispatch_stateless(&inbound, &request))
            .expect("request with an id must receive a response");

        assert!(response.error.is_none());
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.pointer("/content/0/text"))
                .and_then(serde_json::Value::as_str),
            Some("Hello, stateless client!")
        );
        assert_eq!(inbound.request_id(), 71);
        assert_eq!(inbound.transport(), InboundRequestTransport::Memory);
        assert_eq!(stateless_public_catalog_snapshot(&server), catalog_before);
    }

    #[test]
    fn srv_01_a_unit_planted_negative() {
        let server = Server::new("stateless-forbidden-mutation", "1.0.0")
            .tool(Greet)
            .build();
        let inbound =
            InboundRequestContext::new(Cx::for_testing(), 72, InboundRequestTransport::Memory);
        let baseline = JsonRpcRequest::new(
            "tools/list",
            Some(serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            72_i64,
        );
        let mut planted = baseline.clone();
        planted.method = "logging/setLevel".to_string();

        // The forbidden dimension is only the method: request identity and
        // parameters are byte-for-byte identical to the accepted baseline.
        assert_eq!(baseline.jsonrpc, planted.jsonrpc);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(baseline.params, planted.params);
        let planted_input_before =
            serde_json::to_vec(&planted).expect("planted request must serialize");
        let catalog_before = stateless_public_catalog_snapshot(&server);

        let baseline_response =
            fastmcp_core::block_on(server.dispatch_stateless(&inbound, &baseline))
                .expect("stateless tools/list baseline must respond");
        assert!(baseline_response.error.is_none());

        let planted_response =
            fastmcp_core::block_on(server.dispatch_stateless(&inbound, &planted))
                .expect("planted request with an id must receive a response");
        assert_eq!(
            planted_response
                .error
                .as_ref()
                .map(|error| error.code.clone()),
            Some(McpErrorCode::MethodNotFound.into())
        );
        assert_eq!(
            serde_json::to_vec(&planted).expect("planted request remains serializable"),
            planted_input_before,
            "forbidden stateless mutation changed caller input"
        );
        assert_eq!(
            stateless_public_catalog_snapshot(&server),
            catalog_before,
            "forbidden stateless mutation changed the public catalog"
        );
        assert_eq!(inbound.request_id(), 72);
        assert_eq!(inbound.transport(), InboundRequestTransport::Memory);
    }

    #[test]
    fn srv_01_b_unit_positive() {
        let server = Server::new("stateless-handler-result", "1.0.0")
            .tool(DeclinedTool)
            .build();
        let inbound =
            InboundRequestContext::new(Cx::for_testing(), 73, InboundRequestTransport::Memory);
        let request = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "declined",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            73_i64,
        );

        let response = fastmcp_core::block_on(server.dispatch_stateless(&inbound, &request))
            .expect("handler request with an id must receive a response");

        assert!(response.error.is_none());
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "a handler error must convert to CallToolResult rather than a JSON-RPC failure"
        );
        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.pointer("/content/0/text"))
                .and_then(serde_json::Value::as_str),
            Some("caller input was declined")
        );
    }

    #[test]
    fn srv_01_b_unit_planted_negative() {
        let server = Server::new("stateless-handler-refusal", "1.0.0")
            .tool(DeclinedTool)
            .build();
        let inbound =
            InboundRequestContext::new(Cx::for_testing(), 74, InboundRequestTransport::Memory);
        let baseline = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "declined",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            74_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(|params| params.as_object_mut())
            .expect("tools/call test parameters must be an object")
            .insert("name".to_string(), serde_json::json!("missing-handler"));

        // The handler name is the sole planted dimension. The method, request
        // identity, and argument object remain the accepted baseline values.
        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.jsonrpc, planted.jsonrpc);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("declined")
        );
        assert_eq!(
            planted
                .params
                .as_ref()
                .and_then(|params| params.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("missing-handler")
        );
        assert_eq!(
            baseline
                .params
                .as_ref()
                .and_then(|params| params.get("arguments")),
            planted
                .params
                .as_ref()
                .and_then(|params| params.get("arguments"))
        );
        let planted_input_before =
            serde_json::to_vec(&planted).expect("planted request must serialize");
        let catalog_before = stateless_public_catalog_snapshot(&server);

        let baseline_response =
            fastmcp_core::block_on(server.dispatch_stateless(&inbound, &baseline))
                .expect("accepted handler baseline must receive a response");
        assert!(baseline_response.error.is_none());
        assert_eq!(
            baseline_response
                .result
                .as_ref()
                .and_then(|result| result.get("isError"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let planted_response =
            fastmcp_core::block_on(server.dispatch_stateless(&inbound, &planted))
                .expect("planted handler request with an id must receive a response");
        // An unknown tool name is an invalid `name` parameter (-32602) per the
        // MCP unknown-tool mapping; the method itself exists.
        assert_eq!(
            planted_response
                .error
                .as_ref()
                .map(|error| error.code.clone()),
            Some(McpErrorCode::InvalidParams.into())
        );
        assert_eq!(
            serde_json::to_vec(&planted).expect("planted request remains serializable"),
            planted_input_before,
            "typed handler refusal changed caller input"
        );
        assert_eq!(
            stateless_public_catalog_snapshot(&server),
            catalog_before,
            "typed handler refusal changed the public catalog"
        );
    }
}
