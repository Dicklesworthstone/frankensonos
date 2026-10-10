//! Public-server coverage for provider-selected scope implication.
//!
//! These tests use real token verification and HTTP handler dispatch, but not
//! a TCP/TLS peer. The probe reports sanitized authentication facts; it does not
//! substitute for framework catalog visibility or a sealed operation resolver.

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
use fastmcp_core::{AuthContext, McpContext, McpResult, Sha256Digest};
use fastmcp_protocol::{Content, FINAL_PROTOCOL_VERSION, Tool, protocol_policy::ProtocolPolicy};
use fastmcp_server::{
    AuthProvider, AuthRequest, Server, ServerHttpEndpoint, StaticTokenVerifier,
    TokenAuthProvider, ToolHandler,
};
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::HttpSecurityPolicy;
use fastmcp_server::http_admission::security::scope_policy::{
    ScopeImplicationPolicy, ScopePolicyAuthProvider,
};
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpResponse};
use serde_json::{Value, json};

const TOKEN: &str = "scope-policy-test-only-token";
const OWNER: Sha256Digest = Sha256Digest::from_bytes([41; 32]);

#[derive(Clone, Default)]
struct Observations {
    verifications: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct RecordingProvider {
    verifier: StaticTokenVerifier,
    observations: Observations,
}

impl AuthProvider for RecordingProvider {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        self.observations.verifications.fetch_add(1, Ordering::SeqCst);
        TokenAuthProvider::new(self.verifier.clone()).authenticate(ctx, request)
    }
}

struct Probe(Observations);

impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool {
            name: "scope_probe".to_owned(), description: None,
            input_schema: json!({"type":"object"}), output_schema: None,
            icon: None, version: None, tags: Vec::new(), annotations: None,
        }
    }

    fn call(&self, ctx: &McpContext, _: Value) -> McpResult<Vec<Content>> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        let auth = ctx.auth().expect("the real server commits verified authentication first");
        let snapshot = json!({
            "subject":auth.subject,
            "scopes":auth.scopes,
            "claims":auth.claims,
            "same_owner":auth.session_owner() == Some(OWNER),
        });
        Ok(vec![Content::text(snapshot.to_string())])
    }
}

fn provider(grant: &str) -> RecordingProvider {
    let mut facts = AuthContext::with_subject("provider-scoped-user").with_session_owner(OWNER);
    facts.scopes = vec![grant.to_owned()];
    facts.claims = Some(json!({"scope":grant, "tenant":"configured-tenant"}));
    RecordingProvider {
        verifier: StaticTokenVerifier::new([(TOKEN.to_owned(), facts)]).unwrap(),
        observations: Observations::default(),
    }
}

fn scope_policy(edges: &[(&str, &str)]) -> ScopeImplicationPolicy {
    ScopeImplicationPolicy::new(11,
        edges.iter().map(|(grant, implied)| ((*grant).to_owned(), (*implied).to_owned())).collect(),
    ).unwrap()
}

