//! Exact named-operation authorization before application middleware and caches.
//!
//! This policy intersects a mandatory method policy with host-configured rules
//! for tool calls, resource reads, prompt retrieval/completion and resource
//! subscriptions. Modern listens require a ResourceWatch rule for EVERY selected
//! resource. Missing named rules deny; there is no wildcard, URI decoding,
//! prefix matching or request-metadata override. Catalog visibility is separate:
//! refusals never disclose whether a target exists or which scopes it needs.
//! Install on the built server before opening any endpoint or sharing middleware.

use std::fmt;
use std::sync::Arc;

use fastmcp_core::{AuthContext, McpContext, McpError, McpErrorCode, McpResult, Sha256Digest, sha256_bounded};
use fastmcp_protocol::JsonRpcRequest;

use super::{ScopeRequestPolicy, ScopeRequestRejection};
use super::super::RequiredScopes;
use crate::{Middleware, MiddlewareDecision, Server};

const MAX_OPERATIONS: usize = 1024;
const MAX_TARGET_BYTES: usize = 2048;
const MAX_POLICY_BYTES: usize = 64 * 1024;
// Count all requested entries, including duplicates, before evaluating grants.
const MAX_RESOURCE_WATCHES: usize = 1024;
const MAX_RESOURCE_WATCH_BYTES: usize = 64 * 1024;
// v2 binds the additional all-resource authorization of subscriptions/listen.
const DOMAIN: &[u8] = b"fastmcp/named-operation-scopes/v2\0";

/// Host-selected operation identity. Names and URIs are literal, case-sensitive
/// strings; completion permissions are distinct from retrieval permissions.
/// Debug intentionally omits targets. Construction is validated by the policy.
#[derive(Clone, PartialEq, Eq)]
pub enum ScopedOperation {
    ToolCall(String),
    ResourceRead(String),
    PromptGet(String),
    PromptComplete(String),
    ResourceComplete(String),
    LegacyResourceSubscribe(String),
    LegacyResourceUnsubscribe(String),
    /// One exact resource URI in a modern listen's resourceSubscriptions filter.
    /// Reading a resource or subscribing in the legacy era does not grant this.
    ResourceWatch(String),
}

impl ScopedOperation {
    fn key(&self) -> (u8, &str) {
        match self {
            Self::ToolCall(target) => (0, target),
            Self::ResourceRead(target) => (1, target),
            Self::PromptGet(target) => (2, target),
            Self::PromptComplete(target) => (3, target),
            Self::ResourceComplete(target) => (4, target),
            Self::LegacyResourceSubscribe(target) => (5, target),
            Self::LegacyResourceUnsubscribe(target) => (6, target),
            Self::ResourceWatch(target) => (7, target),
        }
    }
}

impl fmt::Debug for ScopedOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::ToolCall(_) => "ToolCall",
            Self::ResourceRead(_) => "ResourceRead",
            Self::PromptGet(_) => "PromptGet",
            Self::PromptComplete(_) => "PromptComplete",
            Self::ResourceComplete(_) => "ResourceComplete",
            Self::LegacyResourceSubscribe(_) => "LegacyResourceSubscribe",
            Self::LegacyResourceUnsubscribe(_) => "LegacyResourceUnsubscribe",
            Self::ResourceWatch(_) => "ResourceWatch",
        };
        f.debug_struct(kind).finish_non_exhaustive()
    }
}

/// Fixed, payload-free configuration failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationScopePolicyError {
    ZeroRevision,
    TooManyOperations,
    InvalidTarget,
    DuplicateOperation,
    PolicyTooLarge,
}

impl fmt::Display for OperationScopePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroRevision => "operation scope policy requires a nonzero revision",
            Self::TooManyOperations => "operation scope policy exceeds its entry bound",
            Self::InvalidTarget => "operation scope policy contains an invalid target",
            Self::DuplicateOperation => "operation scope policy repeats an exact operation",
            Self::PolicyTooLarge => "operation scope policy exceeds its byte bound",
        })
    }
}
impl std::error::Error for OperationScopePolicyError {}

