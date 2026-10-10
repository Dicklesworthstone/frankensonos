//! Native scope challenges through shipped embedding and loopback HTTP paths.
//! No cfg(test) server internals are used. The socket tests bind the secured
//! listener, including its separate JSON and SSE dispatch branches. They do
//! not establish TLS, browser interoperability, or continuous stream revocation.

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
use std::net::SocketAddr;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asupersync::Cx;
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::types::CancelKind;
use fastmcp_core::{AuthContext, McpContext, McpError, McpErrorCode, McpResult};
use fastmcp_protocol::{Content, FINAL_PROTOCOL_VERSION, JsonRpcRequest, Tool, protocol_policy::ProtocolPolicy};
use fastmcp_server::{
    AuthProvider, AuthRequest, HttpServerShutdown, Middleware, MiddlewareDecision,
    Server, ServerHttpEndpoint, StaticTokenVerifier, TokenAuthProvider, ToolHandler,
};
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::{HttpSecurityPolicy, resource_metadata::ProtectedResourceMetadata};
use fastmcp_server::http_admission::security::scope_policy::{RequiredScopes, ScopeImplicationPolicy};
use fastmcp_server::http_admission::security::scope_policy::request::ScopeRequestPolicy;
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpResponse, HttpStatus};
use serde_json::{Value, json};

const TOOL: &str = "native_scope_probe";
const ORIGIN: &str = "https://browser.example";

#[derive(Default)]
struct Counts { auth: AtomicUsize, middleware: AtomicUsize, handler: AtomicUsize }

