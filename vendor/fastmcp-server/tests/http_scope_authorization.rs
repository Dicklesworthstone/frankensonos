//! Public HTTP request admission, not a scope-projection-only fixture.
//! These tests use installed server and secured-endpoint gates, native token
//! verification, ordinary middleware and real registered tools. They cover
//! embedding, not live socket/TLS or external OAuth interoperability.

// bd-pf5n7: the default 128 is exceeded resolving this crate's async blocks through
// `handle_secured_async` -> `await_dispatch` in fastmcp-server PRODUCTION source
// (endpoint.rs:164, :297). The compiler suggests this remedy here, and czvvt is the
// precedent. TWO FACTS SUPPORT IT, SEPARATELY, AND NEITHER CORROBORATES THE OTHER:
//   1. The trait-obligation chain is DEEP BUT BOUNDED. The cold check at 256 —
//      a8574340, 455/455 units rebuilt, 0 cached, 0 diagnostics of any code —
//      COMPLETED the proof rather than giving up, so a finite budget suffices and
//      this is a fix, not a suppression. Default feature set only.
//   2. SEPARATELY, by direct call-graph reading: `await_dispatch` has no self-call
//      and no direct call back into `handle_secured_async`. DIRECT CALLS ONLY — an
//      edge through a trait object is invisible to that reading, and this module
//      holds two erasure points (scope_policy.rs:251 `Arc<dyn AuthProvider>`;
//      endpoint/listener.rs:285 a boxed `dyn Future`), one of them on the path
//      `await_dispatch` traverses at endpoint.rs:183.
// Type erasure defeats BOTH instruments in the same direction, so (2) does not
// establish that the call graph is acyclic and (1) does not cover it.
// The depth accumulates inside fastmcp-server, which is why boxing on the test side
// changes nothing: measured, not assumed.
// COST: it removes the early warning that composition depth is growing. It does NOT
// change codegen — `recursion_limit` bounds compile-time trait resolution only — so
// no runtime exposure is created or removed. The mechanism-level repair, boxing the
// three unboxed call sites in endpoint.rs (:180, :199, :217 — the idiom already
// exists at :392), is a production change and is filed apart.
#![recursion_limit = "256"]

use std::future::Future;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::{SystemTime, UNIX_EPOCH};

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpContext, McpError, McpResult};
use fastmcp_protocol::{Content, FINAL_PROTOCOL_VERSION, JsonRpcRequest, Tool, protocol_policy::ProtocolPolicy};
use fastmcp_server::{
    AuthProvider, AuthRequest, Middleware, MiddlewareDecision, Server,
    ServerHttpEndpoint, ServerHttpEndpointResponse, StaticTokenVerifier, TokenAuthProvider, ToolHandler,
};
use fastmcp_server::http_admission::security::scope_policy::{RequiredScopes, ScopeImplicationPolicy};
use fastmcp_server::http_admission::security::scope_policy::request::ScopeRequestPolicy;
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpResponse};
use serde_json::{Value, json};

const TOOL: &str = "scope_admission_probe";

#[derive(Default)]
struct Counts {
    auth: AtomicUsize,
    middleware: AtomicUsize,
    error_hooks: AtomicUsize,
    handler: AtomicUsize,
}

#[derive(Clone)]
struct Probe {
    token: String,
    verifier: StaticTokenVerifier,
    counts: Arc<Counts>,
}

impl Probe {
    fn new(grants: &[&str]) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let token = format!("scope-admission-{}-{nonce}", std::process::id());
        let mut facts = AuthContext::with_subject(format!("scope-owner-{nonce}"));
        facts.scopes = grants.iter().map(|grant| (*grant).to_owned()).collect();
        facts.claims = Some(json!({"scope":grants.join(" ")}));
        let verifier = StaticTokenVerifier::new([(token.clone(), facts)]).unwrap();
        Self { token, verifier, counts: Arc::new(Counts::default()) }
    }
    fn snapshot(&self) -> (usize, usize, usize, usize) {
        (self.counts.auth.load(Ordering::SeqCst), self.counts.middleware.load(Ordering::SeqCst),
            self.counts.error_hooks.load(Ordering::SeqCst), self.counts.handler.load(Ordering::SeqCst))
    }
}

impl AuthProvider for Probe {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        self.counts.auth.fetch_add(1, Ordering::SeqCst);
        TokenAuthProvider::new(self.verifier.clone()).authenticate(ctx, request)
    }
}

impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool {
            name: TOOL.to_owned(), description: None, input_schema: json!({"type":"object"}),
            output_schema: None, icon: None, version: None, tags: Vec::new(), annotations: None,
        }
    }
    fn call(&self, ctx: &McpContext, _: Value) -> McpResult<Vec<Content>> {
        self.counts.handler.fetch_add(1, Ordering::SeqCst);
        let facts = ctx.auth().unwrap_or_else(AuthContext::anonymous);
        Ok(vec![Content::text(json!({"scopes":facts.scopes,"claims":facts.claims}).to_string())])
    }
}

struct ApplicationMiddleware { probe: Probe, cached: bool }
impl Middleware for ApplicationMiddleware {
    fn on_request(&self, ctx: &McpContext, _: &JsonRpcRequest) -> McpResult<MiddlewareDecision> {
        self.probe.counts.middleware.fetch_add(1, Ordering::SeqCst);
        if self.cached {
            // A final result may be completed by middleware only as a recorded
            // cache hit, exactly as the shipped response cache does.
            assert!(ctx.mark_response_cache_hit(1), "the request scope must admit a cache hit");
            Ok(MiddlewareDecision::Respond(json!({
                "resultType":"complete", "content":[{"type":"text","text":"cache-hit"}]
            })))
        } else {
            Ok(MiddlewareDecision::Continue)
        }
    }
    fn on_error(&self, _: &McpContext, _: &JsonRpcRequest, error: McpError) -> McpError {
        self.probe.counts.error_hooks.fetch_add(1, Ordering::SeqCst);
        error
    }
}

fn required(scopes: &[&str]) -> RequiredScopes {
    RequiredScopes::new(scopes.iter().map(|scope| (*scope).to_owned()).collect()).unwrap()
}
fn policy(edges: &[(&str, &str)], scopes: &[&str]) -> ScopeRequestPolicy {
    ScopeRequestPolicy::new(8, ScopeImplicationPolicy::new(7,
        edges.iter().map(|(a,b)| ((*a).to_owned(),(*b).to_owned())).collect()).unwrap(), vec![
        ("tools/list".to_owned(), required(&[])),
        ("tools/call".to_owned(), required(scopes)),
    ]).unwrap()
}
fn server(probe: &Probe, cached: bool, authenticate: bool) -> Server {
    let builder = Server::new("scope-admission", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
        .middleware(ApplicationMiddleware { probe:probe.clone(), cached })
        .tool(probe.clone());
    if authenticate { builder.auth_provider(probe.clone()).build() } else { builder.build() }
}
fn endpoint(server: Server) -> ServerHttpEndpoint {
    #[cfg(not(feature = "legacy-2024-11-05"))]
    let endpoint = server.into_http_endpoint();
    #[cfg(feature = "legacy-2024-11-05")]
    let endpoint = server.into_http_endpoint("http://scope.example");
    endpoint.unwrap()
}
fn request(probe: &Probe, id: i64, method: &str, mut params: Value) -> HttpRequest {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities":{},
    });
    let mut request = HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("host", "scope.example")
        .with_header("authorization", format!("Bearer {}",probe.token))
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_header("mcp-method", method)
        .with_body(serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).unwrap());
    if method == "tools/call" { request = request.with_header("mcp-name", TOOL); }
    request
}
fn call(probe: &Probe, id: i64) -> HttpRequest {
    request(probe,id,"tools/call",json!({"name":TOOL,"arguments":{}}))
}
async fn dispatch(cx: &Cx, endpoint: &ServerHttpEndpoint, request: HttpRequest) -> HttpResponse {
    let mut session = endpoint.open_session(cx).unwrap();
    let response = session.handle_async(cx, request).await.unwrap();
    session.close(cx).await;
    match response {
        ServerHttpEndpointResponse::Immediate(response) => response,
        _ => panic!("JSON request should yield a complete response"),
    }
}
fn body(response: &HttpResponse) -> Value { serde_json::from_slice(&response.body).unwrap() }
fn success(response: &HttpResponse) -> Value {
    let document = body(response);
    assert!(document.get("error").is_none(), "unexpected protocol failure: {document}");
    assert_eq!(response.status.0, 200);
    document["result"].clone()
}
fn denied(response: &HttpResponse, id: i64, probe: &Probe) -> Value {
    let document = body(response);
    assert_eq!(document["id"], id);
    assert!(document.get("result").is_none());
    assert!(document["error"]["code"].is_number());
    assert_eq!(document["error"]["message"], "Operation is not permitted");
    assert!(document["error"].get("data").is_none());
    assert!(!String::from_utf8_lossy(&response.body).contains(&probe.token));
    document["error"].clone()
}
fn run<F,Fut>(scenario: F)
where F: FnOnce(Cx) -> Fut + Send + 'static, Fut: Future<Output=()> + Send + 'static {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .blocking_threads(1,4).build().unwrap().block_on(async move {
            let parent = Cx::current().unwrap();
            let mut task = parent.spawn(scenario).unwrap();
            task.join(&parent).await.unwrap();
        });
}

