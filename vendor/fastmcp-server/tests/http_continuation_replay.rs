//! Public secured-HTTP embedding tests: real ingress, authentication, middleware,
//! request-owned router execution and MRTR continuation consumption. Discarding
//! a returned response models reply loss at the embedding boundary; these are
//! not socket-loss, TLS, external-issuer, or durable-recovery proofs.
#![recursion_limit = "256"]

use std::future::Future;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;

use asupersync::{Cx, Outcome};
use fastmcp_core::{AuthContext, McpContext, McpError, McpOutcome, McpRequestCancellation, McpResult, Sha256Digest};
use fastmcp_core::ingress::{SecurityPartitionDescriptor, VerifiedAudienceBinding, VerifiedIdentityFacts, VerifiedIngressAuthentication};
use fastmcp_core::partition::{ContinuationPartitionKey, DurableOwnerKey, PartitionAuthorization};
use fastmcp_core::runtime::{ProcessGenerationGuard, SnapshotCloneStance};
use fastmcp_protocol::{CompleteResult, Content, CoreResultDiscriminatorPolicy, DecodedResult,
    FinalCallToolResult, InputRequiredResult, ResultMeta, ResultPeerEra, Tool,
    FINAL_PROTOCOL_VERSION, decode_peer_result, protocol_policy::ProtocolPolicy};
use fastmcp_server::{AuthProvider, AuthRequest, Server, ServerHttpEndpoint, StaticTokenVerifier, TokenAuthProvider, ToolHandler};
use fastmcp_server::bidirectional::MrtrCompletedInputs;
use fastmcp_server::{BoxFuture, FinalToolOutcome, ToolExecutionMode};
use fastmcp_server::{Middleware, MiddlewareDecision};
use fastmcp_server::{ContinuationReplayAuthority, ContinuationReplayLimits, ContinuationReplayMiddleware};
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::HttpSecurityPolicy;
use fastmcp_transport::http::{HttpMethod, HttpRequest};
use serde_json::{Value, json};

