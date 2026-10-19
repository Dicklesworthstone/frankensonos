use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpContext, McpError, block_on};
use fastmcp_protocol::{FINAL_PROTOCOL_VERSION, JsonRpcRequest, RequestId};
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpStatus};
use serde_json::json;

use super::*;
use crate::{
    HttpServerConfig, LiveHttpLegacyIngress, LiveHttpSession, ServerHttpEndpointResponse,
    StaticTokenVerifier, TokenAuthProvider, admit_live_http_legacy_request,
};

fn request(subject: &str) -> HttpRequest {
    HttpRequest::new(HttpMethod::Post, "/mcp").with_header("x-test-subject", subject)
}

fn config() -> HttpServerConfig {
    HttpServerConfig::new().identity_resolver(|peer, headers| {
        assert!(peer.ip().is_loopback());
        Ok(AuthContext::with_subject(
            headers
                .iter()
                .find(|(name, _)| name == "x-test-subject")
                .map_or("unknown", |(_, value)| value.as_str()),
        ))
    })
}

fn custody(server: &Server, request: &HttpRequest) -> TransportAuthorization {
    server
        .resolve_native_http_identity(
            "127.0.0.1:32123".parse().unwrap(),
            request,
            TransportAuthorization::default(),
        )
        .expect("trusted native identity")
}

#[test]
fn receipts_preserve_each_callers_identity_and_exact_request_id() {
    let server = Server::new("identity-receipts", "1")
        .http_config(config())
        .build();
    let cx = Cx::for_testing();
    let original = JsonRpcRequest::new(
        "tools/list",
        Some(json!({
            "_meta": {"subject": "forged"}, "client": "forged",
        })),
        0_i64,
    );
    let mut contexts = Vec::new();
    for subject in ["alice", "bob", "alice"] {
        let authorization = custody(&server, &request(subject));
        let receipt = server
            .preauthenticate_http_request(&cx, &original, &authorization)
            .unwrap();
        let forged = JsonRpcRequest::notification("tools/list", original.params.clone());
        let rejected = McpContext::new(cx.clone(), 0);
        assert!(receipt.commit_legacy(&rejected, &forged).is_err());
        assert!(rejected.auth().is_none());
        let context = McpContext::new(cx.clone(), 0);
        receipt.commit_legacy(&context, &original).unwrap();
        contexts.push(context);
    }
    assert_eq!(
        contexts
            .iter()
            .map(|context| context.auth().unwrap().subject.unwrap())
            .collect::<Vec<_>>(),
        ["alice", "bob", "alice"],
    );
}

#[test]
fn resolver_errors_panics_and_invalid_facts_never_fall_back() {
    for failure in ["error", "panic", "oversized"] {
        let server = Server::new("identity-failure", "1")
            .http_config(
                HttpServerConfig::new().identity_resolver(move |_, _| match failure {
                    "error" => Err(McpError::internal_error("lookup failed")),
                    "panic" => panic!("resolver panic"),
                    _ => Ok(AuthContext::with_subject("x".repeat(1025))),
                }),
            )
            .build();
        let response = match server.resolve_native_http_identity(
            "127.0.0.1:32123".parse().unwrap(),
            &request("alice"),
            TransportAuthorization::default(),
        ) {
            Ok(_) => panic!("resolver failure must reject admission"),
            Err(response) => response,
        };
        assert_eq!(response.status, HttpStatus::FORBIDDEN);
        assert!(response.body.is_empty());
        assert!(!response.headers.contains_key("www-authenticate"));
    }
}

#[test]
fn anonymous_resolution_is_explicit_and_bearer_authority_cannot_override_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let server = Server::new("identity-anonymous", "1")
        .http_config(HttpServerConfig::new().identity_resolver(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(AuthContext::anonymous())
        }))
        .build();
    let authorization = custody(&server, &request("untrusted-claim"));
    assert!(authorization.identity().unwrap().subject.is_none());
    let cx = Cx::for_testing();
    let message = JsonRpcRequest::new("tools/list", None, 1_i64);
    let receipt = server
        .preauthenticate_http_request(&cx, &message, &authorization)
        .unwrap();
    let context = McpContext::new(cx, 1);
    receipt.commit_legacy(&context, &message).unwrap();
    assert!(context.auth().is_some());
    assert!(context.auth().unwrap().subject.is_none());
    assert!(matches!(
        server.resolve_native_http_identity(
            "127.0.0.1:32123".parse().unwrap(), &request("alice"),
            TransportAuthorization::from_singleton_header(Some("Bearer token")),
        ),
        Err(response) if response.status == HttpStatus::FORBIDDEN
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn embedding_cannot_manufacture_native_custody_or_pin_an_era() {
    let endpoint = Server::new("identity-embedding", "1")
        .http_config(config())
        .build_http_endpoint("http://identity.test")
        .unwrap();
    let cx = Cx::for_testing();
    let mut session = endpoint.open_session(&cx).unwrap();
    let request = request("alice")
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-method", "tools/list")
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_body(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list",
                "params": {"_meta": {
                    "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {},
                }},
            }))
            .unwrap(),
        );
    assert!(matches!(
        block_on(session.handle_async(&cx, request)).unwrap(),
        ServerHttpEndpointResponse::Immediate(response) if response.status == HttpStatus::FORBIDDEN
    ));
    assert!(session.selected_era.is_none());
}

