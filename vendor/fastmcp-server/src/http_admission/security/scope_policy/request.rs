//! Default-deny scope admission before application middleware and dispatch.
//!
//! Method rules may be used alone or intersected with exact named-operation
//! rules via ScopeRequestPolicy::for_operations. Every execution path must use
//! authorize_request_verified when named rules are installed; the method-only
//! evaluator refuses such policies rather than silently dropping restrictions.
//!
//! Install with Server::with_scope_authorization after building the server and
//! before sharing it or opening a listener. The server inserts the private gate
//! ahead of application middleware, including caches and short-circuit handlers.
//! Authentication still runs first. The gate reads its verified facts without
//! expanding or mutating them; implication is evaluated against the pinned graph.
//!
//! This is an additional request gate, not a replacement for catalog visibility
//! or continuous authorization of streams/Tasks. No token, HTTP authentication
//! field or OAuth profile is added to stdio.

/// Exact named-operation rules intersected with the method policy.
pub mod operation;

use std::fmt;
use std::sync::Arc;

use fastmcp_core::{AuthContext, McpContext, McpError, McpErrorCode, McpResult, Sha256Digest, sha256_bounded};
use fastmcp_protocol::JsonRpcRequest;

use super::{RequiredScopes, ScopeImplicationPolicy, validate_scopes};
use crate::{Middleware, MiddlewareDecision, Server};

const MAX_METHODS: usize = 64;
const MAX_METHOD_BYTES: usize = 128;
const MAX_RULE_BYTES: usize = 64 * 1024;
const RULE_DOMAIN: &[u8] = b"fastmcp/method-scope-admission/v1\0";
const REFUSAL: &str = "Operation is not permitted";

/// Fixed configuration diagnostics do not retain method or scope names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRequestPolicyError {
    ZeroRevision,
    TooManyMethods,
    InvalidMethod,
    DuplicateMethod,
    PolicyTooLarge,
    NestedOperationPolicy,
}

impl fmt::Display for ScopeRequestPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroRevision => "request scope policy requires a nonzero revision",
            Self::TooManyMethods => "request scope policy exceeds its method count bound",
            Self::InvalidMethod => "request scope policy contains an invalid method",
            Self::DuplicateMethod => "request scope policy repeats a method",
            Self::PolicyTooLarge => "request scope policy exceeds its byte bound",
            Self::NestedOperationPolicy => "operation scope policies must use a method-only base",
        })
    }
}
impl std::error::Error for ScopeRequestPolicyError {}

/// Local admission outcome. None of these variants grants a token authority or
/// carries a required scope set that a peer could use to probe hidden resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRequestRejection {
    UnconfiguredMethod,
    InsufficientScope,
    InvalidFacts,
}

impl fmt::Display for ScopeRequestRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnconfiguredMethod => "request method has no configured scope rule",
            Self::InsufficientScope => "verified grants do not satisfy the complete request scope rule",
            Self::InvalidFacts => "request scope admission rejected invalid authentication facts",
        })
    }
}
impl std::error::Error for ScopeRequestRejection {}

struct CompiledRules {
    revision: u64,
    implications: ScopeImplicationPolicy,
    methods: Vec<(String, RequiredScopes)>,
    fingerprint: Sha256Digest,
}

/// Immutable scope requirements, optionally including exact named operations.
///
/// Absence of a method rule denies a method; an explicitly empty requirement
/// permits anonymous access to that method gate only. Named rules, authentication
/// and downstream policy may still refuse it. Empty configuration is deny-all.
/// Method lookup uses a bounded sorted table of at most 64 methods and 64 KiB.
/// Named policy adds its own bounded table, never a recursive policy chain.
#[derive(Clone)]
pub struct ScopeRequestPolicy {
    inner: Arc<CompiledRules>,
    operations: Option<operation::OperationScopePolicy>,
}

