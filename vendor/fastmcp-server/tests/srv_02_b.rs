//! Frozen SRV-02 B public server-dispatch harnesses.
//!
//! The root test IDs are intentionally unqualified so the frozen exact runner
//! discovers and starts each one through the shipped server surface.

#![recursion_limit = "256"]

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult};
use fastmcp_derive::tool;
#[cfg(not(feature = "legacy-2024-11-05"))]
use fastmcp_protocol::JsonRpcMessage;
use fastmcp_protocol::protocol_policy::MODERN_PROTOCOL_VERSION;
use fastmcp_protocol::protocol_policy::ProtocolPolicy;
use fastmcp_protocol::{
    CompletionValues, Content, FinalCompletionParams, FinalCompletionValues,
    LegacyCompletionParams, Prompt, PromptMessage, Role,
};
use fastmcp_protocol::{
    FINAL_CLIENT_CAPABILITIES_META_KEY, FINAL_PROTOCOL_VERSION_META_KEY, SERVER_DISCOVER_METHOD,
};
use fastmcp_protocol::{JsonRpcRequest, MAX_SERVER_INSTRUCTIONS_BYTES, ServerDiscoverResult};
use fastmcp_server::ServerHttpEndpointResponse;
use fastmcp_server::{
    CompletionHandler, FinalToolOutcome, InboundRequestContext, InboundRequestTransport,
    PromptHandler, Server, ToolHandler,
};
#[cfg(not(feature = "legacy-2024-11-05"))]
use fastmcp_server::{HttpServerConfig, ServerHttpEndpointError};
use fastmcp_transport::http::HttpStatus;
use fastmcp_transport::http::{HttpMethod, HttpRequest};
#[cfg(not(feature = "legacy-2024-11-05"))]
use fastmcp_transport::{Transport, TransportError};
use serde_json::json;
#[cfg(not(feature = "legacy-2024-11-05"))]
use std::collections::VecDeque;
use std::sync::Arc;
#[cfg(not(feature = "legacy-2024-11-05"))]
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(not(feature = "legacy-2024-11-05"))]
#[derive(Default)]
struct FeatureOffTransportState {
    incoming: VecDeque<JsonRpcMessage>,
    outgoing: Vec<JsonRpcMessage>,
    recv_calls: usize,
    close_calls: usize,
}

/// A one-frame ordinary public transport, deliberately not a test-only server
/// adapter. The integration crate links the server as a production dependency.
#[cfg(not(feature = "legacy-2024-11-05"))]
struct FeatureOffTransport {
    state: Arc<Mutex<FeatureOffTransportState>>,
}

#[cfg(not(feature = "legacy-2024-11-05"))]
impl FeatureOffTransport {
    fn single_request(request: JsonRpcRequest) -> (Self, Arc<Mutex<FeatureOffTransportState>>) {
        let state = Arc::new(Mutex::new(FeatureOffTransportState {
            incoming: VecDeque::from([JsonRpcMessage::Request(request)]),
            ..FeatureOffTransportState::default()
        }));
        (
            Self {
                state: Arc::clone(&state),
            },
            state,
        )
    }
}

#[cfg(not(feature = "legacy-2024-11-05"))]
impl Transport for FeatureOffTransport {
    fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        let mut state = self
            .state
            .lock()
            .expect("feature-off transport mutex must not be poisoned");
        state.outgoing.push(message.clone());
        Ok(())
    }

    fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        let mut state = self
            .state
            .lock()
            .expect("feature-off transport mutex must not be poisoned");
        state.recv_calls += 1;
        state.incoming.pop_front().ok_or(TransportError::Closed)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.state
            .lock()
            .expect("feature-off transport mutex must not be poisoned")
            .close_calls += 1;
        Ok(())
    }
}

#[tool(name = "discoverable", description = "SRV-02 catalog fixture")]
fn discoverable(ctx: &McpContext) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok("available".to_owned())
}

#[tool(
    name = "runtime-child",
    description = "Returns caller-owned child output"
)]
fn caller_child_echo(ctx: &McpContext, value: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(value)
}

struct CallerRuntimeTool {
    caller_thread: std::thread::ThreadId,
    entered: Arc<AtomicUsize>,
    children_completed: Arc<AtomicUsize>,
}

impl ToolHandler for CallerRuntimeTool {
    fn definition(&self) -> fastmcp_protocol::Tool {
        CallerChildEcho.definition()
    }

    fn execution_mode(&self) -> fastmcp_server::ToolExecutionMode {
        fastmcp_server::ToolExecutionMode::Async
    }

    fn call(
        &self,
        ctx: &McpContext,
        arguments: serde_json::Value,
    ) -> McpResult<Vec<fastmcp_protocol::Content>> {
        CallerChildEcho.call(ctx, arguments)
    }