#[test]
fn resolver_and_bearer_provider_are_rejected_at_endpoint_setup() {
    let verifier =
        StaticTokenVerifier::new([("token", AuthContext::with_subject("alice"))]).unwrap();
    let result = Server::new("identity-conflict", "1")
        .http_config(config())
        .auth_provider(TokenAuthProvider::new(verifier))
        .build_http_endpoint("http://identity.test");
    assert!(matches!(
        result,
        Err(crate::ServerHttpEndpointError::InvalidConfiguration(_))
    ));
}

#[test]
fn legacy_opener_identity_owns_posts_reverse_responses_and_cancellation() {
    let cx = Cx::for_testing();
    let endpoint = Server::new("identity-legacy", "1")
        .http_config(config())
        .build_http_endpoint("http://identity.test")
        .unwrap();
    let mut session = endpoint.open_session(&cx).unwrap();
    let opener = HttpRequest::new(HttpMethod::Get, "/sse").with_header("x-test-subject", "alice");
    let authorization = custody(&endpoint.server, &opener);
    let stream = block_on(
        session.handle_with_modern_request_cancellation_and_transport_authorization_async(
            &cx,
            opener,
            authorization,
            None,
            false,
            None,
        ),
    )
    .unwrap();
    let ServerHttpEndpointResponse::LegacySse(_stream) = stream else {
        panic!("trusted native opener must create the SSE session");
    };
    let alice = auth::principal_fingerprint(Some(&AuthContext::with_subject("alice"))).unwrap();
    let bob = auth::principal_fingerprint(Some(&AuthContext::with_subject("bob"))).unwrap();
    assert!(
        session
            .legacy_session
            .principal_binding()
            .verify_existing(alice)
    );
    assert!(
        !session
            .legacy_session
            .principal_binding()
            .verify_existing(bob)
    );
    let session_id = session.legacy_session_id().to_owned();
    let live = Arc::new(LiveHttpSession {
        cancellation: session.cancellation_control(),
        legacy_pending_requests: Arc::clone(&session.legacy_pending_requests),
        session: Mutex::new(Some(session)),
        session_available: Condvar::new(),
        closing: AtomicBool::new(false),
    });
    let sessions = Arc::new(Mutex::new(HashMap::from([(
        session_id.clone(),
        Arc::clone(&live),
    )])));
    let post = |subject: &str, body: serde_json::Value| {
        HttpRequest::new(HttpMethod::Post, "/messages")
            .with_header("content-type", "application/json")
            .with_header("x-test-subject", subject)
            .with_query("session_id", &session_id)
            .with_body(serde_json::to_vec(&body).unwrap())
    };
    let target = post(
        "alice",
        json!({"jsonrpc":"2.0","id":41,"method":"tools/list"}),
    );
    let _admission = admit_live_http_legacy_request(
        &cx,
        &endpoint,
        &sessions,
        &target,
        &custody(&endpoint.server, &target),
    )
    .unwrap_or_else(|_| panic!("owner request must be admitted"));
    let cancellation = live
        .cancellation
        .admissions
        .admitted_request_cancellation(&RequestId::Number(41))
        .unwrap();
    let foreign = post(
        "bob",
        json!({"jsonrpc":"2.0","id":42,"method":"tools/list"}),
    );
    assert!(
        admit_live_http_legacy_request(
            &cx,
            &endpoint,
            &sessions,
            &foreign,
            &custody(&endpoint.server, &foreign),
        )
        .is_err()
    );
    let reverse = post("bob", json!({"jsonrpc":"2.0","id":9,"result":{}}));
    assert!(
        admit_live_http_legacy_request(
            &cx,
            &endpoint,
            &sessions,
            &reverse,
            &custody(&endpoint.server, &reverse),
        )
        .is_err()
    );
    let reverse = post("alice", json!({"jsonrpc":"2.0","id":9,"result":{}}));
    assert!(
        admit_live_http_legacy_request(
            &cx,
            &endpoint,
            &sessions,
            &reverse,
            &custody(&endpoint.server, &reverse),
        )
        .is_ok()
    );
    for (subject, expected) in [
        ("bob", HttpStatus::BAD_REQUEST),
        ("alice", HttpStatus::ACCEPTED),
    ] {
        let request = post(
            subject,
            json!({
                "jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":41},
            }),
        );
        let response = admit_live_http_legacy_request(
            &cx,
            &endpoint,
            &sessions,
            &request,
            &custody(&endpoint.server, &request),
        )
        .unwrap_or_else(|_| panic!("cancellation must produce a bounded response"));
        assert!(
            matches!(response, LiveHttpLegacyIngress::Immediate(response) if response.status == expected)
        );
        assert_eq!(cancellation.is_cancel_requested(), subject == "alice");
    }
}

mod bounded;
