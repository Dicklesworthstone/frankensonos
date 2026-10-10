//! Frozen AUTH-01 implementation A proofs: transport credential extraction
//! and principal admission.
//!
//! These two IDs deliberately live in an external integration target rather
//! than in `fastmcp-server`'s inline `#[cfg(test)]` module. An inline test
//! compiles only under `cfg(test)` and reaches sibling items through
//! `use super::*`, so it can prove the test build and nothing else. PL-3
//! forecloses that: `cfg(test)` behavior cannot prove shipped behavior, and
//! the frozen acceptance names the evaluator as the public AUTH-01 consumer.
//!
//! This target links `fastmcp-server` the way the AUTH-01 integration child
//! (`bd-zzmlo`) does — as an ordinary downstream dependency reached only
//! through `use fastmcp_server::...`. Every symbol touched below is part of
//! the shipped, non-`cfg(test)` surface: `Server`/`ServerBuilder`,
//! `AuthProvider`, `TokenAuthProvider`, `StaticTokenVerifier`, `Middleware`,
//! `ToolHandler`, `ServerHttpEndpoint::open_session`, and
//! `ServerHttpSession::handle_async`.
//!
//! The capability under test is the native HTTP credential boundary described
//! in the README: a native HTTP request carries its bearer credential *only*
//! in the `Authorization` header, and a recognized credential in the JSON-RPC
//! body or the URI query is refused with a fixed migration diagnostic before
//! the authentication provider runs at all.
//!
//! The inline unit tests at `src/auth.rs` remain in place and are untouched;
//! they cover the same predicates at the unit level against crate internals.

// Raised for the trait solver: coercing the nested async blocks this target
// spawns through `Cx::spawn_via_gateway` to `Pin<Box<dyn Future + Send>>`
// overflows the default limit of 128 on newer rustc. Without this the crate
// emits a FUTURE-INCOMPATIBLE `recursion_depth_exceeding_limit` warning
// (rust-lang/rust#159228) that rustc states will become a hard error, and the
// lint is crate-attached so it cannot be silenced on the one test that trips
// it. 256 matches the sibling target `srv_02_b.rs`; `fastmcp-server`'s and
// `fastmcp-client`'s own `src/lib.rs` use 512 for the same reason.
#![recursion_limit = "256"]

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpContext, McpError, McpResult};
use fastmcp_protocol::protocol_policy::{MODERN_PROTOCOL_VERSION, ProtocolPolicy};
use fastmcp_protocol::{
    Content, FINAL_CLIENT_CAPABILITIES_META_KEY, FINAL_PROTOCOL_VERSION_META_KEY, JsonRpcRequest,
    JsonRpcResponse, Tool,
};
use fastmcp_server::{
    AuthProvider, AuthRequest, Middleware, MiddlewareDecision, Server, ServerHttpEndpoint,
    ServerHttpEndpointResponse, StaticTokenVerifier, TokenAuthProvider, ToolHandler,
};
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpResponse, HttpStatus};
use serde_json::json;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The tool registered on the public builder. Its name is mirrored in the
/// strict modern `mcp-name` header, so the request is admitted by the shipped
/// modern POST validator before authentication is reached.
const AUTH_01_TOOL: &str = "auth_01_a_principal_probe";

/// One fixed JSON-RPC id shared by the positive and the planted negative, so
/// the two requests differ in exactly one dimension.
const AUTH_01_REQUEST_ID: i64 = 945;

/// Application data that deliberately uses two *recognized* credential field
/// names inside `arguments`. Authentication must not treat nested application
/// arguments as a credential location, and must not strip them.
fn auth_01_application_arguments() -> serde_json::Value {
    json!({"token": "application-data", "access_token": "application-data"})
}

/// Where the single bearer credential is placed on the wire.
///
/// This enum is the only variable between the positive and the planted
/// negative. Everything else — server, provider, token, subject, tool,
/// arguments, request id, protocol version, headers, path — is constructed by
/// the same code for every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialPlacement {
    /// Shipped-compliant native HTTP: `Authorization: Bearer <token>` only.
    AuthorizationHeader,
    /// Forbidden. Exactly one field added to the admitted positive request:
    /// `params.headers.Authorization`. The valid native header is retained,
    /// which isolates "a recognized body credential location is present" as
    /// the sole delta.
    AuthorizationHeaderPlusBodyCredential,
    /// Forbidden. The identical credential relocated out of the native header
    /// into `params.headers.Authorization`. This is the "a body token cannot
    /// authenticate an HTTP request" case.
    BodyCredentialOnly,
    /// Forbidden. Exactly one query key added to the admitted positive
    /// request: `?access_token=<token>`, native header retained.
    AuthorizationHeaderPlusQueryCredential,
}

impl CredentialPlacement {
    fn carries_native_header(self) -> bool {
        matches!(
            self,
            Self::AuthorizationHeader
                | Self::AuthorizationHeaderPlusBodyCredential
                | Self::AuthorizationHeaderPlusQueryCredential
        )
    }

    fn carries_body_credential(self) -> bool {
        matches!(
            self,
            Self::AuthorizationHeaderPlusBodyCredential | Self::BodyCredentialOnly
        )
    }

