//! Public-server tests for OAuth protected-resource metadata and challenges.
//! The native codec has separate raw-head/framing tests. These tests exercise
//! real authentication and tool dispatch through the secured embedding entry;
//! they do not substitute for a deployed HTTPS or browser interoperability test.

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
use fastmcp_server::{AuthProvider, AuthRequest, Server, ServerHttpEndpoint,
    StaticTokenVerifier, TokenAuthProvider, ToolHandler};
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::{HttpSecurityPolicy, resource_metadata::ProtectedResourceMetadata};
use fastmcp_transport::http::{HttpMethod, HttpRequest};
use serde_json::json;

#[derive(Clone)]
struct Probe {
    provider: Arc<TokenAuthProvider>,
    token: String,
    authentication: Arc<AtomicUsize>,
    execution: Arc<AtomicUsize>,
}
impl Probe {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let token = format!("metadata-{}-{nonce}", std::process::id());
        let verifier = StaticTokenVerifier::new([(
            token.clone(), AuthContext::with_subject("verified-metadata-test-subject".to_owned()),
        )]).unwrap();
        Self {
            provider: Arc::new(TokenAuthProvider::new(verifier)), token,
            authentication: Arc::new(AtomicUsize::new(0)), execution: Arc::new(AtomicUsize::new(0)),
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
            name: "metadata_probe".to_owned(), description: None,
            input_schema: json!({"type":"object"}), output_schema: None,
            icon: None, version: None, tags: Vec::new(), annotations: None,
        }
    }
    fn call(&self, cx: &McpContext, _: serde_json::Value) -> McpResult<Vec<Content>> {
        self.execution.fetch_add(1, Ordering::AcqRel);
        Ok(vec![Content::text(cx.auth().and_then(|auth| auth.subject).unwrap_or_default())])
    }
}

fn endpoint(probe: &Probe) -> ServerHttpEndpoint {
    let builder = Server::new("resource-metadata", "1.0.0")
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
    ).unwrap().with_resource_metadata(ProtectedResourceMetadata::new(
        vec!["https://issuer.example/tenant".to_owned()],
    ).unwrap().with_scopes_supported(vec!["tools:call".to_owned()]).unwrap()).unwrap()
}
fn get(policy: &HttpSecurityPolicy) -> HttpRequest {
    HttpRequest::new(HttpMethod::Get, policy.resource_metadata_path().unwrap())
        .with_header("host", "service.example")
}
fn operation(token: &str) -> HttpRequest {
    HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("host", "service.example")
        .with_header("authorization", format!("Bearer {token}"))
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_header("mcp-method", "tools/call")
        .with_header("mcp-name", "metadata_probe")
        .with_body(serde_json::to_vec(&json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
                "name":"metadata_probe", "arguments":{}, "_meta":{
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
fn public_metadata_bootstrap_is_unauthenticated_but_tool_execution_is_not() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let policy = policy();
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy, get(&policy))).await.unwrap();
        assert!(!response.is_streaming());
        assert_eq!(response.response().status.0, 200);
        let metadata: serde_json::Value = serde_json::from_slice(&response.response().body).unwrap();
        assert_eq!(metadata["resource"], "https://service.example/mcp");
        assert_eq!(metadata["authorization_servers"], json!(["https://issuer.example/tenant"]));
        assert_eq!(metadata["bearer_methods_supported"], json!(["header"]));
        assert_eq!(probe.counts(), (0, 0));

        let rejected = Box::pin(endpoint.handle_secured_async(&cx, &policy, operation("wrong-token"))).await.unwrap();
        assert_eq!(rejected.response().status.0, 401);
        assert!(rejected.response().headers["www-authenticate"].contains(&format!(
            "resource_metadata=\"{}\"", policy.resource_metadata_url().unwrap(),
        )));
        assert_eq!(probe.counts(), (1, 0));
        let accepted = Box::pin(endpoint.handle_secured_async(&cx, &policy, operation(&probe.token))).await.unwrap();
        assert_eq!(accepted.response().status.0, 200);
        let result: serde_json::Value = serde_json::from_slice(&accepted.response().body).unwrap();
        assert!(result.get("error").is_none());
        assert_eq!(result["result"]["content"][0]["text"], "verified-metadata-test-subject");
        assert!(!accepted.response().headers.contains_key("www-authenticate"));
        assert_eq!(probe.counts(), (2, 1));
    });
}

#[test]
fn public_metadata_is_not_selected_by_credentials_host_or_query() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let policy = policy();
        let request = get(&policy).with_header("authorization", format!("Bearer {}", probe.token));
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy, request.clone())).await.unwrap();
        assert_eq!(response.response().status.0, 200);
        assert!(!String::from_utf8_lossy(&response.response().body).contains(&probe.token));
        for (request, status) in [
            (request.clone().with_header("host", "attacker.example").with_header("x-forwarded-host", "service.example"), 403),
            (request.clone().with_query("access_token", &probe.token), 404),
            (request.clone().with_header("origin", "https://attacker.example"), 403),
            (request.with_body(b"unrequested body".to_vec()), 400),
        ] {
            let rejected = Box::pin(endpoint.handle_secured_async(&cx, &policy, request)).await.unwrap();
            assert_eq!(rejected.response().status.0, status);
            assert!(rejected.response().body.is_empty());
        }
        assert_eq!(probe.counts(), (0, 0));
    });
}

#[test]
fn public_browser_metadata_get_and_preflight_share_the_exact_origin_policy() {
    run(|cx| async move {
        let probe = Probe::new();
        let endpoint = endpoint(&probe);
        let policy = policy();
        let request = get(&policy).with_header("origin", "https://app.example");
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy, request)).await.unwrap();
        assert_eq!(response.response().status.0, 200);
        assert_eq!(response.response().headers["access-control-allow-origin"], "https://app.example");
        assert!(!response.response().headers.contains_key("access-control-allow-credentials"));
        let preflight = HttpRequest::new(HttpMethod::Options, policy.resource_metadata_path().unwrap())
            .with_header("host", "service.example").with_header("origin", "https://app.example")
            .with_header("access-control-request-method", "GET").with_header("access-control-request-headers", "Accept");
        let response = Box::pin(endpoint.handle_secured_async(&cx, &policy, preflight.clone())).await.unwrap();
        assert_eq!(response.response().status.0, 204);
        assert_eq!(response.response().headers["access-control-allow-methods"], "GET");
        let rejected = Box::pin(endpoint.handle_secured_async(&cx, &policy,
            preflight.with_header("access-control-request-method", "POST"))).await.unwrap();
        assert_eq!(rejected.response().status.0, 400);
        assert_eq!(probe.counts(), (0, 0));
    });
}