/// Local rejection details, never a catalog-visibility or challenge receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationScopeRejection {
    Method(ScopeRequestRejection),
    UnconfiguredOperation,
    InsufficientScope,
}

impl fmt::Display for OperationScopeRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Operation is not permitted")
    }
}
impl std::error::Error for OperationScopeRejection {}

struct CompiledOperations {
    revision: u64,
    methods: ScopeRequestPolicy,
    entries: Vec<(ScopedOperation, RequiredScopes)>,
    fingerprint: Sha256Digest,
}

/// Immutable default-deny named rules intersected with a method scope policy.
///
/// Every named operation requires an explicit entry, even when its method rule
/// permits anonymous access. A modern listen additionally requires ResourceWatch
/// permission for every URI in resourceSubscriptions. Catalog-only and empty
/// resource selections have no per-resource requirements; their method gate and
/// the native filter/extension admission still apply. No implicit read-to-watch
/// grant is inferred. Other methods use the mandatory default-deny method policy.
/// Empty required scopes never bypass authentication or downstream authorization.
/// Up to 1024 entries and 64 KiB of framed named configuration are retained, in
/// addition to the separately bounded method policy. No per-request table or
/// expanded grant set is allocated.
#[derive(Clone)]
pub struct OperationScopePolicy {
    inner: Arc<CompiledOperations>,
}

impl fmt::Debug for OperationScopePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationScopePolicy")
            .field("revision", &self.inner.revision)
            .field("operation_count", &self.inner.entries.len())
            .finish_non_exhaustive()
    }
}