    fn call_final_outcome_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = McpOutcome<FinalToolOutcome>> + Send + 'a>,
    > {
        Box::pin(async move {
            eprintln!(
                "HTTP handler thread: {:?}; caller: {:?}",
                std::thread::current().id(),
                self.caller_thread
            );
            assert_eq!(std::thread::current().id(), self.caller_thread);
            let ambient = Cx::current().expect("the caller runtime supplies the handler context");
            eprintln!(
                "HTTP handler ambient: {:?}/{:?}; explicit: {:?}/{:?}",
                ambient.task_id(),
                ambient.region_id(),
                ctx.cx().task_id(),
                ctx.cx().region_id()
            );
            assert_eq!(ambient.task_id(), ctx.cx().task_id());
            assert_eq!(ambient.region_id(), ctx.cx().region_id());
            ctx.checkpoint().expect("positive handler is live");
            self.entered.fetch_add(1, Ordering::SeqCst);
            asupersync::runtime::yield_now().await;

            let value = arguments["value"]
                .as_str()
                .expect("the real tool schema validates the string argument")
                .to_owned();
            let caller_thread = self.caller_thread;
            let handler_task = ctx.cx().task_id();
            let handler_region = ctx.cx().region_id();
            let completed = Arc::clone(&self.children_completed);
            let mut child = ctx
                .cx()
                .spawn(move |child_cx| async move {
                    eprintln!(
                        "HTTP child thread: {:?}; task/region: {:?}/{:?}; handler: {:?}/{:?}",
                        std::thread::current().id(),
                        child_cx.task_id(),
                        child_cx.region_id(),
                        handler_task,
                        handler_region
                    );
                    assert_eq!(std::thread::current().id(), caller_thread);
                    assert_ne!(child_cx.task_id(), handler_task);
                    assert_eq!(child_cx.region_id(), handler_region);
                    child_cx.checkpoint().expect("the owned child is live");
                    asupersync::runtime::yield_now().await;
                    completed.fetch_add(1, Ordering::SeqCst);
                    format!("owned:{value}")
                })
                .expect("the provided handler context admits its owned child");
            let value = child
                .join(ctx.cx())
                .await
                .expect("the caller runtime drives and joins the owned child");
            assert_eq!(std::thread::current().id(), self.caller_thread);
            McpOutcome::Ok(FinalToolOutcome::Complete(
                CallerChildEcho
                    .call_final(ctx, json!({"value": value}))
                    .expect("the joined child output is a valid final result"),
            ))
        })
    }
}

/// Moves the prompts axis of the capability matrix and nothing else.
struct MatrixPrompt;

impl PromptHandler for MatrixPrompt {
    fn definition(&self) -> Prompt {
        Prompt {
            name: "matrix_prompt".to_owned(),
            description: Some("SRV-02 capability-matrix fixture".to_owned()),
            arguments: Vec::new(),
            icon: None,
            version: None,
            tags: Vec::new(),
        }
    }

    /// Never dispatched by this test: the matrix exercises `prompts/list`, not
    /// `prompts/get`. Returning one message rather than an error keeps an
    /// accidental future call legible instead of turning it into a refusal.
    fn get(
        &self,
        _ctx: &McpContext,
        _arguments: std::collections::HashMap<String, String>,
    ) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage {
            role: Role::User,
            content: Content::text("SRV-02 capability-matrix fixture"),
        }])
    }
}

/// Moves the completions axis. Every `CompletionHandler` method has a default,
/// so installing this is the smallest thing that can flip
/// `has_final_completion_handler` and therefore the advertised capability.
struct MatrixCompletions;

impl CompletionHandler for MatrixCompletions {
    /// Neither method is dispatched by this test — the completions axis is
    /// advertisement-variance only, for the reason recorded at its assertions.
    /// Both return an empty value set rather than an error so that an
    /// accidental future call is legible instead of looking like a refusal.
    fn complete_legacy(
        &self,
        _ctx: &McpContext,
        _params: LegacyCompletionParams,
    ) -> Result<CompletionValues, McpError> {
        Ok(CompletionValues {
            values: Vec::new(),
            total: None,
            has_more: None,
        })
    }

    fn complete_final(
        &self,
        _ctx: &McpContext,
        _params: FinalCompletionParams,
    ) -> Result<FinalCompletionValues, McpError> {
        Ok(FinalCompletionValues {
            values: Vec::new(),
            total: None,
            has_more: None,
        })
    }
}

fn public_catalog_snapshot(server: &Server) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "info": server.info(),
        "capabilities": server.capabilities(),
        "tools": server.tools(),
        "resources": server.resources(),
        "resourceTemplates": server.resource_templates(),
        "prompts": server.prompts(),
    }))
    .expect("public catalog snapshot must serialize")
}

/// Runs a test body on a caller-owned runtime, so dispatch awaits on its Cx.
fn on_caller_runtime(body: impl Future<Output = ()>) {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("the caller runtime builds")
        .block_on(body);
}

fn caller_cx() -> Cx {
    Cx::current().expect("the caller runtime installs its Cx")
}