    fn carries_query_credential(self) -> bool {
        matches!(self, Self::AuthorizationHeaderPlusQueryCredential)
    }
}

/// Every named mutable state field owned by the AUTH-01 probe.
///
/// The planted negative proves each of these byte-for-byte unchanged across a
/// rejection. `Err` alone is not accepted as evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AdmissionState {
    provider_calls: usize,
    middleware_calls: usize,
    handler_calls: usize,
    observed_credentials: Vec<String>,
    observed_subjects: Vec<String>,
    observed_arguments: Vec<String>,
    credential_leaks: usize,
}

impl AdmissionState {
    /// Canonical byte encoding used for the byte-for-byte unchanged proof.
    fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "provider_calls": self.provider_calls,
            "middleware_calls": self.middleware_calls,
            "handler_calls": self.handler_calls,
            "observed_credentials": self.observed_credentials,
            "observed_subjects": self.observed_subjects,
            "observed_arguments": self.observed_arguments,
            "credential_leaks": self.credential_leaks,
        }))
        .expect("bounded admission state must serialize")
    }
}

/// A single object installed on the public builder as the authentication
/// provider, as extension middleware, and as the registered tool, so one
/// state bag observes every stage that a credential could reach.
#[derive(Clone)]
struct Auth01Probe {
    provider: Arc<TokenAuthProvider>,
    provider_calls: Arc<AtomicUsize>,
    middleware_calls: Arc<AtomicUsize>,
    handler_calls: Arc<AtomicUsize>,
    observed_credentials: Arc<Mutex<Vec<String>>>,
    observed_subjects: Arc<Mutex<Vec<String>>>,
    observed_arguments: Arc<Mutex<Vec<String>>>,
    credential_leaks: Arc<AtomicUsize>,
    token: String,
    subject: String,
}

impl Auth01Probe {
    /// Selects the credential and the principal at run time (RH-12): neither
    /// value is a checked-in fixture, so a hard-coded success path cannot
    /// satisfy these assertions.
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock must follow the Unix epoch")
            .as_nanos();
        let token = format!("auth01a-{}-{nonce}", std::process::id());
        let subject = format!("auth01a-principal-{nonce}");
        let verifier =
            StaticTokenVerifier::new([(token.clone(), AuthContext::with_subject(subject.clone()))])
                .expect("runtime-selected static credential must be admissible");
        Self {
            provider: Arc::new(TokenAuthProvider::new(verifier)),
            provider_calls: Arc::new(AtomicUsize::new(0)),
            middleware_calls: Arc::new(AtomicUsize::new(0)),
            handler_calls: Arc::new(AtomicUsize::new(0)),
            observed_credentials: Arc::new(Mutex::new(Vec::new())),
            observed_subjects: Arc::new(Mutex::new(Vec::new())),
            observed_arguments: Arc::new(Mutex::new(Vec::new())),
            credential_leaks: Arc::new(AtomicUsize::new(0)),
            token,
            subject,
        }
    }

    fn snapshot(&self) -> AdmissionState {
        AdmissionState {
            provider_calls: self.provider_calls.load(Ordering::Acquire),
            middleware_calls: self.middleware_calls.load(Ordering::Acquire),
            handler_calls: self.handler_calls.load(Ordering::Acquire),
            observed_credentials: self
                .observed_credentials
                .lock()
                .expect("credential observation mutex must not be poisoned")
                .clone(),
            observed_subjects: self
                .observed_subjects
                .lock()
                .expect("subject observation mutex must not be poisoned")
                .clone(),
            observed_arguments: self
                .observed_arguments
                .lock()
                .expect("argument observation mutex must not be poisoned")
                .clone(),
            credential_leaks: self.credential_leaks.load(Ordering::Acquire),
        }
    }

    fn record_leak(&self) {
        self.credential_leaks.fetch_add(1, Ordering::AcqRel);
    }
}

impl AuthProvider for Auth01Probe {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        self.provider_calls.fetch_add(1, Ordering::AcqRel);

        // Observe transport credential extraction itself: the scheme the
        // shipped extractor parsed out of the transport-private Authorization
        // field, and whether the extracted bytes are the credential this run
        // selected.
        let observation = match request.access_token() {
            Some(access) => format!("{}:{}", access.scheme, access.token == self.token),
            None => "absent:false".to_owned(),
        };
        self.observed_credentials
            .lock()
            .expect("credential observation mutex must not be poisoned")
            .push(observation);

        // Native HTTP must never reach a provider without the transport-private
        // Authorization field. This is an admission fact rather than a leak, so
        // it is recorded through `observed_credentials` above, not the leak
        // counter.
        if request.transport_authorization.is_none() {
            return Err(McpError::invalid_request(
                "native HTTP must supply its credential through Authorization",
            ));
        }
        if request
            .params
            .map(|params| params.to_string())
            .is_some_and(|params| params.contains(&self.token))
        {
            self.record_leak();
            return Err(McpError::invalid_request(
                "a raw credential reached the provider through JSON-RPC params",
            ));
        }

        self.provider.authenticate(ctx, request)
    }
}

