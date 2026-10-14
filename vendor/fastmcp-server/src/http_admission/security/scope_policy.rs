//! Explicit, revisioned scope implication for verified provider facts.
//!
//! Scope names are opaque, case-sensitive tokens. Neither separators nor `*`
//! imply any hierarchy. Only administrator-configured directed edges grant an
//! implication, and their transitive closure is compiled once before use.
//! Cycles, duplicate edges and unbounded configurations are rejected.
//!
//! Install [`ScopePolicyAuthProvider`] around the exact authentication provider
//! whose grants this policy interprets. It runs that provider first, then
//! projects effective authorization scopes without changing the principal,
//! session owner, claims or credentials. This does not verify an issuer or
//! audience itself and does not create an authorization lease. Existing catalog
//! visibility and operation authorization must still run after authentication.

/// Default-deny method scope enforcement before application middleware.
pub mod request;

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use fastmcp_core::{AuthContext, McpContext, McpError, McpResult, Sha256Digest, sha256_bounded};

use crate::{AuthProvider, AuthRequest};

const MAX_SCOPES: usize = 64;
const MAX_SCOPE_BYTES: usize = 256;
const MAX_IMPLICATIONS: usize = 256;
const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_REQUIRED_BYTES: usize = 8 * 1024;
const POLICY_DOMAIN: &[u8] = b"fastmcp/scope-implication/v1\0";

/// Fixed diagnostics contain no scope names, claims, credentials or subjects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopePolicyError {
    ZeroRevision,
    InvalidScope,
    OfflineAccess,
    TooManyScopes,
    TooManyImplications,
    PolicyTooLarge,
    RepeatedImplication,
    CyclicImplication,
    InvalidPrincipal,
    EffectiveScopeLimit,
}

impl fmt::Display for ScopePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroRevision => "scope policy requires a nonzero configuration revision",
            Self::InvalidScope => "invalid bounded OAuth scope token",
            Self::OfflineAccess => "offline_access is not a resource-server permission",
            Self::TooManyScopes => "scope set exceeds its count bound",
            Self::TooManyImplications => "scope implication count exceeds its bound",
            Self::PolicyTooLarge => "scope policy exceeds its encoded-byte bound",
            Self::RepeatedImplication => "scope policy repeats an implication",
            Self::CyclicImplication => "scope policy contains an implication cycle",
            Self::InvalidPrincipal => "scope projection requires a verified principal",
            Self::EffectiveScopeLimit => "effective scopes exceed authentication admission bounds",
        })
    }
}
impl std::error::Error for ScopePolicyError {}

/// The complete scope set of an already-resolved, visible operation.
///
/// Construction deduplicates and sorts exact tokens, but never removes a scope
/// because another one implies it. The same complete set can therefore be used
/// for all-of authorization and a deterministic insufficient-scope challenge.
/// Construct this from trusted operation policy, not from a peer's request.
/// Do not disclose it until catalog visibility has admitted the operation.
#[derive(Clone)]
pub struct RequiredScopes {
    scopes: Vec<String>,
}

impl fmt::Debug for RequiredScopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequiredScopes").field("count", &self.scopes.len()).finish()
    }
}

impl RequiredScopes {
    pub fn new(mut scopes: Vec<String>) -> Result<Self, ScopePolicyError> {
        validate_scopes(&scopes)?;
        let mut bytes = 0_usize;
        for scope in &scopes {
            if scope == "offline_access" { return Err(ScopePolicyError::OfflineAccess); }
            bytes = bytes.saturating_add(scope.len()).saturating_add(1);
            if bytes > MAX_REQUIRED_BYTES { return Err(ScopePolicyError::PolicyTooLarge); }
        }
        scopes.sort_unstable();
        scopes.dedup();
        Ok(Self { scopes })
    }

    pub fn as_slice(&self) -> &[String] { &self.scopes }