impl OperationScopePolicy {
    pub fn new(
        revision: u64,
        methods: ScopeRequestPolicy,
        mut entries: Vec<(ScopedOperation, RequiredScopes)>,
    ) -> Result<Self, OperationScopePolicyError> {
        if revision == 0 { return Err(OperationScopePolicyError::ZeroRevision); }
        if entries.len() > MAX_OPERATIONS { return Err(OperationScopePolicyError::TooManyOperations); }
        let mut bytes = DOMAIN.len() + 8 + 32 + 8;
        for (operation, scopes) in &entries {
            let (_, target) = operation.key();
            if !valid_target(target) { return Err(OperationScopePolicyError::InvalidTarget); }
            bytes = bytes.saturating_add(1 + 8 + 8).saturating_add(target.len());
            for scope in scopes.as_slice() {
                bytes = bytes.saturating_add(8).saturating_add(scope.len());
            }
            if bytes > MAX_POLICY_BYTES { return Err(OperationScopePolicyError::PolicyTooLarge); }
        }
        entries.sort_unstable_by(|left, right| left.0.key().cmp(&right.0.key()));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(OperationScopePolicyError::DuplicateOperation);
        }
        let mut identity = Vec::with_capacity(bytes);
        identity.extend_from_slice(DOMAIN);
        identity.extend_from_slice(&revision.to_be_bytes());
        identity.extend_from_slice(methods.fingerprint().as_bytes());
        identity.extend_from_slice(&(entries.len() as u64).to_be_bytes());
        for (operation, scopes) in &entries {
            let (kind, target) = operation.key();
            identity.push(kind);
            identity.extend_from_slice(&(target.len() as u64).to_be_bytes());
            identity.extend_from_slice(target.as_bytes());
            identity.extend_from_slice(&(scopes.as_slice().len() as u64).to_be_bytes());
            for scope in scopes.as_slice() {
                identity.extend_from_slice(&(scope.len() as u64).to_be_bytes());
                identity.extend_from_slice(scope.as_bytes());
            }
        }
        let fingerprint = sha256_bounded(&identity, MAX_POLICY_BYTES)
            .map_err(|_| OperationScopePolicyError::PolicyTooLarge)?;
        Ok(Self { inner: Arc::new(CompiledOperations { revision, methods, entries, fingerprint }) })
    }

    pub fn revision(&self) -> u64 { self.inner.revision }
    pub fn fingerprint(&self) -> Sha256Digest { self.inner.fingerprint }
    pub fn method_policy(&self) -> &ScopeRequestPolicy { &self.inner.methods }

    /// Requires both the base method grants and all of the request's exact named
    /// operation grants. Only protocol-defined selectors are inspected; arguments,
    /// metadata and nested headers cannot replace them. A modern resource filter
    /// is evaluated as one all-or-nothing selection, without rewriting or dropping
    /// unauthorized entries. Up to 1024 URI entries and 64 KiB of decoded target
    /// bytes are admitted, including duplicates. Native wire bounds may be tighter.
    /// The strict raw protocol boundary still owns duplicate-member admission.
    /// Success is not catalog visibility, a new credential, or a continuing lease.
    pub fn authorize_verified(
        &self,
        request: &JsonRpcRequest,
        facts: Option<&AuthContext>,
    ) -> Result<(), OperationScopeRejection> {
        self.inner.methods.authorize_verified(&request.method, facts)
            .map_err(OperationScopeRejection::Method)?;
        let grants = facts.map_or(&[][..], |facts| facts.scopes.as_slice());
        if request.method == "subscriptions/listen" {
            return self.authorize_resource_watches(request, grants);
        }
        let Some(key) = request_key(request)? else { return Ok(()); };
        self.authorize_key(key, grants)
    }

    fn authorize_key(&self, key: (u8, &str), grants: &[String]) -> Result<(), OperationScopeRejection> {
        let index = self.inner.entries.binary_search_by(|entry| entry.0.key().cmp(&key))
            .map_err(|_| OperationScopeRejection::UnconfiguredOperation)?;
        if self.inner.methods.inner.implications.permits(grants, &self.inner.entries[index].1)
            .map_err(|_| OperationScopeRejection::Method(ScopeRequestRejection::InvalidFacts))?
        {
            Ok(())
        } else {
            Err(OperationScopeRejection::InsufficientScope)
        }
    }

    fn authorize_resource_watches(
        &self,
        request: &JsonRpcRequest,
        grants: &[String],
    ) -> Result<(), OperationScopeRejection> {
        let notifications = request.params.as_ref().and_then(|params| params.get("notifications"))
            .and_then(serde_json::Value::as_object)
            .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
        let Some(resources) = notifications.get("resourceSubscriptions") else { return Ok(()); };
        let resources = resources.as_array().filter(|resources| resources.len() <= MAX_RESOURCE_WATCHES)
            .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
        // Validate the whole selection before scope evaluation. Never create a
        // partially authorized subscription or silently narrow the user's filter.
        let mut bytes = 0_usize;
        for value in resources {
            let target = value.as_str().filter(|target| valid_target(target))
                .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
            bytes = bytes.checked_add(target.len()).filter(|bytes| *bytes <= MAX_RESOURCE_WATCH_BYTES)
                .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
        }
        for value in resources {
            let target = value.as_str().ok_or(OperationScopeRejection::UnconfiguredOperation)?;
            self.authorize_key((7, target), grants)?;
        }
        Ok(())
    }
}

fn valid_target(target: &str) -> bool {
    !target.is_empty() && target.len() <= MAX_TARGET_BYTES && !target.chars().any(char::is_control)
}

fn request_key(request: &JsonRpcRequest) -> Result<Option<(u8, &str)>, OperationScopeRejection> {
    let (kind, field) = match request.method.as_str() {
        "tools/call" => (0, "name"),
        "resources/read" => (1, "uri"),
        "prompts/get" => (2, "name"),
        "resources/subscribe" => (5, "uri"),
        "resources/unsubscribe" => (6, "uri"),
        "completion/complete" => {
            let reference = request.params.as_ref().and_then(|params| params.get("ref"))
                .and_then(serde_json::Value::as_object)
                .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
            let (kind, field) = match reference.get("type").and_then(serde_json::Value::as_str) {
                Some("ref/prompt") => (3, "name"),
                Some("ref/resource") => (4, "uri"),
                _ => return Err(OperationScopeRejection::UnconfiguredOperation),
            };
            let target = reference.get(field).and_then(serde_json::Value::as_str)
                .filter(|target| valid_target(target))
                .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
            return Ok(Some((kind, target)));
        }
        _ => return Ok(None),
    };
    let target = request.params.as_ref().and_then(|params| params.get(field))
        .and_then(serde_json::Value::as_str).filter(|target| valid_target(target))
        .ok_or(OperationScopeRejection::UnconfiguredOperation)?;
    Ok(Some((kind, target)))
}