impl Middleware for Auth01Probe {
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        self.middleware_calls.fetch_add(1, Ordering::AcqRel);
        let serialized = serde_json::to_string(request)
            .map_err(|error| McpError::internal_error(error.to_string()))?;
        if serialized.contains(&self.token) {
            self.record_leak();
            return Err(McpError::internal_error(
                "a raw credential reached extension middleware",
            ));
        }
        if ctx.auth().and_then(|auth| auth.subject).as_deref() != Some(self.subject.as_str()) {
            return Err(McpError::internal_error(
                "the admitted principal is absent at extension middleware",
            ));
        }
        Ok(MiddlewareDecision::Continue)
    }
}

impl ToolHandler for Auth01Probe {
    fn definition(&self) -> Tool {
        Tool {
            name: AUTH_01_TOOL.to_owned(),
            description: Some("AUTH-01 A principal admission probe".to_owned()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: None,
        }
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        self.handler_calls.fetch_add(1, Ordering::AcqRel);
        let serialized = serde_json::to_string(&arguments)
            .map_err(|error| McpError::internal_error(error.to_string()))?;
        if serialized.contains(&self.token) {
            self.record_leak();
            return Err(McpError::internal_error(
                "a raw credential reached the tool handler",
            ));
        }
        self.observed_arguments
            .lock()
            .expect("argument observation mutex must not be poisoned")
            .push(serialized);

        let subject = ctx.auth().and_then(|auth| auth.subject).unwrap_or_default();
        self.observed_subjects
            .lock()
            .expect("subject observation mutex must not be poisoned")
            .push(subject.clone());
        Ok(vec![Content::text(subject)])
    }
}

/// Builds the single public server used by both frozen IDs.
fn auth_01_endpoint(probe: &Auth01Probe) -> ServerHttpEndpoint {
    let builder = Server::new("auth-01-a", "1.0.0")
        .protocol_policy(ProtocolPolicy::ModernOnly)
        .expect("ModernOnly is available in every server feature set")
        .auth_provider(probe.clone())
        .middleware(probe.clone())
        .tool(probe.clone());
    // The public constructor's arity is the one thing that varies with the
    // dual-era feature. The composed server, policy, and route are identical.
    #[cfg(not(feature = "legacy-2024-11-05"))]
    let endpoint = builder.build_http_endpoint();
    #[cfg(feature = "legacy-2024-11-05")]
    let endpoint = builder.build_http_endpoint("http://auth01a.invalid");
    endpoint.expect("the public builder must construct its modern HTTP endpoint")
}

/// Builds one modern Streamable HTTP `tools/call`. Every field is fixed; only
/// `placement` decides where the single credential is carried.
fn auth_01_http_request(token: &str, placement: CredentialPlacement) -> HttpRequest {
    let authorization = format!("Bearer {token}");
    let mut params = json!({
        "name": AUTH_01_TOOL,
        "arguments": auth_01_application_arguments(),
        "_meta": {
            FINAL_PROTOCOL_VERSION_META_KEY: MODERN_PROTOCOL_VERSION,
            FINAL_CLIENT_CAPABILITIES_META_KEY: {},
        },
    });
    if placement.carries_body_credential() {
        params["headers"] = json!({"Authorization": authorization.clone()});
    }
    let envelope = JsonRpcRequest::new("tools/call", Some(params), AUTH_01_REQUEST_ID);

    let mut request = HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("mcp-protocol-version", MODERN_PROTOCOL_VERSION)
        .with_header("mcp-method", "tools/call")
        .with_header("mcp-name", AUTH_01_TOOL)
        .with_body(serde_json::to_vec(&envelope).expect("the modern tool request must serialize"));
    if placement.carries_native_header() {
        request = request.with_header("authorization", authorization.clone());
    }
    if placement.carries_query_credential() {
        request = request.with_query("access_token", token);
    }
    request
}

/// Drives one HTTP request through the public session and requires the
/// shipped surface to return a complete (non-streaming) response.
fn immediate(response: ServerHttpEndpointResponse) -> HttpResponse {
    match response {
        ServerHttpEndpointResponse::Immediate(response) => response,
        _ => panic!("AUTH-01 A admission must return a complete HTTP response"),
    }
}

/// Runs an AUTH-01 scenario on a caller-owned runtime. FastMCP never creates
/// a runtime; the blocking pool exists because the registered tool uses the
/// default `ToolExecutionMode::Blocking` hook.
fn run_auth_01_scenario<F, Fut>(scenario: F)
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(
            asupersync::runtime::reactor::create_reactor()
                .expect("the caller AUTH-01 runtime reactor must initialize"),
        )
        .blocking_threads(4, 64)
        .build()
        .expect("the caller AUTH-01 runtime must initialize");
    runtime.block_on(async move {
        let parent = Cx::current().expect("the caller runtime supplies the ambient context");
        let mut task = parent
            .spawn(scenario)
            .expect("the caller runtime admits the AUTH-01 scenario");
        task.join(&parent)
            .await
            .expect("the caller runtime joins the complete AUTH-01 scenario");
    });
}