#[test]
fn http_scope_admission_one_edge_denial_stops_handler_and_reaccepts_baseline() {
    run(|cx| async move {
        let probe = Probe::new(&["admin"]);
        let allowed = endpoint(server(&probe,false,true).with_scope_authorization(
            policy(&[("admin","write"),("write","read")], &["read","write"])).unwrap());
        let blocked = endpoint(server(&probe,false,true).with_scope_authorization(
            policy(&[("admin","write")], &["read","write"])).unwrap());
        let first = success(&dispatch(&cx,&allowed,call(&probe,1)).await);
        let facts: Value = serde_json::from_str(first["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(facts, json!({"scopes":["admin"],"claims":{"scope":"admin"}}));
        assert_eq!(probe.snapshot(),(1,1,0,1));
        denied(&dispatch(&cx,&blocked,call(&probe,2)).await,2,&probe);
        assert_eq!(probe.snapshot(),(2,1,0,1));
        assert_eq!(success(&dispatch(&cx,&allowed,call(&probe,3)).await),first);
        assert_eq!(probe.snapshot(),(3,2,0,2));
    });
}

#[test]
fn http_scope_admission_requires_all_permissions_not_just_one_matching_grant() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let blocked = endpoint(server(&probe,false,true).with_scope_authorization(policy(&[],&["read","write"])).unwrap());
        let allowed = endpoint(server(&probe,false,true).with_scope_authorization(policy(&[],&["read"])).unwrap());
        denied(&dispatch(&cx,&blocked,call(&probe,1)).await,1,&probe);
        assert_eq!(probe.snapshot(),(1,0,0,0));
        success(&dispatch(&cx,&allowed,call(&probe,2)).await);
        assert_eq!(probe.snapshot(),(2,1,0,1));
    });
}

#[test]
fn http_scope_admission_explicit_public_rule_does_not_make_unlisted_methods_public() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,false,true).with_scope_authorization(policy(&[],&["write"])).unwrap());
        let list = success(&dispatch(&cx,&endpoint,request(&probe,1,"tools/list",json!({}))).await);
        assert_eq!(list["tools"][0]["name"],TOOL);
        assert_eq!(probe.snapshot(),(1,1,0,0));
        denied(&dispatch(&cx,&endpoint,request(&probe,2,"resources/list",json!({}))).await,2,&probe);
        assert_eq!(probe.snapshot(),(2,1,0,0));
        denied(&dispatch(&cx,&endpoint,call(&probe,3)).await,3,&probe);
        assert_eq!(probe.snapshot(),(3,1,0,0));
    });
}

#[test]
fn http_scope_admission_caller_metadata_and_arguments_cannot_install_permissions() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,false,true).with_scope_authorization(policy(&[],&["write"])).unwrap());
        let baseline = denied(&dispatch(&cx,&endpoint,call(&probe,1)).await,1,&probe);
        let mut injected = call(&probe,2);
        let mut document: Value = serde_json::from_slice(&injected.body).unwrap();
        document["params"]["_meta"]["com.example/scopePolicy"] = json!({"scopes":["write"],"implications":[["read","write"]]});
        document["params"]["arguments"] = json!({"scopes":["write"],"requiredScopes":[],"policyRevision":999});
        injected.body = serde_json::to_vec(&document).unwrap();
        assert_eq!(denied(&dispatch(&cx,&endpoint,injected).await,2,&probe),baseline);
        assert_eq!(probe.snapshot(),(2,0,0,0));
    });
}