struct OperationScopeMiddleware(OperationScopePolicy);

impl Middleware for OperationScopeMiddleware {
    fn on_request(&self, ctx: &McpContext, request: &JsonRpcRequest) -> McpResult<MiddlewareDecision> {
        Server::enforce_request_context(ctx)?;
        let facts = ctx.auth();
        let decision = self.0.authorize_verified(request, facts.as_ref());
        Server::enforce_request_context(ctx)?;
        match decision {
            Ok(()) => Ok(MiddlewareDecision::Continue),
            Err(OperationScopeRejection::Method(ScopeRequestRejection::InvalidFacts)) =>
                Err(McpError::internal_error("operation scope admission rejected provider facts")),
            Err(_) => Err(McpError::new(McpErrorCode::ResourceForbidden, "Operation is not permitted")),
        }
    }
}

impl Server {
    /// Installs named-operation and method authorization ahead of application
    /// caches, short-circuit middleware and handlers on ordinary dispatch paths.
    /// Authentication remains first; this gate trusts only its committed facts.
    /// Calling twice intersects policies. Installation after middleware sharing
    /// is refused, rather than mutating live authorization or subscription state.
    ///
    /// This does not filter catalogs, authorize a detached Task's lifetime, add
    /// stdio credentials, or turn application JSON-RPC failures into native HTTP
    /// OAuth challenges. Existing visibility and long-lived authorization remain
    /// required. Unknown targets and insufficient grants have the same wire error.
    pub fn with_operation_scope_authorization(mut self, policy: OperationScopePolicy) -> McpResult<Self> {
        let middleware = Arc::get_mut(&mut self.middleware)
            .ok_or_else(|| McpError::invalid_request("operation authorization must be installed before sharing the server"))?;
        middleware.try_reserve(1)
            .map_err(|_| McpError::internal_error("operation authorization installation exceeds capacity"))?;
        middleware.insert(0, Box::new(OperationScopeMiddleware(policy)));
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::ScopeImplicationPolicy;
    use fastmcp_protocol::RequestId;
    use serde_json::{Value, json};

    fn required(scopes: &[&str]) -> RequiredScopes {
        RequiredScopes::new(scopes.iter().map(|scope| (*scope).to_owned()).collect()).unwrap()
    }
    fn facts(scopes: &[&str]) -> AuthContext {
        let mut facts = AuthContext::with_subject("operation-owner");
        facts.scopes = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        facts
    }
    fn methods() -> ScopeRequestPolicy {
        ScopeRequestPolicy::new(2, ScopeImplicationPolicy::new(1, vec![
            ("admin".to_owned(), "invoke".to_owned()),
            ("admin".to_owned(), "read".to_owned()),
        ]).unwrap(), ["tools/call", "resources/read", "prompts/get", "completion/complete",
            "resources/subscribe", "resources/unsubscribe", "tools/list"].into_iter()
            .map(|method| (method.to_owned(), required(&["invoke"]))).collect()).unwrap()
    }
    fn policy(entries: Vec<(ScopedOperation, RequiredScopes)>) -> OperationScopePolicy {
        OperationScopePolicy::new(3, methods(), entries).unwrap()
    }
    fn request(method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest::new(method, Some(params), RequestId::Number(1))
    }

    #[test]
    fn each_named_operation_kind_has_an_exact_independent_rule() {
        let cases = [
            (ScopedOperation::ToolCall("item".into()), "tools/call", json!({"name":"item"})),
            (ScopedOperation::ResourceRead("item".into()), "resources/read", json!({"uri":"item"})),
            (ScopedOperation::PromptGet("item".into()), "prompts/get", json!({"name":"item"})),
            (ScopedOperation::PromptComplete("item".into()), "completion/complete", json!({"ref":{"type":"ref/prompt","name":"item"}})),
            (ScopedOperation::ResourceComplete("item".into()), "completion/complete", json!({"ref":{"type":"ref/resource","uri":"item"}})),
            (ScopedOperation::LegacyResourceSubscribe("item".into()), "resources/subscribe", json!({"uri":"item"})),
            (ScopedOperation::LegacyResourceUnsubscribe("item".into()), "resources/unsubscribe", json!({"uri":"item"})),
        ];
        let facts = facts(&["admin"]);
        for (operation, _, _) in &cases {
            let policy = policy(vec![(operation.clone(), required(&["read"]))]);
            for (candidate, method, params) in &cases {
                let admitted = policy.authorize_verified(&request(method, params.clone()), Some(&facts));
                assert_eq!(admitted.is_ok(), candidate == operation);
            }
        }
    }

    #[test]
    fn method_and_named_permissions_intersect_and_preserve_facts() {
        let policy = policy(vec![(ScopedOperation::ToolCall("read".into()), required(&["read"]))]);
        let request = request("tools/call", json!({"name":"read"}));
        for (grants, permitted) in [(&["admin"][..], true), (&["invoke", "read"][..], true),
            (&["read"][..], false), (&["invoke"][..], false)]
        {
            let facts = facts(grants).with_session_owner(Sha256Digest::from_bytes([7; 32]));
            let before = serde_json::to_vec(&facts).unwrap();
            let owner = facts.session_owner();
            assert_eq!(policy.authorize_verified(&request, Some(&facts)).is_ok(), permitted);
            assert_eq!(serde_json::to_vec(&facts).unwrap(), before);
            assert_eq!(facts.session_owner(), owner);
        }
    }

    #[test]
    fn missing_targets_and_metadata_cannot_borrow_an_allowed_tool_rule() {
        let policy = policy(vec![(ScopedOperation::ToolCall("read".into()), required(&[]))]);
        let facts = facts(&["admin"]);
        for params in [json!({}), json!({"name":null}), json!({"name":7}),
            json!({"name":"delete","_meta":{"name":"read"}}),
            json!({"arguments":{"name":"read"}}), json!({"_meta":{"name":"read"}})]
        {
            let request = request("tools/call", params);
            let before = serde_json::to_vec(&request).unwrap();
            assert!(policy.authorize_verified(&request, Some(&facts)).is_err());
            assert_eq!(serde_json::to_vec(&request).unwrap(), before);
        }
        assert!(policy.authorize_verified(&request("tools/call", json!({"name":"read","arguments":{"name":"delete"}})), Some(&facts)).is_ok());
    }

    #[test]
    fn resource_rules_never_decode_or_expand_uri_aliases() {
        let policy = policy(vec![(ScopedOperation::ResourceRead("file:///private/report".into()), required(&[]))]);
        let facts = facts(&["invoke"]);
        assert!(policy.authorize_verified(&request("resources/read", json!({"uri":"file:///private/report"})), Some(&facts)).is_ok());
        for target in ["file:///private/REPORT", "file:///private/%72eport", "file:///private/report?x=1",
            "file:///private/report/child", "file:///private/*", "file:///private/../private/report"]
        {
            assert!(policy.authorize_verified(&request("resources/read", json!({"uri":target})), Some(&facts)).is_err());
        }
    }

    #[test]
    fn malformed_completion_reference_is_not_a_method_only_bypass() {
        let policy = policy(vec![(ScopedOperation::PromptComplete("p".into()), required(&[]))]);
        let facts = facts(&["invoke"]);
        for params in [json!({"name":"p"}), json!({"ref":null}),
            json!({"ref":{"type":"ref/other","name":"p"}}),
            json!({"ref":{"type":"ref/prompt","uri":"p"}}),
            json!({"ref":{"type":"ref/prompt","name":[]}})]
        {
            assert!(policy.authorize_verified(&request("completion/complete", params), Some(&facts)).is_err());
        }
    }

    #[test]
    fn deny_all_named_rules_leave_only_explicit_non_named_methods_usable() {
        let policy = policy(vec![]);
        let facts = facts(&["admin"]);
        assert!(policy.authorize_verified(&request("tools/list", json!({})), Some(&facts)).is_ok());
        assert!(policy.authorize_verified(&request("tools/call", json!({"name":"anything"})), Some(&facts)).is_err());
        assert!(policy.authorize_verified(&request("unknown", json!({})), Some(&facts)).is_err());
        assert!(policy.authorize_verified(&request("tools/list", json!({})), None).is_err());
    }

    #[test]
    fn anonymous_operation_requires_explicit_empty_method_and_named_rules() {
        let base = ScopeRequestPolicy::new(1, ScopeImplicationPolicy::exact(1).unwrap(), vec![
            ("tools/call".into(), required(&[])),
        ]).unwrap();
        let policy = OperationScopePolicy::new(1, base, vec![(ScopedOperation::ToolCall("public".into()), required(&[]))]).unwrap();
        let request = request("tools/call", json!({"name":"public"}));
        assert!(policy.authorize_verified(&request, None).is_ok());
        let mut invalid = facts(&["read"]);
        invalid.subject = None;
        assert_eq!(policy.authorize_verified(&request, Some(&invalid)), Err(OperationScopeRejection::Method(ScopeRequestRejection::InvalidFacts)));
    }

    #[test]
    fn operation_configuration_rejects_duplicates_controls_and_exact_bound_overflow() {
        assert!(matches!(OperationScopePolicy::new(0, methods(), vec![]), Err(OperationScopePolicyError::ZeroRevision)));
        let entry = (ScopedOperation::ToolCall("x".into()), required(&[]));
        assert!(matches!(OperationScopePolicy::new(1, methods(), vec![entry.clone(), entry]), Err(OperationScopePolicyError::DuplicateOperation)));
        for target in [String::new(), "bad\nname".to_owned(), "x".repeat(2049)] {
            assert!(matches!(OperationScopePolicy::new(1, methods(), vec![(ScopedOperation::ToolCall(target), required(&[]))]), Err(OperationScopePolicyError::InvalidTarget)));
        }
        assert!(OperationScopePolicy::new(1, methods(), vec![(ScopedOperation::ToolCall("x".repeat(2048)), required(&[]))]).is_ok());
        let entries = |count| (0..count).map(|i| (ScopedOperation::ToolCall(format!("tool{i}")), required(&[]))).collect();
        assert!(OperationScopePolicy::new(1, methods(), entries(1024)).is_ok());
        assert!(matches!(OperationScopePolicy::new(1, methods(), entries(1025)), Err(OperationScopePolicyError::TooManyOperations)));
        let entries = (0..33).map(|i| (ScopedOperation::ToolCall(format!("{i:03}{}", "x".repeat(2045))), required(&[]))).collect();
        assert!(matches!(OperationScopePolicy::new(1, methods(), entries), Err(OperationScopePolicyError::PolicyTooLarge)));
    }

    #[test]
    fn fingerprint_binds_target_kind_grants_and_base_policy_independent_of_order() {
        let tool = (ScopedOperation::ToolCall("same".into()), required(&["read"]));
        let resource = (ScopedOperation::ResourceRead("same".into()), required(&[]));
        let baseline = policy(vec![tool.clone(), resource.clone()]);
        assert_eq!(baseline.fingerprint(), policy(vec![resource.clone(), tool.clone()]).fingerprint());
        assert_ne!(policy(vec![tool.clone()]).fingerprint(), policy(vec![resource]).fingerprint());
        assert_ne!(policy(vec![tool.clone()]).fingerprint(), policy(vec![(ScopedOperation::ToolCall("same".into()), required(&[]))]).fingerprint());
        assert_ne!(policy(vec![tool.clone()]).fingerprint(), policy(vec![(ScopedOperation::ToolCall("other".into()), required(&["read"]))]).fingerprint());
        assert_ne!(policy(vec![tool.clone()]).fingerprint(), OperationScopePolicy::new(4, methods(), vec![tool.clone()]).unwrap().fingerprint());
        let different_base = ScopeRequestPolicy::new(1, super::super::super::ScopeImplicationPolicy::exact(1).unwrap(), vec![]).unwrap();
        assert_ne!(policy(vec![tool.clone()]).fingerprint(), OperationScopePolicy::new(3, different_base, vec![tool]).unwrap().fingerprint());
    }

    #[test]
    fn diagnostics_redact_targets_scopes_and_principals() {
        let operation = ScopedOperation::ToolCall("private-target-canary".into());
        let policy = policy(vec![(operation.clone(), required(&["private-scope-canary"]))]);
        let rejection = policy.authorize_verified(&request("tools/call", json!({"name":"private-target-canary"})), Some(&facts(&["invoke"]))).unwrap_err();
        let text = format!("{operation:?} {policy:?} {rejection:?} {rejection}");
        assert!(!text.contains("canary"));
        assert!(!text.contains("operation-owner"));
    }

    fn watch_policy(entries: Vec<(ScopedOperation, RequiredScopes)>) -> OperationScopePolicy {
        let methods = ScopeRequestPolicy::new(1, ScopeImplicationPolicy::new(1, vec![
            ("admin".into(), "listen".into()), ("admin".into(), "watch-a".into()),
            ("admin".into(), "watch-b".into()),
        ]).unwrap(), vec![("subscriptions/listen".into(), required(&["listen"]))]).unwrap();
        OperationScopePolicy::new(1, methods, entries).unwrap()
    }
    fn watch_request(resources: Value) -> JsonRpcRequest {
        request("subscriptions/listen", json!({"notifications":{"resourceSubscriptions":resources,"toolsListChanged":true}}))
    }

    #[test]
    fn resource_watches_require_every_grant_and_preserve_the_complete_selection() {
        let policy = watch_policy(vec![
            (ScopedOperation::ResourceWatch("file:///a".into()), required(&["watch-a"])),
            (ScopedOperation::ResourceWatch("file:///b".into()), required(&["watch-b"])),
        ]);
        for resources in [json!(["file:///a", "file:///b"]), json!(["file:///b", "file:///a", "file:///a"])] {
            let request = watch_request(resources);
            let before = serde_json::to_vec(&request).unwrap();
            for (grants, allowed) in [(&["admin"][..], true), (&["listen", "watch-a", "watch-b"][..], true),
                (&["listen", "watch-a"][..], false), (&["watch-a", "watch-b"][..], false)]
            {
                let facts = facts(grants);
                let original = serde_json::to_vec(&facts).unwrap();
                assert_eq!(policy.authorize_verified(&request, Some(&facts)).is_ok(), allowed);
                assert_eq!(serde_json::to_vec(&facts).unwrap(), original);
                assert_eq!(serde_json::to_vec(&request).unwrap(), before);
            }
        }
    }

    #[test]
    fn adding_one_unpermitted_resource_refuses_without_narrowing_or_poisoning_reuse() {
        let policy = watch_policy(vec![(ScopedOperation::ResourceWatch("file:///a".into()), required(&["watch-a"]))]);
        let facts = facts(&["admin"]);
        let allowed = watch_request(json!(["file:///a"]));
        assert!(policy.authorize_verified(&allowed, Some(&facts)).is_ok());
        for target in ["file:///b", "file:///A", "file:///%61", "file:///a/child", "file:///*"] {
            let denied = watch_request(json!(["file:///a", target]));
            let before = serde_json::to_vec(&denied).unwrap();
            assert_eq!(policy.authorize_verified(&denied, Some(&facts)), Err(OperationScopeRejection::UnconfiguredOperation));
            assert_eq!(serde_json::to_vec(&denied).unwrap(), before);
        }
        assert!(policy.authorize_verified(&allowed, Some(&facts)).is_ok());
    }

    #[test]
    fn modern_watch_authority_is_not_inferred_from_read_or_legacy_subscribe_rules() {
        let request = watch_request(json!(["file:///a"]));
        let facts = facts(&["admin"]);
        let rules = vec![
            (ScopedOperation::ResourceRead("file:///a".into()), required(&[])),
            (ScopedOperation::LegacyResourceSubscribe("file:///a".into()), required(&[])),
        ];
        let no_watch = watch_policy(rules.clone());
        assert_eq!(no_watch.authorize_verified(&request, Some(&facts)), Err(OperationScopeRejection::UnconfiguredOperation));
        let mut rules = rules;
        rules.push((ScopedOperation::ResourceWatch("file:///a".into()), required(&["watch-a"])));
        let explicit = watch_policy(rules);
        assert!(explicit.authorize_verified(&request, Some(&facts)).is_ok());
        assert_ne!(explicit.fingerprint(), no_watch.fingerprint());
        assert!(!format!("{:?}", ScopedOperation::ResourceWatch("private-canary".into())).contains("canary"));
    }

    #[test]
    fn empty_and_catalog_only_listens_keep_their_method_gate() {
        let policy = watch_policy(vec![]);
        for notifications in [json!({}), json!({"resourceSubscriptions":[]}), json!({"toolsListChanged":true})] {
            let request = request("subscriptions/listen", json!({"notifications":notifications}));
            assert!(policy.authorize_verified(&request, Some(&facts(&["listen"]))).is_ok());
            assert!(policy.authorize_verified(&request, Some(&facts(&["watch-a"]))).is_err());
        }
    }

    #[test]
    fn malformed_or_spoofed_watch_filters_do_not_acquire_resource_authority() {
        let policy = watch_policy(vec![(ScopedOperation::ResourceWatch("file:///a".into()), required(&[]))]);
        let facts = facts(&["listen"]);
        for resources in [json!(null), json!("file:///a"), json!({"uri":"file:///a"}),
            json!(["file:///a", null]), json!(["file:///a", 7]), json!([""]), json!(["bad\nuri"])]
        {
            assert!(policy.authorize_verified(&watch_request(resources), Some(&facts)).is_err());
        }
        for params in [json!({}), json!({"notifications":null}), json!({"notifications":[]}),
            json!({"_meta":{"notifications":{"resourceSubscriptions":["file:///a"]}}}),
            json!({"notifications":{"resourceSubscriptions":["file:///b"]},"arguments":{"resourceSubscriptions":["file:///a"]}})]
        {
            assert!(policy.authorize_verified(&request("subscriptions/listen", params), Some(&facts)).is_err());
        }
        assert!(policy.authorize_verified(&watch_request(json!(["file:///a"])), Some(&facts)).is_ok());
    }

    #[test]
    fn requested_watch_count_and_bytes_are_bounded_even_for_repeated_uris() {
        let facts = facts(&["listen"]);
        let policy = watch_policy(vec![(ScopedOperation::ResourceWatch("x".into()), required(&[]))]);
        assert!(policy.authorize_verified(&watch_request(json!(vec!["x"; MAX_RESOURCE_WATCHES])), Some(&facts)).is_ok());
        assert!(policy.authorize_verified(&watch_request(json!(vec!["x"; MAX_RESOURCE_WATCHES + 1])), Some(&facts)).is_err());
        let target = "x".repeat(MAX_TARGET_BYTES);
        let policy = watch_policy(vec![(ScopedOperation::ResourceWatch(target.clone()), required(&[]))]);
        let count = MAX_RESOURCE_WATCH_BYTES / MAX_TARGET_BYTES;
        assert!(policy.authorize_verified(&watch_request(json!(vec![target.clone(); count])), Some(&facts)).is_ok());
        assert!(policy.authorize_verified(&watch_request(json!(vec![target; count + 1])), Some(&facts)).is_err());
    }
}