/// Asserts the fully admitted native-header outcome for one request.
///
/// `sequence` is the number of admitted requests completed *before* this one,
/// so exactly-once effects are checked cumulatively rather than by resetting
/// state between calls.
fn assert_admitted(probe: &Auth01Probe, response: &HttpResponse, sequence: usize) {
    assert_eq!(
        response.status,
        HttpStatus::OK,
        "a native Authorization credential must be admitted"
    );
    let body: JsonRpcResponse = serde_json::from_slice(&response.body)
        .expect("an admitted modern tool call must return JSON-RPC");
    assert_eq!(body.id, Some(AUTH_01_REQUEST_ID.into()));
    assert!(body.error.is_none(), "unexpected error: {:?}", body.error);
    let result = body
        .result
        .expect("an admitted tool call must carry a result");

    // Observed field: the public receipt carries the admitted principal that
    // the shipped tool handler read from its context.
    assert_eq!(
        result["content"][0]["text"], probe.subject,
        "the admitted principal must reach the public result"
    );

    // The response must never echo the credential back to the peer.
    assert!(
        !String::from_utf8_lossy(&response.body).contains(&probe.token),
        "the admitted response leaked the raw credential"
    );

    let state = probe.snapshot();
    assert_eq!(state.provider_calls, sequence + 1);
    assert_eq!(state.middleware_calls, sequence + 1);
    assert_eq!(state.handler_calls, sequence + 1);
    assert_eq!(state.credential_leaks, 0);
    assert_eq!(
        state.observed_credentials,
        vec!["Bearer:true".to_owned(); sequence + 1],
        "the shipped extractor must recover the exact native bearer credential"
    );
    assert_eq!(
        state.observed_subjects,
        vec![probe.subject.clone(); sequence + 1],
        "every admitted request must publish exactly the verified principal"
    );
    assert_eq!(
        state.observed_arguments,
        vec![
            serde_json::to_string(&auth_01_application_arguments())
                .expect("application arguments must serialize");
            sequence + 1
        ],
        "application fields that merely share a credential field name must survive intact"
    );
}

/// Asserts the fixed native-HTTP credential-location refusal.
fn assert_credential_location_refusal(probe: &Auth01Probe, response: &HttpResponse) {
    assert_eq!(
        response.status,
        HttpStatus::UNAUTHORIZED,
        "a recognized non-Authorization credential location must be refused"
    );
    assert_eq!(
        response.headers.get("www-authenticate").map(String::as_str),
        Some("Bearer"),
        "the refusal must remain a bounded bearer challenge"
    );
    let body: serde_json::Value = serde_json::from_slice(&response.body)
        .expect("the migration diagnostic must be a JSON body");
    assert_eq!(
        body,
        json!({
            "error": "invalid_request",
            "message": "HTTP credentials must use the Authorization header",
        }),
        "the refusal must be the fixed migration diagnostic"
    );
    // No token oracle: neither the credential nor the principal may appear.
    let rendered = String::from_utf8_lossy(&response.body);
    assert!(
        !rendered.contains(&probe.token),
        "the refusal reflected the credential"
    );
    assert!(
        !rendered.contains(&probe.subject),
        "the refusal reflected the principal"
    );
}

/// AUTH-01 A positive: a native HTTP request whose bearer credential is
/// carried only in the `Authorization` header is extracted at the transport
/// boundary, verified once, and admits its principal into the shipped
/// handler's context — with the raw credential never entering JSON-RPC
/// params, extension middleware, the handler, or the response.
#[test]
fn auth_01_a_positive() {
    run_auth_01_scenario(|cx| async move {
        let probe = Auth01Probe::new();
        let endpoint = auth_01_endpoint(&probe);
        let mut session = endpoint
            .open_session(&cx)
            .expect("the public HTTP session must open");

        assert_eq!(
            probe.snapshot(),
            AdmissionState {
                provider_calls: 0,
                middleware_calls: 0,
                handler_calls: 0,
                observed_credentials: Vec::new(),
                observed_subjects: Vec::new(),
                observed_arguments: Vec::new(),
                credential_leaks: 0,
            },
            "the scenario must start from an untouched public server"
        );

        let request = auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader);
        let body_before = request.body.clone();

        let response = immediate(
            session
                .handle_async(&cx, request.clone())
                .await
                .expect("the public HTTP session must complete the admitted request"),
        );

        assert_admitted(&probe, &response, 0);
        assert_eq!(
            request.body, body_before,
            "admission must not mutate the caller's request body"
        );

        // Admission is repeatable on the same public session: the second
        // request is verified on its own merits, not replayed from the first.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("the public HTTP session must complete the repeated request"),
        );
        assert_admitted(&probe, &response, 1);
    });
}