    /// All required scopes, not merely the first missing one or a graph-reduced
    /// subset. Tokens cannot contain quotes, backslashes or HTTP controls.
    pub fn challenge_scope(&self) -> String { self.scopes.join(" ") }
}

struct CompiledPolicy {
    revision: u64,
    scopes: Vec<String>,
    // Exactly one bounded reachability word per node; 64 nodes fit in u64.
    reachable: Vec<u64>,
    fingerprint: Sha256Digest,
}

/// Immutable, cheaply cloned provider-selected implication policy.
///
/// At most 64 names, 256 edges and 64 KiB of edge-name input are accepted.
/// Compiling the graph uses a bounded bitset closure with no recursion or I/O.
/// Request-time projection never traverses or recompiles the original graph.
/// An empty graph is exact-match-only, including for names unknown to the graph.
#[derive(Clone)]
pub struct ScopeImplicationPolicy {
    inner: Arc<CompiledPolicy>,
}

impl fmt::Debug for ScopeImplicationPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeImplicationPolicy")
            .field("revision", &self.inner.revision)
            .field("scope_count", &self.inner.scopes.len())
            .finish_non_exhaustive()
    }
}

impl ScopeImplicationPolicy {
    /// Every pair is `(granted_scope, implied_scope)`. Configuration must be
    /// selected by the host for its provider, never deserialized from claims,
    /// introspection output, discovery extensions or request metadata.
    pub fn new(revision: u64, implications: Vec<(String, String)>) -> Result<Self, ScopePolicyError> {
        if revision == 0 { return Err(ScopePolicyError::ZeroRevision); }
        if implications.len() > MAX_IMPLICATIONS { return Err(ScopePolicyError::TooManyImplications); }
        let mut names = BTreeSet::new();
        let mut edges = BTreeSet::new();
        let mut bytes = 0_usize;
        for (granted, implied) in &implications {
            for scope in [granted, implied] {
                validate_scope(scope)?;
                if scope == "offline_access" { return Err(ScopePolicyError::OfflineAccess); }
                bytes = bytes.saturating_add(scope.len());
                if bytes > MAX_POLICY_BYTES { return Err(ScopePolicyError::PolicyTooLarge); }
                names.insert(scope.clone());
                if names.len() > MAX_SCOPES { return Err(ScopePolicyError::TooManyScopes); }
            }
            if !edges.insert((granted.as_str(), implied.as_str())) {
                return Err(ScopePolicyError::RepeatedImplication);
            }
        }
        let scopes: Vec<String> = names.into_iter().collect();
        let mut reachable = vec![0_u64; scopes.len()];
        for (granted, implied) in edges {
            let from = scopes.binary_search_by(|value| value.as_str().cmp(granted))
                .map_err(|_| ScopePolicyError::InvalidScope)?;
            let to = scopes.binary_search_by(|value| value.as_str().cmp(implied))
                .map_err(|_| ScopePolicyError::InvalidScope)?;
            reachable[from] |= 1_u64 << to;
        }
        for via in 0..scopes.len() {
            let successors = reachable[via];
            for row in &mut reachable {
                if *row & (1_u64 << via) != 0 { *row |= successors; }
            }
        }
        if reachable.iter().enumerate().any(|(index, row)| row & (1_u64 << index) != 0) {
            return Err(ScopePolicyError::CyclicImplication);
        }
        // Bind the revision AND actual semantics. Reusing a numeric revision for
        // a different graph cannot accidentally reproduce the policy identity.
        let mut identity = POLICY_DOMAIN.to_vec();
        identity.extend_from_slice(&revision.to_be_bytes());
        identity.extend_from_slice(&(scopes.len() as u64).to_be_bytes());
        for (scope, row) in scopes.iter().zip(&reachable) {
            identity.extend_from_slice(&(scope.len() as u64).to_be_bytes());
            identity.extend_from_slice(scope.as_bytes());
            identity.extend_from_slice(&row.to_be_bytes());
        }
        let fingerprint = sha256_bounded(&identity, MAX_POLICY_BYTES)
            .map_err(|_| ScopePolicyError::PolicyTooLarge)?;
        Ok(Self { inner: Arc::new(CompiledPolicy { revision, scopes, reachable, fingerprint }) })
    }

