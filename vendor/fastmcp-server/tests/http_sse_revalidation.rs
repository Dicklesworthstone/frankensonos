//! Public secured SSE ownership and revalidation, including a loopback listener.
//! No test-only server implementation is used. The native test uses plain HTTP
//! behind a configured public authority; it does not verify TLS or a browser.

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

use std::future::{Future, poll_fn};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asupersync::{Cx, io::{AsyncReadExt, AsyncWriteExt}, types::CancelKind};
use fastmcp_core::{AuthContext, McpContext, McpResult};
use fastmcp_protocol::{Content, FINAL_PROTOCOL_VERSION, Tool, protocol_policy::ProtocolPolicy};
use fastmcp_server::{AuthProvider, AuthRequest, HttpServerShutdown, Server, ServerHttpEndpoint,
    StaticTokenVerifier, TokenAuthProvider, ToolHandler};
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::HttpSecurityPolicy;
use fastmcp_server::http_admission::security::endpoint::{SecuredHttpSseResponse, SecuredHttpEndpointError};
use fastmcp_server::http_admission::security::endpoint::revalidation::{SseRevalidationPolicy, SseAuthorizationError};
use fastmcp_server::http_admission::security::scope_policy::{RequiredScopes, ScopeImplicationPolicy};
use fastmcp_server::http_admission::security::scope_policy::request::ScopeRequestPolicy;
use fastmcp_transport::http::{HttpMethod, HttpRequest};
use serde_json::{Value, json};

const TOOL: &str = "revalidation_probe";
const INTERVAL: Duration = Duration::from_millis(100);
const WAIT: Duration = Duration::from_millis(150);

#[derive(Clone)]
struct Probe {
    tokens: [String; 2],
    verifier: StaticTokenVerifier,
    checks: Arc<AtomicUsize>,
    change: Arc<AtomicUsize>,
}
impl Probe {
    fn new() -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let tokens = [format!("lease-a-{}-{nonce}", std::process::id()), format!("lease-b-{}-{nonce}", std::process::id())];
        let facts = |index| {
            let mut facts = AuthContext::with_subject(format!("lease-owner-{nonce}-{index}"));
            facts.scopes = vec!["read".to_owned()];
            facts.claims = Some(json!({"tenant":"original"}));
            facts
        };
        let verifier = StaticTokenVerifier::new([
            (tokens[0].clone(), facts(0)), (tokens[1].clone(), facts(1)),
        ]).unwrap();
        Self { tokens, verifier, checks:Arc::new(AtomicUsize::new(0)), change:Arc::new(AtomicUsize::new(0)) }
    }
    fn calls(&self) -> usize { self.checks.load(Ordering::SeqCst) }
}
impl AuthProvider for Probe {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        let mut facts = TokenAuthProvider::new(self.verifier.clone()).authenticate(ctx, request)?;
        match self.change.load(Ordering::SeqCst) {
            1 => facts.scopes.push("write".to_owned()),
            2 => facts.claims = Some(json!({"tenant":"changed"})),
            3 => facts.subject = Some("replacement-principal".to_owned()),
            _ => {},
        }
        Ok(facts)
    }
}
impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool { name:TOOL.to_owned(), description:None, input_schema:json!({"type":"object"}),
            output_schema:None, icon:None, version:None, tags:vec![], annotations:None }
    }
    fn call(&self, _: &McpContext, _: Value) -> McpResult<Vec<Content>> { Ok(vec![Content::text("unchanged")]) }
}
fn server(probe: &Probe) -> Server {
    Server::new("lease-integration", "1").protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
        .auth_provider(probe.clone()).tool(probe.clone()).build()
}
fn endpoint(probe: &Probe) -> ServerHttpEndpoint {
    #[cfg(not(feature = "legacy-2024-11-05"))]
    let endpoint = server(probe).into_http_endpoint();
    #[cfg(feature = "legacy-2024-11-05")]
    let endpoint = server(probe).into_http_endpoint("http://lease.example");
    endpoint.unwrap()
}
fn policy(checks: usize) -> HttpSecurityPolicy {
    let scopes = ScopeRequestPolicy::new(1, ScopeImplicationPolicy::exact(1).unwrap(),
        ["tools/list", "subscriptions/listen"].into_iter().map(|method| (
            method.to_owned(), RequiredScopes::new(vec!["read".to_owned()]).unwrap(),
        )).collect()).unwrap();
    HttpSecurityPolicy::new(
        HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32,8192,65536).unwrap()).unwrap(),
        "https://lease.example", vec![],
    ).unwrap().with_scope_authorization(scopes).unwrap()
        .with_sse_revalidation(SseRevalidationPolicy::new(INTERVAL,INTERVAL,checks).unwrap()).unwrap()
}
fn request(probe: &Probe, token: usize, listen: bool) -> HttpRequest {
    let method = if listen { "subscriptions/listen" } else { "tools/list" };
    let mut params = if listen { json!({"notifications":{"toolsListChanged":true}}) } else { json!({}) };
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities":{},
    });
    HttpRequest::new(HttpMethod::Post,"/mcp")
        .with_header("host","lease.example")
        .with_header("authorization",format!("Bearer {}",probe.tokens[token]))
        .with_header("content-type","application/json").with_header("accept","text/event-stream")
        .with_header("mcp-protocol-version",FINAL_PROTOCOL_VERSION).with_header("mcp-method",method)
        .with_body(serde_json::to_vec(&json!({"jsonrpc":"2.0","id":7,"method":method,"params":params})).unwrap())
}
async fn open(cx: &Cx, endpoint: &ServerHttpEndpoint, policy: &HttpSecurityPolicy, request: HttpRequest) -> SecuredHttpSseResponse {
    let response = Box::pin(endpoint.handle_secured_async(cx,policy,request)).await.unwrap();
    assert_eq!(response.response().status.0,200);
    let (_,stream) = response.into_parts();
    stream.expect("native SSE response")
}
async fn next(cx: &Cx, stream: &mut SecuredHttpSseResponse) -> Result<Option<fastmcp_transport::sse::SseEvent>,SecuredHttpEndpointError> {
    asupersync::time::timeout(cx.now(),Duration::from_secs(3),stream.next_event(cx)).await.expect("bounded event/authorization wait")
}
fn encoded(event: fastmcp_transport::sse::SseEvent) -> String {
    String::from_utf8(event.to_bytes().unwrap()).unwrap()
}
fn run<F,Fut>(scenario: F)
where F: FnOnce(Cx)->Fut+Send+'static, Fut: Future<Output=()>+Send+'static {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .blocking_threads(1,4).build().unwrap().block_on(async move {
            let cx = Cx::current().unwrap();
            let mut task = cx.spawn(scenario).unwrap();
            task.join(&cx).await.unwrap();
        });
}