/// AUTH-01 A planted negative: the same server, the same credential, the same
/// principal, the same tool, the same arguments and the same request id as
/// `auth_01_a_positive`, varying only the forbidden dimension — where the
/// credential is carried.
///
/// Each forbidden placement must be refused with the fixed migration
/// diagnostic before the provider runs, and must leave every named mutable
/// state field byte-for-byte unchanged. Returning `Err` is explicitly not
/// accepted as the proof.
#[test]
fn auth_01_a_planted_negative() {
    run_auth_01_scenario(|cx| async move {
        let probe = Auth01Probe::new();
        let endpoint = auth_01_endpoint(&probe);
        let mut session = endpoint
            .open_session(&cx)
            .expect("the public HTTP session must open");

        let before = probe.snapshot();
        assert_eq!(
            before,
            AdmissionState {
                provider_calls: 0,
                middleware_calls: 0,
                handler_calls: 0,
                observed_credentials: Vec::new(),
                observed_subjects: Vec::new(),
                observed_arguments: Vec::new(),
                credential_leaks: 0,
            },
            "the scenario must start from an untouched public server"
        );
        let before_bytes = before.canonical_bytes();

        for placement in [
            CredentialPlacement::AuthorizationHeaderPlusBodyCredential,
            CredentialPlacement::BodyCredentialOnly,
            CredentialPlacement::AuthorizationHeaderPlusQueryCredential,
        ] {
            let request = auth_01_http_request(&probe.token, placement);
            let body_before = request.body.clone();

            let response = immediate(
                session
                    .handle_async(&cx, request.clone())
                    .await
                    .expect("a credential-location refusal is an ordinary HTTP response"),
            );
            assert_credential_location_refusal(&probe, &response);

            let after = probe.snapshot();
            assert_eq!(
                after, before,
                "{placement:?} mutated named AUTH-01 state after refusal"
            );
            assert_eq!(
                after.canonical_bytes(),
                before_bytes,
                "{placement:?} left AUTH-01 state that is not byte-for-byte unchanged"
            );
            assert_eq!(
                request.body, body_before,
                "{placement:?} mutated the caller's request body"
            );
        }

        // The refusals are caused by the varied dimension alone: the
        // otherwise-identical native-header request still succeeds on this
        // same session, and is the first request to move any named state.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("the permitted credential location must still be admitted"),
        );
        assert_admitted(&probe, &response, 0);
    });
}

/// Supplementary external-consumer proof. Deliberately **not** one of the two
/// frozen AUTH-01 A IDs, and named so that no `--exact` selection of either can
/// match it; it adds a side, it does not restate one.
///
/// Both frozen tests refuse BEFORE they admit. That ordering cannot observe the
/// failure it most matters to exclude: a session that, having once admitted a
/// valid `Authorization` credential, starts honouring an in-band credential
/// afterwards because it cached the principal. A server that authenticated only
/// the first request of a session and then trusted the connection would pass
/// both frozen tests and fail this one. Reversing the order is the whole test.
///
/// `provider_calls` is the load-bearing observable rather than the status code:
/// it separates "refused before the provider ran" from "ran the provider and
/// reported a refusal afterwards", which the response alone cannot distinguish.
#[test]
fn auth_01_a_admission_does_not_license_a_later_in_band_credential() {
    run_auth_01_scenario(|cx| async move {
        let probe = Auth01Probe::new();
        let endpoint = auth_01_endpoint(&probe);
        let mut session = endpoint
            .open_session(&cx)
            .expect("the public HTTP session must open");

        // Side one, the control: a native header credential is admitted, so the
        // session has now seen a verified principal.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("the public HTTP session must complete the admitted request"),
        );
        assert_admitted(&probe, &response, 0);
        let admitted = probe.snapshot();
        let admitted_bytes = admitted.canonical_bytes();

        // Side two, the one variable: same session, same credential, same
        // principal, same tool, same arguments, same request id - only the
        // credential's location changes. The earlier admission licenses nothing.
        for placement in [
            CredentialPlacement::AuthorizationHeaderPlusBodyCredential,
            CredentialPlacement::BodyCredentialOnly,
            CredentialPlacement::AuthorizationHeaderPlusQueryCredential,
        ] {
            let response = immediate(
                session
                    .handle_async(&cx, auth_01_http_request(&probe.token, placement))
                    .await
                    .expect("a credential-location refusal is an ordinary HTTP response"),
            );
            assert_credential_location_refusal(&probe, &response);

            let after = probe.snapshot();
            assert_eq!(
                after, admitted,
                "{placement:?} moved named AUTH-01 state after an earlier admission"
            );
            assert_eq!(
                after.canonical_bytes(),
                admitted_bytes,
                "{placement:?} left AUTH-01 state that is not byte-for-byte the \
                 post-admission state"
            );
        }

        // Side three: the refusals are a no-op, not a wedge. The session still
        // admits the permitted placement, and admits it at sequence 1 - which is
        // what proves the three refusals contributed no admission of their own.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("a refused credential location must not wedge the session"),
        );
        assert_admitted(&probe, &response, 1);
    });
}