#[test]
fn srv_02_b_positive() {
    on_caller_runtime(async {
        let server = Server::new("discoverable-server", "1.0.0")
            .instructions("")
            .tool(Discoverable)
            .build();
        let stdio = InboundRequestContext::new(caller_cx(), 401, InboundRequestTransport::Stdio);
        let discover = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            401_i64,
        );

        let first_response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &stdio, &discover)
            .await
            .expect("first discovery request has an id");
        let first_result = first_response
            .result
            .as_ref()
            .expect("first discovery request succeeds");

        assert_eq!(
            first_result["supportedVersions"],
            json!([MODERN_PROTOCOL_VERSION])
        );
        assert_eq!(
            first_result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            json!("discoverable-server")
        );
        assert_eq!(first_result["instructions"], json!(""));
        assert_eq!(first_result["ttlMs"], json!(60_000));
        assert_eq!(first_result["cacheScope"], json!("private"));
        assert!(first_result["capabilities"].get("tools").is_some());
        assert!(first_result["capabilities"].get("logging").is_none());
        assert!(first_result["capabilities"].get("completions").is_none());
        assert!(first_result["capabilities"].get("resources").is_none());
        assert!(first_result["capabilities"].get("prompts").is_none());
        assert!(first_result["capabilities"].get("subscriptions").is_none());
        assert!(first_result.get("extensions").is_none());

        // "Capabilities exactly match enabled behavior" and "discovery reflects
        // runtime policy, not compile-time possibility" are claims about AGREEMENT
        // between two independent derivations from registration: the behavior
        // registry that builds `capabilities` (router.rs server_discovery_behavior_registry,
        // consulted in exactly one production place to author the advertisement)
        // and the dispatch table, which never reads that registry. Asserting the
        // advertisement alone, as the checks above do, cannot fail interestingly —
        // it restates the derivation instead of testing it.
        //
        // This is the load-bearing proof: two servers from the same binary with
        // the same name and the same instructions, differing ONLY in whether a
        // tool was registered. A hard-coded or compile-time-derived capability set
        // advertises identically for both and fails here.
        let toolless = Server::new("discoverable-server", "1.0.0")
            .instructions("")
            .build();
        let toolless_inbound =
            InboundRequestContext::new(caller_cx(), 407, InboundRequestTransport::Stdio);
        let toolless_discover = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            407_i64,
        );
        let toolless_result = toolless
            .dispatch_with_protocol_policy(
                ProtocolPolicy::ModernOnly,
                &toolless_inbound,
                &toolless_discover,
            )
            .await
            .expect("tool-less discovery request has an id")
            .result
            .expect("tool-less discovery succeeds");
        assert!(
            toolless_result["capabilities"].get("tools").is_none(),
            "an unregistered behavior must be absent from the advertisement, not \
         encoded as a placeholder: {}",
            toolless_result["capabilities"]
        );
        assert_ne!(
            toolless_result["capabilities"], first_result["capabilities"],
            "the advertised capabilities did not follow runtime registration"
        );

        // Corroborating direction: advertised => dispatchable. The registry admits
        // a tool only when it carries a final registration, while dispatch reads
        // the catalog, so the two can disagree. The server that advertised `tools`
        // must actually serve tools/list with that exact tool, and the one that did
        // not advertise it must not serve it either.
        // `u32` so the one request identity converts losslessly to both the
        // sanitized ingress `u64` and the JSON-RPC `i64` without a fallible cast.
        let listed_tool_names = async |server: &Server, id: u32| -> Vec<String> {
            let inbound = InboundRequestContext::new(
                caller_cx(),
                u64::from(id),
                InboundRequestTransport::Stdio,
            );
            let request = JsonRpcRequest::new(
                "tools/list",
                Some(json!({
                    "_meta": {
                        FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                        FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                    },
                })),
                i64::from(id),
            );
            let response = server
                .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &inbound, &request)
                .await
                .expect("tools/list request has an id");
            assert!(
                response.error.is_none(),
                "discovery advertised tools but tools/list refused: {:?}",
                response.error
            );
            response
                .result
                .as_ref()
                .and_then(|result| result.get("tools"))
                .and_then(serde_json::Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool.get("name"))
                        .filter_map(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let advertised_names = listed_tool_names(&server, 408).await;
        assert!(
            advertised_names.iter().any(|name| name == "discoverable"),
            "discovery advertised tools but tools/list omitted the registered tool: \
         {advertised_names:?}"
        );
        let unadvertised_names = listed_tool_names(&toolless, 409).await;
        assert!(
            !unadvertised_names.iter().any(|name| name == "discoverable"),
            "a server that never advertised tools dispatched one anyway: \
         {unadvertised_names:?}"
        );

        // FULL CAPABILITY-TO-BEHAVIOR MATRIX. The tools axis above established the
        // principle; the plan's acceptance names completions and prompts
        // explicitly, and each is a SEPARATE independent derivation inside
        // server_discovery_behavior_registry, so proving one says nothing about the
        // others.
        //
        // This belongs at the PUBLIC surface specifically. The router's own
        // cfg(test) suite already asserts "discovery advertises completion only
        // after the handler is installed" (crates/fastmcp-server/src/router.rs:17437,
        // inside the cfg(test) region that begins at :9153). Under PL-3 a cfg(test)
        // assertion cannot prove shipped behavior, which is exactly why the same
        // property is re-established here through the public builder and the public
        // discovery surface.
        //
        // `first_result` above is the baseline: the SAME name, the SAME instructions
        // and the SAME registered tool. Only the prompt and completion handler are
        // added, so each axis below varies alone against it.
        let matrixed = Server::new("discoverable-server", "1.0.0")
            .instructions("")
            .tool(Discoverable)
            .prompt(MatrixPrompt)
            .completion_handler(MatrixCompletions)
            .build();

        // ServerBuilder only logs a rejected handler, so confirm registration
        // really happened. Otherwise every assertion below passes vacuously.
        let registered_prompts: Vec<String> = matrixed
            .prompts()
            .into_iter()
            .map(|prompt| prompt.name)
            .collect();
        assert!(
            registered_prompts
                .iter()
                .any(|name| name == "matrix_prompt"),
            "the matrix prompt was not registered: {registered_prompts:?}"
        );

        let matrix_inbound =
            InboundRequestContext::new(caller_cx(), 410, InboundRequestTransport::Stdio);
        let matrix_discover = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            410_i64,
        );
        let matrix_result = matrixed
            .dispatch_with_protocol_policy(
                ProtocolPolicy::ModernOnly,
                &matrix_inbound,
                &matrix_discover,
            )
            .await
            .expect("matrix discovery request has an id")
            .result
            .expect("matrix discovery succeeds");
        let matrix_capabilities = &matrix_result["capabilities"];

        // Prompts axis. Registering a prompt must both add the parent capability
        // and its exact listChanged sub-capability, and the tool-only baseline must
        // still omit it entirely rather than carry a false placeholder.
        assert_eq!(
            matrix_capabilities["prompts"]["listChanged"],
            json!(true),
            "a registered prompt must advertise its exact listChanged sub-capability"
        );
        assert!(
            first_result["capabilities"].get("prompts").is_none(),
            "the tool-only baseline must omit prompts entirely: {}",
            first_result["capabilities"]
        );

        // Completions axis, same contrast against the same baseline.
        assert!(
            matrix_capabilities.get("completions").is_some(),
            "installing a completion handler must advertise completions: {matrix_capabilities}"
        );
        assert!(
            first_result["capabilities"].get("completions").is_none(),
            "the baseline without a completion handler must omit completions"
        );

        // Advertised implies dispatchable, prompts edge: the advertisement above
        // claimed prompts, so prompts/list must actually serve that exact prompt.
        let prompts_inbound =
            InboundRequestContext::new(caller_cx(), 411, InboundRequestTransport::Stdio);
        let prompts_request = JsonRpcRequest::new(
            "prompts/list",
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            411_i64,
        );
        let prompts_response = matrixed
            .dispatch_with_protocol_policy(
                ProtocolPolicy::ModernOnly,
                &prompts_inbound,
                &prompts_request,
            )
            .await
            .expect("prompts/list request has an id");
        assert!(
            prompts_response.error.is_none(),
            "discovery advertised prompts but prompts/list refused: {:?}",
            prompts_response.error
        );
        let listed_prompts: Vec<String> = prompts_response
            .result
            .as_ref()
            .and_then(|result| result.get("prompts"))
            .and_then(serde_json::Value::as_array)
            .map(|prompts| {
                prompts
                    .iter()
                    .filter_map(|prompt| prompt.get("name"))
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            listed_prompts.iter().any(|name| name == "matrix_prompt"),
            "discovery advertised prompts but prompts/list omitted the registered \
         prompt: {listed_prompts:?}"
        );

        // Completions is deliberately advertisement-variance only, with no dispatch
        // cross-check. A valid completion/complete request needs a populated `ref`
        // naming a prompt or resource template, and building one would make this
        // assertion about completion argument shapes rather than about the
        // capability matrix. Recording the limit rather than implying coverage.

        let instructionless = Server::new("discoverable-server", "1.0.0")
            .tool(Discoverable)
            .build();
        let instructionless_wire = serde_json::to_value(
            instructionless
                .server_discovery()
                .expect("instructionless discovery result is constructed"),
        )
        .expect("instructionless discovery result serializes");
        assert!(instructionless_wire.get("instructions").is_none());
        assert_eq!(
            instructionless_wire["capabilities"], first_result["capabilities"],
            "instructions cannot alter the advertised behavior registry"
        );

        let oversized = Server::new("bounded-instructions", "1.0.0")
            .instructions("x".repeat(MAX_SERVER_INSTRUCTIONS_BYTES + 1))
            .build();
        assert!(
            oversized.server_discovery().is_err(),
            "oversized local instructions are refused before discovery copies them"
        );

        let auto = InboundRequestContext::new(caller_cx(), 402, InboundRequestTransport::Http);
        let auto_discover = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            402_i64,
        );
        let auto_response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::Auto, &auto, &auto_discover)
            .await
            .expect("Auto-composed discovery request has an id");
        assert_eq!(
            auto_response
                .result
                .as_ref()
                .and_then(|result| result.get("supportedVersions")),
            Some(&json!([MODERN_PROTOCOL_VERSION]))
        );

        let initialize = JsonRpcRequest::new("initialize", None, 403_i64);
        let stdio_initialize =
            InboundRequestContext::new(caller_cx(), 403, InboundRequestTransport::Stdio);
        let http_initialize =
            InboundRequestContext::new(caller_cx(), 403, InboundRequestTransport::Http);
        let stdio_error = server
            .dispatch_with_protocol_policy(
                ProtocolPolicy::ModernOnly,
                &stdio_initialize,
                &initialize,
            )
            .await
            .and_then(|response| response.error)
            .expect("ModernOnly stdio initialize is rejected");
        let http_response = server
            .dispatch_http_with_protocol_policy(
                ProtocolPolicy::ModernOnly,
                &http_initialize,
                &initialize,
            )
            .await;
        assert_eq!(http_response.status, HttpStatus::BAD_REQUEST);
        let http_error =
            serde_json::from_slice::<fastmcp_protocol::JsonRpcResponse>(&http_response.body)
                .expect("ModernOnly HTTP response is JSON-RPC")
                .error
                .expect("ModernOnly HTTP initialize is rejected");
        assert_eq!(stdio_error.code.as_i32(), Some(-32601));
        assert_eq!(
            stdio_error.message,
            "Initialization-based MCP is not enabled"
        );
        assert_eq!(stdio_error.data, Some(json!({"supported": ["2026-07-28"]})));
        assert_eq!(http_error.code, stdio_error.code);
        assert_eq!(http_error.message, stdio_error.message);
        assert_eq!(http_error.data, stdio_error.data);
    });
}