#[test]
fn revalidated_public_sse_delivers_the_real_catalog_without_raw_body_access() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let mut stream = open(&cx,&endpoint,&policy(64),request(&probe,0,false)).await;
        assert!(stream.stream().is_none(),"guarded body cannot be extracted");
        let before = probe.calls();
        asupersync::time::sleep(cx.now(),WAIT).await;
        let event = encoded(next(&cx,&mut stream).await.unwrap().unwrap());
        assert!(event.contains(TOOL));
        assert!(event.contains("\"resultType\":\"complete\""));
        assert!(probe.calls()>before,"a current provider verdict is required");
        assert!(next(&cx,&mut stream).await.unwrap().is_none());
        stream.close(&cx).await;
        assert!(cx.checkpoint().is_ok());
    });
}

#[test]
fn revoked_credential_withholds_an_already_queued_terminal_result() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let mut stream = open(&cx,&endpoint,&policy(64),request(&probe,0,false)).await;
        assert!(probe.verifier.revoke_token(&probe.tokens[0]).unwrap());
        asupersync::time::sleep(cx.now(),WAIT).await;
        assert!(matches!(next(&cx,&mut stream).await,
            Err(SecuredHttpEndpointError::Revalidation(SseAuthorizationError::Rejected))));
        assert!(matches!(next(&cx,&mut stream).await,Err(SecuredHttpEndpointError::BodyClosed)));
        assert!(stream.stream().is_none());
        stream.close(&cx).await;
        assert!(cx.checkpoint().is_ok());
    });
}

#[test]
fn public_sse_refuses_changed_grants_claims_and_identity_without_adopting_them() {
    run(|cx| async move {
        for change in [1,2,3] {
            let probe = Probe::new();
            let endpoint = endpoint(&probe);
            let mut stream = open(&cx,&endpoint,&policy(64),request(&probe,0,false)).await;
            probe.change.store(change,Ordering::SeqCst);
            asupersync::time::sleep(cx.now(),WAIT).await;
            assert!(matches!(next(&cx,&mut stream).await,
                Err(SecuredHttpEndpointError::Revalidation(SseAuthorizationError::FactsChanged))));
            probe.change.store(0,Ordering::SeqCst);
            assert!(matches!(next(&cx,&mut stream).await,Err(SecuredHttpEndpointError::BodyClosed)));
            stream.close(&cx).await;
        }
    });
}