/// The OTHER refusal limb. Supplementary, not a frozen ID.
///
/// The shipped extractor refuses on two distinct grounds, and the frozen
/// negative exercises only the first:
///
/// - `native_http_credential_location_rejection` — a credential outside the
///   `Authorization` header. 401, challenge, and a JSON migration diagnostic.
/// - `native_http_authentication_rejection` — credential AMBIGUITY, raised by
///   `singleton.replace(..).is_some()`. 401, challenge, and **no body**.
///
/// `HttpRequest::headers` is a `HashMap<String, String>`, so a duplicate header
/// *name* is impossible — but the extractor matches with `eq_ignore_ascii_case`,
/// so `authorization` and `Authorization` are two distinct entries that both
/// match. That is reachable by any consumer, and it is the dangerous shape: a
/// server that resolved the pair instead of refusing would pick a credential by
/// `HashMap` iteration order, i.e. NONDETERMINISTICALLY, and would therefore
/// fail only intermittently in exactly the way nobody debugs.
///
/// The empty body is the discriminator. Asserting only the status would let a
/// server collapse both limbs into one diagnostic and still pass.
///
/// DO NOT DELETE THIS AS UNREACHABLE. There are two independent defenses and
/// this one is the inner. The wire codec (`fastmcp-transport/src/http.rs:2774`)
/// lowercases every field name and then refuses a repeated one with
/// `HttpError::InvalidHeader("duplicate header: ..")` — its comment is explicit
/// that "duplicated singletons must remain an admission failure". So a request
/// arriving over a socket never reaches the extractor's `singleton` check, and
/// it is tempting to conclude this case cannot happen.
///
/// It can. `HttpRequest` is a public struct with public fields, and
/// `handle_async` accepts one directly, so an in-process consumer composes the
/// map without the codec ever running. That is a shipped path, not a synthetic
/// one. Proving only the outer defense would leave the extractor free to start
/// resolving ambiguity the day the codec's normalization changes — which is the
/// same defense-in-depth argument that keeps a second check meaningful even
/// when an earlier one usually fires first.
#[test]
fn auth_01_a_ambiguous_authorization_headers_are_refused_before_any_provider_call() {
    run_auth_01_scenario(|cx| async move {
        let probe = Auth01Probe::new();
        let endpoint = auth_01_endpoint(&probe);
        let mut session = endpoint
            .open_session(&cx)
            .expect("the public HTTP session must open");

        let before = probe.snapshot();
        let before_bytes = before.canonical_bytes();

        // A second credential that must never be chosen. It is deliberately not
        // the probe's token, so "which one won" is answerable from the receipt.
        let rival = "Bearer auth-01-a-rival-credential";
        let mut request =
            auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader);
        // The builder writes the lowercase name; this is a SECOND, distinct map
        // entry that still matches the extractor's case-insensitive compare.
        request
            .headers
            .insert("Authorization".to_owned(), rival.to_owned());
        assert_eq!(
            request
                .headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("authorization"))
                .count(),
            2,
            "the ambiguity this case exists to prove must actually be present in the request"
        );

        let response = immediate(
            session
                .handle_async(&cx, request)
                .await
                .expect("an ambiguous credential is an ordinary HTTP refusal"),
        );

        assert_eq!(
            response.status,
            HttpStatus::UNAUTHORIZED,
            "two Authorization headers must not be silently resolved to one"
        );
        assert_eq!(
            response.headers.get("www-authenticate").map(String::as_str),
            Some("Bearer"),
            "the ambiguity refusal must remain a bounded bearer challenge"
        );
        assert!(
            response.body.is_empty(),
            "the ambiguity refusal carries no body; a JSON migration diagnostic here would \
             mean the two refusal limbs have been collapsed into one"
        );

        // Refused BEFORE the provider ran - the status alone cannot show this.
        let after = probe.snapshot();
        assert_eq!(
            after, before,
            "an ambiguous credential moved named AUTH-01 state"
        );
        assert_eq!(
            after.canonical_bytes(),
            before_bytes,
            "an ambiguous credential left state that is not byte-for-byte unchanged"
        );

        let rendered = String::from_utf8_lossy(&response.body);
        assert!(
            !rendered.contains(&probe.token) && !rendered.contains(rival),
            "the ambiguity refusal reflected a credential"
        );

        // Still usable: the ambiguity is rejected, not the session.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("an ambiguous credential must not wedge the session"),
        );
        assert_admitted(&probe, &response, 0);
    });
}