#[test]
fn http_scope_admission_precedes_an_application_cache_that_short_circuits_dispatch() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let blocked = endpoint(server(&probe,true,true).with_scope_authorization(policy(&[],&["write"])).unwrap());
        let allowed = endpoint(server(&probe,true,true).with_scope_authorization(policy(&[],&["read"])).unwrap());
        denied(&dispatch(&cx,&blocked,call(&probe,1)).await,1,&probe);
        assert_eq!(probe.snapshot(),(1,0,0,0));
        let cached = success(&dispatch(&cx,&allowed,call(&probe,2)).await);
        assert_eq!(cached["content"][0]["text"],"cache-hit");
        assert_eq!(probe.snapshot(),(2,1,0,0));
        denied(&dispatch(&cx,&blocked,call(&probe,3)).await,3,&probe);
        assert_eq!(probe.snapshot(),(3,1,0,0));
    });
}

#[test]
fn http_scope_admission_stacked_policies_intersect_instead_of_replacing_restrictions() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        for strict_first in [true,false] {
            let strict = policy(&[],&["write"]);
            let public = policy(&[],&[]);
            let (first,second) = if strict_first { (strict,public) } else { (public,strict) };
            let endpoint = endpoint(server(&probe,false,true).with_scope_authorization(first).unwrap()
                .with_scope_authorization(second).unwrap());
            denied(&dispatch(&cx,&endpoint,call(&probe,1)).await,1,&probe);
        }
        assert_eq!(probe.snapshot(),(2,0,0,0));
    });
}

#[test]
fn http_scope_admission_anonymous_access_requires_an_explicit_empty_rule() {
    run(|cx| async move {
        let probe = Probe::new(&[]);
        let allowed = endpoint(server(&probe,false,false).with_scope_authorization(policy(&[],&[])).unwrap());
        let blocked = endpoint(server(&probe,false,false).with_scope_authorization(policy(&[],&["read"])).unwrap());
        let mut anonymous = call(&probe,1);
        anonymous.headers.remove("authorization");
        success(&dispatch(&cx,&allowed,anonymous.clone()).await);
        assert_eq!(probe.snapshot(),(0,1,0,1));
        denied(&dispatch(&cx,&blocked,anonymous).await,1,&probe);
        assert_eq!(probe.snapshot(),(0,1,0,1));
    });
}

#[test]
fn http_scope_admission_does_not_cache_a_prior_credential_grant_after_revocation() {
    run(|cx| async move {
        let probe = Probe::new(&["admin"]);
        let endpoint = endpoint(server(&probe,false,true).with_scope_authorization(policy(&[("admin","read")],&["read"])).unwrap());
        success(&dispatch(&cx,&endpoint,call(&probe,1)).await);
        assert_eq!(probe.snapshot(),(1,1,0,1));
        assert!(probe.verifier.revoke_token(&probe.token).unwrap());
        let response = dispatch(&cx,&endpoint,call(&probe,2)).await;
        assert_eq!(response.status.0,401);
        assert!(response.headers.contains_key("www-authenticate"));
        assert_eq!(probe.counts.auth.load(Ordering::SeqCst),2);
        assert_eq!(probe.counts.middleware.load(Ordering::SeqCst),1);
        assert_eq!(probe.counts.handler.load(Ordering::SeqCst),1);
    });
}

mod named_operation_tests {
    use super::*;
    use std::time::Duration;
    use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
    use fastmcp_server::http_admission::security::HttpSecurityPolicy;
    use fastmcp_server::http_admission::security::endpoint::{SecuredHttpEndpointError, revalidation::{SseAuthorizationError, SseRevalidationPolicy}};
    use fastmcp_server::http_admission::security::scope_policy::request::operation::{OperationScopePolicy, ScopedOperation};