#[test]
fn srv_02_b_planted_negative() {
    on_caller_runtime(async {
        let server = Server::new("discover-negative", "1.0.0").build();
        let inbound = InboundRequestContext::new(caller_cx(), 404, InboundRequestTransport::Memory);
        let baseline = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            404_i64,
        );
        let mut planted = baseline.clone();
        planted.method = "initialize".to_owned();

        assert_eq!(baseline.jsonrpc, planted.jsonrpc);
        assert_eq!(baseline.id, planted.id);
        assert_eq!(baseline.params, planted.params);
        let input_before = serde_json::to_vec(&planted).expect("planted request must serialize");
        let catalog_before = public_catalog_snapshot(&server);

        let baseline_response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &inbound, &baseline)
            .await
            .expect("baseline discovery request responds");
        assert!(baseline_response.error.is_none());

        let planted_response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &inbound, &planted)
            .await
            .expect("planted initialize request responds");
        let planted_error = planted_response
            .error
            .expect("modern-marked initialize reaches the final method refusal boundary");
        assert_eq!(planted_error.code.as_i32(), Some(-32601));
        assert_eq!(planted_error.message, "Method not found");
        assert_eq!(planted_error.data, None);
        assert_eq!(
            serde_json::to_vec(&planted).expect("planted request remains serializable"),
            input_before,
            "typed ModernOnly refusal changed caller input"
        );
        assert_eq!(
            public_catalog_snapshot(&server),
            catalog_before,
            "typed ModernOnly refusal changed public server state"
        );
    });
}