/// Negative authority, the sharpest form. Supplementary, not a frozen ID.
///
/// `:649` proves a prior admission does not license a credential in a FORBIDDEN
/// LOCATION. This proves the stronger thing: it does not license a request
/// carrying **no credential at all**. A server that verified once and then
/// trusted the session would admit this, and it would pass every other case in
/// this file.
///
/// `AuthRequest` is still BUILT for this request, with
/// `transport_authorization: None` (`router.rs:113/126`) - but it is refused
/// before the provider sees it, by the transport guard in
/// `preauthenticate_http_request` - the `transport_authorization.is_none()` arm
/// returning `native_http_authentication_rejection()` - which runs before
/// `authenticate_request_without_commit` reaches `provider.authenticate`. So
/// `provider_calls` does NOT move for the uncredentialed request: it stays at
/// the single consultation the earlier credentialed request caused.
///
/// The load-bearing assertion is therefore the UNAUTHORIZED refusal SHAPE -
/// status, `www-authenticate: Bearer`, and an empty body together - which
/// shows the request was refused by that guard rather than admitted on a
/// cached principal.
///
/// REVISED 2026-09-17. Until then this paragraph said the provider IS invoked,
/// that `provider_calls` was expected to MOVE, and that `observed_credentials`
/// gaining `absent:false` was load-bearing. All three described the pre-75099bf1
/// mechanism and are now false: since 2026-09-05 the server refuses first, so
/// `absent:false` is never recorded. The prose is corrected with the assertions
/// rather than left arguing for a contract the server no longer implements.
///
/// Deliberately NOT asserted, with reasons, rather than guessed:
/// - `middleware_calls`. Whether middleware runs before or after authentication
///   is not part of the AUTH-01 contract, and pinning it here would couple this
///   proof to an unrelated design choice that is free to change.
#[test]
fn auth_01_a_admission_does_not_license_a_later_uncredentialed_request() {
    run_auth_01_scenario(|cx| async move {
        let probe = Auth01Probe::new();
        let endpoint = auth_01_endpoint(&probe);
        let mut session = endpoint
            .open_session(&cx)
            .expect("the public HTTP session must open");

        // Control: a native header credential is admitted.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("the public HTTP session must complete the admitted request"),
        );
        assert_admitted(&probe, &response, 0);

        // The one variable: the identical request with its Authorization field
        // removed. The builder writes the lowercase name.
        let mut request =
            auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader);
        request
            .headers
            .remove("authorization")
            .expect("the builder must have written the field this case removes");
        assert!(
            !request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization")),
            "the uncredentialed request must carry no Authorization field in any spelling"
        );

        let response = immediate(
            session
                .handle_async(&cx, request)
                .await
                .expect("an uncredentialed request is an ordinary HTTP response"),
        );

        // The refusal SHAPE, not merely "not 200". `assert_ne!(status, OK)`
        // also passes on a 500, so it cannot distinguish a fail-closed refusal
        // from a crash -- and with the mechanism counts below relaxed to match
        // the shipped guard, this is now the assertion carrying the property.
        // Strengthened to the three-part idiom this file already uses for the
        // ambiguity refusal at :792-804, matching what
        // `native_http_authentication_rejection` produces.
        assert_eq!(
            response.status,
            HttpStatus::UNAUTHORIZED,
            "a request carrying no credential was admitted after an earlier one succeeded, so \
             the session is honouring a cached principal"
        );
        assert_eq!(
            response.headers.get("www-authenticate").map(String::as_str),
            Some("Bearer"),
            "the uncredentialed refusal must remain a bounded bearer challenge"
        );
        assert!(
            response.body.is_empty(),
            "an empty body is what separates the authentication refusal from the \
             credential-location refusal (`native_http_credential_location_rejection`), which \
             renders the same status and \
             header but carries an `invalid_request` diagnostic"
        );

        let state = probe.snapshot();
        assert_eq!(
            state.handler_calls, 1,
            "the tool ran for an uncredentialed request"
        );
        // REPAIRED 2026-09-17, authorised by WildMountain after a contract
        // ruling. These two read `provider_calls == 2` and
        // `observed_credentials == ["Bearer:true", "absent:false"]`, asserting
        // that the server consults the auth provider for an uncredentialed
        // request. The shipped design refuses BEFORE the provider:
        // `preauthenticate_http_request` returns
        // `native_http_authentication_rejection()` from its
        // `transport_authorization.is_none()` arm, before
        // `authenticate_request_without_commit` reaches `provider.authenticate`.
        //
        // ANCHORED BY SYMBOL, NOT BY LINE. This rationale previously cited
        // `lib.rs:19893` and `:19904` as "eleven lines" apart. Both rotted: by
        // 2026-09-21 those lines held unrelated code and the two sites were 210
        // lines apart in different functions. A line citation in a 28k-line file
        // decays fast, and this one carries the whole justification for an
        // authorised relaxation - a reviewer who checks a dead line number and
        // finds unrelated code may read the relaxation as unjustified. Symbols
        // survive motion; line numbers do not.
        //
        // The guard is DELIBERATE, not a regression. It landed in 75099bf1 on
        // 2026-09-05, "feat(auth): require Authorization header and reject
        // query/body credentials for HTTP transport". Refusing without
        // consulting the provider is fail-closed and strictly stronger than
        // consulting it and refusing afterwards. This test asserted the
        // pre-change mechanism; the security property it is named for never
        // stopped holding and is asserted above by the UNAUTHORIZED check.
        // Only the mechanism counts were stale.
        //
        // WHY `== 1` IS A REAL ASSERTION AND NOT MERELY A SMALLER NUMBER: it
        // excludes both worthless implementations at once.
        //   - never-consult cannot reach 1, because the FIRST (credentialed)
        //     request must consult the provider in order to be admitted;
        //   - always-consult reaches 2, because it would also consult for the
        //     uncredentialed request.
        // Only "consulted for the credentialed request and NOT for the
        // uncredentialed one" satisfies it, which is exactly the guarantee
        // 75099bf1 introduced.
        assert_eq!(
            state.provider_calls, 1,
            "the provider was consulted for the uncredentialed request; the shipped guard at \
             transport guard in preauthenticate_http_request must refuse it before any \
             user-supplied provider code runs"
        );
        assert_eq!(
            state.observed_credentials,
            vec!["Bearer:true".to_owned()],
            "the extractor observed a second credential, so the uncredentialed request reached \
             the provider instead of being refused at the transport guard"
        );
        assert_eq!(
            state.observed_subjects,
            vec![probe.subject.clone()],
            "a second principal was published for a request that carried no credential"
        );
        assert_eq!(state.credential_leaks, 0);

        // No oracle: the refusal must not disclose the credential, the
        // principal, or which provider check failed.
        //
        // NOTE 2026-09-17: both checks below are now SUBSUMED by the
        // `body.is_empty()` assertion above -- an empty body cannot contain the
        // token, the subject, or the provider's denial text, so neither can
        // fail while that assertion holds. Retained deliberately rather than
        // deleted: they name WHICH strings are forbidden, where `is_empty()`
        // only says none are present, so they are the checks that must keep
        // passing if the refusal ever grows a diagnostic body.
        let rendered = String::from_utf8_lossy(&response.body);
        assert!(
            !rendered.contains(&probe.token) && !rendered.contains(&probe.subject),
            "the uncredentialed refusal reflected the credential or the principal"
        );
        assert!(
            !rendered.contains("native HTTP must supply its credential through Authorization"),
            "the refusal echoed the provider's own denial text, which is a failure oracle; the \
             shipped path replaces it with the fixed diagnostic from \
             native_http_authentication_rejection"
        );

        // Still usable: the refusal is not a wedge. Asserted directly rather
        // than through `assert_admitted`, whose sequence model assumes every
        // prior observation was an admission - which is false here.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("an uncredentialed refusal must not wedge the session"),
        );
        assert_eq!(response.status, HttpStatus::OK);
        let state = probe.snapshot();
        assert_eq!(state.handler_calls, 2);
        // THIRD instance of the pre-75099bf1 premise, repaired 2026-09-17
        // alongside the two above. This expected
        // ["Bearer:true", "absent:false", "Bearer:true"].
        //
        // The middle entry is gone BY DERIVATION, not by shrinking a vector
        // until it matched a run. `observed_credentials` gains one entry per
        // PROVIDER INVOCATION (:222), and this test makes three requests:
        //   1. credentialed   -> provider consulted      -> "Bearer:true"
        //   2. uncredentialed -> refused at the preauthenticate_http_request guard
        //                        BEFORE the provider     -> nothing recorded
        //   3. recovered      -> provider consulted      -> "Bearer:true"
        // Two entries, and the vector's LENGTH is itself the proof that the
        // uncredentialed request never reached the provider.
        //
        // `handler_calls == 2` above is the independent discriminator that the
        // third request was really SERVED and not merely re-extracted: an
        // implementation that re-ran extraction but declined to dispatch would
        // produce this identical vector and fail there instead.
        assert_eq!(
            state.observed_credentials,
            vec!["Bearer:true".to_owned(), "Bearer:true".to_owned()],
            "the recovered admission must re-extract the credential from the wire; two entries \
             with no absence marker is what proves the uncredentialed request was refused at \
             the transport guard rather than recorded as an absent credential"
        );
    });
}