#[derive(Clone)]
struct Probe {
    token: String,
    subject: String,
    grants: Vec<String>,
    verifier: StaticTokenVerifier,
    counts: Arc<Counts>,
}
impl Probe {
    fn new(grants: &[&str]) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let token = format!("native-scope-{}-{nonce}", std::process::id());
        let subject = format!("native-scope-owner-{nonce}");
        let grants: Vec<String> = grants.iter().map(|grant| (*grant).to_owned()).collect();
        let mut facts = AuthContext::with_subject(subject.clone());
        facts.scopes = grants.clone();
        facts.claims = Some(json!({"scope":grants.join(" ")}));
        let verifier = StaticTokenVerifier::new([(token.clone(), facts)]).unwrap();
        Self { token, subject, grants, verifier, counts: Arc::new(Counts::default()) }
    }
    fn counts(&self) -> (usize, usize, usize) {
        (self.counts.auth.load(Ordering::SeqCst), self.counts.middleware.load(Ordering::SeqCst),
            self.counts.handler.load(Ordering::SeqCst))
    }
    fn expected(&self) -> Value {
        json!({"subject":self.subject,"scopes":self.grants,"claims":{"scope":self.grants.join(" ")}})
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
        Ok(vec![Content::text(json!({
            "subject":facts.subject,"scopes":facts.scopes,"claims":facts.claims,
        }).to_string())])
    }
}
struct Application { probe: Probe, forged_refusal: bool }
impl Middleware for Application {
    fn on_request(&self, _: &McpContext, _: &JsonRpcRequest) -> McpResult<MiddlewareDecision> {
        self.probe.counts.middleware.fetch_add(1, Ordering::SeqCst);
        if self.forged_refusal {
            Err(McpError::new(McpErrorCode::ResourceForbidden, "insufficient_scope"))
        } else {
            Ok(MiddlewareDecision::Continue)
        }
    }
}
fn server(probe: &Probe, authenticated: bool, forged_refusal: bool) -> Server {
    let builder = Server::new("native-scope", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
        .middleware(Application { probe: probe.clone(), forged_refusal })
        .tool(probe.clone());
    if authenticated { builder.auth_provider(probe.clone()).build() } else { builder.build() }
}
fn endpoint(server: Server) -> ServerHttpEndpoint {
    #[cfg(not(feature = "legacy-2024-11-05"))]
    let result = server.into_http_endpoint();
    #[cfg(feature = "legacy-2024-11-05")]
    let result = server.into_http_endpoint("http://scope.example");
    result.unwrap()
}
fn policy(edges: &[(&str, &str)], required: &[&str]) -> HttpSecurityPolicy {
    let scopes = ScopeRequestPolicy::new(2, ScopeImplicationPolicy::new(1,
        edges.iter().map(|(a,b)| ((*a).to_owned(),(*b).to_owned())).collect()).unwrap(), vec![
        ("tools/call".to_owned(), RequiredScopes::new(required.iter().map(|scope| (*scope).to_owned()).collect()).unwrap()),
        ("tools/list".to_owned(), RequiredScopes::new(vec![]).unwrap()),
    ]).unwrap();
    HttpSecurityPolicy::new(
        HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
        "https://scope.example", vec![ORIGIN.to_owned()],
    ).unwrap().with_resource_metadata(ProtectedResourceMetadata::new(
        vec!["https://issuer.example/tenant".to_owned()],
    ).unwrap()).unwrap().with_scope_authorization(scopes).unwrap()
}
fn request(probe: &Probe, method: &str, mut params: Value, sse: bool) -> HttpRequest {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities":{},
    });
    let mut request = HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("host", "scope.example")
        .with_header("authorization", format!("Bearer {}", probe.token))
        .with_header("content-type", "application/json")
        .with_header("accept", if sse { "text/event-stream" } else { "application/json" })
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_header("mcp-method", method)
        .with_body(serde_json::to_vec(&json!({"jsonrpc":"2.0","id":17,"method":method,"params":params})).unwrap());
    if method == "tools/call" { request = request.with_header("mcp-name", TOOL); }
    request
}
fn call(probe: &Probe, sse: bool) -> HttpRequest {
    request(probe, "tools/call", json!({"name":TOOL,"arguments":{}}), sse)
}
async fn immediate(cx: &Cx, endpoint: &ServerHttpEndpoint, policy: &HttpSecurityPolicy, request: HttpRequest) -> HttpResponse {
    let response = Box::pin(endpoint.handle_secured_async(cx, policy, request)).await.unwrap();
    let (response, mut stream) = response.into_parts();
    let streamed = stream.is_some();
    if let Some(stream) = &mut stream { stream.close(cx).await; }
    assert!(!streamed, "denial or ordinary JSON must not allocate an SSE response");
    response
}
fn scope_denied(response: &HttpResponse, policy: &HttpSecurityPolicy, probe: &Probe, required: &str) {
    assert_eq!(response.status.0, 403);
    assert!(response.body.is_empty());
    assert_eq!(response.headers["cache-control"], "no-store");
    assert_eq!(response.headers["www-authenticate"], format!(
        "Bearer error=\"insufficient_scope\", scope=\"{required}\", resource_metadata=\"{}\"",
        policy.resource_metadata_url().unwrap(),
    ));
    assert!(!response.headers.contains_key("mcp-session-id"));
    assert!(!response.headers.contains_key("transfer-encoding"));
    assert!(!format!("{:?}", response.headers).contains(&probe.token));
    assert!(!format!("{:?}", response.headers).contains(&probe.subject));
}
fn authenticated_result(response: &HttpResponse) -> Value {
    assert_eq!(response.status.0, 200);
    assert!(!response.headers.contains_key("www-authenticate"));
    let message: Value = if response.headers.get("content-type").is_some_and(|value| value.starts_with("text/event-stream")) {
        let body = String::from_utf8(response.body.clone()).unwrap().replace("\r\n", "\n");
        let messages: Vec<Value> = body.split("\n\n").filter_map(|event| {
            let data = event.lines().filter_map(|line| line.strip_prefix("data:")).map(str::trim_start).collect::<Vec<_>>().join("\n");
            if data.is_empty() { None } else { Some(serde_json::from_str(&data).unwrap()) }
        }).filter(|message: &Value| message.get("id") == Some(&json!(17))).collect();
        assert_eq!(messages.len(), 1, "one terminal response per request");
        messages.into_iter().next().unwrap()
    } else {
        serde_json::from_slice(&response.body).unwrap()
    };
    assert_eq!(message["id"], 17);
    assert!(message.get("error").is_none(), "{message}");
    serde_json::from_str(message["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn run<F, Fut>(scenario: F)
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
fn native_scope_embedding_edge_denial_blocks_json_and_sse_then_reaccepts() {
    run(|cx| async move {
        let probe = Probe::new(&["admin"]);
        let endpoint = endpoint(server(&probe,true,false));
        let allowed = policy(&[("admin","write"),("write","read")], &["read","write"]);
        let denied = policy(&[("admin","write")], &["read","write"]);
        let baseline = immediate(&cx,&endpoint,&allowed,call(&probe,false)).await;
        assert_eq!(authenticated_result(&baseline),probe.expected());
        assert_eq!(probe.counts(),(1,1,1));
        for sse in [false,true] {
            let response = immediate(&cx,&endpoint,&denied,call(&probe,sse)).await;
            scope_denied(&response,&denied,&probe,"read write");
        }
        assert_eq!(probe.counts(),(3,1,1));
        let reaccepted = immediate(&cx,&endpoint,&allowed,call(&probe,false)).await;
        assert_eq!(authenticated_result(&reaccepted),probe.expected());
        assert_eq!(probe.counts(),(4,2,2));
    });
}

#[test]
fn native_scope_challenge_includes_satisfied_and_missing_requirements() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,true,false));
        let denied = policy(&[],&["write","read","write"]);
        scope_denied(&immediate(&cx,&endpoint,&denied,call(&probe,false)).await,&denied,&probe,"read write");
        assert_eq!(probe.counts(),(1,0,0));
        let accepted = immediate(&cx,&endpoint,&policy(&[],&["read"]),call(&probe,false)).await;
        assert_eq!(authenticated_result(&accepted),probe.expected());
        assert_eq!(probe.counts(),(2,1,1));
    });
}