impl fmt::Debug for ScopeRequestPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeRequestPolicy")
            .field("revision", &self.revision())
            .field("method_count", &self.inner.methods.len())
            .field("named_operations", &self.operations.is_some())
            .finish_non_exhaustive()
    }
}

impl ScopeRequestPolicy {
    /// Compiles trusted `(exact_method, complete_required_scopes)` entries.
    /// Public methods must be listed explicitly with an empty RequiredScopes.
    /// Changing the numeric revision, any rule, or the implication graph changes
    /// the policy fingerprint. Input ordering does not change that identity.
    pub fn new(
        revision: u64,
        implications: ScopeImplicationPolicy,
        mut methods: Vec<(String, RequiredScopes)>,
    ) -> Result<Self, ScopeRequestPolicyError> {
        if revision == 0 { return Err(ScopeRequestPolicyError::ZeroRevision); }
        if methods.len() > MAX_METHODS { return Err(ScopeRequestPolicyError::TooManyMethods); }
        let mut size = RULE_DOMAIN.len() + 8 + 32 + 8;
        for (method, required) in &methods {
            if method.is_empty() || method.len() > MAX_METHOD_BYTES
                || !method.bytes().all(|byte| byte.is_ascii_graphic())
            { return Err(ScopeRequestPolicyError::InvalidMethod); }
            size = size.saturating_add(16).saturating_add(method.len());
            for scope in required.as_slice() {
                size = size.saturating_add(8).saturating_add(scope.len());
            }
            if size > MAX_RULE_BYTES { return Err(ScopeRequestPolicyError::PolicyTooLarge); }
        }
        methods.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if methods.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(ScopeRequestPolicyError::DuplicateMethod);
        }
        let mut identity = Vec::with_capacity(size);
        identity.extend_from_slice(RULE_DOMAIN);
        identity.extend_from_slice(&revision.to_be_bytes());
        identity.extend_from_slice(implications.fingerprint().as_bytes());
        identity.extend_from_slice(&(methods.len() as u64).to_be_bytes());
        for (method, required) in &methods {
            identity.extend_from_slice(&(method.len() as u64).to_be_bytes());
            identity.extend_from_slice(method.as_bytes());
            identity.extend_from_slice(&(required.as_slice().len() as u64).to_be_bytes());
            for scope in required.as_slice() {
                identity.extend_from_slice(&(scope.len() as u64).to_be_bytes());
                identity.extend_from_slice(scope.as_bytes());
            }
        }
        let fingerprint = sha256_bounded(&identity, MAX_RULE_BYTES)
            .map_err(|_| ScopeRequestPolicyError::PolicyTooLarge)?;
        Ok(Self { inner: Arc::new(CompiledRules { revision, implications, methods, fingerprint }), operations: None })
    }

    /// Adapts exact operation rules for every existing request-scope consumer,
    /// including secured HTTP pre-dispatch admission and SSE revalidation.
    /// Install this with HttpSecurityPolicy::with_scope_authorization for native
    /// HTTP 401/403 handling, or Server::with_scope_authorization for ordinary
    /// transport dispatch. The complete semantic fingerprint is retained.
    /// Nested operation bases are rejected to keep evaluation depth bounded;
    /// separately installed server gates still intersect their restrictions.
    pub fn for_operations(policy: operation::OperationScopePolicy) -> Result<Self, ScopeRequestPolicyError> {
        let methods = policy.method_policy();
        if methods.operations.is_some() { return Err(ScopeRequestPolicyError::NestedOperationPolicy); }
        Ok(Self { inner: Arc::clone(&methods.inner), operations: Some(policy) })
    }

    pub fn revision(&self) -> u64 {
        self.operations.as_ref().map_or(self.inner.revision, operation::OperationScopePolicy::revision)
    }
    pub fn fingerprint(&self) -> Sha256Digest {
        self.operations.as_ref().map_or(self.inner.fingerprint, operation::OperationScopePolicy::fingerprint)
    }
    pub(crate) fn has_operation_rules(&self) -> bool { self.operations.is_some() }

    /// Trusted-host inspection of a method-wide requirement only. Named rules
    /// may impose additional requirements; this is never visibility authority.
    pub fn required_scopes(&self, method: &str) -> Option<&RequiredScopes> {
        self.inner.methods.binary_search_by(|entry| entry.0.as_str().cmp(method))
            .ok().map(|index| &self.inner.methods[index].1)
    }

    /// Evaluates a method-only policy against already verified provider facts.
    /// An operation-scoped policy cannot be authorized without its request and
    /// therefore fails closed here; use authorize_request_verified instead.
    pub fn authorize_verified(
        &self,
        method: &str,
        facts: Option<&AuthContext>,
    ) -> Result<(), ScopeRequestRejection> {
        if self.operations.is_some() { return Err(ScopeRequestRejection::InsufficientScope); }
        let required = self.required_scopes(method)
            .ok_or(ScopeRequestRejection::UnconfiguredMethod)?;
        let grants = facts.map_or(&[][..], |facts| facts.scopes.as_slice());
        validate_scopes(grants).map_err(|_| ScopeRequestRejection::InvalidFacts)?;
        if let Some(facts) = facts {
            if facts.subject.as_ref().is_some_and(String::is_empty)
                || (!grants.is_empty() && facts.subject.is_none() && facts.session_owner().is_none())
            { return Err(ScopeRequestRejection::InvalidFacts); }
        }
        if self.inner.implications.permits(grants, required)
            .map_err(|_| ScopeRequestRejection::InvalidFacts)?
        {
            Ok(())
        } else {
            Err(ScopeRequestRejection::InsufficientScope)
        }
    }

    /// Evaluates the complete pinned request policy without modifying facts or
    /// trusting request-provided grants. Missing, malformed or unconfigured named
    /// operations share the insufficient-permission outcome; no target discovery
    /// or required-scope disclosure is authorized by this result.
    pub fn authorize_request_verified(
        &self,
        request: &JsonRpcRequest,
        facts: Option<&AuthContext>,
    ) -> Result<(), ScopeRequestRejection> {
        match &self.operations {
            None => self.authorize_verified(&request.method, facts),
            Some(policy) => policy.authorize_verified(request, facts).map_err(|rejection| match rejection {
                operation::OperationScopeRejection::Method(error) => error,
                operation::OperationScopeRejection::UnconfiguredOperation
                    | operation::OperationScopeRejection::InsufficientScope => ScopeRequestRejection::InsufficientScope,
            }),
        }
    }
}