    pub fn exact(revision: u64) -> Result<Self, ScopePolicyError> { Self::new(revision, Vec::new()) }
    pub fn revision(&self) -> u64 { self.inner.revision }

    /// Semantic policy identity for trusted cache/configuration owners. This is
    /// not a principal, issuer, audience, revocation handle or authorization lease.
    pub fn fingerprint(&self) -> Sha256Digest { self.inner.fingerprint }

    /// Tests every required scope using exact or explicitly transitive grants.
    /// All supplied grants are validated before any successful early return.
    /// `true` means scope sufficiency only, not authentication or visibility.
    pub fn permits(&self, verified_grants: &[String], required: &RequiredScopes) -> Result<bool, ScopePolicyError> {
        validate_scopes(verified_grants)?;
        Ok(required.scopes.iter().all(|required| {
            verified_grants.iter().any(|grant| self.implies(grant, required))
        }))
    }

    fn implies(&self, granted: &str, required: &str) -> bool {
        if granted == required { return true; }
        let Ok(from) = self.inner.scopes.binary_search_by(|value| value.as_str().cmp(granted)) else { return false; };
        let Ok(to) = self.inner.scopes.binary_search_by(|value| value.as_str().cmp(required)) else { return false; };
        self.inner.reachable[from] & (1_u64 << to) != 0
    }

    /// Atomically projects only already-verified authorization scopes. On error
    /// every field remains unchanged. The effective set stays within the native
    /// provider admission ceiling of 64 scopes, each at most 256 bytes. Original
    /// grants outside the graph survive exactly; no wildcard or prefix expansion
    /// is performed. An empty anonymous context acquires no permissions.
    pub fn project_verified(&self, facts: &mut AuthContext) -> Result<(), ScopePolicyError> {
        validate_scopes(&facts.scopes)?;
        if facts.scopes.is_empty() { return Ok(()); }
        if facts.subject.as_ref().is_some_and(String::is_empty)
            || (facts.subject.is_none() && facts.session_owner().is_none())
        { return Err(ScopePolicyError::InvalidPrincipal); }
        let mut effective: BTreeSet<String> = facts.scopes.iter().cloned().collect();
        for grant in &facts.scopes {
            let Ok(index) = self.inner.scopes.binary_search(grant) else { continue; };
            let row = self.inner.reachable[index];
            for (target, scope) in self.inner.scopes.iter().enumerate() {
                if row & (1_u64 << target) != 0 && !effective.contains(scope) {
                    if effective.len() == MAX_SCOPES { return Err(ScopePolicyError::EffectiveScopeLimit); }
                    effective.insert(scope.clone());
                }
            }
        }
        facts.scopes = effective.into_iter().collect();
        Ok(())
    }
}

/// Authentication-provider composition: verification first, bounded projection
/// second, ordinary server authorization afterwards. This wraps the provider
/// installed with `ServerBuilder::auth_provider`; no middleware substitution or
/// request-metadata authority is involved. A verifier refusal is never retried.
///
/// Policy clones are immutable snapshots. Installing another revision requires
/// explicit host reconfiguration; this wrapper does not invalidate already-live
/// subscriptions, rewrite provider leases or implement hot policy replacement.
/// A lease-aware provider must apply the same policy on its revalidation path.
#[derive(Clone)]
pub struct ScopePolicyAuthProvider {
    provider: Arc<dyn AuthProvider>,
    policy: ScopeImplicationPolicy,
}

impl ScopePolicyAuthProvider {
    pub fn new<P: AuthProvider + 'static>(provider: P, policy: ScopeImplicationPolicy) -> Self {
        Self { provider: Arc::new(provider), policy }
    }
    pub fn policy(&self) -> &ScopeImplicationPolicy { &self.policy }
}