#[test]
fn native_scope_invalid_missing_and_revoked_tokens_remain_authentication_failures() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,true,false));
        let policy = policy(&[],&["read"]);
        authenticated_result(&immediate(&cx,&endpoint,&policy,call(&probe,false)).await);
        assert!(probe.verifier.revoke_token(&probe.token).unwrap());
        let mut missing = call(&probe,false);
        missing.headers.remove("authorization");
        // A presented credential reaches the provider and fails there. A missing
        // Authorization header is refused before any provider call (75099bf1).
        for (request, reaches_provider) in [
            (call(&probe,false), true),
            (call(&probe,true).with_header("authorization","Bearer wrong-token"), true),
            (missing, false),
        ] {
            let before = probe.counts().0;
            let response = immediate(&cx,&endpoint,&policy,request).await;
            assert_eq!(response.status.0,401);
            let challenge = &response.headers["www-authenticate"];
            assert!(!challenge.contains("insufficient_scope"));
            assert!(!challenge.contains("scope=\""));
            assert!(challenge.contains("resource_metadata="));
            assert_eq!(probe.counts().0 - before, usize::from(reaches_provider));
        }
        assert_eq!(probe.counts(),(3,1,1));
    });
}

#[test]
fn native_scope_request_metadata_and_arguments_cannot_replace_policy() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,true,false));
        let policy = policy(&[],&["write"]);
        let baseline = immediate(&cx,&endpoint,&policy,call(&probe,false)).await;
        let mut injected = call(&probe,false);
        let mut body: Value = serde_json::from_slice(&injected.body).unwrap();
        body["params"]["_meta"]["com.example/scopePolicy"] = json!({"scopes":["write"],"implications":[["read","write"]]});
        body["params"]["arguments"] = json!({"scopes":["write"],"requiredScopes":[],"revision":999});
        injected.body = serde_json::to_vec(&body).unwrap();
        let response = immediate(&cx,&endpoint,&policy,injected).await;
        scope_denied(&response,&policy,&probe,"write");
        assert_eq!(response.headers,baseline.headers);
        assert_eq!(response.body,baseline.body);
        assert_eq!(probe.counts(),(2,0,0));
    });
}

