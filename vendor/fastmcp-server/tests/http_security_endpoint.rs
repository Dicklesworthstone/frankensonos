//! Exercises the public secured embedding entry against the actual HTTP server,
//! authentication provider, and registered tool. These are not live socket/TLS
//! proofs. No cfg(test) seam substitutes for the production dispatch entry.

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

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpContext, McpResult};
use fastmcp_protocol::{Content, FINAL_PROTOCOL_VERSION, Tool, protocol_policy::ProtocolPolicy};
use fastmcp_server::{
    AuthProvider, AuthRequest, Server, ServerHttpEndpoint, ServerHttpEndpointResponse,
    StaticTokenVerifier, TokenAuthProvider, ToolHandler,
};
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::HttpSecurityPolicy;
use fastmcp_server::http_admission::security::endpoint::SecuredHttpEndpointError;
use fastmcp_transport::http::{HttpMethod, HttpRequest};
use serde_json::json;

#[derive(Clone)]
struct Probe {
    provider: Arc<TokenAuthProvider>,
    authentication: Arc<AtomicUsize>,
    execution: Arc<AtomicUsize>,
}

impl Probe {
    fn new() -> Self {
        let verifier = StaticTokenVerifier::new([(
            "http-security-test-token".to_owned(), AuthContext::with_subject("verified-subject".to_owned()),
        )]).unwrap();
        Self {
            provider: Arc::new(TokenAuthProvider::new(verifier)),
            authentication: Arc::new(AtomicUsize::new(0)),
            execution: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn counts(&self) -> (usize, usize) {
        (self.authentication.load(Ordering::Acquire), self.execution.load(Ordering::Acquire))
    }
}

impl AuthProvider for Probe {
    fn authenticate(&self, cx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        self.authentication.fetch_add(1, Ordering::AcqRel);
        self.provider.authenticate(cx, request)
    }
}

impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool {
            name: "security_probe".to_owned(), description: None,
            input_schema: json!({"type":"object"}), output_schema: None,
            icon: None, version: None, tags: Vec::new(), annotations: None,
        }
    }
    fn call(&self, cx: &McpContext, _: serde_json::Value) -> McpResult<Vec<Content>> {
        self.execution.fetch_add(1, Ordering::AcqRel);
        let subject = cx.auth().and_then(|auth| auth.subject).unwrap_or_default();
        Ok(vec![Content::text(subject)])
    }
}

fn endpoint(probe: &Probe) -> ServerHttpEndpoint {
    let builder = Server::new("secured-http", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
        .auth_provider(probe.clone()).tool(probe.clone());
    #[cfg(not(feature = "legacy-2024-11-05"))]
    let endpoint = builder.build_http_endpoint();
    #[cfg(feature = "legacy-2024-11-05")]
    let endpoint = builder.build_http_endpoint("http://service.example");
    endpoint.unwrap()
}

fn policy() -> HttpSecurityPolicy {
    HttpSecurityPolicy::new(
        HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
        "https://service.example", vec!["https://app.example".to_owned()],
    ).unwrap()
}

fn request() -> HttpRequest {
    HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("host", "service.example")
        .with_header("authorization", "Bearer http-security-test-token")
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_header("mcp-method", "tools/call")
        .with_header("mcp-name", "security_probe")
        .with_body(serde_json::to_vec(&json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
                "name":"security_probe", "arguments":{}, "_meta":{
                    "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities":{}
                }
            }
        })).unwrap())
}

fn run<F, Fut>(scenario: F)
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .blocking_threads(1, 4).build().unwrap().block_on(async move {
            let parent = Cx::current().unwrap();
            let mut task = parent.spawn(scenario).unwrap();
            task.join(&parent).await.unwrap();
        });
}

#[test]
fn secured_http_native_post_reaches_real_authentication_and_tool_once() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(), request())).await.unwrap();
        assert!(!response.is_streaming());
        let (response, stream) = response.into_parts();
        assert!(stream.is_none());
        assert_eq!(response.status.0, 200);
        let result: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert!(result.get("error").is_none());
        assert_eq!(result["result"]["content"][0]["text"], "verified-subject");
        assert_eq!(probe.counts(), (1, 1));
        assert!(!response.headers.contains_key("access-control-allow-origin"));
    });
}