#[test]
fn srv_02_i_positive() {
    on_caller_runtime(async {
        let server = Server::new("discoverable-integration", "1.0.0")
            .instructions("Integration instructions")
            .tool(Discoverable)
            .build();
        let inbound = InboundRequestContext::new(caller_cx(), 405, InboundRequestTransport::Memory);
        let discover = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            405_i64,
        );

        let response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &inbound, &discover)
            .await
            .expect("modern discovery request receives response");
        assert!(response.error.is_none());
        let result = response.result.expect("discovery result is present");
        assert_eq!(
            result["supportedVersions"],
            json!([MODERN_PROTOCOL_VERSION])
        );
        assert_eq!(result["instructions"], json!("Integration instructions"));
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            json!("discoverable-integration")
        );
        assert!(result["capabilities"].get("tools").is_some());

        // The integration claim is the join: B's server composition must emit
        // exactly the typed contract A defines. Every assertion above stays inside
        // serde_json and so cannot show the two slices agree — a server emitting a
        // field A's type does not model, or omitting one A requires, would satisfy
        // all of them. Decoding the live wire result with A's public type, and
        // requiring the re-encode to reproduce the server's bytes exactly, is what
        // actually binds the A contract to the B runtime.
        let typed: ServerDiscoverResult = serde_json::from_value(result.clone()).expect(
            "the server's live discovery result must decode as the typed SRV-02 A contract",
        );
        assert_eq!(typed.result_type(), "complete");
        assert_eq!(
            typed
                .supported_versions()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [MODERN_PROTOCOL_VERSION],
            "the typed contract must carry the same final-only version list as the wire"
        );
        assert_eq!(
            typed.peer_diagnostic(),
            None,
            "a locally authored discovery result must need no peer-compatibility diagnostic"
        );
        assert_eq!(
            serde_json::to_value(&typed).expect("typed discovery result re-encodes"),
            result,
            "the typed SRV-02 A contract did not round-trip the server's own discovery bytes"
        );
    });
}