#[test]
fn native_scope_anonymous_and_unconfigured_rules_do_not_disclose_permissions() {
    run(|cx| async move {
        let probe = Probe::new(&[]);
        let anonymous = endpoint(server(&probe,false,false));
        let mut request = call(&probe,false);
        request.headers.remove("authorization");
        let allowed = immediate(&cx,&anonymous,&policy(&[],&[]),request.clone()).await;
        assert_eq!(authenticated_result(&allowed),json!({"subject":null,"scopes":[],"claims":null}));
        let denied = immediate(&cx,&anonymous,&policy(&[],&["read"]),request).await;
        assert_eq!(denied.status.0,401);
        assert!(!denied.headers["www-authenticate"].contains("scope=\""));
        assert_eq!(probe.counts(),(0,1,1));
        let authenticated = endpoint(server(&probe,true,false));
        let unlisted = self::request(&probe,"resources/list",json!({}),false);
        let response = immediate(&cx,&authenticated,&policy(&[],&[]),unlisted).await;
        assert_eq!(response.status.0,403);
        assert!(response.body.is_empty());
        assert!(!response.headers.contains_key("www-authenticate"));
        assert_eq!(probe.counts(),(1,1,1));
    });
}

#[test]
fn native_scope_metadata_preflight_and_origin_rejection_stay_outside_authentication() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,true,false));
        let policy = policy(&[],&["write"]);
        let metadata = HttpRequest::new(HttpMethod::Get,policy.resource_metadata_path().unwrap())
            .with_header("host","scope.example").with_header("origin",ORIGIN);
        let preflight = HttpRequest::new(HttpMethod::Options,"/mcp")
            .with_header("host","scope.example").with_header("origin",ORIGIN)
            .with_header("access-control-request-method","POST")
            .with_header("access-control-request-headers","authorization, content-type, mcp-method, mcp-name, mcp-protocol-version");
        let public = immediate(&cx,&endpoint,&policy,metadata).await;
        assert_eq!(public.status.0,200);
        let document: Value = serde_json::from_slice(&public.body).unwrap();
        assert_eq!(document["resource"],"https://scope.example/mcp");
        assert_eq!(immediate(&cx,&endpoint,&policy,preflight).await.status.0,204);
        let rejected = immediate(&cx,&endpoint,&policy,
            call(&probe,false).with_header("origin","https://attacker.example").with_body(b"bad JSON".to_vec())).await;
        assert_eq!(rejected.status.0,403);
        assert!(!rejected.headers.contains_key("www-authenticate"));
        assert!(!rejected.headers.contains_key("access-control-allow-origin"));
        assert_eq!(probe.counts(),(0,0,0));
        scope_denied(&immediate(&cx,&endpoint,&policy,call(&probe,false)).await,&policy,&probe,"write");
        assert_eq!(probe.counts(),(1,0,0));
    });
}

#[test]
fn native_scope_does_not_reclassify_application_forbidden_errors_as_oauth() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let endpoint = endpoint(server(&probe,true,true));
        let response = immediate(&cx,&endpoint,&policy(&[],&["read"]),call(&probe,false)).await;
        assert_eq!(response.status.0,200);
        assert!(!response.headers.contains_key("www-authenticate"));
        let document: Value = serde_json::from_slice(&response.body).unwrap();
        assert!(document["error"].is_object());
        assert!(document.get("result").is_none());
        assert_eq!(probe.counts(),(1,1,0));
    });
}