#[test]
fn idle_public_subscription_revalidates_without_an_application_notification() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let mut stream = open(&cx,&endpoint,&policy(64),request(&probe,0,true)).await;
        let ack = encoded(next(&cx,&mut stream).await.unwrap().unwrap());
        assert!(ack.contains("notifications/subscriptions/acknowledged"));
        assert!(probe.verifier.revoke_token(&probe.tokens[0]).unwrap());
        assert!(matches!(next(&cx,&mut stream).await,
            Err(SecuredHttpEndpointError::Revalidation(SseAuthorizationError::Rejected))));
        stream.close(&cx).await;
        assert!(cx.checkpoint().is_ok());
    });
}

#[test]
fn revoking_one_response_does_not_cancel_a_sibling_credential_or_session() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let mut first = open(&cx,&endpoint,&policy(64),request(&probe,0,false)).await;
        let mut second = open(&cx,&endpoint,&policy(64),request(&probe,1,false)).await;
        assert!(probe.verifier.revoke_token(&probe.tokens[0]).unwrap());
        asupersync::time::sleep(cx.now(),WAIT).await;
        assert!(next(&cx,&mut first).await.is_err());
        first.close(&cx).await;
        assert!(encoded(next(&cx,&mut second).await.unwrap().unwrap()).contains(TOOL));
        second.close(&cx).await;
        assert!(cx.checkpoint().is_ok());
    });
}

#[test]
fn exhausted_revalidation_budget_closes_instead_of_leaving_an_unchecked_subscription() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let mut stream = open(&cx,&endpoint,&policy(1),request(&probe,0,true)).await;
        assert!(encoded(next(&cx,&mut stream).await.unwrap().unwrap()).contains("subscriptions/acknowledged"));
        assert!(matches!(next(&cx,&mut stream).await,
            Err(SecuredHttpEndpointError::Revalidation(SseAuthorizationError::CheckLimit))));
        assert_eq!(probe.calls(),2,"one opening verdict and one revalidation only");
        stream.close(&cx).await;
    });
}

#[test]
fn abandoning_a_polled_public_read_retires_its_body_without_reusing_the_lease() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let mut stream = open(&cx,&endpoint,&policy(64),request(&probe,0,true)).await;
        assert!(next(&cx,&mut stream).await.unwrap().is_some());
        {
            let mut pending = std::pin::pin!(stream.next_event(&cx));
            poll_fn(|task| {
                assert!(pending.as_mut().poll(task).is_pending());
                Poll::Ready(())
            }).await;
        }
        assert!(matches!(next(&cx,&mut stream).await,Err(SecuredHttpEndpointError::BodyClosed)));
        stream.close(&cx).await;
        assert!(cx.checkpoint().is_ok());
    });
}