#[test]
fn srv_02_i_planted_negative() {
    on_caller_runtime(async {
        let server = Server::new("discoverable-integration-refusal", "1.0.0")
            .tool(Discoverable)
            .build();
        let inbound = InboundRequestContext::new(caller_cx(), 406, InboundRequestTransport::Memory);
        let baseline = JsonRpcRequest::new(
            SERVER_DISCOVER_METHOD,
            Some(json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                    FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                },
            })),
            406_i64,
        );
        let mut planted = baseline.clone();
        planted
            .params
            .as_mut()
            .and_then(|params| params.as_object_mut())
            .and_then(|obj| obj.get_mut("_meta"))
            .and_then(|meta| meta.as_object_mut())
            .expect("modern metadata object")
            .insert(
                FINAL_PROTOCOL_VERSION_META_KEY.to_owned(),
                json!("2025-11-25"),
            );

        assert_eq!(baseline.method, planted.method);
        assert_eq!(baseline.jsonrpc, planted.jsonrpc);
        assert_eq!(baseline.id, planted.id);
        let input_before = serde_json::to_vec(&planted).expect("planted request must serialize");
        let catalog_before = public_catalog_snapshot(&server);

        let baseline_response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &inbound, &baseline)
            .await
            .expect("baseline discovery request responds");
        assert!(baseline_response.error.is_none());

        let planted_response = server
            .dispatch_with_protocol_policy(ProtocolPolicy::ModernOnly, &inbound, &planted)
            .await
            .expect("planted request receives response");
        let planted_error = planted_response
            .error
            .expect("unsupported version 2025-11-25 must be refused at version boundary");
        assert_eq!(planted_error.code.as_i32(), Some(-32600));
        assert_eq!(
            serde_json::to_vec(&planted).expect("planted request remains serializable"),
            input_before,
            "typed refusal changed caller input"
        );
        assert_eq!(
            public_catalog_snapshot(&server),
            catalog_before,
            "typed refusal changed server state"
        );
    });
}