    const PRIVATE_TOOL: &str = "private_write_probe";
    struct OtherTool(Probe);
    impl ToolHandler for OtherTool {
        fn definition(&self) -> Tool {
            let mut definition = self.0.definition();
            definition.name = PRIVATE_TOOL.to_owned();
            definition
        }
        fn call(&self, ctx: &McpContext, arguments: Value) -> McpResult<Vec<Content>> {
            self.0.call(ctx, arguments)
        }
    }
    fn named_server(probe: &Probe, cached: bool) -> Server {
        Server::new("named-operation-test", "1")
            .protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
            .auth_provider(probe.clone())
            .middleware(ApplicationMiddleware { probe: probe.clone(), cached })
            .tool(probe.clone()).tool(OtherTool(probe.clone())).build()
    }
    fn named_rules() -> OperationScopePolicy {
        OperationScopePolicy::new(10, policy(&[], &["invoke"]), vec![
            (ScopedOperation::ToolCall(TOOL.into()), required(&["read"])),
            (ScopedOperation::ToolCall(PRIVATE_TOOL.into()), required(&["write"])),
        ]).unwrap()
    }
    fn named_call(probe: &Probe, id: i64, name: &str, sse: bool) -> HttpRequest {
        request(probe, id, "tools/call", json!({"name":name,"arguments":{}}))
            .with_header("mcp-name", name)
            .with_header("accept", if sse { "text/event-stream" } else { "application/json" })
    }
    fn native_policy(revalidate: bool) -> HttpSecurityPolicy {
        let policy = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://scope.example", vec![],
        ).unwrap().with_scope_authorization(ScopeRequestPolicy::for_operations(named_rules()).unwrap()).unwrap();
        if revalidate {
            policy.with_sse_revalidation(SseRevalidationPolicy::new(
                Duration::from_millis(25), Duration::from_millis(25), 64,
            ).unwrap()).unwrap()
        } else { policy }
    }
    async fn native_immediate(cx: &Cx, endpoint: &ServerHttpEndpoint, policy: &HttpSecurityPolicy, request: HttpRequest) -> HttpResponse {
        let response = Box::pin(endpoint.handle_secured_async(cx, policy, request)).await.unwrap();
        let (response, mut stream) = response.into_parts();
        let streaming = stream.is_some();
        if let Some(stream) = &mut stream { stream.close(cx).await; }
        assert!(!streaming, "JSON or refused operation must not allocate an SSE response");
        response
    }

    #[test]
    fn named_permissions_precede_real_handlers_and_short_circuit_caches() {
        run(|cx| async move {
            for cached in [false, true] {
                for adapted in [false, true] {
                    let probe = Probe::new(&["invoke", "read"]);
                    let server = named_server(&probe, cached);
                    let server = if adapted {
                        server.with_scope_authorization(ScopeRequestPolicy::for_operations(named_rules()).unwrap()).unwrap()
                    } else { server.with_operation_scope_authorization(named_rules()).unwrap() };
                    let endpoint = endpoint(server);
                    let first = success(&dispatch(&cx, &endpoint, named_call(&probe, 1, TOOL, false)).await);
                    if cached { assert_eq!(first["content"][0]["text"], "cache-hit"); }
                    else {
                        let facts: Value = serde_json::from_str(first["content"][0]["text"].as_str().unwrap()).unwrap();
                        assert_eq!(facts, json!({"scopes":["invoke","read"],"claims":{"scope":"invoke read"}}));
                    }
                    let effects = usize::from(!cached);
                    assert_eq!(probe.snapshot(), (1, 1, 0, effects));
                    let forbidden = denied(&dispatch(&cx, &endpoint, named_call(&probe, 2, PRIVATE_TOOL, false)).await, 2, &probe);
                    let unknown = denied(&dispatch(&cx, &endpoint, named_call(&probe, 3, "unregistered_tool", false)).await, 3, &probe);
                    assert_eq!(unknown, forbidden, "existing and absent forbidden targets have the same error");
                    assert_eq!(probe.snapshot(), (3, 1, 0, effects));
                    let mut spoofed = named_call(&probe, 4, PRIVATE_TOOL, false);
                    let mut document: Value = serde_json::from_slice(&spoofed.body).unwrap();
                    document["params"]["arguments"] = json!({"name":TOOL,"scopes":["write"]});
                    document["params"]["_meta"]["com.example/operation"] = json!({"name":TOOL,"requiredScopes":[]});
                    spoofed.body = serde_json::to_vec(&document).unwrap();
                    assert_eq!(denied(&dispatch(&cx, &endpoint, spoofed).await, 4, &probe), forbidden);
                    assert_eq!(probe.snapshot(), (4, 1, 0, effects));
                    assert_eq!(success(&dispatch(&cx, &endpoint, named_call(&probe, 5, TOOL, false)).await), first);
                    assert_eq!(probe.snapshot(), (5, 2, 0, 2 * effects));
                }
            }
        });
    }

    #[test]
    fn native_named_json_and_sse_denials_precede_dispatch_and_preserve_valid_reuse() {
        run(|cx| async move {
            let probe = Probe::new(&["invoke", "read"]);
            let endpoint = endpoint(named_server(&probe, false));
            let policy = native_policy(false);
            let baseline = success(&native_immediate(&cx, &endpoint, &policy, named_call(&probe, 1, TOOL, false)).await);
            assert_eq!(probe.snapshot(), (1, 1, 0, 1));
            let mut id = 2;
            for sse in [false, true] {
                let forbidden = native_immediate(&cx, &endpoint, &policy, named_call(&probe, id, PRIVATE_TOOL, sse)).await;
                id += 1;
                let unknown = native_immediate(&cx, &endpoint, &policy, named_call(&probe, id, "unregistered_tool", sse)).await;
                id += 1;
                assert_eq!(forbidden.status.0, 403);
                assert_eq!(unknown.status.0, 403);
                assert_eq!(unknown.headers, forbidden.headers);
                assert_eq!(unknown.body, forbidden.body);
                assert!(forbidden.body.is_empty());
                assert_eq!(forbidden.headers["www-authenticate"], "Bearer error=\"insufficient_scope\"");
                assert_eq!(forbidden.headers["cache-control"], "no-store");
                assert!(!forbidden.headers.contains_key("transfer-encoding"));
                assert!(!format!("{:?}", forbidden.headers).contains(PRIVATE_TOOL));
            }
            assert_eq!(probe.snapshot(), (5, 1, 0, 1));
            assert_eq!(success(&native_immediate(&cx, &endpoint, &policy, named_call(&probe, 6, TOOL, false)).await), baseline);
            assert_eq!(probe.snapshot(), (6, 2, 0, 2));
            assert!(probe.verifier.revoke_token(&probe.token).unwrap());
            let revoked = native_immediate(&cx, &endpoint, &policy, named_call(&probe, 7, TOOL, false)).await;
            assert_eq!(revoked.status.0, 401);
            assert_eq!(probe.snapshot(), (7, 2, 0, 2));
        });
    }

    #[test]
    fn operation_scoped_sse_delivers_valid_results_but_withholds_revoked_credentials() {
        run(|cx| async move {
            for revoked in [false, true] {
                let probe = Probe::new(&["invoke", "read"]);
                let endpoint = endpoint(named_server(&probe, false));
                let policy = native_policy(true);
                let response = Box::pin(endpoint.handle_secured_async(&cx, &policy, named_call(&probe, 1, TOOL, true))).await.unwrap();
                assert_eq!(response.response().status.0, 200);
                let (_, stream) = response.into_parts();
                let mut stream = stream.expect("authorized operation produces a guarded SSE response");
                assert!(stream.stream().is_none(), "raw body access cannot bypass authorization");
                asupersync::time::timeout(cx.now(), Duration::from_secs(3), async {
                    while probe.counts.handler.load(Ordering::SeqCst) == 0 {
                        asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
                    }
                }).await.expect("registered tool must execute within the fixture bound");
                if revoked { assert!(probe.verifier.revoke_token(&probe.token).unwrap()); }
                asupersync::time::sleep(cx.now(), Duration::from_millis(40)).await;
                let event = asupersync::time::timeout(cx.now(), Duration::from_secs(3), stream.next_event(&cx))
                    .await.expect("bounded SSE authorization wait");
                if revoked {
                    assert!(matches!(event, Err(SecuredHttpEndpointError::Revalidation(SseAuthorizationError::Rejected))));
                } else {
                    let bytes = event.unwrap().unwrap().to_bytes().unwrap();
                    let wire = String::from_utf8(bytes).unwrap();
                    let data = wire.lines().filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start).collect::<Vec<_>>().join("\n");
                    let message: Value = serde_json::from_str(&data).unwrap();
                    assert_eq!(message["id"], 1);
                    assert!(message.get("error").is_none());
                    let facts: Value = serde_json::from_str(message["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
                    assert_eq!(facts["scopes"], json!(["invoke", "read"]));
                    assert!(asupersync::time::timeout(cx.now(), Duration::from_secs(3), stream.next_event(&cx))
                        .await.unwrap().unwrap().is_none());
                }
                assert!(probe.counts.auth.load(Ordering::SeqCst) >= 2, "fresh provider verdict is required after interval expiry");
                assert_eq!(probe.counts.handler.load(Ordering::SeqCst), 1);
                stream.close(&cx).await;
                assert!(cx.checkpoint().is_ok());
            }
        });
    }
}