// Private so the supported installation cannot accidentally place this gate
// behind an application cache or an early Respond middleware.
struct ScopeAdmissionMiddleware(ScopeRequestPolicy);

impl Middleware for ScopeAdmissionMiddleware {
    fn on_request(&self, ctx: &McpContext, request: &JsonRpcRequest) -> McpResult<MiddlewareDecision> {
        Server::enforce_request_context(ctx)?;
        let facts = ctx.auth();
        let decision = self.0.authorize_request_verified(request, facts.as_ref());
        Server::enforce_request_context(ctx)?;
        match decision {
            Ok(()) => Ok(MiddlewareDecision::Continue),
            Err(ScopeRequestRejection::InvalidFacts) => {
                Err(McpError::internal_error("request scope admission rejected provider facts"))
            }
            Err(_) => Err(McpError::new(McpErrorCode::ResourceForbidden, REFUSAL)),
        }
    }
}

impl Server {
    /// Installs an immutable, default-deny request gate at the front of the
    /// server's existing middleware chain, before caches and early Respond hooks.
    /// Build first, call this method, then open an endpoint/listener. Installation
    /// fails if middleware ownership has already been shared; no shared instance
    /// or live subscription is mutated. Calling it twice intersects the policies
    /// rather than silently replacing or widening the earlier policy.
    ///
    /// Authentication remains on the existing transport boundary. This gate uses
    /// only the verified request context, checks every required permission, and
    /// prevents denied requests from entering application middleware or handlers.
    /// It evaluates the implication graph directly, so ScopePolicyAuthProvider
    /// is optional and raw provider scope claims need not be rewritten.
    ///
    /// Rules cover methods that enter ordinary request dispatch. Transport
    /// preflight, public resource metadata and protocol control handled before
    /// that dispatch retain their own admission. Policies created by
    /// ScopeRequestPolicy::for_operations also enforce their exact named rules.
    /// Neither form filters catalogs. A subscription grant is checked on opening
    /// only; its native lease and revocation contract is unchanged. JSON-RPC
    /// policy failures retain the native error mapping, not a fabricated OAuth
    /// HTTP status/challenge inferred from application error data.
    pub fn with_scope_authorization(mut self, policy: ScopeRequestPolicy) -> McpResult<Self> {
        let middleware = Arc::get_mut(&mut self.middleware)
            .ok_or_else(|| McpError::invalid_request("scope authorization must be installed before sharing the server"))?;
        middleware.try_reserve(1)
            .map_err(|_| McpError::internal_error("scope authorization installation exceeds capacity"))?;
        middleware.insert(0, Box::new(ScopeAdmissionMiddleware(policy)));
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required(scopes: &[&str]) -> RequiredScopes {
        RequiredScopes::new(scopes.iter().map(|scope| (*scope).to_owned()).collect()).unwrap()
    }
    fn facts(scopes: &[&str]) -> AuthContext {
        let mut facts = AuthContext::with_subject("verified-owner");
        facts.scopes = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        facts
    }
    fn graph(edges: &[(&str, &str)]) -> ScopeImplicationPolicy {
        ScopeImplicationPolicy::new(3, edges.iter().map(|(a,b)| ((*a).to_owned(),(*b).to_owned())).collect()).unwrap()
    }
    fn rules(edges: &[(&str, &str)]) -> ScopeRequestPolicy {
        ScopeRequestPolicy::new(4, graph(edges), vec![
            ("tools/list".to_owned(), required(&[])),
            ("tools/call".to_owned(), required(&["read", "write"])),
        ]).unwrap()
    }

    #[test]
    fn all_of_scope_admission_is_enforced_without_mutating_verified_facts() {
        let policy = rules(&[("admin", "write"), ("write", "read")]);
        let mut facts = facts(&["admin"]).with_session_owner(Sha256Digest::from_bytes([7;32]));
        facts.claims = Some(serde_json::json!({"scope":"admin"}));
        let before = serde_json::to_vec(&facts).unwrap();
        let owner = facts.session_owner();
        assert_eq!(policy.authorize_verified("tools/call", Some(&facts)), Ok(()));
        assert_eq!(serde_json::to_vec(&facts).unwrap(), before);
        assert_eq!(facts.session_owner(), owner);
        assert_eq!(facts.scopes, vec!["admin"]);
        assert_eq!(policy.required_scopes("tools/call").unwrap().challenge_scope(), "read write");
    }

    #[test]
    fn removing_one_edge_denies_the_same_call_and_preserves_the_baseline() {
        let complete = rules(&[("admin", "write"), ("write", "read")]);
        let missing = rules(&[("admin", "write")]);
        let facts = facts(&["admin"]);
        let before = serde_json::to_vec(&facts).unwrap();
        assert!(complete.authorize_verified("tools/call", Some(&facts)).is_ok());
        assert_eq!(missing.authorize_verified("tools/call", Some(&facts)), Err(ScopeRequestRejection::InsufficientScope));
        assert_eq!(serde_json::to_vec(&facts).unwrap(), before);
        assert!(complete.authorize_verified("tools/call", Some(&facts)).is_ok());
    }

    #[test]
    fn public_is_explicit_and_unknown_methods_are_never_implicitly_public() {
        let policy = rules(&[]);
        assert_eq!(policy.authorize_verified("tools/list", None), Ok(()));
        assert_eq!(policy.authorize_verified("tools/call", None), Err(ScopeRequestRejection::InsufficientScope));
        for method in ["tools", "tools/list/", "TOOLS/LIST", "tools/list ", "resources/list"] {
            assert_eq!(policy.authorize_verified(method, None), Err(ScopeRequestRejection::UnconfiguredMethod));
        }
        let none = ScopeRequestPolicy::new(1, graph(&[]), vec![]).unwrap();
        assert_eq!(none.authorize_verified("tools/list", None), Err(ScopeRequestRejection::UnconfiguredMethod));
    }

    #[test]
    fn every_required_permission_is_needed_and_scope_prefixes_have_no_authority() {
        let policy = rules(&[]);
        for grants in [&["read"][..], &["write"], &["*"], &["read", "WRITE"], &["read", "write:all"]] {
            assert_eq!(policy.authorize_verified("tools/call", Some(&facts(grants))), Err(ScopeRequestRejection::InsufficientScope));
        }
        assert!(policy.authorize_verified("tools/call", Some(&facts(&["write", "read"]))).is_ok());
    }

    #[test]
    fn empty_public_rules_still_reject_malformed_or_ownerless_grants() {
        let policy = rules(&[]);
        for mut facts in [facts(&["read", "bad scope"]), facts(&["read"]), AuthContext::anonymous()] {
            if facts.scopes == ["read"] { facts.subject = None; }
            if facts.scopes.is_empty() { facts.subject = Some(String::new()); }
            assert_eq!(policy.authorize_verified("tools/list", Some(&facts)), Err(ScopeRequestRejection::InvalidFacts));
        }
        assert!(policy.authorize_verified("tools/list", Some(&AuthContext::anonymous())).is_ok());
        let owned = AuthContext::anonymous().with_session_owner(Sha256Digest::from_bytes([9;32]));
        assert!(policy.authorize_verified("tools/list", Some(&owned)).is_ok());
    }

    #[test]
    fn configuration_rejects_ambiguous_methods_and_observes_exact_bounds() {
        for invalid in ["", "tools call", "tools/call\n", "m\u{e9}thod"] {
            assert!(matches!(ScopeRequestPolicy::new(1, graph(&[]), vec![(invalid.to_owned(),required(&[]))]), Err(ScopeRequestPolicyError::InvalidMethod)));
        }
        assert!(ScopeRequestPolicy::new(1, graph(&[]), vec![("m".repeat(128),required(&[]))]).is_ok());
        assert!(matches!(ScopeRequestPolicy::new(1, graph(&[]), vec![("m".repeat(129),required(&[]))]), Err(ScopeRequestPolicyError::InvalidMethod)));
        assert!(matches!(ScopeRequestPolicy::new(0, graph(&[]), vec![]), Err(ScopeRequestPolicyError::ZeroRevision)));
        assert!(matches!(ScopeRequestPolicy::new(1, graph(&[]), vec![("tools/call".to_owned(),required(&[])),("tools/call".to_owned(),required(&["write"]))]), Err(ScopeRequestPolicyError::DuplicateMethod)));
        let entries = |count| (0..count).map(|i| (format!("method/{i}"),required(&[]))).collect();
        assert!(ScopeRequestPolicy::new(1, graph(&[]), entries(64)).is_ok());
        assert!(matches!(ScopeRequestPolicy::new(1, graph(&[]), entries(65)), Err(ScopeRequestPolicyError::TooManyMethods)));
    }

    #[test]
    fn aggregate_configuration_bytes_are_bounded_before_identity_allocation() {
        let scopes = (0..30).map(|i| format!("{i:03}{}", "x".repeat(253))).collect();
        let required = RequiredScopes::new(scopes).unwrap();
        let entries = (0..9).map(|i| (format!("method/{i}"),required.clone())).collect();
        assert!(matches!(ScopeRequestPolicy::new(1, graph(&[]), entries), Err(ScopeRequestPolicyError::PolicyTooLarge)));
    }

    #[test]
    fn identity_binds_complete_requirements_revision_and_implication_semantics() {
        let baseline = rules(&[("admin", "write"), ("write", "read")]);
        let reordered = ScopeRequestPolicy::new(4, graph(&[("write", "read"), ("admin", "write")]), vec![
            ("tools/call".to_owned(),required(&["write", "read", "write"])),
            ("tools/list".to_owned(),required(&[])),
        ]).unwrap();
        assert_eq!(baseline.fingerprint(), reordered.fingerprint());
        assert_ne!(baseline.fingerprint(), rules(&[("admin", "write")]).fingerprint());
        let changed = ScopeRequestPolicy::new(4, graph(&[("admin", "write"), ("write", "read")]), vec![
            ("tools/list".to_owned(),required(&[])), ("tools/call".to_owned(),required(&["write"])),
        ]).unwrap();
        assert_ne!(baseline.fingerprint(), changed.fingerprint());
        assert_ne!(ScopeRequestPolicy::new(1, graph(&[]),vec![]).unwrap().fingerprint(),
            ScopeRequestPolicy::new(2, graph(&[]),vec![]).unwrap().fingerprint());
    }

    #[test]
    fn local_diagnostics_do_not_publish_method_scope_or_principal_names() {
        let policy = ScopeRequestPolicy::new(1, graph(&[]), vec![
            ("private-method-canary".to_owned(),required(&["private-scope-canary"])),
        ]).unwrap();
        let error = policy.authorize_verified("private-method-canary", Some(&facts(&["other"]))).unwrap_err();
        let diagnostic = format!("{policy:?} {error:?} {error}");
        assert!(!diagnostic.contains("canary"));
        assert!(!diagnostic.contains("verified-owner"));
    }

    #[test]
    fn operation_adapter_preserves_identity_and_refuses_method_only_authorization() {
        let operations = operation::OperationScopePolicy::new(9, rules(&[]), vec![
            (operation::ScopedOperation::ToolCall("allowed".into()), required(&["specific"])),
        ]).unwrap();
        let expected = operations.fingerprint();
        let policy = ScopeRequestPolicy::for_operations(operations).unwrap();
        assert_eq!(policy.fingerprint(), expected);
        assert_eq!(policy.clone().fingerprint(), expected);
        assert_eq!(policy.revision(), 9);
        let facts = facts(&["read", "write", "specific"]);
        let mut request = JsonRpcRequest::new("tools/call", Some(serde_json::json!({"name":"allowed"})), fastmcp_protocol::RequestId::Number(1));
        assert!(policy.authorize_request_verified(&request, Some(&facts)).is_ok());
        assert_eq!(policy.authorize_verified("tools/call", Some(&facts)), Err(ScopeRequestRejection::InsufficientScope));
        request.params = Some(serde_json::json!({"name":"other"}));
        assert_eq!(policy.authorize_request_verified(&request, Some(&facts)), Err(ScopeRequestRejection::InsufficientScope));
    }

    #[test]
    fn operation_adapter_rejects_recursive_composition_without_changing_existing_policy() {
        let operations = operation::OperationScopePolicy::new(1, rules(&[]), vec![]).unwrap();
        let policy = ScopeRequestPolicy::for_operations(operations).unwrap();
        let before = policy.fingerprint();
        let nested = operation::OperationScopePolicy::new(2, policy.clone(), vec![]).unwrap();
        assert!(matches!(ScopeRequestPolicy::for_operations(nested), Err(ScopeRequestPolicyError::NestedOperationPolicy)));
        assert_eq!(policy.fingerprint(), before);
    }
}