#[test]
fn secured_http_browser_preflight_never_invokes_authentication_or_tool() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let preflight = HttpRequest::new(HttpMethod::Options, "/mcp")
            .with_header("host", "service.example")
            .with_header("origin", "https://app.example")
            .with_header("access-control-request-method", "POST")
            .with_header("access-control-request-headers", "authorization, mcp-method, mcp-name, mcp-protocol-version, content-type");
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(), preflight.clone())).await.unwrap();
        assert_eq!(response.response().status.0, 204);
        assert!(response.response().body.is_empty());
        assert_eq!(response.response().headers["access-control-allow-origin"], "https://app.example");
        assert_eq!(probe.counts(), (0, 0));
        let rejected = preflight.with_header("access-control-request-method", "DELETE");
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(), rejected)).await.unwrap();
        assert_eq!(response.response().status.0, 400);
        assert_eq!(probe.counts(), (0, 0));
    });
}

#[test]
fn secured_http_forbidden_origin_cannot_spend_a_valid_bearer_credential() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(),
            request().with_header("origin", "https://attacker.example"))).await.unwrap();
        assert_eq!(response.response().status.0, 403);
        assert!(response.response().body.is_empty());
        assert_eq!(probe.counts(), (0, 0));
        let control = Box::pin(endpoint.handle_secured_async(&cx, &policy(), request())).await.unwrap();
        assert_eq!(control.response().status.0, 200);
        assert_eq!(probe.counts(), (1, 1));
    });
}

#[test]
fn secured_http_forwarded_authority_cannot_override_the_public_host() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(), request()
            .with_header("host", "attacker.example")
            .with_header("x-forwarded-host", "service.example")
            .with_header("forwarded", "host=service.example;proto=https"))).await.unwrap();
        assert_eq!(response.response().status.0, 403);
        assert_eq!(probe.counts(), (0, 0));
        let control = Box::pin(endpoint.handle_secured_async(&cx, &policy(),
            request().with_header("host", "SERVICE.EXAMPLE:443"))).await.unwrap();
        assert_eq!(control.response().status.0, 200);
        assert_eq!(probe.counts(), (1, 1));
    });
}

#[test]
fn secured_http_keeps_native_query_credential_refusal_and_challenge() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(),
            request().with_query("access_token", "http-security-test-token"))).await.unwrap();
        assert_eq!(response.response().status.0, 401);
        assert_eq!(response.response().headers["www-authenticate"], "Bearer");
        let body: serde_json::Value = serde_json::from_slice(&response.response().body).unwrap();
        assert_eq!(body["error"], "invalid_request");
        assert_eq!(probe.counts(), (0, 0));
        assert!(!String::from_utf8_lossy(&response.response().body).contains("http-security-test-token"));
    });
}

#[test]
fn secured_http_allowed_authority_is_not_authentication() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(),
            request().with_header("authorization", "Bearer wrong-token"))).await.unwrap();
        assert_eq!(response.response().status.0, 401);
        assert!(response.response().headers.contains_key("www-authenticate"));
        assert_eq!(probe.counts(), (1, 0));
    });
}

#[test]
fn secured_http_preserves_the_dispatchers_protocol_error_response() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let malformed = request().with_body(b"not JSON".to_vec());
        let mut session = endpoint.open_session(&cx).unwrap();
        let ServerHttpEndpointResponse::Immediate(expected) = session.handle_async(&cx, malformed.clone()).await.unwrap()
            else { panic!("native protocol refusal must be immediate") };
        session.close(&cx).await;
        let actual = Box::pin(endpoint.handle_secured_async(&cx, &policy(), malformed)).await.unwrap();
        assert_eq!(actual.response().status, expected.status);
        assert_eq!(actual.response().body, expected.body);
        assert_eq!(probe.counts(), (0, 0));
    });
}

#[test]
fn secured_http_policy_route_mismatch_is_a_configuration_error_before_dispatch() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let wrong = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/other", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec![],
        ).unwrap();
        assert!(matches!(Box::pin(endpoint.handle_secured_async(&cx, &wrong, request())).await,
            Err(SecuredHttpEndpointError::PolicyRouteMismatch)));
        assert_eq!(probe.counts(), (0, 0));
    });
}

#[test]
fn secured_http_returned_sse_retains_its_session_until_explicit_close() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy(),
            request().with_header("accept", "text/event-stream"))).await.unwrap();
        assert!(response.is_streaming());
        let (head, stream) = response.into_parts();
        assert_eq!(head.status.0, 200);
        let mut stream = stream.expect("SSE owns the native session");
        let cancellation = stream.stream().unwrap().cancellation();
        assert!(!cancellation.is_cancelled(), "returning the response must not drop its session");
        stream.close(&cx).await;
        assert!(stream.stream().is_none());
        assert!(cancellation.is_cancelled());
        assert!(cx.checkpoint().is_ok(), "stream close cannot cancel the parent task");
    });
}