impl fmt::Debug for ScopePolicyAuthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopePolicyAuthProvider").field("policy", &self.policy).finish_non_exhaustive()
    }
}

impl AuthProvider for ScopePolicyAuthProvider {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        let mut facts = self.provider.authenticate(ctx, request)?;
        self.policy.project_verified(&mut facts)
            .map_err(|_| McpError::internal_error("authentication scope policy rejected provider facts"))?;
        Ok(facts)
    }
}

fn validate_scope(scope: &str) -> Result<(), ScopePolicyError> {
    if scope.is_empty() || scope.len() > MAX_SCOPE_BYTES
        || !scope.bytes().all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
    { return Err(ScopePolicyError::InvalidScope); }
    Ok(())
}

fn validate_scopes(scopes: &[String]) -> Result<(), ScopePolicyError> {
    if scopes.len() > MAX_SCOPES { return Err(ScopePolicyError::TooManyScopes); }
    for scope in scopes { validate_scope(scope)?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(values: &[&str]) -> Vec<String> { values.iter().map(|value| (*value).to_owned()).collect() }
    fn policy(edges: &[(&str, &str)]) -> ScopeImplicationPolicy {
        ScopeImplicationPolicy::new(7, edges.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()).unwrap()
    }
    fn facts(values: &[&str]) -> AuthContext {
        let mut facts = AuthContext::with_subject("verified-principal");
        facts.scopes = scopes(values);
        facts
    }

    #[test]
    fn transitive_grant_satisfies_all_requirements_without_reducing_the_challenge() {
        let policy = policy(&[("admin", "write"), ("write", "read")]);
        let required = RequiredScopes::new(scopes(&["write", "read", "write"])).unwrap();
        assert!(policy.permits(&scopes(&["admin"]), &required).unwrap());
        assert!(!policy.permits(&scopes(&["read"]), &required).unwrap());
        assert_eq!(required.challenge_scope(), "read write");
        let original = required.as_slice().to_vec();
        assert!(!policy.permits(&scopes(&["unrelated"]), &required).unwrap());
        assert_eq!(required.as_slice(), original);
    }

    #[test]
    fn removing_one_edge_removes_only_its_derived_authority() {
        let complete = policy(&[("admin", "write"), ("write", "read")]);
        let missing = policy(&[("admin", "write")]);
        let read = RequiredScopes::new(scopes(&["read"])).unwrap();
        assert!(complete.permits(&scopes(&["admin"]), &read).unwrap());
        assert!(!missing.permits(&scopes(&["admin"]), &read).unwrap());
        assert!(missing.permits(&scopes(&["read"]), &read).unwrap());
        assert!(complete.permits(&scopes(&["admin"]), &read).unwrap());
    }

    #[test]
    fn scope_spelling_and_wildcards_have_no_implicit_hierarchy() {
        let policy = policy(&[("explicit:*", "allowed")]);
        let required = RequiredScopes::new(scopes(&["files:read"])).unwrap();
        for granted in ["*", "files", "files:*", "FILES:READ", "files:read:all"] {
            assert!(!policy.permits(&scopes(&[granted]), &required).unwrap());
        }
        assert!(policy.permits(&scopes(&["files:read"]), &required).unwrap());
        assert!(policy.permits(&scopes(&["explicit:*"]), &RequiredScopes::new(scopes(&["allowed"])).unwrap()).unwrap());
        assert!(!policy.permits(&scopes(&["explicit:*"]), &RequiredScopes::new(scopes(&["other"])).unwrap()).unwrap());
    }

    #[test]
    fn cycles_and_duplicate_edges_fail_during_configuration() {
        for edges in [vec![("a", "a")], vec![("a", "b"), ("b", "a")],
            vec![("a", "b"), ("b", "c"), ("c", "a")]]
        {
            let edges = edges.into_iter().map(|(a, b)| (a.to_owned(), b.to_owned())).collect();
            assert!(matches!(ScopeImplicationPolicy::new(1, edges), Err(ScopePolicyError::CyclicImplication)));
        }
        assert!(matches!(ScopeImplicationPolicy::new(1, vec![("a".to_owned(), "b".to_owned()); 2]),
            Err(ScopePolicyError::RepeatedImplication)));
        assert!(ScopeImplicationPolicy::new(1, vec![("a".to_owned(), "b".to_owned())]).is_ok());
    }

    #[test]
    fn configuration_limits_include_the_highest_bit_and_longest_transitive_path() {
        let chain = |count: usize| (1..count).map(|i| (format!("s{:02}", i - 1), format!("s{i:02}"))).collect();
        let policy = ScopeImplicationPolicy::new(1, chain(64)).unwrap();
        let last = RequiredScopes::new(scopes(&["s63"])).unwrap();
        assert!(policy.permits(&scopes(&["s00"]), &last).unwrap());
        let mut facts = facts(&["s00"]);
        policy.project_verified(&mut facts).unwrap();
        assert_eq!(facts.scopes.len(), 64);
        assert!(matches!(ScopeImplicationPolicy::new(1, chain(65)), Err(ScopePolicyError::TooManyScopes)));
        assert!(matches!(ScopeImplicationPolicy::new(1, vec![("a".to_owned(), "b".to_owned()); 257]),
            Err(ScopePolicyError::TooManyImplications)));
        assert!(matches!(ScopeImplicationPolicy::exact(0), Err(ScopePolicyError::ZeroRevision)));
    }

    #[test]
    fn diamond_paths_deduplicate_effective_permissions() {
        let policy = policy(&[("admin", "left"), ("admin", "right"), ("left", "read"), ("right", "read")]);
        let mut facts = facts(&["admin", "outside", "admin"]);
        policy.project_verified(&mut facts).unwrap();
        assert_eq!(facts.scopes, scopes(&["admin", "left", "outside", "read", "right"]));
        let before = serde_json::to_vec(&facts).unwrap();
        policy.project_verified(&mut facts).unwrap();
        assert_eq!(serde_json::to_vec(&facts).unwrap(), before);
    }

    #[test]
    fn projection_preserves_principal_claims_and_session_owner_exactly() {
        let owner = Sha256Digest::from_bytes([19; 32]);
        let mut facts = facts(&["write"]).with_session_owner(owner);
        facts.claims = Some(serde_json::json!({"scope":"write", "tenant":"provider-selected"}));
        let subject = facts.subject.clone();
        let claims = facts.claims.clone();
        policy(&[("write", "read")]).project_verified(&mut facts).unwrap();
        assert_eq!(facts.subject, subject);
        assert_eq!(facts.session_owner(), Some(owner));
        assert_eq!(facts.claims, claims);
        assert_eq!(facts.scopes, scopes(&["read", "write"]));
    }

    #[test]
    fn projection_limit_failure_is_atomic() {
        let policy = policy(&[("admin", "extra")]);
        let mut facts = facts(&["admin"]);
        facts.scopes.extend((0..63).map(|i| format!("unrelated-{i}")));
        let before = serde_json::to_vec(&facts).unwrap();
        assert_eq!(policy.project_verified(&mut facts), Err(ScopePolicyError::EffectiveScopeLimit));
        assert_eq!(serde_json::to_vec(&facts).unwrap(), before);
        facts.scopes.pop();
        policy.project_verified(&mut facts).unwrap();
        assert_eq!(facts.scopes.len(), 64);
    }

    #[test]
    fn every_grant_is_validated_even_for_empty_or_already_satisfied_requirements() {
        let policy = ScopeImplicationPolicy::exact(1).unwrap();
        for required in [RequiredScopes::new(vec![]).unwrap(), RequiredScopes::new(scopes(&["read"])).unwrap()] {
            assert_eq!(policy.permits(&scopes(&["read", "invalid scope"]), &required), Err(ScopePolicyError::InvalidScope));
        }
        assert!(policy.permits(&[], &RequiredScopes::new(vec![]).unwrap()).unwrap());
        assert_eq!(policy.permits(&vec!["read".to_owned(); 65], &RequiredScopes::new(vec![]).unwrap()), Err(ScopePolicyError::TooManyScopes));
    }

    #[test]
    fn anonymous_facts_cannot_acquire_implied_permissions() {
        let policy = policy(&[("admin", "read")]);
        let mut anonymous = AuthContext::anonymous();
        policy.project_verified(&mut anonymous).unwrap();
        assert!(anonymous.scopes.is_empty());
        anonymous.scopes = scopes(&["admin"]);
        let before = serde_json::to_vec(&anonymous).unwrap();
        assert_eq!(policy.project_verified(&mut anonymous), Err(ScopePolicyError::InvalidPrincipal));
        assert_eq!(serde_json::to_vec(&anonymous).unwrap(), before);
    }

    #[test]
    fn malformed_scopes_and_login_permissions_are_not_policy_or_challenge_authority() {
        for invalid in ["", "two scopes", "scope\"quote", "scope\\slash", "line\r\n", "unicode-\u{e9}"] {
            assert!(matches!(RequiredScopes::new(scopes(&[invalid])), Err(ScopePolicyError::InvalidScope)));
            assert!(matches!(ScopeImplicationPolicy::new(1, vec![(invalid.to_owned(), "read".to_owned())]), Err(ScopePolicyError::InvalidScope)));
        }
        assert!(RequiredScopes::new(vec!["x".repeat(256)]).is_ok());
        assert!(RequiredScopes::new(vec!["x".repeat(257)]).is_err());
        assert!(matches!(RequiredScopes::new(scopes(&["offline_access"])), Err(ScopePolicyError::OfflineAccess)));
        assert!(matches!(ScopeImplicationPolicy::new(1, vec![("admin".to_owned(), "offline_access".to_owned())]), Err(ScopePolicyError::OfflineAccess)));
    }

    #[test]
    fn policy_identity_binds_revision_and_semantics_not_input_order() {
        let left = policy(&[("a", "b"), ("b", "c")]);
        let reordered = policy(&[("b", "c"), ("a", "b")]);
        assert_eq!(left.fingerprint(), reordered.fingerprint());
        assert_ne!(left.fingerprint(), policy(&[("a", "b")]).fingerprint());
        assert_ne!(left.fingerprint(), ScopeImplicationPolicy::new(8,
            vec![("a".to_owned(), "b".to_owned()), ("b".to_owned(), "c".to_owned())]).unwrap().fingerprint());
        let clone = left.clone();
        assert_eq!(clone.revision(), 7);
        assert_eq!(clone.fingerprint(), left.fingerprint());
    }

    #[test]
    fn provider_verifies_native_credentials_before_scope_projection() {
        use crate::{StaticTokenVerifier, TokenAuthProvider};
        let verifier = StaticTokenVerifier::new([("test-only-token".to_owned(), facts(&["admin"]))]).unwrap();
        let provider = ScopePolicyAuthProvider::new(TokenAuthProvider::new(verifier), policy(&[("admin", "read")]));
        let ctx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let request = |authorization| AuthRequest {
            method: "tools/call", params: None, transport_authorization: authorization, request_id: 1,
        };
        let admitted = provider.authenticate(&ctx, request(Some("Bearer test-only-token"))).unwrap();
        assert_eq!(admitted.scopes, scopes(&["admin", "read"]));
        assert!(provider.authenticate(&ctx, request(Some("Bearer wrong-token"))).is_err());
        assert!(provider.authenticate(&ctx, request(None)).is_err());
    }

    #[test]
    fn policy_and_requirement_debug_output_does_not_disclose_scope_names() {
        let policy = policy(&[("private-canary-admin", "private-canary-read")]);
        let required = RequiredScopes::new(scopes(&["private-canary-read"])).unwrap();
        assert!(!format!("{policy:?} {required:?}").contains("private-canary"));
    }
}