// This integration target compiles `fastmcp-server` as an ordinary dependency.
// Therefore `cargo test -p fastmcp-server --no-default-features --test srv_02_b`
// exercises the shipped `modern_http_only` composition rather than the
// unit-test-only dual-era adapter retained for legacy contract tests.
#[cfg(not(feature = "legacy-2024-11-05"))]
#[test]
fn srv_02_b_feature_off_http_modern_positive_and_legacy_route_refusal() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(
            asupersync::runtime::reactor::create_reactor()
                .expect("caller HTTP runtime reactor initializes"),
        )
        .blocking_threads(0, 16)
        .build()
        .expect("caller HTTP runtime initializes");
    runtime.block_on(async {
    let cx = Cx::current().expect("caller runtime supplies the HTTP session context");
    let mut builder = Server::new("feature-off-modern-http", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly)
        .expect("ModernOnly is available in every server feature set");
    assert_eq!(
        builder.try_set_protocol_policy(ProtocolPolicy::Auto),
        Err(fastmcp_server::ServerLaunchPolicyError::FeatureUnavailable),
        "Auto requires the unavailable legacy adapter"
    );
    assert_eq!(builder.configured_protocol_policy(), ProtocolPolicy::ModernOnly);
    // The same builder must still serve modern HTTP after refusing Auto.
    let endpoint = builder
        .build_http_endpoint()
        .expect("feature-off server must construct its modern HTTP endpoint");
    let mut session = endpoint
        .open_session(&cx)
        .expect("feature-off server must open a modern HTTP session");
    let discovery = JsonRpcRequest::new(
        SERVER_DISCOVER_METHOD,
        Some(json!({
            "_meta": {
                FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                FINAL_CLIENT_CAPABILITIES_META_KEY: {},
            },
        })),
        4_101_i64,
    );
    let modern_request = HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-protocol-version", MODERN_PROTOCOL_VERSION)
        .with_header("mcp-method", SERVER_DISCOVER_METHOD)
        .with_body(
            serde_json::to_vec(&discovery).expect("modern discovery request must serialize"),
        );

    let modern_response = session
        .handle_async(&cx, modern_request.clone())
        .await
        .expect("feature-off modern request must reach modern_http_only");
    let ServerHttpEndpointResponse::Immediate(modern_response) = modern_response else {
        panic!("ordinary modern discovery must retain its JSON response representation");
    };
    assert_eq!(modern_response.status, HttpStatus::OK);
    let modern_response: fastmcp_protocol::JsonRpcResponse =
        serde_json::from_slice(&modern_response.body)
            .expect("feature-off modern response must remain JSON-RPC");
    assert_eq!(modern_response.id, Some(4_101_i64.into()));
    assert!(modern_response.error.is_none());
    assert_eq!(
        modern_response
            .result
            .as_ref()
            .and_then(|result| result["supportedVersions"].as_array())
            .and_then(|versions| versions.first())
            .and_then(serde_json::Value::as_str),
        Some(MODERN_PROTOCOL_VERSION),
        "the feature-off production path must retain final-era discovery",
    );

    // This reaches the same final-era capability through the public stdio
    // entry point. Unlike a lib unit test, the server linked by this
    // integration target is the shipped no-default-features dependency.
    {
    let (modern_transport, modern_transport_state) =
        FeatureOffTransport::single_request(discovery.clone());
    let modern_stdio_result = Server::new("feature-off-modern-stdio", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly)
        .expect("ModernOnly is available in every server feature set")
        .build()
        .run_transport_returning_with_cx(&cx, modern_transport);
    assert!(
        modern_stdio_result.is_ok(),
        "public feature-off stdio must deliver final server/discover"
    );
    let modern_transport_state = modern_transport_state
        .lock()
        .expect("feature-off transport mutex must not be poisoned");
    assert_eq!(
        modern_transport_state.recv_calls, 2,
        "one request then clean EOF"
    );
    assert_eq!(modern_transport_state.close_calls, 1);
    let [JsonRpcMessage::Response(modern_stdio_response)] =
        modern_transport_state.outgoing.as_slice()
    else {
        panic!("public modern stdio discovery must emit one JSON-RPC response");
    };
    assert_eq!(modern_stdio_response.id, Some(4_101_i64.into()));
    assert_eq!(
        modern_stdio_response
            .result
            .as_ref()
            .and_then(|result| result["supportedVersions"].as_array())
            .and_then(|versions| versions.first())
            .and_then(serde_json::Value::as_str),
        Some(MODERN_PROTOCOL_VERSION),
    );
    }

    // This is deliberately the same fully admitted modern request except for
    // the path. `/messages` is the historical exact-2024 ingress, which must
    // be absent before it can pin an era, allocate request authority, or
    // enter an adapter in the feature-off library.
    let mut legacy_route_request = modern_request.clone();
    legacy_route_request.path = "/messages".to_owned();
    let legacy_refusal = session
        .handle_async(&cx, legacy_route_request)
        .await
        .expect("a disabled legacy route must be a normal HTTP response");
    assert!(matches!(
        legacy_refusal,
        ServerHttpEndpointResponse::Immediate(response) if response.status == HttpStatus::NOT_FOUND
    ));

    // The refusal must not disturb the previously selected modern HTTP era.
    let modern_after_refusal = session
        .handle_async(&cx, modern_request)
        .await
        .expect("legacy-route refusal must not poison the selected modern session");
    assert!(matches!(
        modern_after_refusal,
        ServerHttpEndpointResponse::Immediate(response) if response.status == HttpStatus::OK
    ));

    let legacy_only =
        Server::new("feature-off-legacy-only", "1.0.0").protocol_policy(ProtocolPolicy::LegacyOnly);
    assert!(matches!(
        legacy_only,
        Err(fastmcp_server::ServerLaunchPolicyError::FeatureUnavailable)
    ));

    let legacy_http_endpoint = Server::new("feature-off-legacy-only-http", "1.0.0")
        .protocol_policy(ProtocolPolicy::LegacyOnly)
        .map(|builder| builder.build_http_endpoint());
    assert!(matches!(
        legacy_http_endpoint,
        Err(fastmcp_server::ServerLaunchPolicyError::FeatureUnavailable)
    ));

    // The legacy selection fails before a server or transport exists, so no
    // deferred stdio refusal can perform I/O after construction.
    let legacy_stdio = Server::new("feature-off-legacy-only-stdio", "1.0.0")
        .protocol_policy(ProtocolPolicy::LegacyOnly);
    assert!(matches!(
        legacy_stdio,
        Err(fastmcp_server::ServerLaunchPolicyError::FeatureUnavailable)
    ));
    });
}