fn endpoint(provider: &RecordingProvider, policy: ScopeImplicationPolicy) -> ServerHttpEndpoint {
    let builder = Server::new("http-scope-policy", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
        .auth_provider(ScopePolicyAuthProvider::new(provider.clone(), policy))
        .tool(Probe(provider.observations.clone()));
    #[cfg(not(feature = "legacy-2024-11-05"))]
    let endpoint = builder.build_http_endpoint();
    #[cfg(feature = "legacy-2024-11-05")]
    let endpoint = builder.build_http_endpoint("http://service.example");
    endpoint.unwrap()
}

fn ingress_policy() -> HttpSecurityPolicy {
    HttpSecurityPolicy::new(
        HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
        "https://service.example", Vec::new(),
    ).unwrap()
}

fn request(id: i64, extra_meta: Option<Value>) -> HttpRequest {
    let mut meta = json!({
        "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities":{},
    });
    if let Some(extra) = extra_meta { meta["com.example/scope-policy"] = extra; }
    HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("host", "service.example")
        .with_header("authorization", format!("Bearer {TOKEN}"))
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_header("mcp-method", "tools/call")
        .with_header("mcp-name", "scope_probe")
        .with_body(serde_json::to_vec(&json!({
            "jsonrpc":"2.0", "id":id, "method":"tools/call",
            "params":{"name":"scope_probe", "arguments":{}, "_meta":meta},
        })).unwrap())
}

async fn dispatch(cx: &Cx, endpoint: &ServerHttpEndpoint, request: HttpRequest) -> HttpResponse {
    // Boxed inside the helper rather than at its twelve call sites: shrinking
    // `dispatch`'s own future below the threshold clears the callers too, so this
    // is one edit instead of thirteen and one allocation per call either way.
    let response = Box::pin(endpoint.handle_secured_async(cx, &ingress_policy(), request)).await.unwrap();
    let (response, stream) = response.into_parts();
    assert!(stream.is_none(), "the probe requests one immediate JSON response");
    assert!(!String::from_utf8_lossy(&response.body).contains(TOKEN));
    response
}

fn snapshot(response: &HttpResponse) -> Value {
    assert_eq!(response.status.0, 200);
    let envelope: Value = serde_json::from_slice(&response.body).unwrap();
    assert!(envelope.get("error").is_none());
    assert_ne!(envelope["result"]["isError"], true);
    serde_json::from_str(envelope["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
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
fn http_scope_policy_projects_transitive_grants_without_rewriting_identity_or_claims() {
    run(|cx| async move {
        let provider = provider("admin");
        let endpoint = endpoint(&provider, scope_policy(&[("admin", "write"), ("write", "read")]));
        let response = dispatch(&cx, &endpoint, request(1, None)).await;
        assert_eq!(snapshot(&response), json!({
            "subject":"provider-scoped-user", "scopes":["admin", "read", "write"],
            "claims":{"scope":"admin", "tenant":"configured-tenant"}, "same_owner":true,
        }));
        assert_eq!(provider.observations.verifications.load(Ordering::SeqCst), 1);
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn http_scope_policy_one_missing_edge_removes_derived_permission_and_baseline_still_works() {
    run(|cx| async move {
        let provider = provider("admin");
        let baseline = endpoint(&provider, scope_policy(&[("admin", "write"), ("write", "read")]));
        let missing = endpoint(&provider, scope_policy(&[("admin", "write")]));
        let first = snapshot(&dispatch(&cx, &baseline, request(1, None)).await);
        let second = snapshot(&dispatch(&cx, &missing, request(2, None)).await);
        assert_eq!(first["scopes"], json!(["admin", "read", "write"]));
        assert_eq!(second["scopes"], json!(["admin", "write"]));
        for key in ["subject", "claims", "same_owner"] { assert_eq!(first[key], second[key]); }
        let third = snapshot(&dispatch(&cx, &baseline, request(3, None)).await);
        assert_eq!(third, first);
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn http_scope_policy_request_metadata_cannot_install_rules_or_self_report_grants() {
    run(|cx| async move {
        let provider = provider("read");
        let endpoint = endpoint(&provider, scope_policy(&[("admin", "write"), ("write", "read")]));
        let baseline = snapshot(&dispatch(&cx, &endpoint, request(1, None)).await);
        let injected = json!({
            "revision":999, "scopes":["admin", "write"],
            "implications":[["read", "admin"]],
        });
        let planted = snapshot(&dispatch(&cx, &endpoint, request(2, Some(injected))).await);
        assert_eq!(planted, baseline);
        assert_eq!(planted["scopes"], json!(["read"]));
        assert_eq!(provider.observations.verifications.load(Ordering::SeqCst), 2);
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn http_scope_policy_invalid_or_revoked_credentials_never_reach_the_handler() {
    run(|cx| async move {
        let provider = provider("admin");
        let endpoint = endpoint(&provider, scope_policy(&[("admin", "read")]));
        let invalid = dispatch(&cx, &endpoint,
            request(1, None).with_header("authorization", "Bearer different-token")).await;
        assert_eq!(invalid.status.0, 401);
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 0);
        assert_eq!(snapshot(&dispatch(&cx, &endpoint, request(2, None)).await)["scopes"], json!(["admin", "read"]));
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 1);
        assert!(provider.verifier.revoke_token(TOKEN).unwrap());
        let revoked = dispatch(&cx, &endpoint, request(3, None)).await;
        assert_eq!(revoked.status.0, 401);
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.observations.verifications.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn http_scope_policy_isolation_keeps_exact_only_provider_from_borrowing_another_policy() {
    run(|cx| async move {
        let provider = provider("admin");
        let extended = endpoint(&provider, scope_policy(&[("admin", "read")]));
        let exact = endpoint(&provider, ScopeImplicationPolicy::exact(12).unwrap());
        assert_eq!(snapshot(&dispatch(&cx, &extended, request(1, None)).await)["scopes"], json!(["admin", "read"]));
        assert_eq!(snapshot(&dispatch(&cx, &exact, request(2, None)).await)["scopes"], json!(["admin"]));
        assert_eq!(snapshot(&dispatch(&cx, &extended, request(3, None)).await)["scopes"], json!(["admin", "read"]));
        assert_eq!(provider.observations.verifications.load(Ordering::SeqCst), 3);
        assert_eq!(provider.observations.calls.load(Ordering::SeqCst), 3);
    });
}
