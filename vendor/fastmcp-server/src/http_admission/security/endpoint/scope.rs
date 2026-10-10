//! Native request-scope admission for the secured modern HTTP endpoint.
//!
//! Authentication produces the existing request-bound receipt exactly once.
//! The complete policy examines those admitted facts before an era is selected,
//! a transport request is enqueued, or an SSE response body is allocated. The
//! same receipt then enters the existing native dispatcher. No application
//! error code, message, response body or metadata is interpreted as an OAuth
//! challenge, and no authentication result is cached for a later POST.

use std::sync::Arc;

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpRequestCancellation};
use fastmcp_protocol::JsonRpcRequest;
use fastmcp_protocol::protocol_policy::{ProtocolEra, ProtocolPolicy};
use fastmcp_transport::TransportError;
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpResponse, HttpStatus};

use super::{SecuredHttpEndpointError, checkpoint, guard_response};
use super::revalidation::{SseAuthorizationLease, SseRevalidationPolicy};
use super::super::{HttpSecurityError, HttpSecurityPolicy};
use super::super::scope_policy::request::{ScopeRequestPolicy, ScopeRequestRejection};
use crate::{
    AuthDispatchCustody, DualEraHttpEndpointError, DualEraHttpEndpointResponse,
    DualEraHttpSseResponse, LiveModernHttpSessionRegistry, ServerHttpEndpoint,
    ServerHttpEndpointError, ServerHttpEndpointResponse, ServerHttpSession,
    TransportAuthorization, http_endpoint_error_response, http_endpoint_response_to_static,
};

impl HttpSecurityPolicy {
    /// Requires scopes on the secured modern HTTP execution path. Use
    /// ScopeRequestPolicy::for_operations to include exact named-operation rules.
    ///
    /// Both `ServerHttpEndpoint::handle_secured_async` and the secured socket
    /// listener use this policy. Strict protocol admission and the server's
    /// installed authentication provider run before scope evaluation. A valid
    /// principal lacking permissions receives an empty, uncacheable HTTP 403.
    /// Method-only policy challenges contain the complete configured requirement.
    /// Named-operation policies omit scope names: this boundary has not established
    /// catalog visibility, and unknown/forbidden targets must be indistinguishable.
    /// Anonymous denial remains HTTP 401 without required-scope disclosure.
    ///
    /// Public metadata GET and browser preflight remain unauthenticated. The
    /// head-only and structural `admit` helpers do not execute authentication
    /// or scope checks. Plain HTTP entry points that are not supplied this
    /// security policy, stdio, WebSocket and the exact-2024 adapter are unchanged.
    /// Install the same request policy on Server::with_scope_authorization for
    /// those ordinary dispatch paths. Additional server middleware restrictions
    /// still apply, but their errors are not reclassified as OAuth. Revalidation
    /// is a separate explicit choice and retains the full named request policy.
    ///
    /// Configuration is immutable after installation. A second installation
    /// fails rather than replacing or widening the previous scope policy.
    pub fn with_scope_authorization(
        mut self,
        policy: ScopeRequestPolicy,
    ) -> Result<Self, HttpSecurityError> {
        if self.scope_authorization.is_some() {
            return Err(HttpSecurityError::InvalidPolicy);
        }
        self.scope_authorization = Some(policy);
        Ok(self)
    }

    /// Revalidates the original credential throughout secured SSE delivery.
    /// Install request scopes first and supply a real authentication provider.
    /// A changed principal, scope set or claim closes the stream rather than
    /// updating a handler that began with different facts. Revocation is observed
    /// within the configured cached-verdict interval plus caller scheduling and
    /// bounded provider work, not instantaneously. No token renewal is attempted.
    ///
    /// The socket writer checks while idle, selecting a response and writing.
    /// Embedders must drive `SecuredHttpSseResponse::next_event`; raw body access
    /// is unavailable on guarded responses. Synchronous providers must enforce
    /// their own I/O/work timeouts. This does not hot-reload policy, provide
    /// named-resource visibility, or revalidate Tasks detached from this response.
    pub fn with_sse_revalidation(mut self, policy: SseRevalidationPolicy) -> Result<Self, HttpSecurityError> {
        if self.scope_authorization.is_none() || self.sse_revalidation.is_some() {
            return Err(HttpSecurityError::InvalidPolicy);
        }
        self.sse_revalidation = Some(policy);
        Ok(self)
    }
}