#[test]
fn srv_02_b_http_session_caller_runtime_and_cancelled_negative() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(
            asupersync::runtime::reactor::create_reactor()
                .expect("caller HTTP runtime reactor initializes"),
        )
        .build()
        .expect("caller HTTP runtime initializes");
    runtime.block_on(async {
        let parent_cx = Cx::current().expect("the caller runtime supplies the parent context");
        // `current_thread` has one scheduler worker; `block_on` polls its
        // outer future on the invoking thread. Start the embedding scenario
        // on that worker so its children must progress on the same thread.
        let mut scenario = parent_cx
            .spawn(|cx| async move {
                let caller_thread = std::thread::current().id();
                let entered = Arc::new(AtomicUsize::new(0));
                let children_completed = Arc::new(AtomicUsize::new(0));
                let server = Server::new("caller-runtime-http", "1.0.0")
                    .protocol_policy(ProtocolPolicy::ModernOnly)
                    .expect("ModernOnly is available in every profile")
                    .tool(CallerRuntimeTool {
                        caller_thread,
                        entered: Arc::clone(&entered),
                        children_completed: Arc::clone(&children_completed),
                    })
                    .build();
                #[cfg(not(feature = "legacy-2024-11-05"))]
                let endpoint = server.into_http_endpoint();
                #[cfg(feature = "legacy-2024-11-05")]
                let endpoint = server.into_http_endpoint("http://127.0.0.1");
                let endpoint =
                    endpoint.expect("the real server constructs its public HTTP endpoint");

                let mut session = endpoint
                    .open_session(&cx)
                    .expect("the real public HTTP session opens");
                let request = JsonRpcRequest::new(
                    "tools/call",
                    Some(json!({
                        "name": "runtime-child",
                        "arguments": {"value": "from-request"},
                        "_meta": {
                            FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
                            FINAL_CLIENT_CAPABILITIES_META_KEY: {},
                        },
                    })),
                    4_102_i64,
                );
                let request = HttpRequest::new(HttpMethod::Post, "/mcp")
                    .with_header("content-type", "application/json")
                    .with_header("accept", "application/json")
                    .with_header("mcp-protocol-version", MODERN_PROTOCOL_VERSION)
                    .with_header("mcp-method", "tools/call")
                    .with_header("mcp-name", "runtime-child")
                    .with_body(serde_json::to_vec(&request).expect("the tool request serializes"));
                let request_body_before = request.body.clone();
                let positive = session
                    .handle_async(&cx, request.clone())
                    .await
                    .expect("the async HTTP session completes on the caller runtime");
                let ServerHttpEndpointResponse::Immediate(positive) = positive else {
                    panic!("the ordinary tool call must return its selected JSON representation");
                };
                assert_eq!(positive.status, HttpStatus::OK);
                let positive: fastmcp_protocol::JsonRpcResponse =
                    serde_json::from_slice(&positive.body).expect("the real result is JSON-RPC");
                assert_eq!(positive.id, Some(4_102_i64.into()));
                assert!(
                    positive.error.is_none(),
                    "HTTP result: {:?}; entered: {}; children completed: {}",
                    positive.error,
                    entered.load(Ordering::SeqCst),
                    children_completed.load(Ordering::SeqCst)
                );
                let result = positive
                    .result
                    .expect("the child output reaches the caller");
                assert_eq!(result["resultType"], "complete");
                assert_eq!(result["content"][0]["text"], "owned:from-request");
                assert_eq!(entered.load(Ordering::SeqCst), 1);
                assert_eq!(children_completed.load(Ordering::SeqCst), 1);

                // Keep the endpoint, session, request, and runtime unchanged. Native
                // HTTP currently maps failed cancellation/budget admission to its
                // fixed authentication rejection before it invokes the handler.
                cx.cancel_with(
                    asupersync::CancelKind::User,
                    Some("caller cancelled HTTP admission"),
                );
                let negative = session
                    .handle_async(&cx, request.clone())
                    .await
                    .expect("caller cancellation remains an ordinary admission refusal");
                let ServerHttpEndpointResponse::Immediate(negative) = negative else {
                    panic!("cancelled admission must not allocate a response stream");
                };
                assert_eq!(negative.status, HttpStatus::UNAUTHORIZED);
                assert_eq!(
                    negative.headers.get("www-authenticate").map(String::as_str),
                    Some("Bearer")
                );
                assert!(negative.body.is_empty());
                assert_eq!(
                    entered.load(Ordering::SeqCst),
                    1,
                    "cancellation invoked the handler"
                );
                assert_eq!(
                    children_completed.load(Ordering::SeqCst),
                    1,
                    "cancellation admitted another child"
                );
                assert_eq!(request.body, request_body_before);
                // Acknowledge the scenario's deliberate cancellation only after all
                // refusal/effect assertions ran, so joining requires actual completion.
                assert!(cx.checkpoint().is_err());
            })
            .expect("the caller runtime admits the HTTP embedding task");
        scenario
            .join(&parent_cx)
            .await
            .expect("the caller runtime joins the complete HTTP scenario");
    });
}

#[cfg(not(feature = "legacy-2024-11-05"))]
#[test]
fn srv_02_b_feature_off_endpoint_uses_the_public_modern_error_contract() {
    let result = Server::new("feature-off-invalid-http-config", "1.0.0")
        .http_config(HttpServerConfig::new().request_capacity(0))
        .build_http_endpoint();

    match result {
        Err(ServerHttpEndpointError::InvalidConfiguration(message)) => {
            assert!(message.contains("modern HTTP request capacity must be nonzero"));
        }
        Err(error) => panic!("expected the public configuration error, got {error}"),
        Ok(_) => panic!("zero modern request capacity must be rejected"),
    }
}