// Real HTTP/1 exchanges, bounded independently of the server. Parse the
// response head and dechunk SSE before applying the same payload assertions.
async fn exchange(cx: &Cx, address: SocketAddr, request: HttpRequest) -> Result<Vec<u8>, String> {
    asupersync::time::timeout(cx.now(), Duration::from_secs(10), async move {
        let mut stream = asupersync::net::TcpStream::connect(address).await.map_err(|error| error.to_string())?;
        let mut bytes = format!("{} {} HTTP/1.1\r\n", request.method.as_str(), request.path).into_bytes();
        for (name,value) in request.headers {
            if !name.eq_ignore_ascii_case("content-length") && !name.eq_ignore_ascii_case("connection") {
                bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
            }
        }
        bytes.extend_from_slice(format!("content-length: {}\r\nconnection: close\r\n\r\n",request.body.len()).as_bytes());
        bytes.extend_from_slice(&request.body);
        stream.write_all(&bytes).await.map_err(|error| error.to_string())?;
        stream.flush().await.map_err(|error| error.to_string())?;
        let mut response = Vec::new();
        let mut chunk = [0_u8;4096];
        loop {
            let count = stream.read(&mut chunk).await.map_err(|error| error.to_string())?;
            if count == 0 { break; }
            if response.len() + count > 256 * 1024 { return Err("wire response exceeded test bound".to_owned()); }
            response.extend_from_slice(&chunk[..count]);
        }
        Ok(response)
    }).await.map_err(|_| "wire exchange timed out".to_owned())?
}
fn decode_wire(bytes: &[u8]) -> HttpResponse {
    let head_end = bytes.windows(4).position(|window| window == b"\r\n\r\n").expect("complete response head");
    let head = std::str::from_utf8(&bytes[..head_end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse::<u16>().unwrap();
    let mut response = HttpResponse::new(HttpStatus(status));
    // `HttpResponse::new` presets a JSON content-type; decode only what the wire carried.
    response.headers.clear();
    for line in lines {
        let (name,value) = line.split_once(':').unwrap();
        assert!(response.headers.insert(name.to_ascii_lowercase(),value.trim().to_owned()).is_none(),"duplicate response header {name}");
    }
    let mut body = &bytes[head_end+4..];
    if response.headers.get("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
        loop {
            let end = body.windows(2).position(|window| window == b"\r\n").unwrap();
            let size = usize::from_str_radix(std::str::from_utf8(&body[..end]).unwrap(),16).unwrap();
            body = &body[end+2..];
            if size == 0 { assert_eq!(body,b"\r\n"); break; }
            assert!(size <= body.len().saturating_sub(2));
            response.body.extend_from_slice(&body[..size]);
            assert_eq!(&body[size..size+2],b"\r\n");
            body = &body[size+2..];
        }
    } else {
        response.body = body.to_vec();
        if let Some(length) = response.headers.get("content-length") {
            assert_eq!(length.parse::<usize>().unwrap(),response.body.len());
        }
    }
    response
}
async fn wire_batch(cx: &Cx, server: Server, policy: HttpSecurityPolicy, requests: Vec<HttpRequest>) -> Vec<HttpResponse> {
    let bound = Box::pin(server.bind_secured_http(cx,"127.0.0.1:0",policy)).await.unwrap();
    let address = bound.local_addr().unwrap();
    let (sender,mut receiver) = asupersync::channel::oneshot::channel();
    let mut serving = cx.spawn(move |server_cx| async move {
        let _ = sender.send_blocking(server_cx.clone());
        Box::pin(bound.serve(&server_cx)).await
    }).unwrap();
    let server_cx = receiver.recv(cx).await.unwrap();
    // Delay assertions until after explicit listener/child settlement, including
    // on I/O failures. Cancelling this listener does not cancel the test parent.
    let result = async {
        let mut responses = Vec::new();
        for request in requests { responses.push(exchange(cx,address,request).await?); }
        Ok::<_,String>(responses)
    }.await;
    server_cx.cancel_with(CancelKind::User,Some("native scope socket scenario complete"));
    let shutdown = serving.join(cx).await.unwrap().unwrap();
    if let HttpServerShutdown::Nonquiescent(shutdown) = shutdown { shutdown.settle(cx).await.unwrap(); }
    result.unwrap().iter().map(|bytes| decode_wire(bytes)).collect()
}

#[test]
fn native_scope_socket_json_and_sse_one_edge_controls_retain_exact_authentication() {
    run(|cx| async move {
        let probe = Probe::new(&["admin"]);
        let allowed = policy(&[("admin","write"),("write","read")],&["read","write"]);
        let denied = policy(&[("admin","write")],&["read","write"]);
        let requests = || vec![call(&probe,false),call(&probe,true)];
        let baseline = wire_batch(&cx,server(&probe,true,false),allowed.clone(),requests()).await;
        for response in &baseline { assert_eq!(authenticated_result(response),probe.expected()); }
        assert_eq!(probe.counts(),(2,2,2));
        let blocked = wire_batch(&cx,server(&probe,true,false),denied.clone(),requests()).await;
        for response in &blocked { scope_denied(response,&denied,&probe,"read write"); }
        assert_eq!(probe.counts(),(4,2,2));
        let reaccepted = wire_batch(&cx,server(&probe,true,false),allowed,requests()).await;
        for response in &reaccepted { assert_eq!(authenticated_result(response),probe.expected()); }
        assert_eq!(probe.counts(),(6,4,4));
    });
}

#[test]
fn native_scope_socket_cors_challenges_and_public_routes_share_one_policy() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let policy = policy(&[],&["read","write"]);
        let metadata = HttpRequest::new(HttpMethod::Get,policy.resource_metadata_path().unwrap())
            .with_header("host","scope.example").with_header("origin",ORIGIN);
        let preflight = HttpRequest::new(HttpMethod::Options,"/mcp")
            .with_header("host","scope.example").with_header("origin",ORIGIN)
            .with_header("access-control-request-method","POST")
            .with_header("access-control-request-headers","authorization, content-type, mcp-method, mcp-name, mcp-protocol-version");
        let responses = wire_batch(&cx,server(&probe,true,false),policy.clone(),vec![
            metadata,preflight,call(&probe,false).with_header("origin",ORIGIN),
            call(&probe,true).with_header("origin",ORIGIN),
            call(&probe,false).with_header("origin","https://attacker.example").with_body(b"bad JSON".to_vec()),
        ]).await;
        assert_eq!(responses[0].status.0,200);
        let metadata: Value = serde_json::from_slice(&responses[0].body).unwrap();
        assert_eq!(metadata["resource"],"https://scope.example/mcp");
        assert_eq!(responses[1].status.0,204);
        for response in &responses[..4] {
            assert_eq!(response.headers["access-control-allow-origin"],ORIGIN);
            assert!(!response.headers.contains_key("access-control-allow-credentials"));
        }
        for response in &responses[2..4] {
            scope_denied(response,&policy,&probe,"read write");
            assert!(response.headers["access-control-expose-headers"].contains("WWW-Authenticate"));
        }
        assert_eq!(responses[4].status.0,403);
        assert!(!responses[4].headers.contains_key("www-authenticate"));
        assert!(!responses[4].headers.contains_key("access-control-allow-origin"));
        assert_eq!(probe.counts(),(2,0,0));
    });
}

#[test]
fn native_scope_socket_invalid_credentials_cannot_be_promoted_by_scope_rules() {
    run(|cx| async move {
        let probe = Probe::new(&["read"]);
        let policy = policy(&[],&["read"]);
        let mut missing = call(&probe,true);
        missing.headers.remove("authorization");
        let responses = wire_batch(&cx,server(&probe,true,false),policy,vec![
            call(&probe,false),call(&probe,false).with_header("authorization","Bearer wrong-token"),missing,
        ]).await;
        assert_eq!(authenticated_result(&responses[0]),probe.expected());
        for response in &responses[1..] {
            assert_eq!(response.status.0,401);
            assert!(!response.headers["www-authenticate"].contains("insufficient_scope"));
            assert!(!response.headers["www-authenticate"].contains("scope=\""));
            assert!(!response.headers.contains_key("transfer-encoding"));
        }
        // The valid and wrong tokens reach the provider; the missing header does not.
        assert_eq!(probe.counts(),(2,1,1));
    });
}