fn wire(request: HttpRequest) -> Vec<u8> {
    let mut bytes = format!("{} {} HTTP/1.1\r\n",request.method.as_str(),request.path).into_bytes();
    for (name,value) in request.headers {
        if !name.eq_ignore_ascii_case("content-length") && !name.eq_ignore_ascii_case("connection") {
            bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }
    bytes.extend_from_slice(format!("content-length: {}\r\nconnection: close\r\n\r\n",request.body.len()).as_bytes());
    bytes.extend_from_slice(&request.body);
    bytes
}

#[test]
fn idle_native_socket_revocation_closes_only_the_original_stream_without_a_success_terminal() {
    run(|cx| async move {
        let probe = Probe::new();
        let bound = Box::pin(server(&probe).bind_secured_http(&cx,"127.0.0.1:0",policy(64))).await.unwrap();
        let address = bound.local_addr().unwrap();
        let (sender,mut receiver) = asupersync::channel::oneshot::channel();
        let mut serving = cx.spawn(move |server_cx| async move {
            let _ = sender.send_blocking(server_cx.clone());
            Box::pin(bound.serve(&server_cx)).await
        }).unwrap();
        let server_cx = receiver.recv(&cx).await.unwrap();
        // Keep all network waits bounded and always settle the listener before
        // interpreting scenario results, including an I/O or timeout failure.
        let result = asupersync::time::timeout(cx.now(),Duration::from_secs(8),async {
            let mut stream = asupersync::net::TcpStream::connect(address).await.map_err(|_|"connect")?;
            stream.write_all(&wire(request(&probe,0,true))).await.map_err(|_|"write")?;
            stream.flush().await.map_err(|_|"flush")?;
            let mut received = Vec::new();
            let mut chunk = [0_u8;4096];
            loop {
                let count = stream.read(&mut chunk).await.map_err(|_|"ack read")?;
                if count==0 { return Err("stream ended before acknowledgment"); }
                received.extend_from_slice(&chunk[..count]);
                if received.len()>65536 { return Err("response bound"); }
                if String::from_utf8_lossy(&received).contains("notifications/subscriptions/acknowledged") { break; }
            }
            let before = probe.calls();
            while probe.calls()==before { asupersync::time::sleep(cx.now(),Duration::from_millis(5)).await; }
            let successful_check = probe.calls();
            if !probe.verifier.revoke_token(&probe.tokens[0]).map_err(|_|"revoke")? { return Err("token missing"); }
            loop {
                let count = stream.read(&mut chunk).await.map_err(|_|"closure read")?;
                if count==0 { break; }
                received.extend_from_slice(&chunk[..count]);
                if received.len()>65536 { return Err("response bound"); }
            }
            let rejected_check = probe.calls();
            let mut sibling = asupersync::net::TcpStream::connect(address).await.map_err(|_|"sibling connect")?;
            sibling.write_all(&wire(request(&probe,1,false).with_header("accept","application/json")))
                .await.map_err(|_|"sibling write")?;
            sibling.flush().await.map_err(|_|"sibling flush")?;
            let mut surviving = Vec::new();
            loop {
                let count = sibling.read(&mut chunk).await.map_err(|_|"sibling read")?;
                if count==0 { break; }
                surviving.extend_from_slice(&chunk[..count]);
                if surviving.len()>65536 { return Err("sibling response bound"); }
            }
            Ok((received,surviving,successful_check,rejected_check))
        }).await;
        server_cx.cancel_with(CancelKind::User,Some("SSE revalidation test complete"));
        let shutdown = serving.join(&cx).await.unwrap().unwrap();
        if let HttpServerShutdown::Nonquiescent(shutdown) = shutdown { shutdown.settle(&cx).await.unwrap(); }
        let (received,surviving,before,after) = result.expect("bounded socket test").expect("native socket scenario");
        let received = String::from_utf8(received).unwrap();
        assert!(received.starts_with("HTTP/1.1 200"));
        assert!(received.contains("notifications/subscriptions/acknowledged"));
        assert!(!received.contains("\"resultType\""),"revocation must not manufacture a success terminal");
        assert!(!received.ends_with("0\r\n\r\n"),"failed stream cannot claim clean chunked completion");
        assert!(after>before,"credential was actually revalidated after revocation");
        let surviving = String::from_utf8(surviving).unwrap();
        assert!(surviving.starts_with("HTTP/1.1 200"));
        assert!(surviving.contains(TOOL));
        assert!(!surviving.contains("\"error\":"));
        assert!(cx.checkpoint().is_ok());
    });
}

mod resource_watch_tests {
    use super::*;
    use fastmcp_protocol::{JsonRpcRequest, Resource, ResourceContent};
    use fastmcp_server::{Middleware, MiddlewareDecision, ResourceHandler};
    use fastmcp_server::http_admission::security::scope_policy::request::operation::{OperationScopePolicy, ScopedOperation};
    use fastmcp_transport::http::HttpResponse;

    const A: &str = "watch://resource/a";
    const B: &str = "watch://resource/b";
    const EMIT: &str = "emit_resource_update";

    #[derive(Default)]
    struct Effects { middleware: AtomicUsize, emissions: AtomicUsize }
    struct Observe(Arc<Effects>);
    impl Middleware for Observe {
        fn on_request(&self, _: &McpContext, _: &JsonRpcRequest) -> McpResult<MiddlewareDecision> {
            self.0.middleware.fetch_add(1, Ordering::SeqCst);
            Ok(MiddlewareDecision::Continue)
        }
    }
    struct Emitter(Arc<Effects>);
    impl ToolHandler for Emitter {
        fn definition(&self) -> Tool {
            Tool { name: EMIT.into(), description: None, input_schema: json!({"type":"object","properties":{"uri":{"type":"string"}},"required":["uri"]}),
                output_schema: None, icon: None, version: None, tags: vec![], annotations: None }
        }
        fn call(&self, ctx: &McpContext, arguments: Value) -> McpResult<Vec<Content>> {
            let uri = arguments.get("uri").and_then(Value::as_str)
                .ok_or_else(|| fastmcp_core::McpError::invalid_params("URI required"))?;
            self.0.emissions.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Content::text(if ctx.notify_resource_updated(uri) { "notified" } else { "silent" })])
        }
    }
    struct WatchedResource(&'static str);
    impl ResourceHandler for WatchedResource {
        fn definition(&self) -> Resource {
            Resource { uri: self.0.into(), name: self.0.into(), description: None,
                mime_type: Some("text/plain".into()), icon: None, version: None, tags: vec![] }
        }
        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            ctx.checkpoint()?;
            Ok(vec![ResourceContent { uri: self.0.into(), mime_type: Some("text/plain".into()), text: Some("resource".into()), blob: None }])
        }
    }
    fn watch_endpoint(probe: &Probe, effects: &Arc<Effects>) -> ServerHttpEndpoint {
        let server = Server::new("resource-watch", "1").protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
            .auth_provider(probe.clone()).middleware(Observe(Arc::clone(effects)))
            .tool(Emitter(Arc::clone(effects))).resource(WatchedResource(A)).resource(WatchedResource(B)).build();
        #[cfg(not(feature = "legacy-2024-11-05"))]
        let endpoint = server.into_http_endpoint();
        #[cfg(feature = "legacy-2024-11-05")]
        let endpoint = server.into_http_endpoint("http://lease.example");
        endpoint.unwrap()
    }
    fn watch_policy() -> HttpSecurityPolicy {
        let required = |name: &str| RequiredScopes::new(vec![name.to_owned()]).unwrap();
        let base = ScopeRequestPolicy::new(1, ScopeImplicationPolicy::exact(1).unwrap(), vec![
            ("subscriptions/listen".into(), required("read")), ("tools/call".into(), required("read")),
        ]).unwrap();
        let operations = OperationScopePolicy::new(1, base, vec![
            (ScopedOperation::ResourceWatch(A.into()), required("read")),
            (ScopedOperation::ResourceWatch(B.into()), required("write")),
            (ScopedOperation::ToolCall(EMIT.into()), required("read")),
        ]).unwrap();
        HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32,8192,65536).unwrap()).unwrap(),
            "https://lease.example", vec![],
        ).unwrap().with_scope_authorization(ScopeRequestPolicy::for_operations(operations).unwrap()).unwrap()
            .with_sse_revalidation(SseRevalidationPolicy::new(INTERVAL,INTERVAL,64).unwrap()).unwrap()
    }
    fn watch_request(probe: &Probe, token: usize, resources: &[&str]) -> HttpRequest {
        let mut request = super::request(probe, token, true);
        let mut body: Value = serde_json::from_slice(&request.body).unwrap();
        body["params"]["notifications"] = json!({"resourceSubscriptions":resources});
        request.body = serde_json::to_vec(&body).unwrap();
        request
    }
    fn emit_request(probe: &Probe, uri: &str) -> HttpRequest {
        let mut request = super::request(probe, 1, false)
            .with_header("accept", "application/json").with_header("mcp-method", "tools/call")
            .with_header("mcp-name", EMIT);
        let mut body: Value = serde_json::from_slice(&request.body).unwrap();
        body["id"] = json!(8);
        body["method"] = json!("tools/call");
        body["params"]["name"] = json!(EMIT);
        body["params"]["arguments"] = json!({"uri":uri});
        request.body = serde_json::to_vec(&body).unwrap();
        request
    }
    async fn immediate(cx: &Cx, endpoint: &ServerHttpEndpoint, policy: &HttpSecurityPolicy, request: HttpRequest) -> HttpResponse {
        let response = asupersync::time::timeout(cx.now(), Duration::from_secs(3),
            Box::pin(endpoint.handle_secured_async(cx, policy, request))).await.unwrap().unwrap();
        let (response, mut stream) = response.into_parts();
        let streaming = stream.is_some();
        if let Some(stream) = &mut stream { stream.close(cx).await; }
        assert!(!streaming, "refused selection must not become a partial SSE subscription");
        response
    }
    async fn emit(cx: &Cx, endpoint: &ServerHttpEndpoint, policy: &HttpSecurityPolicy, probe: &Probe, uri: &str) -> String {
        let response = immediate(cx, endpoint, policy, emit_request(probe, uri)).await;
        assert_eq!(response.status.0, 200);
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert!(body.get("error").is_none());
        body["result"]["content"][0]["text"].as_str().unwrap().to_owned()
    }
    fn document(event: fastmcp_transport::sse::SseEvent) -> Value {
        let wire = encoded(event);
        let data = wire.lines().filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start).collect::<Vec<_>>().join("\n");
        serde_json::from_str(&data).unwrap()
    }
    async fn acknowledged(cx: &Cx, stream: &mut SecuredHttpSseResponse) {
        let ack = document(next(cx, stream).await.unwrap().unwrap());
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
        assert_eq!(ack["params"]["notifications"]["resourceSubscriptions"], json!([A]));
        assert_eq!(ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"], 7);
    }

    #[test]
    fn mixed_resource_watch_denial_has_no_partial_subscription_and_valid_reuse_delivers() {
        run(|cx| async move {
            let probe = Probe::new();
            let effects = Arc::new(Effects::default());
            let endpoint = watch_endpoint(&probe, &effects);
            let policy = watch_policy();
            let denied = immediate(&cx, &endpoint, &policy, watch_request(&probe, 0, &[A,B])).await;
            assert_eq!(denied.status.0, 403);
            assert!(denied.body.is_empty());
            assert_eq!(denied.headers["www-authenticate"], "Bearer error=\"insufficient_scope\"");
            assert_eq!(denied.headers["cache-control"], "no-store");
            assert_eq!(probe.calls(), 1);
            assert_eq!(effects.middleware.load(Ordering::SeqCst), 0);
            assert_eq!(effects.emissions.load(Ordering::SeqCst), 0);
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, A).await, "silent",
                "denial must not install even the allowed prefix of the selection");
            let mut live = open(&cx, &endpoint, &policy, watch_request(&probe, 0, &[A])).await;
            acknowledged(&cx, &mut live).await;
            let before = effects.middleware.load(Ordering::SeqCst);
            let unknown = immediate(&cx, &endpoint, &policy, watch_request(&probe, 0, &[A,"watch://resource/unknown"])).await;
            assert_eq!(unknown.status.0, denied.status.0);
            assert_eq!(unknown.headers, denied.headers);
            assert_eq!(unknown.body, denied.body);
            assert_eq!(effects.middleware.load(Ordering::SeqCst), before);
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, B).await, "silent");
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, A).await, "notified");
            let event = document(next(&cx, &mut live).await.unwrap().unwrap());
            assert_eq!(event["method"], "notifications/resources/updated");
            assert_eq!(event["params"]["uri"], A);
            assert_eq!(event["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"], 7);
            live.close(&cx).await;
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, A).await, "silent");
            assert!(cx.checkpoint().is_ok());
        });
    }

    #[test]
    fn revoked_resource_watch_withholds_queued_updates_without_cancelling_a_sibling() {
        run(|cx| async move {
            let probe = Probe::new();
            let effects = Arc::new(Effects::default());
            let endpoint = watch_endpoint(&probe, &effects);
            let policy = watch_policy();
            let mut first = open(&cx, &endpoint, &policy, watch_request(&probe, 0, &[A])).await;
            acknowledged(&cx, &mut first).await;
            let mut sibling = open(&cx, &endpoint, &policy, watch_request(&probe, 1, &[A])).await;
            acknowledged(&cx, &mut sibling).await;
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, A).await, "notified");
            assert!(probe.verifier.revoke_token(&probe.tokens[0]).unwrap());
            let before = probe.calls();
            asupersync::time::sleep(cx.now(), WAIT).await;
            assert!(matches!(next(&cx, &mut first).await,
                Err(SecuredHttpEndpointError::Revalidation(SseAuthorizationError::Rejected))));
            assert!(probe.calls() > before);
            first.close(&cx).await;
            let event = document(next(&cx, &mut sibling).await.unwrap().unwrap());
            assert_eq!(event["method"], "notifications/resources/updated");
            assert_eq!(event["params"]["uri"], A);
            assert_eq!(event["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"], 7);
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, A).await, "notified");
            assert_eq!(document(next(&cx, &mut sibling).await.unwrap().unwrap())["params"]["uri"], A);
            sibling.close(&cx).await;
            assert_eq!(emit(&cx, &endpoint, &policy, &probe, A).await, "silent");
            assert!(cx.checkpoint().is_ok());
        });
    }
}