// Private request-owned handoff. Every consumer uses the same preparation and
// opening receipt. The optional lease retains its own original credential
// custody and is never a shared cache or a transferable public permit.
struct PreparedScopedPost {
    request: HttpRequest,
    raw_params: Option<Arc<str>>,
    receipt: AuthDispatchCustody,
    lease: Option<SseAuthorizationLease>,
}

fn check_admission(
    cx: &Cx,
    cancellation: Option<&McpRequestCancellation>,
) -> Result<(), HttpResponse> {
    if checkpoint(cx).is_err()
        || cancellation.is_some_and(McpRequestCancellation::is_cancel_requested)
    {
        return Err(refusal(503));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare(
    session: &mut ServerHttpSession,
    cx: &Cx,
    policy: &ScopeRequestPolicy,
    request: HttpRequest,
    authorization: &TransportAuthorization,
    cancellation: Option<&McpRequestCancellation>,
    revalidation: Option<SseRevalidationPolicy>,
) -> Result<PreparedScopedPost, HttpResponse> {
    check_admission(cx, cancellation)?;
    if session.closed { return Err(refusal(503)); }
    session.reap_modern_dispatches();
    if request.method != HttpMethod::Post
        || request.path != session.server.http_config.handler_config.base_path
        || matches!(session.server.protocol_policy, ProtocolPolicy::LegacyOnly)
        || session.selected_era.is_some_and(|era| era != ProtocolEra::Modern2026)
        || request.header("mcp-session-id").is_some()
    {
        return Err(HttpResponse::bad_request());
    }
    let (request, admitted, raw_params) = session.prepare_modern_http_request(request)?;
    check_admission(cx, cancellation)?;
    let receipt = session.preauthenticate_modern_http_request(cx, &admitted, authorization)?;
    check_admission(cx, cancellation)?;
    let rejection = scope_rejection(policy, &admitted, receipt.authenticated.as_ref());
    check_admission(cx, cancellation)?;
    if let Some(response) = rejection { return Err(response); }
    let lease = revalidation.filter(|_| crate::http_request_accepts_sse(&request)).map(|config| {
        SseAuthorizationLease::new(cx, Arc::clone(&session.server), &admitted,
            authorization, &receipt, policy.clone(), config)
    }).transpose().map_err(|_| refusal(503))?;
    Ok(PreparedScopedPost { request, raw_params, receipt: AuthDispatchCustody::Http(receipt), lease })
}

/// Embedding keeps its existing outer deadline guard and SSE/session owner.
pub(super) async fn dispatch(
    session: &mut ServerHttpSession,
    cx: &Cx,
    policy: &ScopeRequestPolicy,
    request: HttpRequest,
    revalidation: Option<SseRevalidationPolicy>,
) -> Result<(ServerHttpEndpointResponse, Option<SseAuthorizationLease>), SecuredHttpEndpointError> {
    checkpoint(cx)?;
    let authorization = match crate::transport_authorization_from_http_request(&request) {
        Ok(authorization) => authorization,
        Err(response) => return Ok((ServerHttpEndpointResponse::Immediate(response), None)),
    };
    Box::pin(dispatch_with_authorization(session, cx, policy, request, authorization, None, revalidation))
        .await.map_err(|_| SecuredHttpEndpointError::DispatchFailed)
}

async fn dispatch_with_authorization(
    session: &mut ServerHttpSession,
    cx: &Cx,
    policy: &ScopeRequestPolicy,
    request: HttpRequest,
    authorization: TransportAuthorization,
    cancellation: Option<McpRequestCancellation>,
    revalidation: Option<SseRevalidationPolicy>,
) -> Result<(ServerHttpEndpointResponse, Option<SseAuthorizationLease>), DualEraHttpEndpointError> {
    let prepared = match prepare(session, cx, policy, request, &authorization, cancellation.as_ref(), revalidation) {
        Ok(prepared) => prepared,
        Err(response) => return Ok((ServerHttpEndpointResponse::Immediate(response), None)),
    };
    session.selected_era.get_or_insert(ProtocolEra::Modern2026);
    let http_parameter_headers = crate::http_admission::http_parameter_headers(&prepared.request.headers);
    let endpoint_response = {
        let mut endpoint = session.endpoint_session.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        endpoint.handle(cx, prepared.request)?
    };
    let mut lease = prepared.lease;
    let response = Box::pin(guard_response(cx, &mut lease, session.handle_modern(
        cx, endpoint_response, authorization, prepared.raw_params,
        Some(http_parameter_headers), Some(prepared.receipt), cancellation,
    ))).await;
    match response {
        Ok(response) => Ok((response?, lease)),
        // No response head has been published by this path. Retire the failed
        // native future and let the outer session owner join remaining work.
        Err(_) => Ok((ServerHttpEndpointResponse::Immediate(refusal(503)), None)),
    }
}

/// The socket's joined JSON child keeps its original cancellation token. JSON
/// does not retain a lease after its single response; native error projection
/// and fresh-session ownership remain unchanged.
pub(super) async fn dispatch_socket_json(
    cx: &Cx,
    endpoint: &ServerHttpEndpoint,
    sessions: &LiveModernHttpSessionRegistry,
    policy: &ScopeRequestPolicy,
    request: HttpRequest,
    authorization: TransportAuthorization,
    cancellation: McpRequestCancellation,
) -> HttpResponse {
    let error_request = request.clone();
    sessions.reap_retired_dispatches();
    let mut session = match endpoint.open_session(cx) {
        Ok(session) => session,
        Err(_) => return HttpResponse::internal_error(),
    };
    Box::pin(dispatch_with_authorization(&mut session, cx, policy, request, authorization, Some(cancellation), None))
        .await
        .map_err(ServerHttpEndpointError::from_internal)
        .map(|(response, _)| http_endpoint_response_to_static(cx, response))
        .unwrap_or_else(|error| http_endpoint_error_response(
            &error_request, error, endpoint.server.http_config.handler_config.max_body_size,
        ))
}

/// The refusal is boxed so this Result stays small (bd-cmvwm): the response
/// enum is hundreds of bytes and only the refusal path pays the allocation.
type ScopedSseOpening = Result<
    (JsonRpcRequest, DualEraHttpSseResponse, Option<Arc<str>>, AuthDispatchCustody, Option<SseAuthorizationLease>),
    Box<ServerHttpEndpointResponse>,
>;

/// The socket retains its peer monitor, registry, outcome election and terminal
/// drain. The lease follows the exact native SSE body, not the connection pool.
pub(super) async fn begin_sse(
    session: &mut ServerHttpSession,
    cx: &Cx,
    policy: &ScopeRequestPolicy,
    request: HttpRequest,
    authorization: TransportAuthorization,
    revalidation: Option<SseRevalidationPolicy>,
) -> Result<ScopedSseOpening, DualEraHttpEndpointError> {
    let prepared = match prepare(session, cx, policy, request, &authorization, None, revalidation) {
        Ok(prepared) => prepared,
        Err(response) => return Ok(Err(Box::new(ServerHttpEndpointResponse::Immediate(response)))),
    };
    let http_parameter_headers = crate::http_admission::http_parameter_headers(&prepared.request.headers);
    let endpoint_response = {
        let mut endpoint = session.endpoint_session.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match endpoint.handle(cx, prepared.request) {
            Ok(response) => response,
            Err(DualEraHttpEndpointError::Transport(TransportError::Io(error)))
                if error.kind() == std::io::ErrorKind::InvalidInput =>
            {
                return Ok(Err(Box::new(ServerHttpEndpointResponse::Immediate(HttpResponse::bad_request()))));
            }
            Err(error) => return Err(error),
        }
    };
    let DualEraHttpEndpointResponse::ModernSse(sse) = endpoint_response else {
        let mut lease = prepared.lease;
        return match Box::pin(guard_response(cx, &mut lease, session.handle_modern(
            cx, endpoint_response, authorization, prepared.raw_params,
            Some(http_parameter_headers), Some(prepared.receipt), None,
        ))).await {
            Ok(response) => response.map(|response| Err(Box::new(response))),
            Err(_) => Ok(Err(Box::new(ServerHttpEndpointResponse::Immediate(refusal(503))))),
        };
    };
    let request = session.endpoint_session.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner).recv_modern_request(cx)?;
    if request.method == "notifications/cancelled" {
        return Ok(Err(Box::new(ServerHttpEndpointResponse::Immediate(HttpResponse::bad_request()))));
    }
    Ok(Ok((request, sse, prepared.raw_params, prepared.receipt, prepared.lease)))
}

fn refusal(status: u16) -> HttpResponse {
    HttpResponse::new(HttpStatus(status)).with_header("cache-control", "no-store")
}

/// Only server policy and admitted facts enter this formatter. Named-operation
/// failures do not disclose configuration before catalog visibility is proven.
fn scope_rejection(
    policy: &ScopeRequestPolicy,
    request: &JsonRpcRequest,
    facts: Option<&AuthContext>,
) -> Option<HttpResponse> {
    let rejected = match policy.authorize_request_verified(request, facts) {
        Ok(()) => return None,
        Err(rejected) => rejected,
    };
    if rejected == ScopeRequestRejection::InvalidFacts { return Some(refusal(500)); }
    let authenticated = facts.is_some_and(|facts| {
        facts.subject.as_ref().is_some_and(|subject| !subject.is_empty())
            || facts.session_owner().is_some()
    });
    if !authenticated { return Some(refusal(401).with_header("www-authenticate", "Bearer")); }
    match rejected {
        ScopeRequestRejection::InsufficientScope if policy.has_operation_rules() => Some(
            refusal(403).with_header("www-authenticate", "Bearer error=\"insufficient_scope\""),
        ),
        ScopeRequestRejection::InsufficientScope => Some(match policy.required_scopes(&request.method) {
            Some(required) => refusal(403).with_header(
                "www-authenticate",
                format!("Bearer error=\"insufficient_scope\", scope=\"{}\"", required.challenge_scope()),
            ),
            None => refusal(500),
        }),
        ScopeRequestRejection::UnconfiguredMethod => Some(refusal(403)),
        ScopeRequestRejection::InvalidFacts => Some(refusal(500)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
    use super::super::super::scope_policy::{RequiredScopes, ScopeImplicationPolicy};

    fn policy() -> ScopeRequestPolicy {
        ScopeRequestPolicy::new(1, ScopeImplicationPolicy::new(1, vec![
            ("admin".to_owned(), "write".to_owned()),
            ("write".to_owned(), "read".to_owned()),
        ]).unwrap(), vec![
            ("tools/call".to_owned(), RequiredScopes::new(vec!["write".to_owned(), "read".to_owned()]).unwrap()),
            ("tools/list".to_owned(), RequiredScopes::new(vec![]).unwrap()),
        ]).unwrap()
    }
    fn method_request(method: &str) -> JsonRpcRequest {
        JsonRpcRequest::new(method, None, fastmcp_protocol::RequestId::Number(1))
    }
    fn facts(scopes: &[&str]) -> AuthContext {
        let mut facts = AuthContext::with_subject("private-principal-canary");
        facts.scopes = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        facts
    }

    #[test]
    fn challenge_contains_the_complete_sorted_requirement_not_only_missing_scopes() {
        let response = scope_rejection(&policy(), &method_request("tools/call"), Some(&facts(&["read"]))).unwrap();
        assert_eq!(response.status.0, 403);
        assert_eq!(response.headers["www-authenticate"], "Bearer error=\"insufficient_scope\", scope=\"read write\"");
        assert_eq!(response.headers["cache-control"], "no-store");
        assert!(response.body.is_empty());
        assert!(!format!("{:?}", response.headers).contains("canary"));
    }

    #[test]
    fn transitive_permission_is_accepted_without_rewriting_provider_facts() {
        let facts = facts(&["admin"]);
        let before = serde_json::to_vec(&facts).unwrap();
        assert!(scope_rejection(&policy(), &method_request("tools/call"), Some(&facts)).is_none());
        assert_eq!(serde_json::to_vec(&facts).unwrap(), before);
    }

    #[test]
    fn anonymous_denial_is_401_without_scope_disclosure_and_public_is_explicit() {
        for method in ["tools/call", "unconfigured"] {
            for facts in [None, Some(AuthContext::anonymous())] {
                let response = scope_rejection(&policy(), &method_request(method), facts.as_ref()).unwrap();
                assert_eq!(response.status.0, 401);
                assert_eq!(response.headers["www-authenticate"], "Bearer");
                assert!(response.body.is_empty());
            }
        }
        assert!(scope_rejection(&policy(), &method_request("tools/list"), None).is_none());
    }

    #[test]
    fn unconfigured_method_has_no_scope_or_application_error_oracle() {
        let response = scope_rejection(&policy(), &method_request("private-method-canary"), Some(&facts(&["admin"]))).unwrap();
        assert_eq!(response.status.0, 403);
        assert!(!response.headers.contains_key("www-authenticate"));
        assert!(response.body.is_empty());
        assert!(!format!("{:?}", response.headers).contains("canary"));
    }

    #[test]
    fn malformed_provider_facts_are_not_an_insufficient_scope_challenge() {
        let response = scope_rejection(&policy(), &method_request("tools/call"), Some(&facts(&["bad scope"]))).unwrap();
        assert_eq!(response.status.0, 500);
        assert!(!response.headers.contains_key("www-authenticate"));
        assert!(response.body.is_empty());
    }

    #[test]
    fn installed_http_policy_cannot_be_silently_replaced_and_clones_keep_it() {
        let security = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec![],
        ).unwrap().with_scope_authorization(policy()).unwrap();
        let before = security.scope_authorization.as_ref().unwrap().fingerprint();
        assert!(matches!(security.clone().with_scope_authorization(policy()), Err(HttpSecurityError::InvalidPolicy)));
        assert_eq!(security.scope_authorization.as_ref().unwrap().fingerprint(), before);
        assert_eq!(security.clone().scope_authorization.as_ref().unwrap().fingerprint(), before);
    }

    #[test]
    fn cancelled_socket_request_cannot_enter_authentication_or_scope_work() {
        let cancellation = McpRequestCancellation::new();
        cancellation.cancel();
        let cx = Cx::for_testing();
        let response = check_admission(&cx, Some(&cancellation)).unwrap_err();
        assert_eq!(response.status.0, 503);
        assert!(!response.headers.contains_key("www-authenticate"));
        assert!(response.body.is_empty());
        assert!(cx.checkpoint().is_ok());
    }

    #[test]
    fn revalidation_is_explicit_requires_scope_policy_and_cannot_be_replaced() {
        let security = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec![],
        ).unwrap();
        assert!(security.clone().with_sse_revalidation(SseRevalidationPolicy::default()).is_err());
        let security = security.with_scope_authorization(policy()).unwrap()
            .with_sse_revalidation(SseRevalidationPolicy::default()).unwrap();
        assert!(security.clone().with_sse_revalidation(SseRevalidationPolicy::default()).is_err());
        assert_eq!(security.sse_revalidation.unwrap().interval(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn named_http_denials_do_not_disclose_existence_or_scope_configuration() {
        use super::super::super::scope_policy::request::operation::{OperationScopePolicy, ScopedOperation};
        let named = ScopeRequestPolicy::for_operations(OperationScopePolicy::new(2, policy(), vec![
            (ScopedOperation::ToolCall("hidden-canary".into()), RequiredScopes::new(vec!["secret-scope-canary".into()]).unwrap()),
        ]).unwrap()).unwrap();
        let mut request = method_request("tools/call");
        request.params = Some(serde_json::json!({"name":"hidden-canary"}));
        let forbidden = scope_rejection(&named, &request, Some(&facts(&["admin"]))).unwrap();
        request.params = Some(serde_json::json!({"name":"unknown-canary"}));
        let unknown = scope_rejection(&named, &request, Some(&facts(&["admin"]))).unwrap();
        assert_eq!(forbidden.status.0, 403);
        assert_eq!(unknown.status.0, 403);
        assert_eq!(unknown.headers, forbidden.headers);
        assert_eq!(unknown.body, forbidden.body);
        assert_eq!(forbidden.headers["www-authenticate"], "Bearer error=\"insufficient_scope\"");
        assert_eq!(forbidden.headers["cache-control"], "no-store");
        assert!(forbidden.body.is_empty());
        assert!(!format!("{:?}", forbidden.headers).contains("canary"));
        request.params = Some(serde_json::json!({"name":"hidden-canary"}));
        assert!(scope_rejection(&named, &request, Some(&facts(&["admin", "secret-scope-canary"]))).is_none());
        let anonymous = scope_rejection(&named, &request, None).unwrap();
        assert_eq!(anonymous.status.0, 401);
        assert_eq!(anonymous.headers["www-authenticate"], "Bearer");
    }
}