#[derive(Clone)]
struct Probe {
    provider: Arc<TokenAuthProvider>,
    alice: String,
    bob: String,
    authentications: Arc<AtomicUsize>,
    starts: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
    transforms: Arc<AtomicUsize>,
    revision: Arc<AtomicUsize>,
    lifetime: McpRequestCancellation,
    require_roots: bool,
}
impl Probe {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let alice = format!("replay-alice-{}-{nonce}", std::process::id());
        let bob = format!("replay-bob-{}-{nonce}", std::process::id());
        // Cross-POST MRTR state on stateless HTTP is keyed to a provider-scoped
        // owner, never to a display subject; this provider supplies one each.
        let mut a = AuthContext::with_subject("alice".to_owned())
            .with_session_owner(Sha256Digest::from_bytes([0xA1; 32]));
        let mut b = AuthContext::with_subject("bob".to_owned())
            .with_session_owner(Sha256Digest::from_bytes([0xB0; 32]));
        a.scopes = vec!["tools:call".to_owned()];
        b.scopes = a.scopes.clone();
        let verifier = StaticTokenVerifier::new([(alice.clone(), a), (bob.clone(), b)]).unwrap();
        Self { provider: Arc::new(TokenAuthProvider::new(verifier)), alice, bob,
            authentications: Arc::new(AtomicUsize::new(0)), starts: Arc::new(AtomicUsize::new(0)),
            effects: Arc::new(AtomicUsize::new(0)), transforms: Arc::new(AtomicUsize::new(0)),
            revision: Arc::new(AtomicUsize::new(1)), lifetime: McpRequestCancellation::new(), require_roots: false }
    }
    fn authority(&self, ctx: &McpContext) -> McpResult<ContinuationReplayAuthority> {
        let auth = ctx.auth().ok_or_else(|| McpError::invalid_params("No authenticated owner"))?;
        let subject = auth.subject.as_deref().ok_or_else(|| McpError::invalid_params("No authenticated subject"))?;
        if !auth.scopes.iter().any(|scope| scope == "tools:call") {
            return Err(McpError::invalid_params("Missing current permission"));
        }
        // The test's configured provider is the authority for these already
        // authenticated facts. This is not a JWT/OIDC verification fixture.
        let ingress = VerifiedIngressAuthentication::from_verified_provider_output(VerifiedIdentityFacts {
            provider: "replay-test-provider", configuration_generation: 1,
            issuer: "https://issuer.example", canonical_resource: "https://service.example/mcp",
            verified_audience_binding: VerifiedAudienceBinding::OAuth {
                canonical_resource: "https://service.example/mcp".to_owned(), validated_audience: "https://service.example/mcp".to_owned(),
                audience_policy_id: "test-policy".to_owned(), audience_policy_revision: 1,
                provider: "replay-test-provider".to_owned(), configuration_generation: 1,
            },
            tenant: "test-tenant", subject_or_principal: subject, authorized_party_or_client: "test-client",
            verified_claims: &[], auth_policy_revision: 1, trust_generation: 1,
        }).unwrap();
        let descriptor = SecurityPartitionDescriptor::from_verified_ingress(&ingress).to_partition_descriptor().unwrap();
        let revision = format!("checkout-v{}", self.revision.load(Ordering::SeqCst));
        let key = ContinuationPartitionKey::derive(&descriptor, &["tools:call"], &revision,
            if self.require_roots { "roots-capabilities" } else { "empty-capabilities" }, "test-policy", "test-deployment").unwrap();
        let owner = DurableOwnerKey::derive(&descriptor, 1).unwrap();
        Ok(ContinuationReplayAuthority::new(key, PartitionAuthorization::current(&descriptor, &owner), self.lifetime.clone()))
    }
    fn effects(&self) -> usize { self.effects.load(Ordering::SeqCst) }
}
impl AuthProvider for Probe {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        self.authentications.fetch_add(1, Ordering::SeqCst);
        self.provider.authenticate(ctx, request)
    }
}
fn needs_input(require_roots: bool) -> InputRequiredResult {
    let mut wire = json!({"resultType":"input_required","requestState":"fixture-handler-state"});
    if require_roots {
        wire["inputRequests"] = json!({"left":{"method":"roots/list"}, "right":{"method":"roots/list"}});
    }
    let (decoded, diagnostic) = decode_peer_result(
        &wire.to_string(), ResultPeerEra::Modern, &CoreResultDiscriminatorPolicy,
    ).unwrap();
    assert!(diagnostic.is_none());
    let DecodedResult::InputRequired(result) = decoded else { panic!("input-required branch must decode"); };
    result
}
impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool { name: "checkout".to_owned(), description: None, input_schema: json!({"type":"object"}),
            output_schema: None, icon: None, version: None, tags: vec![], annotations: None }
    }
    fn execution_mode(&self) -> ToolExecutionMode { ToolExecutionMode::Async }
    fn declares_final_mrtr(&self) -> bool { true }
    fn call(&self, _: &McpContext, _: Value) -> McpResult<Vec<Content>> {
        panic!("public final route must use the request-owned hook")
    }
    fn call_final_outcome_async_resuming_in_request<'a>(
        &'a self, ctx: &'a McpContext, request_cx: &'a Cx, arguments: Value,
        resume_inputs: Option<&'a MrtrCompletedInputs>,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        Box::pin(async move {
            request_cx.checkpoint().expect("the request-owned Cx must remain live");
            ctx.ensure_live().unwrap();
            if let Some(inputs) = resume_inputs {
                let accepted = if self.require_roots {
                    assert_eq!(inputs.responses().len(), 2, "both partial answers reach the handler once");
                    json!({"left":inputs.roots("left").unwrap().unwrap(),
                        "right":inputs.roots("right").unwrap().unwrap(),
                        "order":inputs.responses().iter().map(|(key, _)| key).collect::<Vec<_>>()})
                } else {
                    assert!(inputs.responses().is_empty(), "state-only continuation has no invented input");
                    Value::Null
                };
                let effect = self.effects.fetch_add(1, Ordering::SeqCst) + 1;
                let body: FinalCallToolResult = serde_json::from_value(json!({
                    "content":[{"type":"text","text":format!("committed-{effect}")}],
                    "structuredContent":{"quantity":arguments["quantity"],"effect":effect,"accepted":accepted}, "isError":false,
                })).unwrap();
                Outcome::Ok(FinalToolOutcome::Complete(CompleteResult::new(body, ResultMeta::empty())))
            } else {
                self.starts.fetch_add(1, Ordering::SeqCst);
                Outcome::Ok(FinalToolOutcome::InputRequired(needs_input(self.require_roots)))
            }
        })
    }
}
struct Stamp(Arc<AtomicUsize>);
impl Middleware for Stamp {
    fn on_response(&self, _: &McpContext, _: &fastmcp_protocol::JsonRpcRequest, mut value: Value) -> McpResult<Value> {
        if value.get("resultType").and_then(Value::as_str) == Some("complete") {
            value["com.example/transform"] = json!(self.0.fetch_add(1, Ordering::SeqCst) + 1);
        }
        Ok(value)
    }
}
/// A generic response cache: it records a hit exactly as the shipped cache
/// does, then answers with a stored value of the configured result type.
struct GenericCache(&'static str);
impl Middleware for GenericCache {
    fn on_request(&self, ctx: &McpContext, _: &fastmcp_protocol::JsonRpcRequest) -> McpResult<MiddlewareDecision> {
        assert!(ctx.mark_response_cache_hit(u64::MAX), "the request scope must admit a cache hit");
        Ok(MiddlewareDecision::Respond(match self.0 {
            "input_required" => json!({"resultType":"input_required","requestState":"cache-held-state"}),
            _ => json!({"resultType":"complete","content":[{"type":"text","text":"cached"}],"isError":false}),
        }))
    }
}
struct Fixture { endpoint: ServerHttpEndpoint, probe: Probe, journal: Arc<ContinuationReplayMiddleware>, policy: HttpSecurityPolicy }
impl Fixture {
    fn new(cx: &Cx, enabled: bool, limits: ContinuationReplayLimits) -> Self {
        Self::with_probe(cx, enabled, limits, Probe::new(), false)
    }
    fn with_probe(cx: &Cx, enabled: bool, limits: ContinuationReplayLimits, probe: Probe, recover_successors: bool) -> Self {
        Self::build(cx, enabled, limits, probe, recover_successors, None)
    }
    /// No journal: the given middleware is the only one that can short-circuit.
    fn with_generic_cache(cx: &Cx, cache: Arc<dyn Middleware>) -> Self {
        Self::build(cx, false, ContinuationReplayLimits::default(), Probe::new(), false, Some(cache))
    }
    fn build(cx: &Cx, enabled: bool, limits: ContinuationReplayLimits, probe: Probe, recover_successors: bool,
        cache: Option<Arc<dyn Middleware>>) -> Self {
        let authorizer = probe.clone();
        let authorize = move |ctx: &McpContext, _: &fastmcp_protocol::JsonRpcRequest| authorizer.authority(ctx);
        let guard = ProcessGenerationGuard::install().unwrap();
        let journal = Arc::new(if recover_successors {
            ContinuationReplayMiddleware::new_with_successor_recovery(cx, guard,
                SnapshotCloneStance::NoLiveMemoryCloning, limits, authorize)
        } else {
            ContinuationReplayMiddleware::new(cx, guard,
                SnapshotCloneStance::NoLiveMemoryCloning, limits, authorize)
        }.unwrap());
        let mut builder = Server::new("continuation-replay", "1")
            .protocol_policy(ProtocolPolicy::ModernOnly).unwrap().auth_provider(probe.clone()).tool(probe.clone());
        if let Some(cache) = cache { builder = builder.middleware(cache); }
        if enabled { builder = builder.middleware(journal.clone()); }
        builder = builder.middleware(Stamp(probe.transforms.clone()));
        #[cfg(not(feature = "legacy-2024-11-05"))]
        let endpoint = builder.build_http_endpoint();
        #[cfg(feature = "legacy-2024-11-05")]
        let endpoint = builder.build_http_endpoint("http://service.example");
        let policy = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec![],
        ).unwrap();
        Self { endpoint: endpoint.unwrap(), probe, journal, policy }
    }
    async fn post(&self, cx: &Cx, id: i64, token: &str, params: Value) -> (u16, Value) {
        let request = HttpRequest::new(HttpMethod::Post, "/mcp")
            .with_header("host", "service.example").with_header("authorization", format!("Bearer {token}"))
            .with_header("content-type", "application/json").with_header("accept", "application/json")
            .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION).with_header("mcp-method", "tools/call")
            .with_header("mcp-name", "checkout")
            .with_body(serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":params})).unwrap());
        let response = Box::pin(self.endpoint.handle_secured_async(cx, &self.policy, request)).await.unwrap();
        assert!(!response.is_streaming());
        let status = response.response().status.0;
        let body = if status == 200 { serde_json::from_slice(&response.response().body).unwrap() } else { Value::Null };
        (status, body)
    }
    async fn begin(&self, cx: &Cx, id: i64) -> Value {
        let mut params = json!({"name":"checkout","arguments":{"quantity":1},"_meta":{
            "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,"io.modelcontextprotocol/clientCapabilities":{}}});
        if self.probe.require_roots {
            params["_meta"]["io.modelcontextprotocol/clientCapabilities"]["roots"] = json!({});
        }
        let (status, body) = self.post(cx, id, &self.probe.alice, params.clone()).await;
        assert_eq!(status, 200);
        assert!(body.get("error").is_none(), "{body}");
        assert_eq!(body["result"]["resultType"], "input_required");
        let state = body["result"]["requestState"].as_str().unwrap();
        assert!(!state.is_empty());
        assert_ne!(state, "fixture-handler-state", "the router must mint the continuation");
        let mut retry = params;
        retry["requestState"] = json!(state);
        // State-only retries omit inputResponses. A present empty map is a
        // different protocol case and must not stand in for that omission.
        retry
    }
    async fn finish(&self, cx: &Cx, params: &Value, id: i64) -> Value {
        let (status, body) = self.post(cx, id, &self.probe.alice, params.clone()).await;
        assert_eq!(status, 200);
        assert!(body.get("error").is_none(), "{body}");
        assert_eq!(body["id"], id);
        assert_eq!(body["result"]["resultType"], "complete");
        body["result"].clone()
    }
}
fn rejected(response: &(u16, Value)) -> bool { response.0 != 200 || response.1.get("error").is_some() }
fn run<F, Fut>(scenario: F)
where F: FnOnce(Cx) -> Fut + Send + 'static, Fut: Future<Output = ()> + Send + 'static,
{
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap()).blocking_threads(1, 4).build().unwrap()
        .block_on(async move {
            let parent = Cx::current().unwrap();
            let mut task = parent.spawn(move |cx| async move {
                let deadline = cx.now().saturating_add_nanos(15_000_000_000);
                asupersync::time::timeout_at(deadline, Box::pin(scenario(cx))).await.unwrap();
            }).unwrap();
            task.join(&parent).await.unwrap();
        });
}

#[test]
fn public_mrtr_retry_recovers_discarded_terminal_without_a_second_effect_or_transform() {
    run(|cx| async move {
        for enabled in [false, true] {
            let f = Fixture::new(&cx, enabled, ContinuationReplayLimits::default());
            let params = f.begin(&cx, 1).await;
            assert_eq!(f.probe.effects(), 0);
            let delivered = f.finish(&cx, &params, 2).await;
            assert_eq!(delivered["content"][0]["text"], "committed-1");
            let expected = delivered.clone();
            drop(delivered); // simulate losing the first returned reply
            let result = f.post(&cx, 3, &f.probe.alice, params).await;
            if enabled {
                assert_eq!(result.0, 200);
                assert_eq!(result.1["id"], 3);
                assert_eq!(result.1["result"], expected);
            } else { assert!(rejected(&result), "without the journal the consumed continuation cannot replay"); }
            assert_eq!(f.probe.effects(), 1);
            assert_eq!(f.probe.transforms.load(Ordering::SeqCst), 1);
            assert_eq!(f.probe.starts.load(Ordering::SeqCst), 1);
        }
    });
}

#[test]
fn public_mrtr_hits_still_authenticate_and_cannot_cross_principals() {
    run(|cx| async move {
        let f = Fixture::new(&cx, true, ContinuationReplayLimits::default());
        let params = f.begin(&cx, 1).await;
        let expected = f.finish(&cx, &params, 2).await;
        let calls = f.probe.authentications.load(Ordering::SeqCst);
        assert_eq!(f.post(&cx, 3, "wrong-bearer", params.clone()).await.0, 401);
        assert!(rejected(&f.post(&cx, 4, &f.probe.bob, params.clone()).await));
        assert_eq!(f.finish(&cx, &params, 5).await, expected);
        assert!(f.probe.authentications.load(Ordering::SeqCst) >= calls + 3);
        assert_eq!(f.probe.effects(), 1);
    });
}

#[test]
fn public_mrtr_changed_arguments_answers_and_metadata_never_repeat_the_effect() {
    run(|cx| async move {
        let f = Fixture::new(&cx, true, ContinuationReplayLimits::default());
        let params = f.begin(&cx, 1).await;
        let expected = f.finish(&cx, &params, 2).await;
        for case in 0..3 {
            let mut changed = params.clone();
            match case {
                0 => changed["arguments"]["quantity"] = json!(2),
                1 => changed["inputResponses"] = json!({"foreign":{"roots":[]}}),
                _ => changed["_meta"]["com.example/tenant"] = json!("other"),
            }
            assert!(rejected(&f.post(&cx, 3 + case, &f.probe.alice, changed).await));
        }
        assert_eq!(f.finish(&cx, &params, 8).await, expected);
        assert_eq!(f.probe.effects(), 1);
    });
}

#[test]
fn public_mrtr_revocation_and_handler_revision_prevent_old_result_disclosure() {
    run(|cx| async move {
        let f = Fixture::new(&cx, true, ContinuationReplayLimits::default());
        let params = f.begin(&cx, 1).await;
        let expected = f.finish(&cx, &params, 2).await;
        f.probe.revision.store(2, Ordering::SeqCst);
        assert!(rejected(&f.post(&cx, 3, &f.probe.alice, params.clone()).await));
        f.probe.revision.store(1, Ordering::SeqCst);
        assert_eq!(f.finish(&cx, &params, 4).await, expected);
        f.probe.lifetime.cancel();
        assert!(rejected(&f.post(&cx, 5, &f.probe.alice, params).await));
        assert_eq!(f.probe.effects(), 1);
        assert_eq!(f.journal.prune(&cx).unwrap(), 1);
    });
}

#[test]
fn public_mrtr_capacity_refuses_before_resume_and_does_not_evict_the_first_reply() {
    run(|cx| async move {
        let limits = ContinuationReplayLimits::new(1, 4096, 2048, 2048, Duration::from_secs(60)).unwrap();
        let f = Fixture::new(&cx, true, limits);
        let one = f.begin(&cx, 1).await;
        let expected = f.finish(&cx, &one, 2).await;
        let two = f.begin(&cx, 3).await;
        assert!(rejected(&f.post(&cx, 4, &f.probe.alice, two).await));
        assert_eq!(f.finish(&cx, &one, 5).await, expected);
        assert_eq!(f.probe.effects(), 1);
        assert_eq!(f.probe.starts.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn public_mrtr_oversized_terminal_is_uncertain_not_permission_to_reexecute() {
    run(|cx| async move {
        let limits = ContinuationReplayLimits::new(8, 4096, 2048, 16, Duration::from_secs(60)).unwrap();
        let f = Fixture::new(&cx, true, limits);
        let params = f.begin(&cx, 1).await;
        assert!(rejected(&f.post(&cx, 2, &f.probe.alice, params.clone()).await));
        assert_eq!(f.probe.effects(), 1);
        assert!(rejected(&f.post(&cx, 3, &f.probe.alice, params).await));
        assert_eq!(f.probe.effects(), 1);
        assert_eq!(f.journal.prune(&cx).unwrap(), 0);
    });
}

#[test]
fn public_mrtr_rotation_preserves_replies_and_close_does_not_reexecute() {
    run(|cx| async move {
        let f = Fixture::new(&cx, true, ContinuationReplayLimits::default());
        let params = f.begin(&cx, 1).await;
        let expected = f.finish(&cx, &params, 2).await;
        assert_eq!(f.journal.rotate(&cx).unwrap(), 2);
        assert_eq!(f.finish(&cx, &params, 3).await, expected);
        f.journal.close().unwrap();
        assert!(rejected(&f.post(&cx, 4, &f.probe.alice, params).await));
        assert_eq!(f.probe.effects(), 1);
    });
}

#[test]
fn public_generic_cache_may_complete_but_never_replay_an_input_required_reply() {
    run(|cx| async move {
        // Near-identical pair: only the cached result type differs. A recorded
        // hit completes a final request, but only the continuation journal may
        // answer with a router-minted input_required reply.
        for (result_type, admitted) in [("complete", true), ("input_required", false)] {
            let f = Fixture::with_generic_cache(&cx, Arc::new(GenericCache(result_type)));
            let params = json!({"name":"checkout","arguments":{"quantity":1},"_meta":{
                "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities":{}}});
            let (status, body) = f.post(&cx, 1, &f.probe.alice, params).await;
            assert_eq!(status, 200);
            if admitted {
                assert!(body.get("error").is_none(), "{body}");
                assert_eq!(body["result"]["content"][0]["text"], "cached");
            } else {
                assert_eq!(body["error"]["code"], -32603, "{body}");
                assert_eq!(body["error"]["message"], "middleware cannot short-circuit a final core response");
            }
            assert_eq!(f.probe.starts.load(Ordering::SeqCst), 0, "the handler never runs");
            assert_eq!(f.probe.effects(), 0);
        }
    });
}

#[path = "http_continuation_replay/successors.rs"]
mod successors;