/// Idempotency of refusal. Supplementary, not a frozen ID.
///
/// The frozen negative sends each forbidden placement once. That cannot show
/// whether refusals ACCUMULATE — a server that counted attempts, degraded its
/// diagnostic, or admitted on a later try would pass it. Repetition of one
/// identical request is the only way to observe this.
#[test]
fn auth_01_a_repeated_identical_refusal_is_idempotent() {
    run_auth_01_scenario(|cx| async move {
        let probe = Auth01Probe::new();
        let endpoint = auth_01_endpoint(&probe);
        let mut session = endpoint
            .open_session(&cx)
            .expect("the public HTTP session must open");

        let before = probe.snapshot();
        let before_bytes = before.canonical_bytes();
        let mut refusals: Vec<Vec<u8>> = Vec::new();

        for attempt in 0..3 {
            let response = immediate(
                session
                    .handle_async(
                        &cx,
                        auth_01_http_request(&probe.token, CredentialPlacement::BodyCredentialOnly),
                    )
                    .await
                    .expect("a credential-location refusal is an ordinary HTTP response"),
            );
            assert_credential_location_refusal(&probe, &response);
            refusals.push(response.body.clone());

            let after = probe.snapshot();
            assert_eq!(after, before, "refusal {attempt} moved named AUTH-01 state");
            assert_eq!(
                after.canonical_bytes(),
                before_bytes,
                "refusal {attempt} left state that is not byte-for-byte unchanged"
            );
        }

        assert!(
            refusals.windows(2).all(|pair| pair[0] == pair[1]),
            "the refusal diagnostic changed across identical attempts, so it carries \
             attempt-dependent state"
        );

        // And the repetition still did not wedge the session.
        let response = immediate(
            session
                .handle_async(
                    &cx,
                    auth_01_http_request(&probe.token, CredentialPlacement::AuthorizationHeader),
                )
                .await
                .expect("repeated refusals must not wedge the session"),
        );
        assert_admitted(&probe, &response, 0);
    });
}
